use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since Unix epoch.
///
/// Used ONLY at the network boundary to stamp outgoing messages so the client
/// can run TimeSync (RTT/jitter/offset estimation). Wall-clock is forbidden
/// inside `src/game/**` for gameplay decisions — that code reads server ticks
/// instead.
pub fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
