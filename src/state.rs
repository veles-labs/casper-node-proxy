//! Shared state for networks, event streams, and RPC metrics.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::broadcast;

use crate::binary_proxy::BinaryPool;
use crate::metrics::RpcMetrics;
use crate::models::NetworkConfig;

/// SSE/WS event envelope with a monotonically increasing ID.
#[derive(Clone, Debug, Serialize)]
pub struct EventEnvelope {
    pub id: u64,
    pub data: Value,
}

/// Broadcast channel plus in-memory backlog for recent events.
pub struct EventBus {
    sender: broadcast::Sender<EventEnvelope>,
    backlog: Mutex<VecDeque<EventEnvelope>>,
    next_id: AtomicU64,
    backlog_limit: usize,
    api_version: OnceLock<Value>,
    api_version_notify: Notify,
}

impl EventBus {
    /// Create a new event bus with bounded backlog and broadcast capacity.
    pub fn new(backlog_limit: usize, broadcast_capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(broadcast_capacity.max(1));
        Self {
            sender,
            backlog: Mutex::new(VecDeque::with_capacity(backlog_limit.min(1024))),
            next_id: AtomicU64::new(0),
            backlog_limit: backlog_limit.max(1),
            api_version: OnceLock::new(),
            api_version_notify: Notify::new(),
        }
    }

    /// Add an event to the backlog and publish it to subscribers.
    pub fn push(&self, data: Value) -> EventEnvelope {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let envelope = EventEnvelope { id, data };
        {
            let mut backlog = self.backlog.lock().expect("event backlog lock");
            backlog.push_back(envelope.clone());
            // Keep the backlog bounded to the configured size.
            if backlog.len() > self.backlog_limit {
                let _ = backlog.pop_front();
            }
        }
        let _ = self.sender.send(envelope.clone());
        envelope
    }

    /// Store the upstream ApiVersion event once for new clients.
    pub fn remember_api_version(&self, data: Value) -> bool {
        let stored = self.api_version.set(data).is_ok();
        if stored {
            self.api_version_notify.notify_waiters();
        }
        stored
    }

    /// Return the stored ApiVersion event, if any.
    pub fn api_version(&self) -> Option<Value> {
        self.api_version.get().cloned()
    }

    /// Wait until the ApiVersion event is available.
    pub async fn wait_for_api_version(&self) -> Value {
        loop {
            let notified = self.api_version_notify.notified();
            if let Some(value) = self.api_version() {
                return value;
            }
            notified.await;
        }
    }

    /// Snapshot events with IDs greater than or equal to `start_from`.
    pub fn snapshot_from(&self, start_from: u64) -> Vec<EventEnvelope> {
        let backlog = self.backlog.lock().expect("event backlog lock");
        backlog
            .iter()
            .filter(|event| event.id >= start_from)
            .cloned()
            .collect()
    }

    /// Subscribe to live events via broadcast.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }
}

/// Per-network state shared by handlers.
#[derive(Clone)]
pub struct NetworkState {
    pub config: NetworkConfig,
    pub events: Arc<EventBus>,
    pub binary_pool: Arc<BinaryPool>,
}

/// Application-wide state shared by handlers.
#[derive(Clone)]
pub struct AppState {
    pub networks: Arc<HashMap<String, Arc<NetworkState>>>,
    pub rpc_client: reqwest::Client,
    pub metrics: RpcMetrics,
}

#[cfg(test)]
mod tests {
    use super::EventBus;
    use serde_json::json;

    #[test]
    fn event_bus_assigns_incrementing_ids() {
        let bus = EventBus::new(8, 16);
        let first = bus.push(json!({"value": 1}));
        let second = bus.push(json!({"value": 2}));
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
    }

    #[test]
    fn event_bus_enforces_backlog_limit() {
        let bus = EventBus::new(3, 16);
        for i in 0..5 {
            let _ = bus.push(json!({"value": i}));
        }
        let snapshot = bus.snapshot_from(1);
        let ids: Vec<u64> = snapshot.iter().map(|event| event.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn event_bus_snapshot_is_inclusive() {
        let bus = EventBus::new(4, 16);
        for i in 0..4 {
            let _ = bus.push(json!({"value": i}));
        }
        let snapshot = bus.snapshot_from(3);
        let ids: Vec<u64> = snapshot.iter().map(|event| event.id).collect();
        assert_eq!(ids, vec![3, 4]);
    }
}
