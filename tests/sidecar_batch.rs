use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpListener;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::AppMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn proxy_rejects_batched_requests() {
    let network_name = "local";

    let config = NetworkConfig {
        network_name: network_name.to_string(),
        chain_name: "test-chain".to_string(),
        rest: String::new(),
        sse: String::new(),
        rpc: "http://127.0.0.1:0".to_string(),
        binary: "127.0.0.1:0".to_string(),
        gossip: String::new(),
    };

    let metrics = Arc::new(AppMetrics::new());
    let events = Arc::new(EventBus::new(
        network_name,
        16,
        256,
        Some(Arc::clone(&metrics)),
    ));
    let binary_pool = Arc::new(BinaryPool::new(
        network_name.to_string(),
        "127.0.0.1:0".to_string(),
        1,
        Arc::clone(&metrics),
    ));
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
        metrics: Arc::clone(&metrics),
        binary_rate_limit_per_min: 1_000,
        binary_rate_limit_burst: 1_000,
    };

    let app = handlers::routes(app_state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test listener");
    let addr = listener.local_addr().expect("listener has no addr");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            eprintln!("test server error: {err}");
        }
    });
    tokio::task::yield_now().await;

    let batch = vec![
        json!({"jsonrpc":"2.0","id":1,"method":"info_get_status"}),
        json!({"jsonrpc":"2.0","id":2,"method":"info_get_peers"}),
        json!({"jsonrpc":"2.0","id":3,"method":"chain_get_state_root_hash"}),
        json!({"jsonrpc":"2.0","id":4,"method":"chain_get_block"}),
        json!({"jsonrpc":"2.0","id":5,"method":"state_get_auction_info"}),
    ];

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build reqwest client");
    let response = client
        .post(format!("http://{addr}/{network_name}/rpc"))
        .json(&batch)
        .send()
        .await
        .expect("proxy RPC endpoint must be reachable for integration tests");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "proxy RPC returned unexpected status: {}",
        response.status()
    );

    let value: serde_json::Value = response
        .json()
        .await
        .expect("proxy RPC response must be valid JSON");
    assert_eq!(
        value.get("error").and_then(|v| v.as_str()),
        Some("JSON-RPC batching is not supported"),
        "expected batch rejection error"
    );
}
