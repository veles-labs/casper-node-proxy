//! Lightweight in-memory counters for JSON-RPC method usage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

/// Tracks per-network method invocations for JSON-RPC requests.
#[derive(Clone, Default)]
pub struct RpcMetrics {
    counts: DashMap<(String, String), Arc<AtomicU64>>,
}

impl RpcMetrics {
    /// Increment the counter for a (network, method) pair.
    pub fn increment(&self, network: &str, method: &str) {
        let key = (network.to_string(), method.to_string());
        let entry = self
            .counts
            .entry(key)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
        entry.fetch_add(1, Ordering::Relaxed);
    }
}
