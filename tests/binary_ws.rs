use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use casper_binary_port::InformationRequest;
use casper_binary_port::{BinaryResponseAndRequest, Command, CommandHeader, GetRequest};
use casper_types::bytesrepr::{FromBytes, ToBytes};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::RpcMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn binary_ws_roundtrip() {
    let network_name = "local";
    let events = Arc::new(EventBus::new(16, 256));
    let binary_addr =
        std::env::var("BINARY_PORT_ADDR").unwrap_or_else(|_| "127.0.0.1:28101".to_string());

    let config = NetworkConfig {
        network_name: network_name.to_string(),
        chain_name: "test-chain".to_string(),
        rest: String::new(),
        sse: String::new(),
        rpc: "http://127.0.0.1:11101/rpc".to_string(),
        binary: binary_addr.to_string(),
        gossip: String::new(),
    };

    let binary_pool = Arc::new(BinaryPool::new(binary_addr.to_string(), 1));
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

    let (mut socket, _) = connect_async(format!("ws://{addr}/{network_name}/binary"))
        .await
        .expect("failed to connect to binary websocket");

    let payload = build_request_payload();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            payload.clone().into(),
        ))
        .await
        .expect("failed to send binary payload");

    let message = tokio::time::timeout(Duration::from_secs(60), socket.next())
        .await
        .expect("timed out waiting for binary websocket response")
        .expect("binary websocket closed")
        .expect("binary websocket read error");

    let response_bytes = match message {
        tokio_tungstenite::tungstenite::Message::Binary(data) => data,
        other => panic!("unexpected websocket message: {other:?}"),
    };

    let (response, remainder) = BinaryResponseAndRequest::from_bytes(&response_bytes)
        .expect("binary response should decode");
    assert!(
        remainder.is_empty(),
        "binary response should not have trailing bytes"
    );
    assert!(
        response.response().is_success(),
        "binary response indicates failure (code {})",
        response.response().error_code()
    );
    assert!(
        !response.response().payload().is_empty(),
        "binary response payload should not be empty"
    );
}

fn build_request_payload() -> Vec<u8> {
    let info_request = InformationRequest::NodeStatus;
    let get_request = GetRequest::try_from(info_request).expect("build GetRequest");
    let command = Command::Get(get_request);
    let header = CommandHeader::new(command.tag(), 1);
    let mut bytes = Vec::new();
    header
        .write_bytes(&mut bytes)
        .expect("serialize command header");
    command.write_bytes(&mut bytes).expect("serialize command");
    bytes
}
