//! Time adjustments: `POST /api/time-adjustments`, sent to the collector as a NATS request.

mod contract;

use std::sync::Arc;
use std::time::Duration;

use async_nats::RequestErrorKind;
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Observations;
use crate::auth::CloudflareIdentity;

/// Largest time adjustment either way (8 h); the collector enforces the same bound.
const MAX_DELTA_SECONDS: i64 = 8 * 60 * 60;

/// How a NATS request to the collector ended.
#[derive(Debug, Clone)]
pub enum Delivery {
    Replied(Bytes),
    /// Nothing subscribed to the subject, so no change was made.
    NoResponders,
    /// No reply in time; the change may or may not have happened.
    TimedOut,
}

/// Sends `payload` as a NATS request on `subject` and waits for the reply.
pub type SendRequest = Arc<dyn Fn(String, Bytes) -> BoxFuture<'static, Delivery> + Send + Sync>;

/// How long web waits for the collector's reply; it never retries on its own.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// Sends requests over `client`, waiting [`REPLY_TIMEOUT`] for each reply.
pub fn nats_sender(client: async_nats::Client) -> SendRequest {
    Arc::new(move |subject, payload| {
        let client = client.clone();
        Box::pin(async move {
            let request = async_nats::Request::new()
                .payload(payload)
                .timeout(Some(REPLY_TIMEOUT));
            match client.send_request(subject, request).await {
                Ok(message) => Delivery::Replied(message.payload),
                Err(e) => match e.kind() {
                    RequestErrorKind::NoResponders => Delivery::NoResponders,
                    RequestErrorKind::TimedOut => Delivery::TimedOut,
                    // The request may have gone out before failing, so the outcome is unknown
                    _ => {
                        tracing::warn!("Time adjustment request failed: {}", e);
                        Delivery::TimedOut
                    }
                },
            }
        })
    })
}

#[derive(Clone)]
pub struct AdjustState {
    pub observations: Observations,
    pub send_request: SendRequest,
}

/// Body the page posts to `/api/time-adjustments`.
#[derive(Debug, Deserialize)]
struct AdjustRequest {
    host: String,
    user: String,
    delta_seconds: i64,
}

/// The `status` the page gets when there is no collector reply to pass through.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum NoReply {
    /// Nothing listening on the subject, so no change was made.
    Unreachable,
    /// No reply in time; the change may or may not have happened.
    Timeout,
    /// A reply web can't read; the change may or may not have happened.
    Unreadable,
}

impl NoReply {
    fn answer(self) -> Map<String, Value> {
        Map::from_iter([("status".to_string(), serde_json::json!(self))])
    }
}

/// The collector's reply if it is a valid [`contract::Reply`], kept whole so fields web
/// doesn't know still reach the page.
fn readable_reply(reply: &[u8]) -> Option<Map<String, Value>> {
    let reply: Map<String, Value> = serde_json::from_slice(reply).ok()?;
    serde_json::from_value::<contract::Reply>(Value::Object(reply.clone())).ok()?;
    Some(reply)
}

pub fn routes() -> Router<AdjustState> {
    Router::new().route("/api/time-adjustments", post(adjust_time))
}

async fn adjust_time(
    State(state): State<AdjustState>,
    Extension(identity): Extension<Option<CloudflareIdentity>>,
    body: Result<Json<AdjustRequest>, JsonRejection>,
) -> Response {
    let Some(identity) = identity else {
        return (
            StatusCode::UNAUTHORIZED,
            "Time adjustments need a verified identity",
        )
            .into_response();
    };
    let body = match body {
        Ok(Json(body)) => body,
        // Requiring JSON forces a CORS preflight, which keeps cross-origin posts out
        Err(JsonRejection::MissingJsonContentType(e)) => {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, e.body_text()).into_response();
        }
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    if body.delta_seconds == 0
        || !(-MAX_DELTA_SECONDS..=MAX_DELTA_SECONDS).contains(&body.delta_seconds)
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("delta_seconds must be non-zero and within ±{MAX_DELTA_SECONDS}"),
        )
            .into_response();
    }
    // Only pairs seen on `time.obs`, which also keeps `.`, `*`, `>` and spaces out of the subject
    let key = format!("{}:{}", body.host, body.user);
    if !state.observations.read().await.contains_key(&key) {
        return (StatusCode::NOT_FOUND, format!("No observations for {key}")).into_response();
    }
    let request = contract::Request {
        request_id: uuid::Uuid::new_v4().to_string(),
        adjuster: identity.email,
        delta_seconds: body.delta_seconds,
    };
    tracing::info!(
        "Time adjustment {} by {} for {}: {:+}s",
        request.request_id,
        request.adjuster,
        key,
        request.delta_seconds
    );
    let subject = format!("time.adjust.{}.{}", body.host, body.user);
    let payload = serde_json::to_vec(&request).expect("request serializes");

    let mut answer = match (state.send_request)(subject, payload.into()).await {
        Delivery::Replied(reply) => readable_reply(&reply).unwrap_or_else(|| {
            tracing::error!(
                "Unreadable reply to time adjustment {}: {}",
                request.request_id,
                String::from_utf8_lossy(&reply)
            );
            NoReply::Unreadable.answer()
        }),
        Delivery::NoResponders => NoReply::Unreachable.answer(),
        Delivery::TimedOut => NoReply::Timeout.answer(),
    };
    answer.insert("request_id".into(), request.request_id.into());
    let answer = Value::Object(answer);
    tracing::info!("Time adjustment ended: {}", answer);
    Json(answer).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, http::StatusCode};
    use std::sync::Mutex;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    const REPLY_OK: &str = include_str!("examples/reply_ok.json");

    /// Every NATS request a fake sender was asked to send, as (subject, payload).
    type Sent = Arc<Mutex<Vec<(String, Bytes)>>>;

    /// Records each NATS request and answers every one with `delivery`.
    fn fake_sender(delivery: Delivery) -> (SendRequest, Sent) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = sent.clone();
        let send: SendRequest = Arc::new(move |subject, payload| {
            log.lock().unwrap().push((subject, payload));
            let delivery = delivery.clone();
            Box::pin(async move { delivery })
        });
        (send, sent)
    }

    fn observed(keys: &[&str]) -> Observations {
        let event = |key: &str| {
            let (host, user) = key.split_once(':').unwrap();
            crate::ScreenTimeEvent {
                host: host.into(),
                user: user.into(),
                timestamp: chrono::Utc::now(),
                left_day: 3600,
                spent_balance: 0,
                spent_month: 0,
                spent_week: 0,
                spent_day: 0,
            }
        };
        Arc::new(RwLock::new(
            keys.iter()
                .map(|k| (k.to_string(), vec![event(k)]))
                .collect(),
        ))
    }

    fn adjuster() -> Identity {
        Some(CloudflareIdentity {
            email: "me@example.com".into(),
            sub: "abc".into(),
            identity_nonce: None,
            iat: 0,
            exp: u64::MAX,
        })
    }

    fn post(content_type: Option<&str>, body: &str) -> HttpRequest<Body> {
        let mut request = HttpRequest::post("/api/time-adjustments");
        if let Some(ct) = content_type {
            request = request.header("content-type", ct);
        }
        request.body(Body::from(body.to_string())).unwrap()
    }

    fn json_post(body: &str) -> HttpRequest<Body> {
        post(Some("application/json"), body)
    }

    /// The identity the auth middleware attached, or `None` for an anonymous viewer.
    type Identity = Option<CloudflareIdentity>;

    const VALID: &str = r#"{"host":"test-vm","user":"junior","delta_seconds":900}"#;

    fn with_delta(delta: i64) -> String {
        format!(r#"{{"host":"test-vm","user":"junior","delta_seconds":{delta}}}"#)
    }

    async fn call(
        identity: Identity,
        send: SendRequest,
        request: HttpRequest<Body>,
    ) -> (StatusCode, Bytes) {
        let state = AdjustState {
            observations: observed(&["test-vm:junior"]),
            send_request: send,
        };
        let app = routes().with_state(state).layer(Extension(identity));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    fn json(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).unwrap()
    }

    #[tokio::test]
    async fn sends_request_to_collector_and_passes_its_reply_through() {
        let (send, sent) = fake_sender(Delivery::Replied(Bytes::from_static(REPLY_OK.as_bytes())));

        let (status, body) = call(adjuster(), send, json_post(VALID)).await;

        assert_eq!(status, StatusCode::OK);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (subject, payload) = &sent[0];
        assert_eq!(subject, "time.adjust.test-vm.junior");
        let request: contract::Request = serde_json::from_slice(payload).unwrap();
        assert_eq!(request.adjuster, "me@example.com");
        assert_eq!(request.delta_seconds, 900);
        assert_eq!(
            uuid::Uuid::parse_str(&request.request_id)
                .unwrap()
                .get_version_num(),
            4
        );

        let mut expected = json(REPLY_OK.as_bytes());
        expected["request_id"] = request.request_id.clone().into();
        assert_eq!(json(&body), expected);
    }

    #[tokio::test]
    async fn reply_fields_web_does_not_know_still_pass_through() {
        let reply = r#"{"status":"error","code":"later_code","message":"m","hint":"retry"}"#;
        let (send, sent) = fake_sender(Delivery::Replied(Bytes::from(reply)));
        let (status, body) = call(adjuster(), send, json_post(VALID)).await;
        assert_eq!(status, StatusCode::OK);
        let mut expected = json(reply.as_bytes());
        expected["request_id"] = sent_request_id(&sent).into();
        assert_eq!(json(&body), expected);
    }

    #[tokio::test]
    async fn unreadable_reply_is_reported_as_unreadable() {
        for reply in [&b"{\"status\":\"maybe\"}"[..], b"not json", b"[1]"] {
            let (send, sent) = fake_sender(Delivery::Replied(Bytes::from(reply)));
            let (status, body) = call(adjuster(), send, json_post(VALID)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                json(&body),
                serde_json::json!({"status": "unreadable", "request_id": sent_request_id(&sent)})
            );
        }
    }

    /// The `request_id` web put in the one request sent.
    fn sent_request_id(sent: &Sent) -> String {
        let sent = sent.lock().unwrap();
        let request: contract::Request = serde_json::from_slice(&sent[0].1).unwrap();
        request.request_id
    }

    #[tokio::test]
    async fn no_responders_is_reported_as_unreachable() {
        let (send, sent) = fake_sender(Delivery::NoResponders);
        let (status, body) = call(adjuster(), send, json_post(VALID)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json(&body),
            serde_json::json!({"status": "unreachable", "request_id": sent_request_id(&sent)})
        );
    }

    #[tokio::test]
    async fn no_reply_in_time_is_reported_as_timeout() {
        let (send, sent) = fake_sender(Delivery::TimedOut);
        let (status, body) = call(adjuster(), send, json_post(VALID)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json(&body),
            serde_json::json!({"status": "timeout", "request_id": sent_request_id(&sent)})
        );
    }

    /// Asserts web refused with `status` and a text body, without sending anything.
    async fn assert_refused(identity: Identity, request: HttpRequest<Body>, status: StatusCode) {
        let (send, sent) = fake_sender(Delivery::TimedOut);
        let (actual, body) = call(identity, send, request).await;
        assert_eq!(actual, status, "body: {}", String::from_utf8_lossy(&body));
        assert!(!body.is_empty(), "refusal should explain itself");
        assert!(sent.lock().unwrap().is_empty(), "nothing should reach NATS");
    }

    #[tokio::test]
    async fn anonymous_viewer_is_refused_with_401() {
        assert_refused(None, json_post(VALID), StatusCode::UNAUTHORIZED).await;
    }

    #[tokio::test]
    async fn body_that_is_not_declared_json_is_refused_with_415() {
        for content_type in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            assert_refused(
                adjuster(),
                post(content_type, VALID),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn malformed_body_is_refused_with_400() {
        for body in [
            "not json",
            r#"{"host":"test-vm","user":"junior"}"#,
            r#"{"host":"test-vm","user":"junior","delta_seconds":"900"}"#,
            r#"{"host":"test-vm","user":"junior","delta_seconds":1.5}"#,
        ] {
            assert_refused(adjuster(), json_post(body), StatusCode::BAD_REQUEST).await;
        }
    }

    #[tokio::test]
    async fn delta_that_is_zero_or_beyond_eight_hours_is_refused_with_400() {
        for delta in [0, 28801, -28801, i64::MAX, i64::MIN] {
            let body = with_delta(delta);
            assert_refused(adjuster(), json_post(&body), StatusCode::BAD_REQUEST).await;
        }
    }

    #[tokio::test]
    async fn delta_of_exactly_eight_hours_either_way_is_sent() {
        for delta in [28800, -28800] {
            let (send, sent) = fake_sender(Delivery::TimedOut);
            let body = with_delta(delta);
            let (status, _) = call(adjuster(), send, json_post(&body)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(sent.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn host_user_pair_without_observations_is_refused_with_404() {
        for (host, user) in [
            ("test-vm", "senior"),
            ("other-vm", "junior"),
            ("test-vm", "*"),
            ("test-vm.junior", ">"),
            ("test-vm", "junior x"),
        ] {
            let body = serde_json::json!({"host": host, "user": user, "delta_seconds": 900});
            assert_refused(
                adjuster(),
                json_post(&body.to_string()),
                StatusCode::NOT_FOUND,
            )
            .await;
        }
    }
}
