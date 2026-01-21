use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use casper_binary_port::{BinaryResponseAndRequest, Command, CommandHeader, ErrorCode, GetRequest};
use casper_binary_port::{InformationRequest, NodeStatus};
use casper_types::bytesrepr::{FromBytes, ToBytes};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

use casper_node_proxy::binary_proxy::BinaryPool;
use casper_node_proxy::handlers;
use casper_node_proxy::metrics::AppMetrics;
use casper_node_proxy::models::NetworkConfig;
use casper_node_proxy::state::{AppState, EventBus, NetworkState};

#[tokio::test]
async fn binary_ws_roundtrip() {
    let network_name = "local";
    let metrics = Arc::new(AppMetrics::new());
    let events = Arc::new(EventBus::new(
        network_name,
        16,
        256,
        Some(Arc::clone(&metrics)),
    ));
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

    let binary_pool = Arc::new(BinaryPool::new(
        network_name.to_string(),
        binary_addr.to_string(),
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
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("test server error: {err}");
        }
    });
    tokio::task::yield_now().await;

    let (mut socket, _) = connect_async(format!("ws://{addr}/{network_name}/binary"))
        .await
        .expect("failed to connect to binary websocket");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut responses = 0;
    let mut request_id: u16 = 1;

    while Instant::now() < deadline {
        let payload = build_request_payload(request_id, InformationRequest::NodeStatus);
        request_id = request_id.wrapping_add(1);
        socket
            .send(tokio_tungstenite::tungstenite::Message::Binary(
                payload.clone().into(),
            ))
            .await
            .expect("failed to send binary payload");

        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
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
        responses += 1;

        if Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    assert!(
        responses >= 2,
        "expected multiple binary responses over 15s, got {responses}"
    );
}

#[tokio::test]
async fn binary_ws_filters_peers_and_sanitizes_node_status() {
    let network_name = "local";
    let metrics = Arc::new(AppMetrics::new());
    let events = Arc::new(EventBus::new(
        network_name,
        16,
        256,
        Some(Arc::clone(&metrics)),
    ));
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

    let binary_pool = Arc::new(BinaryPool::new(
        network_name.to_string(),
        binary_addr.to_string(),
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
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("test server error: {err}");
        }
    });
    tokio::task::yield_now().await;

    let (mut socket, _) = connect_async(format!("ws://{addr}/{network_name}/binary"))
        .await
        .expect("failed to connect to binary websocket");

    let peers_payload = build_request_payload(1, InformationRequest::Peers);
    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            peers_payload.into(),
        ))
        .await
        .expect("failed to send peers request");
    let peers_message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("timed out waiting for peers response")
        .expect("binary websocket closed")
        .expect("binary websocket read error");
    let peers_response_bytes = match peers_message {
        tokio_tungstenite::tungstenite::Message::Binary(data) => data,
        other => panic!("unexpected websocket message: {other:?}"),
    };
    let (peers_response, peers_remainder) =
        BinaryResponseAndRequest::from_bytes(&peers_response_bytes)
            .expect("peers response should decode");
    assert!(
        peers_remainder.is_empty(),
        "peers response should not have trailing bytes"
    );
    assert!(
        !peers_response.response().is_success(),
        "peers response should indicate failure"
    );
    assert_eq!(
        peers_response.response().error_code(),
        ErrorCode::FunctionDisabled as u16,
        "peers response should be blocked"
    );

    let status_payload = build_request_payload(2, InformationRequest::NodeStatus);
    socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            status_payload.into(),
        ))
        .await
        .expect("failed to send node status request");
    let status_message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("timed out waiting for node status response")
        .expect("binary websocket closed")
        .expect("binary websocket read error");
    let status_response_bytes = match status_message {
        tokio_tungstenite::tungstenite::Message::Binary(data) => data,
        other => panic!("unexpected websocket message: {other:?}"),
    };
    let (status_response, status_remainder) =
        BinaryResponseAndRequest::from_bytes(&status_response_bytes)
            .expect("node status response should decode");
    assert!(
        status_remainder.is_empty(),
        "node status response should not have trailing bytes"
    );
    assert!(
        status_response.response().is_success(),
        "node status response indicates failure (code {})",
        status_response.response().error_code()
    );
    let (status, status_payload_remainder) =
        NodeStatus::from_bytes(status_response.response().payload())
            .expect("node status payload should decode");
    assert!(
        status_payload_remainder.is_empty(),
        "node status payload should not have trailing bytes"
    );
    assert!(
        status.peers.into_inner().is_empty(),
        "node status peers should be empty"
    );
}

fn build_request_payload(request_id: u16, info_request: InformationRequest) -> Vec<u8> {
    let get_request = GetRequest::try_from(info_request).expect("build GetRequest");
    let command = Command::Get(get_request);
    let header = CommandHeader::new(command.tag(), request_id);
    let mut bytes = Vec::new();
    header
        .write_bytes(&mut bytes)
        .expect("serialize command header");
    command.write_bytes(&mut bytes).expect("serialize command");
    bytes
}
