use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Adaptive write throttle based on LSM-tree compaction pressure.
/// Monitors L0 table count and sealed memtable count — the direct signals
/// that compaction can't keep up with writes. Read latency is NOT used
/// because under concurrency reads are naturally slower and don't indicate
/// write-side problems.
pub struct WriteThrottle {
    mode: RwLock<ThrottleMode>,
    write_count: AtomicU64,
    window_start: RwLock<Instant>,
    stall_count: AtomicU64,
    in_stall: AtomicBool,
    config: ThrottleConfig,
    enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ThrottleConfig {
    /// L0 tables above this = Degraded (moderate throttle)
    pub l0_degraded: usize,
    /// L0 tables above this = Critical (aggressive throttle)
    pub l0_critical: usize,
    /// Sealed memtables above this = Paused (stall writes)
    pub sealed_stall: usize,
    /// Max writes/s in Degraded mode
    pub max_writes_degraded: u64,
    /// Max writes/s in Critical mode
    pub max_writes_critical: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self::profile_balanced()
    }
}

impl ThrottleConfig {
    pub fn profile_transactional() -> Self {
        Self {
            l0_degraded: 4,
            l0_critical: 8,
            sealed_stall: 2,
            max_writes_degraded: 5_000,
            max_writes_critical: 1_000,
        }
    }

    pub fn profile_analytical() -> Self {
        Self {
            l0_degraded: 12,
            l0_critical: 24,
            sealed_stall: 4,
            max_writes_degraded: u64::MAX,
            max_writes_critical: 10_000,
        }
    }

    pub fn profile_balanced() -> Self {
        Self {
            l0_degraded: 8,
            l0_critical: 16,
            sealed_stall: 3,
            max_writes_degraded: 8_000,
            max_writes_critical: 2_000,
        }
    }

    pub fn profile_maintenance() -> Self {
        Self {
            l0_degraded: 32,
            l0_critical: 64,
            sealed_stall: 8,
            max_writes_degraded: u64::MAX,
            max_writes_critical: u64::MAX,
        }
    }

    /// Bulk load: throttle completely disabled.
    pub fn profile_bulk() -> Self {
        Self {
            l0_degraded: usize::MAX,
            l0_critical: usize::MAX,
            sealed_stall: usize::MAX,
            max_writes_degraded: u64::MAX,
            max_writes_critical: u64::MAX,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "transactional" => Some(Self::profile_transactional()),
            "analytical" => Some(Self::profile_analytical()),
            "balanced" => Some(Self::profile_balanced()),
            "maintenance" => Some(Self::profile_maintenance()),
            "bulk" => Some(Self::profile_bulk()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThrottleMode {
    Healthy,
    Degraded,
    Critical,
    Paused,
}

impl WriteThrottle {
    pub fn new(config: ThrottleConfig) -> Self {
        Self {
            mode: RwLock::new(ThrottleMode::Healthy),
            write_count: AtomicU64::new(0),
            window_start: RwLock::new(Instant::now()),
            stall_count: AtomicU64::new(0),
            in_stall: AtomicBool::new(false),
            config,
            enabled: true,
        }
    }

    pub fn disabled() -> Self {
        let mut t = Self::new(ThrottleConfig::profile_maintenance());
        t.enabled = false;
        t
    }

    /// Record a read operation's latency. Kept for API compatibility but no
    /// longer used for throttle evaluation.
    pub fn record_read(&self, _latency: Duration) {}

    /// Record a write operation's latency. No longer triggers stall detection
    /// directly — stalls are detected via sealed memtable count.
    pub fn record_write(&self, _latency: Duration) {}

    /// Check if a write should proceed. Returns delay duration.
    pub fn write_delay(&self) -> Duration {
        if !self.enabled {
            return Duration::ZERO;
        }

        let mode = *self.mode.read();
        let limit = match mode {
            ThrottleMode::Healthy => return Duration::ZERO,
            ThrottleMode::Degraded => self.config.max_writes_degraded,
            ThrottleMode::Critical => self.config.max_writes_critical,
            ThrottleMode::Paused => {
                return Duration::from_millis(50);
            }
        };

        if limit == u64::MAX {
            return Duration::ZERO;
        }

        let count = self.write_count.fetch_add(1, Ordering::Relaxed);
        let elapsed = self.window_start.read().elapsed();

        if elapsed >= Duration::from_secs(1) {
            self.write_count.store(0, Ordering::Relaxed);
            *self.window_start.write() = Instant::now();
            return Duration::ZERO;
        }

        if count >= limit {
            let remaining = Duration::from_secs(1).saturating_sub(elapsed);
            self.write_count.store(0, Ordering::Relaxed);
            *self.window_start.write() = Instant::now();
            return remaining;
        }

        Duration::ZERO
    }

    /// Re-evaluate throttle mode based on L0 table count and sealed memtable count.
    /// Called periodically from the server connection handler.
    pub fn evaluate_lsm(&self, l0_count: usize, sealed_count: usize) {
        if !self.enabled {
            return;
        }

        let new_mode = if sealed_count >= self.config.sealed_stall {
            ThrottleMode::Paused
        } else if l0_count >= self.config.l0_critical {
            ThrottleMode::Critical
        } else if l0_count >= self.config.l0_degraded {
            ThrottleMode::Degraded
        } else {
            ThrottleMode::Healthy
        };

        let old_mode = *self.mode.read();
        if new_mode != old_mode {
            if new_mode == ThrottleMode::Paused {
                if !self.in_stall.swap(true, Ordering::Relaxed) {
                    self.stall_count.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "Throttle: {:?} -> Paused (L0={}, sealed={})",
                        old_mode,
                        l0_count,
                        sealed_count,
                    );
                }
            } else {
                if self.in_stall.swap(false, Ordering::Relaxed) {
                    tracing::info!(
                        "Throttle: stall resolved (L0={}, sealed={})",
                        l0_count,
                        sealed_count
                    );
                }
                tracing::info!(
                    "Throttle: {:?} -> {:?} (L0={}, sealed={})",
                    old_mode,
                    new_mode,
                    l0_count,
                    sealed_count,
                );
            }
            *self.mode.write() = new_mode;
        }
    }

    /// Get current status for SHOW THROTTLE.
    pub fn status(&self) -> ThrottleStatus {
        ThrottleStatus {
            mode: *self.mode.read(),
            enabled: self.enabled,
            stall_count: self.stall_count.load(Ordering::Relaxed),
            in_stall: self.in_stall.load(Ordering::Relaxed),
            l0_degraded: self.config.l0_degraded,
            l0_critical: self.config.l0_critical,
            sealed_stall: self.config.sealed_stall,
            max_writes_degraded: self.config.max_writes_degraded,
            max_writes_critical: self.config.max_writes_critical,
        }
    }
}

pub struct ThrottleStatus {
    pub mode: ThrottleMode,
    pub enabled: bool,
    pub stall_count: u64,
    pub in_stall: bool,
    pub l0_degraded: usize,
    pub l0_critical: usize,
    pub sealed_stall: usize,
    pub max_writes_degraded: u64,
    pub max_writes_critical: u64,
}

impl std::fmt::Display for ThrottleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Throttle Status:")?;
        writeln!(f, "  Enabled:      {}", self.enabled)?;
        writeln!(f, "  Mode:         {:?}", self.mode)?;
        writeln!(
            f,
            "  Stalls:       {} (currently: {})",
            self.stall_count,
            if self.in_stall { "IN STALL" } else { "clear" }
        )?;
        writeln!(f, "  Config:")?;
        writeln!(
            f,
            "    L0 Degraded:  > {} tables (limit: {}/s)",
            self.l0_degraded, self.max_writes_degraded
        )?;
        writeln!(
            f,
            "    L0 Critical:  > {} tables (limit: {}/s)",
            self.l0_critical, self.max_writes_critical
        )?;
        writeln!(
            f,
            "    Sealed Stall: > {} memtables (paused)",
            self.sealed_stall
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod throttle_recovery_tests {
    use super::*;
    use std::time::Duration;

    /// 8b — the throttle back-pressures under a sealed-memtable backlog
    /// (Paused) and RECOVERS to Healthy once the backlog drains. The mode is a
    /// pure function of the live (l0, sealed) counts (`evaluate_lsm`), not a
    /// latch, so recovery is automatic when pressure clears.
    #[test]
    fn paused_recovers_to_healthy_when_pressure_clears() {
        let t = WriteThrottle::new(ThrottleConfig::profile_transactional());
        // Healthy at rest → no delay.
        assert_eq!(t.write_delay(), Duration::ZERO);
        // Sealed backlog above sealed_stall → Paused → back-pressure (delay).
        t.evaluate_lsm(0, 100);
        assert!(
            t.write_delay() > Duration::ZERO,
            "Paused must back-pressure writes with a non-zero delay"
        );
        // Backlog drains → recovers to Healthy → no delay again.
        t.evaluate_lsm(0, 0);
        assert_eq!(
            t.write_delay(),
            Duration::ZERO,
            "throttle must recover to Healthy once the backlog clears (not a latch)"
        );
    }
}
