use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpListener;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::RpcMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::rate_limit::rpc_rate_limiter;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn rpc_batch_rate_limits_per_item() {
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
        rpc_rate_limiter: rpc_rate_limiter(1, 1),
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

    assert!(response.status().is_success());

    let value: serde_json::Value = response
        .json()
        .await
        .expect("proxy RPC response must be valid JSON");
    let items = value
        .as_array()
        .expect("proxy RPC response must be a JSON array");
    assert_eq!(items.len(), batch.len());

    let mut error_count = 0;
    for item in items {
        if let Some(err) = item.get("error")
            && err.get("message").and_then(|v| v.as_str()) == Some("rate limit exceeded")
        {
            error_count += 1;
        }
    }
    assert!(
        error_count >= 1,
        "expected at least one item to be rate-limited"
    );
}
