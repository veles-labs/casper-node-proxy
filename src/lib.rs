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
use crate::metrics::{AppMetrics, router as metrics_router, track_http};
use crate::rate_limit::rate_limit_layer;
use crate::state::{AppState, EventBus, NetworkState};

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

/// Run database migrations without starting the HTTP server.
pub async fn migrate() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();

    let config = AppConfig::from_env();
    let metrics = Arc::new(AppMetrics::new());
    let pool = db::init_pool(&config.database_url)?;
    db::run_migrations(&pool, &metrics)?;
    Ok(())
}

/// Build the shared state, wire routes, and run the HTTP server.
pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();

    let config = AppConfig::from_env();
    let metrics = Arc::new(AppMetrics::new());
    let pool = db::init_pool(&config.database_url)?;
    db::run_migrations(&pool, &metrics)?;
    db::spawn_pool_metrics(&pool, Arc::clone(&metrics));
    let networks = db::load_networks(&pool, &metrics)?;

    if networks.is_empty() {
        warn!("no networks configured; only health responses will work");
    }

    let mut network_map: HashMap<String, Arc<NetworkState>> = HashMap::new();
    for cfg in networks {
        let events = Arc::new(EventBus::new(
            cfg.network_name.clone(),
            config.sse_backlog_limit,
            config.sse_broadcast_capacity,
            Some(Arc::clone(&metrics)),
        ));
        let binary_pool = Arc::new(BinaryPool::new(
            cfg.network_name.clone(),
            cfg.binary.clone(),
            config.binary_pool_size,
            Arc::clone(&metrics),
        ));

        let network_name = cfg.network_name.clone();
        let sse_endpoint = cfg.sse.clone();
        let state = Arc::new(NetworkState {
            config: cfg,
            events: events.clone(),
            binary_pool,
        });

        sse::spawn_listener(
            network_name.clone(),
            sse_endpoint,
            events,
            Arc::clone(&metrics),
        );
        network_map.insert(network_name, state);
    }

    let rpc_client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|err| format!("failed to build RPC client: {err}"))?;

    let app_state = AppState {
        networks: Arc::new(network_map),
        rpc_client,
        metrics: Arc::clone(&metrics),
        binary_rate_limit_per_min: config.binary_rate_limit_per_min,
        binary_rate_limit_burst: config.binary_rate_limit_burst,
    };

    let app = handlers::routes(app_state)
        .layer(rate_limit_layer(
            config.rate_limit_per_min,
            config.rate_limit_burst,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&metrics),
            track_http,
        ));

    let metrics_app = metrics_router(Arc::clone(&metrics));
    let metrics_listener = tokio::net::TcpListener::bind(&config.metrics_listen_addr).await?;
    let metrics_addr = metrics_listener.local_addr()?;
    if !metrics_addr.ip().is_loopback() {
        warn!("metrics listener is not bound to a loopback address: {metrics_addr}");
    }
    tokio::spawn(async move {
        if let Err(err) = axum::serve(metrics_listener, metrics_app).await {
            warn!("metrics server error: {err}");
        }
    });

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    info!("listening on {}", config.listen_addr);
    info!("metrics listening on {}", metrics_addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
