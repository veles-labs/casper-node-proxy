use std::collections::HashMap;
use std::sync::Arc;

use std::net::SocketAddr;
use tokio::net::TcpListener;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::RpcMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::rate_limit::{rate_limit_layer, rpc_rate_limiter};
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn rate_limit_rejects_excess_requests() {
    let network_name = "local";
    let events = Arc::new(EventBus::new(16, 256));
    let config = NetworkConfig {
        network_name: network_name.to_string(),
        chain_name: "test-chain".to_string(),
        rest: String::new(),
        sse: String::new(),
        rpc: "http://127.0.0.1:11101/rpc".to_string(),
        binary: "127.0.0.1:0".to_string(),
        gossip: String::new(),
    };

    let binary_pool = Arc::new(BinaryPool::new("127.0.0.1:0".to_string(), 1));
    let network_state = Arc::new(NetworkState {
        config,
        events,
        binary_pool,
    });
    let mut networks = HashMap::new();
    networks.insert(network_name.to_string(), network_state);

    let rpc_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build reqwest client");
    let app_state = AppState {
        networks: Arc::new(networks),
        rpc_client,
        metrics: RpcMetrics::default(),
        rpc_rate_limiter: rpc_rate_limiter(1_000, 1_000),
    };

    let app = handlers::routes(app_state).layer(rate_limit_layer(1, 1));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test listener");
    let addr = listener.local_addr().expect("listener has no addr");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            eprintln!("test server error: {err}");
        }
    });
    tokio::task::yield_now().await;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build reqwest client");
    let url = format!("http://{addr}/{network_name}/");

    let first = client.get(&url).send().await.expect("first request");
    assert!(first.status().is_success(), "first request should succeed");

    let second = client.get(&url).send().await.expect("second request");
    assert_eq!(second.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
}
