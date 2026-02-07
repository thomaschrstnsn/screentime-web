use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_nats::Client;
use axum::{
    extract::{State, WebSocketUpgrade, ws::Message},
    response::{Html, Response},
    routing::{get, get_service},
    Router,
};
use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{error, info};
use tracing_subscriber;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Config {
    #[arg(short, long, default_value = "127.0.0.1:3000")]
    bind: String,

    #[arg(short = 'u', long, default_value = "nats://localhost:4222")]
    nats_url: String,

    #[arg(short = 's', long, default_value = "time.obs.*")]
    nats_subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenTimeEvent {
    pub host: String,
    pub user: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub duration_seconds: u64,
    pub process_name: Option<String>,
    pub window_title: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub screen_time_data: Arc<RwLock<HashMap<String, Vec<ScreenTimeEvent>>>>,
    pub event_sender: broadcast::Sender<ScreenTimeEvent>,
    pub nats_client: Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::init();

    let config = Config::parse();
    
    let nats_url = env::var("NATS_URL").unwrap_or_else(|_| config.nats_url.clone());
    let nats_subject = env::var("NATS_SUBJECT").unwrap_or_else(|_| config.nats_subject.clone());

    info!("Connecting to NATS at: {}", nats_url);
    let nats_client = async_nats::connect(&nats_url).await?;
    let (event_sender, _) = broadcast::channel::<ScreenTimeEvent>(1000);

    let app_state = AppState {
        screen_time_data: Arc::new(RwLock::new(HashMap::new())),
        event_sender,
        nats_client: nats_client.clone(),
    };

    let nats_state = app_state.clone();
    let nats_subject_clone = nats_subject.clone();
    tokio::spawn(async move {
        subscribe_to_screentime(nats_state, nats_subject_clone).await;
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(websocket_handler))
        .route("/api/screentime", get(get_screentime_data))
        .nest_service("/static", get_service(ServeDir::new("static")))
        .layer(
            ServiceBuilder::new()
                .layer(CorsLayer::permissive())
        )
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    info!("Server running on http://{}", config.bind);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn subscribe_to_screentime(state: AppState, subject: String) {
    let mut subscriber = match state.nats_client.subscribe(&subject).await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to subscribe to NATS: {}", e);
            return;
        }
    };

    info!("Subscribed to NATS subject: {}", subject);

    while let Some(message) = subscriber.next().await {
        let subject = message.subject.clone();
        let payload = message.payload.clone();

        if let Ok(event) = parse_nats_message(&subject, &payload) {
            let key = format!("{}:{}", event.host, event.user);
            
            {
                let mut data = state.screen_time_data.write().await;
                let events = data.entry(key.clone()).or_insert_with(Vec::new);
                events.push(event.clone());
                
                if events.len() > 1000 {
                    events.remove(0);
                }
            }

            if let Err(e) = state.event_sender.send(event) {
                error!("Failed to send event to subscribers: {}", e);
            }
            
            info!("Processed screentime event for: {}", key);
        }
    }
}

fn parse_nats_message(subject: &str, payload: &[u8]) -> Result<ScreenTimeEvent> {
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
        timestamp: chrono::Utc::now(),
        duration_seconds: data.get("duration_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        process_name: data.get("process_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        window_title: data.get("window_title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    Ok(event)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn get_screentime_data(
    State(state): State<AppState>,
) -> axum::Json<HashMap<String, Vec<ScreenTimeEvent>>> {
    let data = state.screen_time_data.read().await;
    axum::Json(data.clone())
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_websocket(socket, state))
}

async fn handle_websocket(mut socket: axum::extract::ws::WebSocket, state: AppState) {
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
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    None => break,
                }
            }
            event = recv.recv() => {
                match event {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if socket.send(Message::Text(json)).await.is_err() {
                                break;
                            }
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