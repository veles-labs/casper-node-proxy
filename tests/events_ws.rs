use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use veles_casper_rust_sdk::sse::event::SseEvent;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::RpcMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::sse;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn events_ws_receives_message() {
    let network_name = "local";
    let upstream_endpoint = std::env::var("SSE_UPSTREAM_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:18101/events".to_string());
    let events = Arc::new(EventBus::new(16, 256));
    let config = NetworkConfig {
        network_name: network_name.to_string(),
        chain_name: "test-chain".to_string(),
        rest: String::new(),
        sse: upstream_endpoint.clone(),
        rpc: "http://127.0.0.1:11101/rpc".to_string(),
        binary: "127.0.0.1:28101".to_string(),
        gossip: String::new(),
    };

    let binary_pool = Arc::new(BinaryPool::new("127.0.0.1:28101".to_string(), 1));
    let network_state = Arc::new(NetworkState {
        config,
        events: events.clone(),
        binary_pool,
    });
    let mut networks = HashMap::new();
    networks.insert(network_name.to_string(), network_state);

    sse::spawn_listener(network_name.to_string(), upstream_endpoint, events);

    let rpc_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build reqwest client");
    let app_state = AppState {
        networks: Arc::new(networks),
        rpc_client,
        metrics: RpcMetrics::default(),
    };

    let app = handlers::routes(app_state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test listener");
    let addr = listener.local_addr().expect("listener has no addr");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("test server error: {err}");
        }
    });
    tokio::task::yield_now().await;

    let (mut socket, _) = connect_async(format!("ws://{addr}/{network_name}/events"))
        .await
        .expect("failed to connect to events websocket");

    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("timed out waiting for events websocket message")
        .expect("events websocket closed")
        .expect("events websocket read error");

    let text = match message {
        tokio_tungstenite::tungstenite::Message::Text(text) => text,
        other => panic!("unexpected websocket message: {other:?}"),
    };

    let value: Value = serde_json::from_str(&text).expect("event payload should be JSON");
    assert_eq!(value.get("id"), Some(&Value::Null));
    let data = value.get("data").cloned().expect("event payload should have data");
    let event: SseEvent = serde_json::from_value(data).expect("event payload should decode");
    assert!(matches!(event, SseEvent::ApiVersion(_)));

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_non_null_id = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = tokio::time::timeout(remaining, socket.next())
            .await
            .expect("timed out waiting for events websocket message")
            .expect("events websocket closed")
            .expect("events websocket read error");

        let text = match message {
            tokio_tungstenite::tungstenite::Message::Text(text) => text,
            other => panic!("unexpected websocket message: {other:?}"),
        };
        let value: Value = serde_json::from_str(&text).expect("event payload should be JSON");
        if matches!(value.get("id"), Some(Value::Number(_))) {
            saw_non_null_id = true;
            break;
        }
    }

    assert!(saw_non_null_id, "expected SSE event with non-null id after ApiVersion");
}
