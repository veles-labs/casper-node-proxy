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
async fn proxy_accepts_batched_requests() {
    let url = std::env::var("SIDECAR_RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11101/rpc".to_string());
    let network_name = "local";

    let config = NetworkConfig {
        network_name: network_name.to_string(),
        chain_name: "test-chain".to_string(),
        rest: String::new(),
        sse: String::new(),
        rpc: url,
        binary: "127.0.0.1:0".to_string(),
        gossip: String::new(),
    };

    let events = Arc::new(EventBus::new(16, 256));
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

    assert!(
        response.status().is_success(),
        "proxy RPC returned unexpected status: {}",
        response.status()
    );

    let value: serde_json::Value = response
        .json()
        .await
        .expect("proxy RPC response must be valid JSON");

    let items = value
        .as_array()
        .expect("proxy RPC response must be a JSON array");
    assert_eq!(
        items.len(),
        batch.len(),
        "sidecar RPC response size mismatch"
    );

    for item in items {
        assert!(item.get("id").is_some(), "missing response id");
        assert!(
            item.get("result").is_some() || item.get("error").is_some(),
            "missing result or error in response item"
        );
    }
}
