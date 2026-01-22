use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpListener;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::AppMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

const NETWORK_NAME: &str = "local";

#[tokio::test]
async fn jsonrpc_rejects_info_get_peers() {
    let proxy_addr = spawn_proxy(upstream_rpc_url()).await;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build reqwest client");
    let response = client
        .post(format!("http://{proxy_addr}/{NETWORK_NAME}/rpc"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"info_get_peers"}))
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
    assert_eq!(
        value
            .get("error")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_i64()),
        Some(-32601)
    );
    assert_eq!(
        value
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str()),
        Some("Method not found")
    );
    assert_eq!(value.get("id").and_then(|v| v.as_i64()), Some(1));
}

#[tokio::test]
async fn jsonrpc_sanitizes_info_get_status_peers() {
    let proxy_addr = spawn_proxy(upstream_rpc_url()).await;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build reqwest client");
    let response = client
        .post(format!("http://{proxy_addr}/{NETWORK_NAME}/rpc"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"info_get_status"}))
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
    let peers = value
        .get("result")
        .and_then(|result| result.get("peers"))
        .and_then(|peers| peers.as_array())
        .expect("response should contain result.peers array");
    assert!(peers.is_empty(), "expected sanitized peers array");
}

async fn spawn_proxy(rpc_url: String) -> SocketAddr {
    let config = NetworkConfig {
        network_name: NETWORK_NAME.to_string(),
        chain_name: "test-chain".to_string(),
        rest: String::new(),
        sse: String::new(),
        rpc: rpc_url,
        binary: "127.0.0.1:0".to_string(),
        gossip: String::new(),
    };

    let metrics = Arc::new(AppMetrics::new());
    let events = Arc::new(EventBus::new(
        NETWORK_NAME,
        16,
        256,
        Some(Arc::clone(&metrics)),
    ));
    let binary_pool = Arc::new(BinaryPool::new(
        NETWORK_NAME.to_string(),
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
    networks.insert(NETWORK_NAME.to_string(), network_state);

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
        .expect("failed to bind proxy listener");
    let addr = listener.local_addr().expect("listener has no addr");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("proxy server error: {err}");
        }
    });
    tokio::task::yield_now().await;
    addr
}

fn upstream_rpc_url() -> String {
    std::env::var("UPSTREAM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:11101/rpc".to_string())
}
