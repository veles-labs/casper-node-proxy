//! Rate limit helpers for HTTP requests.

use std::num::NonZeroU32;
use std::time::Duration;

use axum::body::Body;
use governor::clock::DefaultClock;
use governor::middleware::StateInformationMiddleware;
use governor::state::{InMemoryState, NotKeyed};
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

pub type BinaryConnectionRateLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Build a per-connection rate limiter for binary-port requests.
pub fn binary_connection_rate_limiter(
    per_minute: u32,
    burst_size: u32,
) -> BinaryConnectionRateLimiter {
    let period = per_minute_to_period(per_minute);
    let burst = NonZeroU32::new(burst_size.max(1)).expect("non-zero burst");
    let quota = Quota::with_period(period)
        .expect("rate limit period must be non-zero")
        .allow_burst(burst);
    RateLimiter::direct(quota)
}
