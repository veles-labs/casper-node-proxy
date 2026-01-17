//! Rate limit helpers for HTTP and per-item JSON-RPC enforcement.

use std::time::Duration;

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::body::Body;
use governor::clock::DefaultClock;
use governor::middleware::StateInformationMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;

/// Build the HTTP rate limiting layer using per-IP quotas.
pub fn rate_limit_layer(
    per_minute: u32,
    burst_size: u32,
) -> GovernorLayer<SmartIpKeyExtractor, StateInformationMiddleware, Body> {
    let period = per_minute_to_period(per_minute);
    let mut builder = GovernorConfigBuilder::default()
        .key_extractor(SmartIpKeyExtractor)
        .use_headers();
    builder.period(period);
    builder.burst_size(burst_size.max(1));
    let config = builder.finish().expect("rate limit config should be valid");
    GovernorLayer::new(config)
}

/// Convert a per-minute limit into a governor period.
fn per_minute_to_period(per_minute: u32) -> Duration {
    if per_minute == 0 {
        return Duration::from_secs(60);
    }
    let millis = 60_000u64 / per_minute as u64;
    Duration::from_millis(millis.max(1))
}

/// Rate limiter used for per-item JSON-RPC enforcement.
pub type RpcRateLimiter = Arc<
    RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock, StateInformationMiddleware>,
>;

/// Build the per-item JSON-RPC rate limiter with burst support.
pub fn rpc_rate_limiter(per_minute: u32, burst_size: u32) -> RpcRateLimiter {
    let period = per_minute_to_period(per_minute);
    let burst = NonZeroU32::new(burst_size.max(1)).expect("non-zero burst");
    let quota = Quota::with_period(period)
        .expect("rate limit period must be non-zero")
        .allow_burst(burst);
    Arc::new(RateLimiter::keyed(quota).with_middleware::<StateInformationMiddleware>())
}
