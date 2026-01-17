//! SSE listener bridge that feeds the in-memory event bus.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use redis::AsyncCommands;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use veles_casper_rust_sdk::sse::{config::ListenerConfig, event::SseEvent, listener};

use crate::state::EventBus;

const RETRY_DELAY: Duration = Duration::from_secs(3);

/// Spawn a resilient SSE listener for a network and feed the event bus.
pub fn spawn_listener(network_name: String, sse_endpoint: String, events: Arc<EventBus>) {
    tokio::spawn(async move {
        let redis_url =
            Some(std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_string()));
        let redis_key = format!("sse_last_id:{network_name}");
        let redis_client = match redis_url {
            Some(url) => match redis::Client::open(url) {
                Ok(client) => Some(client),
                Err(err) => {
                    warn!(%network_name, "invalid REDIS_URL: {err}");
                    None
                }
            },
            None => None,
        };
        let mut redis_conn = match redis_client.as_ref() {
            Some(client) => connect_redis(client, &network_name).await,
            None => None,
        };
        if redis_client.is_some() && redis_conn.is_none() {
            warn!(%network_name, "redis unavailable at startup; SSE resume disabled");
        }
        let mut start_from = match redis_conn.as_mut() {
            Some(conn) => load_last_id(conn, &network_name, &redis_key).await,
            None => None,
        };

        loop {
            let mut builder = ListenerConfig::builder().with_endpoint(&sse_endpoint);
            if let Some(start_from) = start_from {
                builder = builder.with_start_from(start_from.to_string());
            }
            info!(%network_name, start_from = ?start_from, "SSE listener starting");
            let config = match builder.build() {
                Ok(config) => config,
                Err(err) => {
                    error!(%network_name, "failed to build SSE listener config: {err}");
                    return;
                }
            };

            let cancellation_token = CancellationToken::new();
            match listener(config, cancellation_token).await {
                Ok(stream) => {
                    info!(%network_name, "SSE listener connected");
                    futures_util::pin_mut!(stream);
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(envelope) => {
                                if let Some(id) = envelope.id {
                                    start_from = Some(id);
                                    if let Some(client) = redis_client.as_ref() {
                                        if redis_conn.is_none() {
                                            redis_conn = connect_redis(client, &network_name).await;
                                        }
                                        if let Some(conn) = redis_conn.as_mut() {
                                            if let Err(err) =
                                                conn.set::<_, _, ()>(&redis_key, id).await
                                            {
                                                warn!(%network_name, "failed to write SSE id to redis: {err}");
                                                redis_conn = None;
                                            } else {
                                                debug!(%network_name, sse_id = id, "stored SSE id in redis");
                                            }
                                        }
                                    }
                                }
                                if matches!(&envelope.data, SseEvent::ApiVersion(_)) {
                                    if let Some(value) = sse_to_value(&envelope.data) {
                                        let _ = events.remember_api_version(value);
                                    }
                                    continue;
                                }
                                if let Some(value) = sse_to_value(&envelope.data) {
                                    let _ = events.push(value);
                                }
                            }
                            Err(err) => {
                                warn!(%network_name, "SSE listener error: {err}");
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!(%network_name, "SSE listener failed to start: {err}");
                }
            }

            // Back off briefly before reconnecting.
            tokio::time::sleep(RETRY_DELAY).await;
        }
    });
}

async fn connect_redis(
    client: &redis::Client,
    network_name: &str,
) -> Option<redis::aio::MultiplexedConnection> {
    match client.get_multiplexed_async_connection().await {
        Ok(conn) => Some(conn),
        Err(err) => {
            warn!(%network_name, "failed to connect to redis: {err}");
            None
        }
    }
}

async fn load_last_id(
    conn: &mut redis::aio::MultiplexedConnection,
    network_name: &str,
    redis_key: &str,
) -> Option<u64> {
    match conn.get::<_, Option<u64>>(redis_key).await {
        Ok(id) => id,
        Err(err) => {
            warn!(%network_name, "failed to read SSE start_from from redis: {err}");
            None
        }
    }
}

/// Serialize an SSE event into JSON for storage in the backlog.
fn sse_to_value(event: &SseEvent) -> Option<Value> {
    match serde_json::to_value(event) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!("failed to serialize SSE event: {err}");
            None
        }
    }
}
