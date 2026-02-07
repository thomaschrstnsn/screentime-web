use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use anyhow::Result;
use async_nats::jetstream::{self, consumer::DeliverPolicy};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, Response},
    routing::{get, get_service},
    Router,
};
use clap::Parser;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{error, info};
use tracing_subscriber;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Config {
    #[arg(short, long, default_value = "127.0.0.1:3000")]
    bind: String,

    #[arg(short = 'u', long, default_value = "nats://localhost:4222")]
    nats_url: String,

    #[arg(short = 's', long, default_value = "time.obs.>")]
    nats_subject: String,

    #[arg(long, default_value = "OBSERVATIONS")]
    nats_stream: String,
}

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

#[derive(Clone)]
pub struct AppState {
    pub screen_time_data: Arc<RwLock<HashMap<String, Vec<ScreenTimeEvent>>>>,
    pub event_sender: broadcast::Sender<ScreenTimeEvent>,
    pub jetstream: jetstream::Context,
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    tracing_subscriber::fmt::init();

    let config = Config::parse();

    let nats_url = env::var("NATS_URL").unwrap_or_else(|_| config.nats_url.clone());
    let nats_subject = env::var("NATS_SUBJECT").unwrap_or_else(|_| config.nats_subject.clone());
    let nats_stream = env::var("NATS_STREAM").unwrap_or_else(|_| config.nats_stream.clone());

    info!("Connecting to NATS at: {}", nats_url);
    let nats_client = async_nats::connect(&nats_url).await?;
    let jetstream = jetstream::new(nats_client);

    let (event_sender, _) = broadcast::channel::<ScreenTimeEvent>(1000);

    let app_state = AppState {
        screen_time_data: Arc::new(RwLock::new(HashMap::new())),
        event_sender,
        jetstream: jetstream.clone(),
    };

    let nats_state = app_state.clone();
    let nats_subject_clone = nats_subject.clone();
    let nats_stream_clone = nats_stream.clone();
    tokio::spawn(async move {
        subscribe_to_screentime(nats_state, nats_subject_clone, nats_stream_clone).await;
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(websocket_handler))
        .route("/api/screentime", get(get_screentime_data))
        .nest_service("/static", get_service(ServeDir::new("static")))
        .layer(ServiceBuilder::new().layer(CorsLayer::permissive()))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    info!("Server running on http://{}", config.bind);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn subscribe_to_screentime(state: AppState, subject: String, stream_name: String) {
    // Get the stream
    let stream = match state.jetstream.get_stream(stream_name.clone()).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get JetStream stream '{}': {}", stream_name, e);
            return;
        }
    };

    info!("Found JetStream stream: {}", stream_name);

    // Create a pull consumer with LastPerSubject policy
    let consumer = match stream
        .create_consumer(jetstream::consumer::pull::Config {
            durable_name: None, // Ephemeral consumer
            deliver_policy: DeliverPolicy::LastPerSubject,
            filter_subject: subject.clone(),
            ..Default::default()
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create JetStream consumer: {}", e);
            return;
        }
    };

    info!("Created JetStream consumer for subject: {}", subject);

    // Get messages stream
    let mut messages = match consumer.messages().await {
        Ok(m) => m,
        Err(e) => {
            error!("Failed to get consumer messages: {}", e);
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

                info!("Received message on subject: {}", subject);
                if let Ok(payload_str) = std::str::from_utf8(&payload) {
                    info!("Payload: {}", payload_str);
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
                            error!("Failed to send event to subscribers: {}", e);
                        }

                        info!("Processed screentime event for: {}", key);
                    }
                    Err(e) => {
                        error!("Failed to parse message from {}: {}", subject, e);
                    }
                }

                // Acknowledge the message
                if let Err(e) = message.ack().await {
                    error!("Failed to ack message: {}", e);
                }
            }
            Err(e) => {
                error!("Error receiving message: {}", e);
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

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
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
            if let Some(latest_event) = events.last() {
                if let Ok(json) = serde_json::to_string(latest_event) {
                    if socket.send(Message::Text(json)).await.is_err() {
                        return;
                    }
                }
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
