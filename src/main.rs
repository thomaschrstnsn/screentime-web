use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_nats::jetstream::{self, consumer::DeliverPolicy};
use axum::{
    Extension, Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    middleware,
    response::{Html, Response},
    routing::{get, get_service},
};
use clap::Parser;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast};
use tower_http::services::ServeDir;

mod adjust;
mod auth;
use auth::{AuthMode, CloudflareIdentity};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Config {
    #[arg(short, long, default_value = "127.0.0.1:3000")]
    bind: String,

    #[arg(short = 'u', long, default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Local development only: treat every request as this adjuster email
    #[cfg(debug_assertions)]
    #[arg(long, env = "DEV_IDENTITY")]
    dev_identity: Option<String>,
}

/// JetStream stream holding observations; created and owned by the collector.
const OBSERVATIONS_STREAM: &str = "OBSERVATIONS";
/// Subject filter for observations, published as `time.obs.<host>.<user>`.
const OBSERVATIONS_SUBJECT: &str = "time.obs.>";
/// How long to wait between attempts to find the observations stream.
const STREAM_RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenTimeEvent {
    pub host: String,
    pub user: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub left_day: i64,
    pub spent_balance: i64,
    pub spent_month: i64,
    pub spent_week: i64,
    pub spent_day: i64,
}

/// Latest observations per `host:user`, as held by web.
pub type Observations = Arc<RwLock<HashMap<String, Vec<ScreenTimeEvent>>>>;

#[derive(Clone)]
pub struct AppState {
    pub screen_time_data: Observations,
    pub event_sender: broadcast::Sender<ScreenTimeEvent>,
    pub jetstream: jetstream::Context,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::parse();

    // Decide how requests get their identity; refuses a dev identity alongside CF Access
    let auth_mode = auth::resolve_auth_mode(
        env::var("CF_ACCESS_TEAM").ok(),
        env::var("CF_ACCESS_AUD").ok(),
        #[cfg(debug_assertions)]
        config.dev_identity.clone(),
    )
    .map_err(anyhow::Error::msg)?;
    match &auth_mode {
        AuthMode::CfAccess(cf_config) => tracing::info!(
            "Cloudflare Access authentication enabled for team: {}",
            cf_config.jwks_cache.team()
        ),
        AuthMode::Anonymous => tracing::info!(
            "CF_ACCESS_TEAM and/or CF_ACCESS_AUD not set - running without Cloudflare Access authentication"
        ),
        #[cfg(debug_assertions)]
        AuthMode::DevIdentity(email) => tracing::warn!(
            "!!! DEV IDENTITY ENABLED: every request is treated as adjuster {} without authentication. Never use this in production !!!",
            email
        ),
    }

    let nats_url = env::var("NATS_URL").unwrap_or_else(|_| config.nats_url.clone());

    tracing::info!("Connecting to NATS at: {}", nats_url);
    let nats_client = async_nats::connect(&nats_url).await?;
    let jetstream = jetstream::new(nats_client.clone());

    let (event_sender, _) = broadcast::channel::<ScreenTimeEvent>(1000);

    let app_state = AppState {
        screen_time_data: Arc::new(RwLock::new(HashMap::new())),
        event_sender,
        jetstream: jetstream.clone(),
    };

    let nats_state = app_state.clone();
    tokio::spawn(async move {
        subscribe_to_screentime(nats_state).await;
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(websocket_handler))
        .route("/api/screentime", get(get_screentime_data))
        .route("/api/me", get(me))
        .nest_service("/static", get_service(ServeDir::new("static")));
    let adjust_state = adjust::AdjustState {
        observations: app_state.screen_time_data.clone(),
        send_request: adjust::nats_sender(nats_client),
    };
    let app = app.merge(adjust::routes().with_state(adjust_state));
    let app = with_auth(app, auth_mode).with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!("Server running on http://{}", config.bind);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Attaches the viewer's identity (or `None`) to every request routed by `router`.
fn with_auth<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    auth_mode: AuthMode,
) -> Router<S> {
    router.layer(middleware::from_fn(move |req, next| {
        auth::auth_middleware(auth_mode.clone(), req, next)
    }))
}

/// Runs `op` until it succeeds, warning and sleeping `interval` after each failure.
async fn retry_every<T, E, F, Fut>(interval: Duration, label: &str, mut op: F) -> T
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    loop {
        match op().await {
            Ok(v) => return v,
            Err(e) => {
                tracing::warn!("{} unavailable, retrying in {:?}: {}", label, interval, e);
                tokio::time::sleep(interval).await;
            }
        }
    }
}

async fn subscribe_to_screentime(state: AppState) {
    // The collector creates the stream; wait for it rather than giving up
    let stream = retry_every(
        STREAM_RETRY_INTERVAL,
        &format!("JetStream stream '{}'", OBSERVATIONS_STREAM),
        || state.jetstream.get_stream(OBSERVATIONS_STREAM),
    )
    .await;

    tracing::info!("Found JetStream stream: {}", OBSERVATIONS_STREAM);

    // Create a pull consumer with LastPerSubject policy
    let consumer = match stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: None, // Ephemeral consumer
            deliver_policy: DeliverPolicy::LastPerSubject,
            filter_subject: OBSERVATIONS_SUBJECT.to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to create JetStream consumer: {}", e);
            return;
        }
    };

    tracing::info!(
        "Created JetStream consumer for subject: {}",
        OBSERVATIONS_SUBJECT
    );

    // Get messages stream
    let mut messages = match consumer.messages().await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("Failed to get consumer messages: {}", e);
            return;
        }
    };

    while let Some(message_result) = messages.next().await {
        match message_result {
            Ok(message) => {
                let subject = message.subject.clone();
                let payload = message.payload.clone();
                let timestamp = message
                    .info()
                    .ok()
                    .map(|i| i.published)
                    .map(|t| chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::from(t)))
                    .unwrap_or_else(chrono::Utc::now);

                tracing::info!("Received message on subject: {}", subject);
                if let Ok(payload_str) = std::str::from_utf8(&payload) {
                    tracing::info!("Payload: {}", payload_str);
                }

                match parse_nats_message(&subject, &payload, timestamp) {
                    Ok(event) => {
                        let key = format!("{}:{}", event.host, event.user);
                        // ... existing code ...
                        {
                            let mut data = state.screen_time_data.write().await;
                            let events = data.entry(key.clone()).or_insert_with(Vec::new);
                            events.push(event.clone());

                            if events.len() > 1000 {
                                events.remove(0);
                            }
                        }

                        if let Err(e) = state.event_sender.send(event) {
                            tracing::error!("Failed to send event to subscribers: {}", e);
                        }

                        tracing::info!("Processed screentime event for: {}", key);
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse message from {}: {}", subject, e);
                    }
                }

                // Acknowledge the message
                if let Err(e) = message.ack().await {
                    tracing::error!("Failed to ack message: {}", e);
                }
            }
            Err(e) => {
                tracing::error!("Error receiving message: {}", e);
            }
        }
    }
}

fn parse_nats_message(
    subject: &str,
    payload: &[u8],
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Result<ScreenTimeEvent> {
    let parts: Vec<&str> = subject.split('.').collect();
    if parts.len() < 4 || parts[0] != "time" || parts[1] != "obs" {
        return Err(anyhow::anyhow!("Invalid subject format"));
    }

    let host = parts[2].to_string();
    let user = parts[3].to_string();

    let json_str = String::from_utf8_lossy(payload);
    let data: serde_json::Value = serde_json::from_str(&json_str)?;

    let event = ScreenTimeEvent {
        host,
        user,
        timestamp,
        left_day: data.get("left_day").and_then(|v| v.as_i64()).unwrap_or(0),
        spent_balance: data
            .get("spent_balance")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        spent_month: data
            .get("spent_month")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        spent_week: data.get("spent_week").and_then(|v| v.as_i64()).unwrap_or(0),
        spent_day: data.get("spent_day").and_then(|v| v.as_i64()).unwrap_or(0),
    };

    Ok(event)
}

async fn index(Extension(identity): Extension<Option<CloudflareIdentity>>) -> Html<&'static str> {
    if let Some(user) = identity {
        tracing::info!("Index page accessed by: {}", user.email);
    } else {
        tracing::info!("Index page accessed anonymously");
    }
    Html(include_str!("../static/index.html"))
}

/// What the page needs to know about its viewer.
#[derive(Debug, Serialize)]
struct Me {
    email: Option<String>,
    /// Any verified identity is an adjuster; the CF Access policy is the allowlist.
    adjuster: bool,
}

async fn me(Extension(identity): Extension<Option<CloudflareIdentity>>) -> axum::Json<Me> {
    axum::Json(Me {
        adjuster: identity.is_some(),
        email: identity.map(|i| i.email),
    })
}

async fn get_screentime_data(
    State(state): State<AppState>,
) -> axum::Json<HashMap<String, Vec<ScreenTimeEvent>>> {
    let data = state.screen_time_data.read().await;
    axum::Json(data.clone())
}

async fn websocket_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_websocket(socket, state))
}

async fn handle_websocket(mut socket: WebSocket, state: AppState) {
    // Send existing data to the new client
    {
        let data = state.screen_time_data.read().await;
        for events in data.values() {
            if let Some(latest_event) = events.last()
                && let Ok(json) = serde_json::to_string(latest_event)
                && socket.send(Message::Text(json.into())).await.is_err()
            {
                return;
            }
        }
    }

    let mut recv = state.event_sender.subscribe();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) => {
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::error!("WebSocket error: {}", e);
                        break;
                    }
                    None => break,
                }
            }
            event = recv.recv() => {
                match event {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event)
                            && socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::CfAccessConfig;
    use axum::{body::Body, http::Request};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::ServiceExt;

    async fn get_me(auth: AuthMode, request: Request<Body>) -> serde_json::Value {
        let app = with_auth(Router::new().route("/api/me", get(me)), auth);
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn api_me_reports_adjuster_for_valid_cf_access_jwt() {
        use jsonwebtoken::{EncodingKey, Header, encode};

        let secret = b"test-secret";
        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [{"kty": "oct", "kid": "test-kid", "alg": "HS256", "k": "dGVzdC1zZWNyZXQ"}]
        }))
        .unwrap();
        let auth = AuthMode::CfAccess(CfAccessConfig {
            jwks_cache: auth::JwksCache::with_jwks("myteam".to_string(), jwks),
            audience: "my-aud".to_string(),
        });

        let now = chrono::Utc::now().timestamp();
        let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("test-kid".to_string());
        let token = encode(
            &header,
            &serde_json::json!({
                "email": "parent@example.com",
                "sub": "abc",
                "iat": now,
                "exp": now + 3600,
                "aud": ["my-aud"],
                "iss": "https://myteam.cloudflareaccess.com",
            }),
            &EncodingKey::from_secret(secret),
        )
        .unwrap();

        let request = Request::get("/api/me")
            .header("Cf-Access-Jwt-Assertion", token)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            get_me(auth, request).await,
            serde_json::json!({"email": "parent@example.com", "adjuster": true})
        );
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn api_me_reports_dev_identity_as_adjuster() {
        let request = Request::get("/api/me").body(Body::empty()).unwrap();
        assert_eq!(
            get_me(AuthMode::DevIdentity("me@example.com".to_string()), request).await,
            serde_json::json!({"email": "me@example.com", "adjuster": true})
        );
    }

    #[tokio::test]
    async fn api_me_is_anonymous_without_identity() {
        let request = Request::get("/api/me").body(Body::empty()).unwrap();
        assert_eq!(
            get_me(AuthMode::Anonymous, request).await,
            serde_json::json!({"email": null, "adjuster": false})
        );
    }

    #[tokio::test]
    async fn api_me_is_anonymous_without_jwt_when_cf_access_is_configured() {
        let jwks = jsonwebtoken::jwk::JwkSet { keys: vec![] };
        let auth = AuthMode::CfAccess(CfAccessConfig {
            jwks_cache: auth::JwksCache::with_jwks("myteam".to_string(), jwks),
            audience: "my-aud".to_string(),
        });
        let request = Request::get("/api/me").body(Body::empty()).unwrap();
        assert_eq!(
            get_me(auth, request).await,
            serde_json::json!({"email": null, "adjuster": false})
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_every_waits_interval_between_failures_and_returns_first_ok() {
        let attempts = AtomicU32::new(0);
        let started = tokio::time::Instant::now();

        let result: u32 = retry_every(Duration::from_secs(5), "thing", || async {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 {
                Err(anyhow::anyhow!("not yet"))
            } else {
                Ok(n)
            }
        })
        .await;

        assert_eq!(result, 3);
        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }
}
