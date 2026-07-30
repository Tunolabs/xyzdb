//! Macro-metric helpers (resource sampling). Operationally the bench
//! collects `docker stats` externally during runs; this module is the
//! placeholder for in-process resource probes if/when needed for finer
//! grain than per-30 s docker samples.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub timestamp_ms: u64,
    pub rss_mb: f64,
    pub cpu_pct: f64,
}

pub fn capture_now() -> ResourceSnapshot {
    ResourceSnapshot {
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        rss_mb: 0.0,
        cpu_pct: 0.0,
    }
}
