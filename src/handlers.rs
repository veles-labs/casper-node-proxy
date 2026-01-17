//! HTTP handlers and websocket/SSE bridges for proxy endpoints.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, Stream, StreamExt, stream};
use governor::clock::{Clock, DefaultClock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::sync::broadcast;
use tower_governor::key_extractor::{KeyExtractor, SmartIpKeyExtractor};
use tracing::{debug, warn};

const RPC_RATE_LIMIT_REMAINING_HEADER: &str = "ratelimit-remaining";

use crate::binary_proxy::BinaryPool;
use crate::state::{AppState, EventBus, EventEnvelope, NetworkState};

/// Build the application router with shared state.
pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/{network_name}/", get(network_root))
        .route("/{network_name}/rpc", axum::routing::post(rpc_proxy))
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

/// Proxy JSON-RPC requests, enforcing per-item rate limits and metrics.
async fn rpc_proxy(
    Path(network_name): Path<String>,
    State(state): State<AppState>,
    ConnectInfo(connect_info): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let network = get_network(&state, &network_name)?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|err| AppError::bad_request(format!("invalid JSON-RPC payload: {err}")))?;

    let client_ip = extract_client_ip(&headers, Some(connect_info));
    let mut rpc_remaining: Option<u32> = None;
    for method in extract_methods(&json) {
        state.metrics.increment(&network_name, method);
    }

    if let Value::Array(items) = json {
        if items.is_empty() {
            return Err(AppError::bad_request("empty JSON-RPC batch"));
        }

        let mut responses = Vec::with_capacity(items.len());
        for item in items {
            let id = item.get("id").cloned();
            if !item.is_object() {
                responses.push(jsonrpc_error(id.as_ref(), "batch item must be an object"));
                continue;
            }
            if let Some(ip) = client_ip {
                match state.rpc_rate_limiter.check_key(&ip) {
                    Ok(snapshot) => {
                        rpc_remaining = Some(snapshot.remaining_burst_capacity());
                    }
                    Err(not_until) => {
                        let wait = not_until.wait_time_from(DefaultClock::default().now());
                        rpc_remaining = Some(0);
                        responses.push(jsonrpc_rate_limit_error(id.as_ref(), wait.as_secs()));
                        continue;
                    }
                }
            }
            // Sidecar doesn't accept batches, so forward each item sequentially.
            match send_single_rpc(&state.rpc_client, &network.config.rpc, &item).await {
                Ok(response) => responses.push(response),
                Err(err) => responses.push(jsonrpc_error(id.as_ref(), err)),
            }
        }
        let mut response = Json(Value::Array(responses)).into_response();
        if let Some(remaining) = rpc_remaining {
            insert_rpc_rate_limit_header(&mut response, remaining);
        }
        return Ok(response);
    }

    if let Some(ip) = client_ip {
        match state.rpc_rate_limiter.check_key(&ip) {
            Ok(snapshot) => {
                rpc_remaining = Some(snapshot.remaining_burst_capacity());
            }
            Err(not_until) => {
                let wait = not_until.wait_time_from(DefaultClock::default().now());
                let mut response =
                    Json(jsonrpc_rate_limit_error(None, wait.as_secs())).into_response();
                insert_rpc_rate_limit_header(&mut response, 0);
                return Ok(response);
            }
        }
    }

    let response = state
        .rpc_client
        .post(&network.config.rpc)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|err| AppError::bad_gateway(format!("upstream RPC error: {err}")))?;

    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|err| AppError::bad_gateway(format!("upstream body error: {err}")))?;

    let mut builder = Response::builder().status(status);
    if let Some(content_type) = headers.get("content-type") {
        builder = builder.header("content-type", content_type);
    }

    let mut response = builder
        .body(Body::from(body))
        .map_err(|err| AppError::bad_gateway(format!("response error: {err}")))?;
    if let Some(remaining) = rpc_remaining {
        insert_rpc_rate_limit_header(&mut response, remaining);
    }
    Ok(response)
}

/// Forward a single JSON-RPC request to the upstream sidecar.
async fn send_single_rpc(
    client: &reqwest::Client,
    url: &str,
    payload: &Value,
) -> Result<Value, String> {
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .json(payload)
        .send()
        .await
        .map_err(|err| format!("upstream RPC error: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("upstream RPC status: {}", response.status()));
    }

    response
        .json::<Value>()
        .await
        .map_err(|err| format!("upstream RPC decode error: {err}"))
}

/// Build a JSON-RPC error response with a generic error code.
fn jsonrpc_error(id: Option<&Value>, message: impl Into<String>) -> Value {
    let id_value = id.cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_value,
        "error": {
            "code": -32000,
            "message": message.into(),
        }
    })
}

/// Build a JSON-RPC error response with retry information.
fn jsonrpc_rate_limit_error(id: Option<&Value>, retry_after_secs: u64) -> Value {
    let id_value = id.cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id_value,
        "error": {
            "code": -32000,
            "message": "rate limit exceeded",
            "data": { "retry_after_secs": retry_after_secs }
        }
    })
}

/// Determine the best-effort client IP address for rate limiting.
fn extract_client_ip(
    headers: &axum::http::HeaderMap,
    connect_info: Option<SocketAddr>,
) -> Option<std::net::IpAddr> {
    let mut req = axum::http::Request::new(());
    *req.headers_mut() = headers.clone();
    if let Some(addr) = connect_info {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        req.extensions_mut().insert(addr);
    }
    SmartIpKeyExtractor.extract(&req).ok()
}

/// Insert the per-item JSON-RPC remaining burst header when available.
fn insert_rpc_rate_limit_header(response: &mut Response, remaining: u32) {
    if let Ok(value) = HeaderValue::from_str(&remaining.to_string()) {
        response
            .headers_mut()
            .insert(RPC_RATE_LIMIT_REMAINING_HEADER, value);
    }
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
    let start_from = query.start_from;
    if let Some(ws) = ws {
        return Ok(ws.on_upgrade(move |socket| handle_events_socket(socket, events, start_from)));
    }
    Ok(sse_stream(events, start_from).into_response())
}

/// Proxy binary-port requests over a websocket connection.
async fn binary_ws(
    Path(network_name): Path<String>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let network = get_network(&state, &network_name)?;
    let pool = network.binary_pool.clone();
    Ok(ws.on_upgrade(move |socket| handle_binary_socket(socket, pool)))
}

/// Look up a configured network or return a 404 error.
fn get_network(state: &AppState, name: &str) -> Result<Arc<NetworkState>, AppError> {
    state
        .networks
        .get(name)
        .cloned()
        .ok_or_else(|| AppError::not_found(format!("unknown network: {name}")))
}

/// Extract JSON-RPC method names from a request or batch.
fn extract_methods(value: &Value) -> Vec<&str> {
    match value {
        Value::Object(map) => map
            .get("method")
            .and_then(Value::as_str)
            .into_iter()
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("method").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// Stream events over websocket, optionally replaying from the backlog.
async fn handle_events_socket(
    socket: axum::extract::ws::WebSocket,
    events: Arc<EventBus>,
    start_from: Option<u64>,
) {
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

async fn handle_binary_socket(socket: axum::extract::ws::WebSocket, pool: Arc<BinaryPool>) {
    let (mut sender, mut receiver) = socket.split();

    while let Some(message) = receiver.next().await {
        match message {
            Ok(axum::extract::ws::Message::Binary(payload)) => {
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
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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

    let live_stream = stream::unfold((live_rx, last_sent_id), |(mut rx, mut last)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if event.id > last {
                        last = event.id;
                        return Some((Ok(sse_event(&event)), (rx, last)));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

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
