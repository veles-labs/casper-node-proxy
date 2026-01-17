//! Application entry for the Casper node proxy service.

pub mod binary_proxy;
pub mod config;
pub mod db;
pub mod handlers;
pub mod metrics;
pub mod models;
pub mod rate_limit;
pub mod schema;
pub mod sse;
pub mod state;

use std::collections::HashMap;
use std::sync::Arc;

use std::net::SocketAddr;
use tracing::{info, warn};

use crate::binary_proxy::BinaryPool;
use crate::config::AppConfig;
use crate::metrics::RpcMetrics;
use crate::rate_limit::rate_limit_layer;
use crate::state::{AppState, EventBus, NetworkState};

/// Build the shared state, wire routes, and run the HTTP server.
pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = AppConfig::from_env();
    let pool = db::init_pool(&config.database_url)?;
    db::run_migrations(&pool)?;
    let networks = db::load_networks(&pool)?;

    if networks.is_empty() {
        warn!("no networks configured; only health responses will work");
    }

    let mut network_map: HashMap<String, Arc<NetworkState>> = HashMap::new();
    for cfg in networks {
        let events = Arc::new(EventBus::new(
            config.sse_backlog_limit,
            config.sse_broadcast_capacity,
        ));
        let binary_pool = Arc::new(BinaryPool::new(cfg.binary.clone(), config.binary_pool_size));

        let network_name = cfg.network_name.clone();
        let sse_endpoint = cfg.sse.clone();
        let state = Arc::new(NetworkState {
            config: cfg,
            events: events.clone(),
            binary_pool,
        });

        sse::spawn_listener(network_name.clone(), sse_endpoint, events);
        network_map.insert(network_name, state);
    }

    let rpc_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|err| format!("failed to build RPC client: {err}"))?;

    let app_state = AppState {
        networks: Arc::new(network_map),
        rpc_client,
        metrics: RpcMetrics::default(),
    };

    let app = handlers::routes(app_state).layer(rate_limit_layer(
        config.rate_limit_per_min,
        config.rate_limit_burst,
    ));

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    info!("listening on {}", config.listen_addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
