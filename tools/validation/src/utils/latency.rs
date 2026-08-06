// SPDX-License-Identifier: BUSL-1.1
use std::time::Duration;

/// Collects latency samples and computes percentiles.
pub struct LatencyCollector {
    samples: Vec<Duration>,
}

impl LatencyCollector {
    pub fn new() -> Self {
        Self { samples: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { samples: Vec::with_capacity(cap) }
    }

    pub fn record(&mut self, d: Duration) {
        self.samples.push(d);
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }

    pub fn total(&self) -> Duration {
        self.samples.iter().sum()
    }

    pub fn throughput(&self) -> f64 {
        let secs = self.total().as_secs_f64();
        if secs > 0.0 { self.count() as f64 / secs } else { 0.0 }
    }

    pub fn percentiles(&mut self) -> Percentiles {
        self.samples.sort();
        let n = self.samples.len();
        if n == 0 {
            return Percentiles { p50: Duration::ZERO, p95: Duration::ZERO, p99: Duration::ZERO, count: 0 };
        }
        Percentiles {
            p50: self.samples[n * 50 / 100],
            p95: self.samples[n * 95 / 100],
            p99: self.samples[n.saturating_sub(1).min(n * 99 / 100)],
            count: n,
        }
    }
}

pub struct Percentiles {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub count: usize,
}

impl Percentiles {
    pub fn p50_ms(&self) -> f64 { self.p50.as_secs_f64() * 1000.0 }
    pub fn p95_ms(&self) -> f64 { self.p95.as_secs_f64() * 1000.0 }
    pub fn p99_ms(&self) -> f64 { self.p99.as_secs_f64() * 1000.0 }
}
