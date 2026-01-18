//! Environment-driven configuration for the proxy service.

use std::env;

/// Runtime configuration assembled from environment variables.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub database_url: String,
    pub listen_addr: String,
    pub metrics_listen_addr: String,
    pub rate_limit_per_min: u32,
    pub rate_limit_burst: u32,
    pub binary_rate_limit_per_min: u32,
    pub binary_rate_limit_burst: u32,
    pub sse_broadcast_capacity: usize,
    pub sse_backlog_limit: usize,
    pub binary_pool_size: usize,
}

impl AppConfig {
    /// Load configuration with sensible defaults for local development.
    pub fn from_env() -> Self {
        let rate_limit_per_min = env_u32("RATE_LIMIT_PER_MIN", 60);
        let rate_limit_burst = env_u32("RATE_LIMIT_BURST", 20);
        Self {
            database_url: env_string("DATABASE_URL", "sqlite://./db.sqlite"),
            listen_addr: env_string("BIND_ADDR", "0.0.0.0:8080"),
            metrics_listen_addr: env_string("METRICS_BIND_ADDR", "127.0.0.1:9090"),
            rate_limit_per_min,
            rate_limit_burst,
            binary_rate_limit_per_min: env_u32("BINARY_RATE_LIMIT_PER_MIN", rate_limit_per_min),
            binary_rate_limit_burst: env_u32("BINARY_RATE_LIMIT_BURST", rate_limit_burst),
            sse_broadcast_capacity: env_usize("SSE_BROADCAST_CAPACITY", 256),
            sse_backlog_limit: env_usize("SSE_BACKLOG_LIMIT", 16_384),
            binary_pool_size: env_usize("BINARY_POOL_SIZE", 4),
        }
    }
}

fn env_string(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|val| val.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|val| val.parse().ok())
        .unwrap_or(default)
}
