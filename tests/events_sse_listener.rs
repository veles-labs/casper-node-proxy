use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use veles_casper_rust_sdk::sse::{
    config::ListenerConfig,
    event::{EventEnvelope, SseEvent},
    listener as sse_listener,
};

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::RpcMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::sse;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn events_sse_listener_receives_event() {
    let network_name = "local";
    let upstream_endpoint = std::env::var("SSE_UPSTREAM_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:18101/events".to_string());
    let events = Arc::new(EventBus::new(16, 256));
    let network_config = NetworkConfig {
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
        config: network_config,
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

    let proxy_endpoint = format!("http://{addr}/{network_name}/events");
    let listener_config = ListenerConfig::builder()
        .with_endpoint(proxy_endpoint)
        .build()
        .expect("listener config");
    let cancellation_token = CancellationToken::new();
    let stream = sse_listener(listener_config, cancellation_token)
        .await
        .expect("listener");
    futures_util::pin_mut!(stream);

    let first: EventEnvelope = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timed out waiting for SSE event")
        .expect("SSE stream closed")
        .expect("SSE listener error");

    assert!(
        first.id.is_none(),
        "expected first SSE event to have null id: {first:?}"
    );
    assert!(
        matches!(first.data, SseEvent::ApiVersion(_)),
        "expected first SSE event to be ApiVersion"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut second: Option<EventEnvelope> = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = tokio::time::timeout(remaining, stream.next())
            .await
            .expect("timed out waiting for SSE event")
            .expect("SSE stream closed")
            .expect("SSE listener error");
        if next.id.is_some() {
            second = Some(next);
            break;
        }
    }

    second.expect("expected SSE event with non-null id after ApiVersion");
}
