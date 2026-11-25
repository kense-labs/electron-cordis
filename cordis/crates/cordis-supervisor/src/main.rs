//! CLI entrypoint for M5 Stage B top-level Rust Supervisor (`electron-cordis-supervisor`).
//!
//! Usage:
//!   electron-cordis-supervisor --daemon-bin <path> [--load-plan <path>] [--headless | --ui-bin <path> ...]

use std::env;
use std::path::PathBuf;

use electron_cordis_supervisor::{SupervisorConfig, run_supervised};

fn parse_args() -> Result<SupervisorConfig, String> {
    let mut args = env::args().skip(1);
    let mut daemon_bin: Option<PathBuf> = None;
    let mut load_plan: Option<PathBuf> = None;
    let mut private_volume_dir: Option<PathBuf> = None;
    let mut ui_bin: Option<PathBuf> = None;
    let mut ui_args: Vec<String> = Vec::new();
    let mut max_ui_restarts: usize = 3;
    let mut headless = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--daemon-bin" => daemon_bin = args.next().map(PathBuf::from),
            "--load-plan" => load_plan = args.next().map(PathBuf::from),
            "--private-volume-dir" => private_volume_dir = args.next().map(PathBuf::from),
            "--headless" => headless = true,
            "--ui-bin" => ui_bin = args.next().map(PathBuf::from),
            "--ui-arg" => {
                if let Some(v) = args.next() {
                    ui_args.push(v);
                }
            }
            "--max-ui-restarts" => {
                if let Some(v) = args.next() {
                    max_ui_restarts = v.parse().unwrap_or(3);
                }
            }
            _ => {}
        }
    }

    let daemon_bin = daemon_bin.ok_or("--daemon-bin is required")?;
    if headless {
        ui_bin = None;
    }

    Ok(SupervisorConfig {
        daemon_bin,
        load_plan,
        private_volume_dir,
        ui_bin,
        ui_args,
        max_ui_restarts,
        extra_daemon_args: Vec::new(),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = parse_args()?;
    let report = run_supervised(&cfg).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.clean_shutdown {
        std::process::exit(1);
    }
    Ok(())
}
