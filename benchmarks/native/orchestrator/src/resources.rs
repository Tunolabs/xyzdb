//! Resource sampling for cross-engine bench runs.
//!
//! Spawns a background thread that polls `docker stats` (CPU%, mem)
//! every `CPU_SAMPLE_SECS` and `du -sb` (disk) every `DISK_SAMPLE_SECS`,
//! tagging each sample with the orchestrator's current phase. On stop,
//! aggregates to peak / avg / final values and returns
//! [`ResourceMetrics`].
//!
//! `docker stats --no-stream --format <go-template>` is used because
//! the JSON formatter on older Docker versions emits non-stable shape;
//! the `MemUsage` and `CPUPerc` fields in template form are stable
//! across versions.
//!
//! The sampler is best-effort: if `docker stats` or `du` fail (e.g.,
//! container has not started yet), the failure is logged and the
//! sampling thread continues. A run that finishes with zero samples
//! still produces a `ResourceMetrics` with `n_samples=0` so the
//! caller's report shape stays stable.

use native_generator::bench::{ResourceMetrics, ResourceSample};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

const CPU_SAMPLE_SECS: u64 = 5;
const DISK_SAMPLE_SECS: u64 = 30;

pub struct ResourceSampler {
    container: String,
    data_path: String,
    samples: Arc<Mutex<Vec<ResourceSample>>>,
    current_phase: Arc<Mutex<String>>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    t0: Instant,
}

impl ResourceSampler {
    /// Start the sampler. Returns a handle whose `set_phase` should be
    /// called as the orchestrator advances phases. `stop()` consumes the
    /// handle, joins the thread, and returns the aggregate.
    ///
    /// `container` may be empty — the sampler then becomes a no-op
    /// (still emits `ResourceMetrics` with `n_samples=0`).
    pub fn start(container: &str, data_path: &str, t0: Instant) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let samples: Arc<Mutex<Vec<ResourceSample>>> = Arc::new(Mutex::new(Vec::new()));
        let current_phase: Arc<Mutex<String>> = Arc::new(Mutex::new("phase0".to_string()));

        let handle = if container.is_empty() {
            None
        } else {
            let container = container.to_string();
            let data_path = data_path.to_string();
            let samples_t = samples.clone();
            let phase_t = current_phase.clone();
            let stop_t = stop_flag.clone();
            Some(thread::spawn(move || {
                run_loop(&container, &data_path, t0, samples_t, phase_t, stop_t);
            }))
        };
        Self {
            container: container.to_string(),
            data_path: data_path.to_string(),
            samples,
            current_phase,
            stop_flag,
            handle,
            t0,
        }
    }

    pub fn set_phase(&self, name: &str) {
        if let Ok(mut g) = self.current_phase.lock() {
            *g = name.to_string();
        }
    }

    /// Stop the sampler thread, join, aggregate.
    pub fn stop(mut self) -> ResourceMetrics {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let samples = self.samples.lock().map(|g| g.clone()).unwrap_or_default();
        let n = samples.len();
        let mut cpu_peak: f64 = 0.0;
        let mut mem_peak: f64 = 0.0;
        let mut disk_peak: f64 = 0.0;
        let mut cpu_sum: f64 = 0.0;
        let mut mem_sum: f64 = 0.0;
        let mut disk_final: f64 = 0.0;
        for s in &samples {
            cpu_peak = cpu_peak.max(s.cpu_percent);
            mem_peak = mem_peak.max(s.mem_mb);
            disk_peak = disk_peak.max(s.disk_mb);
            cpu_sum += s.cpu_percent;
            mem_sum += s.mem_mb;
            disk_final = s.disk_mb;
        }
        let n_f = n.max(1) as f64;
        ResourceMetrics {
            container: self.container,
            data_path: self.data_path,
            samples,
            cpu_peak,
            cpu_avg: cpu_sum / n_f,
            mem_peak_mb: mem_peak,
            mem_avg_mb: mem_sum / n_f,
            disk_peak_mb: disk_peak,
            disk_final_mb: disk_final,
            n_samples: n,
        }
    }

    /// Snapshot current values without stopping the thread.
    #[allow(dead_code)]
    pub fn live_snapshot(&self) -> Option<ResourceSample> {
        self.samples.lock().ok()?.last().cloned()
    }

    #[allow(dead_code)]
    pub fn t0(&self) -> Instant {
        self.t0
    }
}

fn run_loop(
    container: &str,
    data_path: &str,
    t0: Instant,
    samples: Arc<Mutex<Vec<ResourceSample>>>,
    current_phase: Arc<Mutex<String>>,
    stop_flag: Arc<AtomicBool>,
) {
    let mut last_disk_mb: f64 = 0.0;
    let mut iter: u64 = 0;
    while !stop_flag.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(CPU_SAMPLE_SECS));
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let stats = read_docker_stats(container);
        // Disk is sampled every DISK_SAMPLE_SECS / CPU_SAMPLE_SECS iterations.
        let due_disk = iter % (DISK_SAMPLE_SECS / CPU_SAMPLE_SECS).max(1) == 0;
        if due_disk && !data_path.is_empty() {
            if let Some(b) = read_disk_bytes(data_path) {
                last_disk_mb = (b as f64) / (1024.0 * 1024.0);
            }
        }
        if let Some((cpu, mem)) = stats {
            let phase = current_phase.lock().map(|g| g.clone()).unwrap_or_default();
            let s = ResourceSample {
                ts_secs: t0.elapsed().as_secs_f64(),
                phase,
                cpu_percent: cpu,
                mem_mb: mem,
                disk_mb: last_disk_mb,
            };
            if let Ok(mut g) = samples.lock() {
                g.push(s);
            }
        }
        iter += 1;
    }
    // Final disk sample on stop.
    if !data_path.is_empty() {
        if let Some(b) = read_disk_bytes(data_path) {
            let final_mb = (b as f64) / (1024.0 * 1024.0);
            if let Ok(mut g) = samples.lock() {
                if let Some(last) = g.last().cloned() {
                    g.push(ResourceSample {
                        ts_secs: t0.elapsed().as_secs_f64(),
                        phase: "post_phase5".to_string(),
                        cpu_percent: last.cpu_percent,
                        mem_mb: last.mem_mb,
                        disk_mb: final_mb,
                    });
                }
            }
        }
    }
}

/// Returns (cpu_percent, mem_mb).
fn read_docker_stats(container: &str) -> Option<(f64, f64)> {
    // Format: `<cpu>|<mem usage / limit>` e.g. `124.34%|512MiB / 8GiB`
    let out = Command::new("docker")
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.CPUPerc}}|{{.MemUsage}}",
            container,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        debug!(target: "resources", "docker stats failed for {container}");
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?.trim();
    let mut parts = line.split('|');
    let cpu_s = parts.next()?.trim().trim_end_matches('%');
    let mem_part = parts.next()?.trim();
    let cpu: f64 = cpu_s.parse().ok()?;
    // mem_part is "X.YMiB / Z.ZGiB" → take left side.
    let mem_str = mem_part.split('/').next()?.trim();
    let mem_mb = parse_size_to_mb(mem_str)?;
    Some((cpu, mem_mb))
}

fn parse_size_to_mb(s: &str) -> Option<f64> {
    // Accepts: "123MiB", "1.2GiB", "456KiB", "12B", "100MB", "1GB", etc.
    let s = s.trim();
    let (num_end, mult): (usize, f64) = if let Some(p) = s.find("KiB") {
        (p, 1.0 / 1024.0)
    } else if let Some(p) = s.find("MiB") {
        (p, 1.0)
    } else if let Some(p) = s.find("GiB") {
        (p, 1024.0)
    } else if let Some(p) = s.find("TiB") {
        (p, 1024.0 * 1024.0)
    } else if let Some(p) = s.find("KB") {
        (p, 1.0 / 1000.0)
    } else if let Some(p) = s.find("MB") {
        (p, 1.0)
    } else if let Some(p) = s.find("GB") {
        (p, 1024.0)
    } else if let Some(p) = s.find('B') {
        (p, 1.0 / (1024.0 * 1024.0))
    } else {
        return None;
    };
    let n: f64 = s[..num_end].trim().parse().ok()?;
    Some(n * mult)
}

/// `du -sb <path>` returns total bytes (logical). Falls back to None on
/// macOS where -sb is not in `du`'s manual; we use `du -s -k` and
/// multiply by 1024 instead.
fn read_disk_bytes(path: &str) -> Option<u64> {
    // macOS `du` doesn't have -b; -k yields kilobytes (block-allocated).
    let out = Command::new("du").args(["-sk", path]).output().ok()?;
    if !out.status.success() {
        warn!(target: "resources", "du failed for {path}");
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?.trim();
    let kb: u64 = line.split_whitespace().next()?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sizes() {
        assert!((parse_size_to_mb("128MiB").unwrap() - 128.0).abs() < 1e-6);
        assert!((parse_size_to_mb("1.5GiB").unwrap() - 1536.0).abs() < 1e-3);
        assert!((parse_size_to_mb("512KiB").unwrap() - 0.5).abs() < 1e-3);
    }
}
