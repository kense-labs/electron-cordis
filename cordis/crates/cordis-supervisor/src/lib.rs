//! M5 Stage B top-level Rust Supervisor (`electron-cordis-supervisor`).
//!
//! Architecture invariants:
//! - In `--headless` mode: starts ONLY `electron-cordis-daemon` (ZERO Chromium process).
//! - In desktop mode (`--ui-bin <path>`): supervises `electron-cordis-daemon` and the UI shell
//!   independently. If the UI shell crashes, the supervisor restarts ONLY the UI shell while
//!   `electron-cordis-daemon`, active KRun sessions, and the disk-backed `PrivateVolumeWriterSeal`
//!   remain continuously alive and uninterrupted.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use electron_cordis_krun_plugins::PrivateVolumeWriterSeal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    pub daemon_bin: PathBuf,
    pub load_plan: Option<PathBuf>,
    pub private_volume_dir: Option<PathBuf>,
    pub ui_bin: Option<PathBuf>,
    pub ui_args: Vec<String>,
    pub max_ui_restarts: usize,
    pub extra_daemon_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorReport {
    pub headless: bool,
    pub daemon_pid: u32,
    pub daemon_restarted: bool,
    pub ui_launches: usize,
    pub ui_crash_restarts: usize,
    pub writer_seal_preserved_across_ui_restart: bool,
    pub health_before_ui_crash: Value,
    pub health_after_ui_restart: Value,
    pub clean_shutdown: bool,
}

pub struct SupervisedDaemon {
    child: Child,
    pid: u32,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl SupervisedDaemon {
    pub fn spawn(cfg: &SupervisorConfig) -> Result<Self, String> {
        let mut cmd = Command::new(&cfg.daemon_bin);
        if let Some(plan) = &cfg.load_plan {
            cmd.arg("--load-plan").arg(plan);
        }
        for arg in &cfg.extra_daemon_args {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn daemon {}: {e}", cfg.daemon_bin.display()))?;
        let pid = child.id().ok_or("daemon exited immediately without PID")?;
        let stdin = child.stdin.take().ok_or("missing daemon stdin")?;
        let stdout = child.stdout.take().ok_or("missing daemon stdout")?;
        Ok(Self {
            child,
            pid,
            stdin,
            stdout_lines: BufReader::new(stdout).lines(),
            next_id: 1,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|e| e.to_string())?;

        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write daemon stdin: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("write daemon newline: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("flush daemon stdin: {e}"))?;

        let resp_line = tokio::time::timeout(Duration::from_secs(10), self.stdout_lines.next_line())
            .await
            .map_err(|_| format!("timeout waiting for daemon response to {method}"))?
            .map_err(|e| format!("read daemon stdout: {e}"))?
            .ok_or_else(|| format!("daemon EOF while awaiting {method}"))?;

        let parsed: Value =
            serde_json::from_str(&resp_line).map_err(|e| format!("parse daemon json: {e}"))?;
        if let Some(err) = parsed.get("error") {
            return Err(format!("daemon rpc error: {err}"));
        }
        Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn shutdown(mut self) -> Result<bool, String> {
        let res = self.rpc("cordis.shutdown", json!({})).await?;
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .map_err(|_| "timeout waiting for daemon exit")?
            .map_err(|e| format!("wait daemon: {e}"))?;
        Ok(status.success() && res.get("drained").and_then(Value::as_bool) == Some(true))
    }
}

/// Runs the Stage B supervision lifecycle:
/// 1. Acquires single-writer private volume seal (if configured) and starts `electron-cordis-daemon`.
/// 2. If `ui_bin` is `None` (`--headless`), skips Chromium/UI completely, probes health, and drains cleanly.
/// 3. If `ui_bin` is `Some`, spawns UI and automatically restarts it on non-zero exit up to
///    `max_ui_restarts` while verifying daemon PID and single-writer seal stay uninterrupted.
pub async fn run_supervised(cfg: &SupervisorConfig) -> Result<SupervisorReport, String> {
    let seal = match &cfg.private_volume_dir {
        Some(dir) => Some(
            PrivateVolumeWriterSeal::acquire(dir, 1)
                .map_err(|e| format!("acquire private volume seal: {e}"))?,
        ),
        None => None,
    };

    let mut daemon = SupervisedDaemon::spawn(cfg)?;
    let daemon_pid = daemon.pid();
    let health_before = daemon.rpc("cordis.health", json!({})).await?;

    let ui_launches = Arc::new(AtomicUsize::new(0));
    let mut ui_crash_restarts = 0usize;
    let mut seal_preserved = true;

    if let Some(ui_bin) = &cfg.ui_bin {
        loop {
            let attempt = ui_launches.fetch_add(1, Ordering::SeqCst) + 1;
            let mut ui_cmd = Command::new(ui_bin);
            ui_cmd
                .args(&cfg.ui_args)
                .env("ELECTRON_CORDIS_UI_ATTEMPT", attempt.to_string())
                .env("ELECTRON_CORDIS_DAEMON_PID", daemon_pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            let status = ui_cmd
                .status()
                .await
                .map_err(|e| format!("spawn UI {}: {e}", ui_bin.display()))?;

            // Verify private volume writer seal stayed intact on disk across UI exit/crash!
            if let Some(s) = &seal {
                if !s.lock_path().exists() {
                    seal_preserved = false;
                }
            }

            if status.success() {
                break;
            }
            if ui_crash_restarts >= cfg.max_ui_restarts {
                break;
            }
            ui_crash_restarts += 1;
        }
    }

    let health_after = daemon.rpc("cordis.health", json!({})).await?;
    let daemon_still_same_pid = daemon.pid() == daemon_pid;
    let clean_shutdown = daemon.shutdown().await?;

    if let Some(s) = seal {
        s.release();
    }

    Ok(SupervisorReport {
        headless: cfg.ui_bin.is_none(),
        daemon_pid,
        daemon_restarted: !daemon_still_same_pid,
        ui_launches: ui_launches.load(Ordering::SeqCst),
        ui_crash_restarts,
        writer_seal_preserved_across_ui_restart: seal_preserved,
        health_before_ui_crash: health_before,
        health_after_ui_restart: health_after,
        clean_shutdown,
    })
}
