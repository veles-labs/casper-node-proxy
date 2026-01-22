//! JSON-RPC proxy endpoint and method allowlist.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use crate::handlers::{AppError, get_network};
use crate::state::AppState;

// Keep in sync with casper-sidecar rpc_sidecar/src/http_server.rs and speculative_exec_server.rs.
const SUPPORTED_JSONRPC_METHODS: &[&str] = &[
    "account_put_deploy",
    "account_put_transaction",
    "chain_get_block",
    "chain_get_block_transfers",
    "chain_get_era_info_by_switch_block",
    "chain_get_era_summary",
    "chain_get_state_root_hash",
    "info_get_chainspec",
    "info_get_deploy",
    "info_get_reward",
    "info_get_status",
    "info_get_transaction",
    "info_get_validator_changes",
    "query_balance",
    "query_balance_details",
    "query_global_state",
    "speculative_exec",
    "speculative_exec_txn",
    "state_get_account_info",
    "state_get_auction_info",
    "state_get_auction_info_v2",
    "state_get_balance",
    "state_get_dictionary_item",
    "state_get_entity",
    "state_get_item",
    "state_get_package",
    "state_get_trie",
];

fn is_supported_jsonrpc_method(method: &str) -> bool {
    SUPPORTED_JSONRPC_METHODS
        .iter()
        .any(|known| known == &method)
}

/// Proxy JSON-RPC requests and capture per-method metrics.
pub async fn rpc_proxy(
    Path(network_name): Path<String>,
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, AppError> {
    let network = get_network(&state, &network_name)?;
    let json: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return Ok(jsonrpc_error_response(Value::Null, -32700, "Parse error"));
        }
    };

    let sanitize_status = match json {
        Value::Object(map) => {
            let id = map.get("id").cloned().unwrap_or(Value::Null);
            let method = match map.get("method").and_then(Value::as_str) {
                Some(method) => method,
                None => {
                    return Ok(jsonrpc_error_response(
                        Value::Null,
                        -32600,
                        "Invalid Request",
                    ));
                }
            };
            if !is_supported_jsonrpc_method(method) {
                return Ok(jsonrpc_error_response(id, -32601, "Method not found"));
            }
            state
                .metrics
                .increment_jsonrpc_method(&network_name, method);
            method == "info_get_status"
        }
        Value::Array(_) => {
            return Ok(jsonrpc_error_response(
                Value::Null,
                -32600,
                "Invalid Request",
            ));
        }
        _ => {
            return Ok(jsonrpc_error_response(
                Value::Null,
                -32600,
                "Invalid Request",
            ));
        }
    };

    let upstream_start = Instant::now();
    let response = match state
        .rpc_client
        .post(&network.config.rpc)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            state.metrics.increment_rpc_upstream_error(&network_name);
            state
                .metrics
                .observe_rpc_upstream_duration(&network_name, upstream_start.elapsed());
            return Err(AppError::bad_gateway(format!("upstream RPC error: {err}")));
        }
    };
    state
        .metrics
        .observe_rpc_upstream_duration(&network_name, upstream_start.elapsed());
    if !response.status().is_success() {
        state
            .metrics
            .increment_rpc_upstream_non_success(&network_name, response.status().as_u16());
    }

    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|err| AppError::bad_gateway(format!("upstream body error: {err}")))?;
    let body = if sanitize_status {
        sanitize_info_get_status_response(&body)?
    } else {
        body.to_vec()
    };

    let mut builder = Response::builder().status(status);
    if let Some(content_type) = headers.get("content-type") {
        builder = builder.header("content-type", content_type);
    }

    let response = builder
        .body(Body::from(body))
        .map_err(|err| AppError::bad_gateway(format!("response error: {err}")))?;
    Ok(response)
}

fn jsonrpc_error_response(id: Value, code: i32, message: &str) -> Response {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("JSON-RPC error response builder")
}

fn sanitize_info_get_status_response(body: &[u8]) -> Result<Vec<u8>, AppError> {
    let mut value: Value = serde_json::from_slice(body).map_err(|err| {
        AppError::bad_gateway(format!("upstream RPC response is not JSON: {err}"))
    })?;
    let mut updated = false;
    if let Value::Object(map) = &mut value
        && let Some(Value::Object(result)) = map.get_mut("result")
    {
        result.insert("peers".to_string(), Value::Array(Vec::new()));
        updated = true;
    }
    if !updated {
        return Ok(body.to_vec());
    }
    serde_json::to_vec(&value)
        .map_err(|err| AppError::bad_gateway(format!("failed to encode JSON-RPC response: {err}")))
}
