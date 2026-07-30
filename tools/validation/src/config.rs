use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Clone)]
#[command(name = "xyzdb-validate", about = "xyzDB Validation Suite")]
pub struct Config {
    #[arg(long, default_value = "localhost")]
    pub host: String,

    #[arg(long, default_value_t = 2505)]
    pub port: u16,

    /// Suite to run: 1-10, quick, full, all
    #[arg(long, default_value = "quick")]
    pub suite: String,

    /// Number of clients to generate
    #[arg(long, default_value_t = 10_000)]
    pub clients: u32,

    /// Path to server binary (for durability tests)
    #[arg(long, default_value = "../target/release/xyzdb-server")]
    pub server_bin: PathBuf,

    /// Path for temporary DB (durability tests)
    #[arg(long, default_value = "/tmp/xyzdb-validate")]
    pub db_path: PathBuf,

    /// Resource monitor interval in seconds
    #[arg(long, default_value_t = 2)]
    pub monitor_interval: u64,

    /// Report output path (JSON)
    #[arg(long)]
    pub report: Option<PathBuf>,
}

impl Config {
    pub fn should_run(&self, suite_num: u32) -> bool {
        match self.suite.as_str() {
            "all" => true,
            "quick" => matches!(suite_num, 1 | 2 | 7),
            "full" => true,
            s => s.parse::<u32>().ok() == Some(suite_num),
        }
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
