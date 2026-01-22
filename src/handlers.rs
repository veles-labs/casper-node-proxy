//! HTTP handlers and websocket/SSE bridges for proxy endpoints.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::convert::Infallible;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use governor::clock::{Clock, DefaultClock};

use crate::binary_proxy::BinaryPool;
use crate::jsonrpc_proxy;
use crate::rate_limit::binary_connection_rate_limiter;
use crate::state::{AppState, EventBus, EventEnvelope, NetworkState};

/// Build the application router with shared state.
pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/{network_name}/", get(network_root))
        .route(
            "/{network_name}/rpc",
            axum::routing::post(jsonrpc_proxy::rpc_proxy),
        )
        .route("/{network_name}/events", get(events_handler))
        .route("/{network_name}/binary", get(binary_ws))
        .with_state(state)
}

/// Query parameters for the events endpoint.
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// Inclusive event ID to start replaying from.
    pub start_from: Option<u64>,
}

/// Hypermedia response for the network root endpoint.
#[derive(Debug, Serialize)]
struct NetworkLinks {
    network_name: String,
    chain_name: String,
    rpc: String,
    events: String,
    binary: String,
}

/// Application-level error that maps to JSON HTTP responses.
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let payload = serde_json::json!({ "error": self.message });
        (self.status, Json(payload)).into_response()
    }
}

/// Return discoverable links for the configured network.
async fn network_root(
    Path(network_name): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let network = get_network(&state, &network_name)?;
    let links = NetworkLinks {
        network_name: network.config.network_name.clone(),
        chain_name: network.config.chain_name.clone(),
        rpc: format!("/{}/rpc", network_name),
        events: format!("/{}/events", network_name),
        binary: format!("/{}/binary", network_name),
    };
    Ok(Json(links))
}

/// Handle `/events` via websocket upgrade or SSE fallback.
async fn events_handler(
    Path(network_name): Path<String>,
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
    MaybeWebSocketUpgrade(ws): MaybeWebSocketUpgrade,
) -> Result<Response, AppError> {
    let network = get_network(&state, &network_name)?;
    let events = network.events.clone();
    let metrics = state.metrics.clone();
    let start_from = query.start_from;
    if let Some(ws) = ws {
        return Ok(ws.on_upgrade(move |socket| {
            handle_events_socket(socket, events, start_from, metrics, network_name)
        }));
    }
    Ok(sse_stream(events, start_from, metrics, network_name).into_response())
}

/// Proxy binary-port requests over a websocket connection.
async fn binary_ws(
    Path(network_name): Path<String>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let network = get_network(&state, &network_name)?;
    let pool = network.binary_pool.clone();
    let metrics = state.metrics.clone();
    let limiter = binary_connection_rate_limiter(
        state.binary_rate_limit_per_min,
        state.binary_rate_limit_burst,
    );
    Ok(ws.on_upgrade(move |socket| {
        handle_binary_socket(socket, pool, metrics, network_name, limiter)
    }))
}

/// Look up a configured network or return a 404 error.
pub(crate) fn get_network(state: &AppState, name: &str) -> Result<Arc<NetworkState>, AppError> {
    state
        .networks
        .get(name)
        .cloned()
        .ok_or_else(|| AppError::not_found(format!("unknown network: {name}")))
}

/// Stream events over websocket, optionally replaying from the backlog.
async fn handle_events_socket(
    socket: axum::extract::ws::WebSocket,
    events: Arc<EventBus>,
    start_from: Option<u64>,
    metrics: Arc<crate::metrics::AppMetrics>,
    network_name: String,
) {
    let _guard = metrics.events_connection_guard(&network_name);
    let (mut sender, mut receiver) = socket.split();
    let mut live_rx = events.subscribe();
    let mut last_sent_id = 0u64;

    let api_version = events.wait_for_api_version().await;
    if send_api_version(&mut sender, api_version).await.is_err() {
        return;
    }

    if let Some(start_from) = start_from {
        // Replay backlog first, then switch to the live broadcast stream.
        let backlog = events.snapshot_from(start_from);
        for event in backlog {
            if send_event(&mut sender, &event).await.is_err() {
                return;
            }
            last_sent_id = event.id;
        }
    }

    loop {
        tokio::select! {
            inbound = receiver.next() => {
                match inbound {
                    Some(Ok(axum::extract::ws::Message::Close(_))) | None => break,
                    Some(Ok(_)) => {},
                    Some(Err(err)) => {
                        debug!("events websocket error: {err}");
                        break;
                    }
                }
            }
            event = live_rx.recv() => {
                match event {
                    Ok(event) => {
                        if event.id > last_sent_id {
                            last_sent_id = event.id;
                            if send_event(&mut sender, &event).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("events websocket lagged; skipped {skipped} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn handle_binary_socket(
    socket: axum::extract::ws::WebSocket,
    pool: Arc<BinaryPool>,
    metrics: Arc<crate::metrics::AppMetrics>,
    network_name: String,
    limiter: crate::rate_limit::BinaryConnectionRateLimiter,
) {
    let _guard = metrics.binary_connection_guard(&network_name);
    let (mut sender, mut receiver) = socket.split();

    while let Some(message) = receiver.next().await {
        match message {
            Ok(axum::extract::ws::Message::Binary(payload)) => {
                metrics.increment_binary_requests(&network_name);
                if let Err(not_until) = limiter.check() {
                    let wait = not_until.wait_time_from(DefaultClock::default().now());
                    let error_payload = serde_json::json!({
                        "error": "rate limit exceeded",
                        "retry_after_secs": wait.as_secs(),
                    });
                    let _ = sender
                        .send(axum::extract::ws::Message::Text(
                            error_payload.to_string().into(),
                        ))
                        .await;
                    metrics.increment_binary_response(&network_name, "rate_limited");
                    continue;
                }
                match pool.send(payload.to_vec()).await {
                    Ok(response) => {
                        if sender
                            .send(axum::extract::ws::Message::Binary(response.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        let error_payload = serde_json::json!({ "error": err.to_string() });
                        let _ = sender
                            .send(axum::extract::ws::Message::Text(
                                error_payload.to_string().into(),
                            ))
                            .await;
                    }
                }
            }
            Ok(axum::extract::ws::Message::Close(_)) => break,
            Ok(_) => {}
            Err(err) => {
                debug!("binary websocket error: {err}");
                break;
            }
        }
    }
}

/// Serialize and send an event envelope as a websocket text frame.
async fn send_event(
    sender: &mut futures_util::stream::SplitSink<
        axum::extract::ws::WebSocket,
        axum::extract::ws::Message,
    >,
    event: &EventEnvelope,
) -> Result<(), axum::Error> {
    let payload = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    sender
        .send(axum::extract::ws::Message::Text(payload.into()))
        .await
}

/// Build an SSE stream that replays backlog and then follows live events.
fn sse_stream(
    events: Arc<EventBus>,
    start_from: Option<u64>,
    metrics: Arc<crate::metrics::AppMetrics>,
    network_name: String,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let guard = metrics.events_connection_guard(&network_name);
    let live_rx = events.subscribe();
    let mut last_sent_id = 0u64;
    let backlog = if let Some(start_from) = start_from {
        let items = events.snapshot_from(start_from);
        if let Some(last) = items.last() {
            last_sent_id = last.id;
        }
        items
    } else {
        Vec::new()
    };

    let backlog_stream = stream::iter(backlog.into_iter().map(|event| Ok(sse_event(&event))));

    let initial_stream = stream::once({
        let events = events.clone();
        async move { Ok(sse_api_version_event(events.wait_for_api_version().await)) }
    });

    let live_stream = stream::unfold(
        (live_rx, last_sent_id, guard),
        |(mut rx, mut last, guard)| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if event.id > last {
                            last = event.id;
                            return Some((Ok(sse_event(&event)), (rx, last, guard)));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(initial_stream.chain(backlog_stream).chain(live_stream))
        .keep_alive(KeepAlive::default())
}

/// Convert an event envelope into an SSE event with ID and JSON payload.
fn sse_event(event: &EventEnvelope) -> Event {
    let payload = serde_json::to_string(&event.data).unwrap_or_else(|_| "{}".to_string());
    Event::default().id(event.id.to_string()).data(payload)
}

/// Convert the upstream ApiVersion event into an SSE event with an empty ID.
fn sse_api_version_event(event: Value) -> Event {
    let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
    Event::default().id("").data(payload)
}

/// Serialize and send the ApiVersion event as a websocket text frame.
async fn send_api_version(
    sender: &mut futures_util::stream::SplitSink<
        axum::extract::ws::WebSocket,
        axum::extract::ws::Message,
    >,
    event: Value,
) -> Result<(), axum::Error> {
    let payload = serde_json::json!({
        "id": Value::Null,
        "data": event,
    });
    let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    sender
        .send(axum::extract::ws::Message::Text(text.into()))
        .await
}

/// Extractor that prefers websocket upgrades but can fall back to SSE.
struct MaybeWebSocketUpgrade(Option<WebSocketUpgrade>);

impl<S> FromRequestParts<S> for MaybeWebSocketUpgrade
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match WebSocketUpgrade::from_request_parts(parts, state).await {
            Ok(ws) => Ok(Self(Some(ws))),
            Err(_) => Ok(Self(None)),
        }
    }
}
