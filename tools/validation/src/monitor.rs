// SPDX-License-Identifier: BUSL-1.1
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

#[derive(Debug, Clone, Serialize)]
pub struct ResourceSample {
    pub elapsed_secs: f64,
    pub cpu_percent: f32,
    pub ram_mb: f64,
    pub db_size_mb: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceSummary {
    pub cpu_avg: f32,
    pub cpu_max: f32,
    pub ram_avg_mb: f64,
    pub ram_max_mb: f64,
    pub db_size_start_mb: f64,
    pub db_size_end_mb: f64,
    pub duration_secs: f64,
    pub samples: usize,
}

pub struct ResourceMonitor {
    samples: Arc<RwLock<Vec<ResourceSample>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    start: Instant,
}

impl ResourceMonitor {
    pub fn start(db_path: &Path, server_port: u16, interval: Duration) -> Self {
        let samples = Arc::new(RwLock::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let db_path = db_path.to_path_buf();

        let s = samples.clone();
        let st = stop.clone();
        let start = Instant::now();

        let handle = std::thread::spawn(move || {
            let mut sys = System::new();
            let server_pid = detect_server_pid(&mut sys, server_port);

            while !st.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                sys.refresh_all();

                let cpu_percent = server_pid
                    .and_then(|pid| sys.process(pid))
                    .map(|p| p.cpu_usage())
                    .unwrap_or(0.0);

                let ram_mb = server_pid
                    .and_then(|pid| sys.process(pid))
                    .map(|p| p.memory() as f64 / (1024.0 * 1024.0))
                    .unwrap_or(0.0);

                let db_size_mb = dir_size_mb(&db_path);

                let sample = ResourceSample {
                    elapsed_secs: start.elapsed().as_secs_f64(),
                    cpu_percent,
                    ram_mb,
                    db_size_mb,
                };

                if let Ok(mut guard) = s.write() {
                    guard.push(sample);
                }
            }
        });

        Self {
            samples,
            stop,
            handle: Some(handle),
            start,
        }
    }

    pub fn stop(mut self) -> Vec<ResourceSample> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.samples.read().unwrap().clone()
    }

    pub fn summary(&self) -> ResourceSummary {
        let guard = self.samples.read().unwrap();
        let n = guard.len();
        if n == 0 {
            return ResourceSummary {
                cpu_avg: 0.0, cpu_max: 0.0,
                ram_avg_mb: 0.0, ram_max_mb: 0.0,
                db_size_start_mb: 0.0, db_size_end_mb: 0.0,
                duration_secs: self.start.elapsed().as_secs_f64(),
                samples: 0,
            };
        }
        let cpu_avg = guard.iter().map(|s| s.cpu_percent).sum::<f32>() / n as f32;
        let cpu_max = guard.iter().map(|s| s.cpu_percent).fold(0.0f32, f32::max);
        let ram_avg = guard.iter().map(|s| s.ram_mb).sum::<f64>() / n as f64;
        let ram_max = guard.iter().map(|s| s.ram_mb).fold(0.0f64, f64::max);

        ResourceSummary {
            cpu_avg, cpu_max,
            ram_avg_mb: ram_avg, ram_max_mb: ram_max,
            db_size_start_mb: guard.first().map(|s| s.db_size_mb).unwrap_or(0.0),
            db_size_end_mb: guard.last().map(|s| s.db_size_mb).unwrap_or(0.0),
            duration_secs: self.start.elapsed().as_secs_f64(),
            samples: n,
        }
    }
}

fn detect_server_pid(sys: &mut System, _port: u16) -> Option<Pid> {
    sys.refresh_all();
    for (pid, process) in sys.processes() {
        if process.name().to_string_lossy().contains("xyzdb-server") {
            return Some(*pid);
        }
    }
    None
}

fn dir_size_mb(path: &Path) -> f64 {
    fn walk(p: &Path) -> u64 {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                let meta = entry.metadata();
                if let Ok(m) = meta {
                    if m.is_dir() {
                        total += walk(&entry.path());
                    } else {
                        total += m.len();
                    }
                }
            }
        }
        total
    }
    walk(path) as f64 / (1024.0 * 1024.0)
}
