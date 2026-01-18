//! Prometheus metrics for the proxy service.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use tower_governor::key_extractor::{KeyExtractor, SmartIpKeyExtractor};

const GLOBAL_NETWORK: &str = "global";

#[derive(Clone)]
pub struct AppMetrics {
    registry: Registry,
    http_requests_total: IntCounterVec,
    http_requests_by_ip_total: IntCounterVec,
    http_request_duration_seconds: HistogramVec,
    http_429_total: IntCounterVec,
    jsonrpc_method_total: IntCounterVec,
    rpc_upstream_duration_seconds: HistogramVec,
    rpc_upstream_errors_total: IntCounterVec,
    rpc_upstream_non_success_total: IntCounterVec,
    sse_ingest_total: IntCounterVec,
    sse_backlog_size: IntGaugeVec,
    sse_backlog_dropped_total: IntCounterVec,
    sse_reconnect_total: IntCounterVec,
    sse_time_to_first_api_version_seconds: HistogramVec,
    sse_connections: IntGaugeVec,
    binary_requests_total: IntCounterVec,
    binary_responses_total: IntCounterVec,
    binary_connections: IntGaugeVec,
    binary_inflight_requests: IntGaugeVec,
    binary_queue_wait_seconds: HistogramVec,
    db_pool_connections: IntGaugeVec,
    db_migrations_total: IntCounterVec,
    db_pool_wait_seconds: HistogramVec,
}

pub struct GaugeGuard {
    metrics: Arc<AppMetrics>,
    network: String,
    kind: GaugeKind,
}

enum GaugeKind {
    EventsConnections,
    BinaryConnections,
    BinaryInflight,
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        match self.kind {
            GaugeKind::EventsConnections => self.metrics.dec_events_connections(&self.network),
            GaugeKind::BinaryConnections => self.metrics.dec_binary_connections(&self.network),
            GaugeKind::BinaryInflight => self.metrics.dec_binary_inflight(&self.network),
        }
    }
}

impl AppMetrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let http_requests_total = register_counter_vec(
            &registry,
            "http_requests_total",
            "HTTP responses by status",
            &["network", "method", "endpoint", "status"],
        );
        let http_requests_by_ip_total = register_counter_vec(
            &registry,
            "http_requests_by_ip_total",
            "HTTP requests by client IP",
            &["network", "ip"],
        );
        let http_request_duration_seconds = register_histogram_vec(
            &registry,
            "http_request_duration_seconds",
            "HTTP request duration in seconds",
            &["network", "method", "endpoint"],
        );
        let http_429_total = register_counter_vec(
            &registry,
            "http_429_total",
            "HTTP 429 responses",
            &["network", "endpoint"],
        );
        let jsonrpc_method_total = register_counter_vec(
            &registry,
            "jsonrpc_method_total",
            "JSON-RPC method call count",
            &["network", "method"],
        );
        let rpc_upstream_duration_seconds = register_histogram_vec(
            &registry,
            "rpc_upstream_duration_seconds",
            "Upstream JSON-RPC request duration in seconds",
            &["network"],
        );
        let rpc_upstream_errors_total = register_counter_vec(
            &registry,
            "rpc_upstream_errors_total",
            "Upstream JSON-RPC transport errors",
            &["network"],
        );
        let rpc_upstream_non_success_total = register_counter_vec(
            &registry,
            "rpc_upstream_non_success_total",
            "Upstream JSON-RPC non-success HTTP responses",
            &["network", "status"],
        );
        let sse_ingest_total = register_counter_vec(
            &registry,
            "sse_ingest_total",
            "SSE events ingested from upstream",
            &["network"],
        );
        let sse_backlog_size = register_gauge_vec(
            &registry,
            "sse_backlog_size",
            "Current SSE backlog size",
            &["network"],
        );
        let sse_backlog_dropped_total = register_counter_vec(
            &registry,
            "sse_backlog_dropped_total",
            "SSE backlog events dropped due to limits",
            &["network"],
        );
        let sse_reconnect_total = register_counter_vec(
            &registry,
            "sse_reconnect_total",
            "SSE listener reconnects",
            &["network"],
        );
        let sse_time_to_first_api_version_seconds = register_histogram_vec(
            &registry,
            "sse_time_to_first_api_version_seconds",
            "Time to first ApiVersion event after connecting",
            &["network"],
        );
        let sse_connections = register_gauge_vec(
            &registry,
            "sse_connections",
            "Active SSE/WS event stream connections",
            &["network"],
        );
        let binary_requests_total = register_counter_vec(
            &registry,
            "binary_requests_total",
            "Binary port requests received",
            &["network"],
        );
        let binary_responses_total = register_counter_vec(
            &registry,
            "binary_responses_total",
            "Binary port responses returned",
            &["network", "outcome"],
        );
        let binary_connections = register_gauge_vec(
            &registry,
            "binary_connections",
            "Active binary websocket connections",
            &["network"],
        );
        let binary_inflight_requests = register_gauge_vec(
            &registry,
            "binary_inflight_requests",
            "Binary port requests in flight",
            &["network"],
        );
        let binary_queue_wait_seconds = register_histogram_vec(
            &registry,
            "binary_queue_wait_seconds",
            "Time waiting for a binary pool connection",
            &["network"],
        );
        let db_pool_connections = register_gauge_vec(
            &registry,
            "db_pool_connections",
            "Database pool connection counts",
            &["network", "state"],
        );
        let db_migrations_total = register_counter_vec(
            &registry,
            "db_migrations_total",
            "Database migrations outcomes",
            &["network", "outcome"],
        );
        let db_pool_wait_seconds = register_histogram_vec(
            &registry,
            "db_pool_wait_seconds",
            "Database connection checkout wait time in seconds",
            &["network", "operation"],
        );

        Self {
            registry,
            http_requests_total,
            http_requests_by_ip_total,
            http_request_duration_seconds,
            http_429_total,
            jsonrpc_method_total,
            rpc_upstream_duration_seconds,
            rpc_upstream_errors_total,
            rpc_upstream_non_success_total,
            sse_ingest_total,
            sse_backlog_size,
            sse_backlog_dropped_total,
            sse_reconnect_total,
            sse_time_to_first_api_version_seconds,
            sse_connections,
            binary_requests_total,
            binary_responses_total,
            binary_connections,
            binary_inflight_requests,
            binary_queue_wait_seconds,
            db_pool_connections,
            db_migrations_total,
            db_pool_wait_seconds,
        }
    }

    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        let _ = encoder.encode(&self.registry.gather(), &mut buffer);
        String::from_utf8(buffer).unwrap_or_default()
    }

    pub fn http_observe(
        &self,
        network: &str,
        method: &str,
        endpoint: &str,
        status: u16,
        duration: Duration,
        client_ip: Option<&str>,
    ) {
        let status_str = status.to_string();
        self.http_requests_total
            .with_label_values(&[network, method, endpoint, &status_str])
            .inc();
        self.http_request_duration_seconds
            .with_label_values(&[network, method, endpoint])
            .observe(duration.as_secs_f64());
        if status == 429 {
            self.http_429_total
                .with_label_values(&[network, endpoint])
                .inc();
        }
        if let Some(ip) = client_ip {
            self.http_requests_by_ip_total
                .with_label_values(&[network, ip])
                .inc();
        }
    }

    pub fn increment_jsonrpc_method(&self, network: &str, method: &str) {
        self.jsonrpc_method_total
            .with_label_values(&[network, method])
            .inc();
    }

    pub fn observe_rpc_upstream_duration(&self, network: &str, duration: Duration) {
        self.rpc_upstream_duration_seconds
            .with_label_values(&[network])
            .observe(duration.as_secs_f64());
    }

    pub fn increment_rpc_upstream_error(&self, network: &str) {
        self.rpc_upstream_errors_total
            .with_label_values(&[network])
            .inc();
    }

    pub fn increment_rpc_upstream_non_success(&self, network: &str, status: u16) {
        let status_str = status.to_string();
        self.rpc_upstream_non_success_total
            .with_label_values(&[network, &status_str])
            .inc();
    }

    pub fn increment_sse_ingest(&self, network: &str) {
        self.sse_ingest_total.with_label_values(&[network]).inc();
    }

    pub fn set_sse_backlog_size(&self, network: &str, size: usize) {
        self.sse_backlog_size
            .with_label_values(&[network])
            .set(size as i64);
    }

    pub fn increment_sse_backlog_dropped(&self, network: &str) {
        self.sse_backlog_dropped_total
            .with_label_values(&[network])
            .inc();
    }

    pub fn increment_sse_reconnect(&self, network: &str) {
        self.sse_reconnect_total.with_label_values(&[network]).inc();
    }

    pub fn observe_sse_time_to_first_api_version(&self, network: &str, duration: Duration) {
        self.sse_time_to_first_api_version_seconds
            .with_label_values(&[network])
            .observe(duration.as_secs_f64());
    }

    pub fn events_connection_guard(self: &Arc<Self>, network: &str) -> GaugeGuard {
        self.sse_connections.with_label_values(&[network]).inc();
        GaugeGuard {
            metrics: Arc::clone(self),
            network: network.to_string(),
            kind: GaugeKind::EventsConnections,
        }
    }

    fn dec_events_connections(&self, network: &str) {
        self.sse_connections.with_label_values(&[network]).dec();
    }

    pub fn increment_binary_requests(&self, network: &str) {
        self.binary_requests_total
            .with_label_values(&[network])
            .inc();
    }

    pub fn increment_binary_response(&self, network: &str, outcome: &str) {
        self.binary_responses_total
            .with_label_values(&[network, outcome])
            .inc();
    }

    pub fn binary_connection_guard(self: &Arc<Self>, network: &str) -> GaugeGuard {
        self.binary_connections.with_label_values(&[network]).inc();
        GaugeGuard {
            metrics: Arc::clone(self),
            network: network.to_string(),
            kind: GaugeKind::BinaryConnections,
        }
    }

    fn dec_binary_connections(&self, network: &str) {
        self.binary_connections.with_label_values(&[network]).dec();
    }

    pub fn binary_inflight_guard(self: &Arc<Self>, network: &str) -> GaugeGuard {
        self.binary_inflight_requests
            .with_label_values(&[network])
            .inc();
        GaugeGuard {
            metrics: Arc::clone(self),
            network: network.to_string(),
            kind: GaugeKind::BinaryInflight,
        }
    }

    fn dec_binary_inflight(&self, network: &str) {
        self.binary_inflight_requests
            .with_label_values(&[network])
            .dec();
    }

    pub fn observe_binary_queue_wait(&self, network: &str, duration: Duration) {
        self.binary_queue_wait_seconds
            .with_label_values(&[network])
            .observe(duration.as_secs_f64());
    }

    pub fn set_db_pool_stats(&self, total: u32, idle: u32) {
        let network = GLOBAL_NETWORK;
        let in_use = total.saturating_sub(idle);
        self.db_pool_connections
            .with_label_values(&[network, "total"])
            .set(total as i64);
        self.db_pool_connections
            .with_label_values(&[network, "idle"])
            .set(idle as i64);
        self.db_pool_connections
            .with_label_values(&[network, "in_use"])
            .set(in_use as i64);
    }

    pub fn increment_db_migration(&self, outcome: &str) {
        self.db_migrations_total
            .with_label_values(&[GLOBAL_NETWORK, outcome])
            .inc();
    }

    pub fn observe_db_pool_wait(&self, operation: &str, duration: Duration) {
        self.db_pool_wait_seconds
            .with_label_values(&[GLOBAL_NETWORK, operation])
            .observe(duration.as_secs_f64());
    }
}

pub fn network_from_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let mut segments = trimmed.split('/');
    let candidate = segments.next().unwrap_or("");
    if candidate.is_empty() {
        GLOBAL_NETWORK.to_string()
    } else {
        candidate.to_string()
    }
}

pub fn endpoint_from_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    let mut segments = trimmed.split('/');
    let _network = segments.next();
    match segments.next() {
        Some("") | None => "root".to_string(),
        Some("rpc") => "rpc".to_string(),
        Some("events") => "events".to_string(),
        Some("binary") => "binary".to_string(),
        Some(_) => "other".to_string(),
    }
}

pub async fn track_http(
    State(metrics): State<Arc<AppMetrics>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let network = network_from_path(&path);
    let endpoint = endpoint_from_path(&path);
    let client_ip = SmartIpKeyExtractor
        .extract(&req)
        .ok()
        .map(|ip| ip.to_string());
    let start = Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    metrics.http_observe(
        &network,
        &method,
        &endpoint,
        status,
        start.elapsed(),
        client_ip.as_deref(),
    );
    response
}

pub fn router(metrics: Arc<AppMetrics>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics)
}

async fn metrics_handler(State(metrics): State<Arc<AppMetrics>>) -> Response {
    let body = metrics.render();
    Response::builder()
        .header("content-type", "text/plain; version=0.0.4")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn register_counter_vec(
    registry: &Registry,
    name: &str,
    help: &str,
    labels: &[&str],
) -> IntCounterVec {
    let counter = IntCounterVec::new(Opts::new(name, help), labels).expect("counter config");
    registry
        .register(Box::new(counter.clone()))
        .expect("register counter");
    counter
}

fn register_gauge_vec(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> IntGaugeVec {
    let gauge = IntGaugeVec::new(Opts::new(name, help), labels).expect("gauge config");
    registry
        .register(Box::new(gauge.clone()))
        .expect("register gauge");
    gauge
}

fn register_histogram_vec(
    registry: &Registry,
    name: &str,
    help: &str,
    labels: &[&str],
) -> HistogramVec {
    let histogram =
        HistogramVec::new(HistogramOpts::new(name, help), labels).expect("histogram config");
    registry
        .register(Box::new(histogram.clone()))
        .expect("register histogram");
    histogram
}
