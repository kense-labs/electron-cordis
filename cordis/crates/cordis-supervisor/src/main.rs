//! CLI entrypoint for M5 Stage B top-level Rust Supervisor (`electron-cordis-supervisor`).
//!
//! Usage:
//!   electron-cordis-supervisor --daemon-bin <path> [--load-plan <path>] [--headless | --ui-bin <path> ...]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;

use electron_cordis_supervisor::{
    run_supervised, run_supervised_until, SupervisorConfig, SupervisorReport,
};
use tokio::sync::oneshot;

#[cfg(windows)]
mod windows_job;

fn parse_args() -> Result<(SupervisorConfig, bool, bool), String> {
    let mut args = env::args().skip(1);
    let mut daemon_bin: Option<PathBuf> = None;
    let mut load_plan: Option<PathBuf> = None;
    let mut extra_daemon_args = Vec::new();
    let mut private_volume_dir: Option<PathBuf> = None;
    let mut ui_bin: Option<PathBuf> = None;
    let mut ui_args: Vec<String> = Vec::new();
    let mut max_ui_restarts: usize = 3;
    let mut headless = false;
    let mut stop_on_stdin_eof = false;
    let mut cleanup_dev_session = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--daemon-bin" => daemon_bin = args.next().map(PathBuf::from),
            "--load-plan" => load_plan = args.next().map(PathBuf::from),
            "--daemon-arg" => {
                extra_daemon_args.push(args.next().ok_or("--daemon-arg requires a value")?)
            }
            "--private-volume-dir" => private_volume_dir = args.next().map(PathBuf::from),
            "--headless" => headless = true,
            "--stop-on-stdin-eof" => stop_on_stdin_eof = true,
            "--cleanup-dev-session" => cleanup_dev_session = true,
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
    if cleanup_dev_session && (!stop_on_stdin_eof || ui_bin.is_none()) {
        return Err("--cleanup-dev-session requires a managed desktop session".into());
    }

    let cfg = SupervisorConfig {
        daemon_bin,
        load_plan,
        private_volume_dir,
        ui_bin,
        ui_args,
        max_ui_restarts,
        extra_daemon_args,
    };
    Ok((cfg, stop_on_stdin_eof, cleanup_dev_session))
}

#[cfg(windows)]
fn dev_temp_root() -> PathBuf {
    // Match the Windows root used by Node's os.tmpdir().
    let nonempty = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
    nonempty("TEMP")
        .or_else(|| nonempty("TMP"))
        .map(PathBuf::from)
        .or_else(|| {
            nonempty("SystemRoot")
                .or_else(|| nonempty("windir"))
                .map(|root| PathBuf::from(root).join("temp"))
        })
        .unwrap_or_else(env::temp_dir)
}

#[cfg(not(windows))]
fn dev_temp_root() -> PathBuf {
    PathBuf::from("/tmp")
}

fn dev_session_directory(load_plan: Option<&Path>) -> io::Result<PathBuf> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid Cordis dev session path");
    let plan = load_plan
        .filter(|path| path.is_absolute() && path.file_name() == Some(OsStr::new("load-plan.json")))
        .ok_or_else(invalid)?;
    let directory = plan.parent().ok_or_else(invalid)?;
    let root = dev_temp_root().canonicalize()?;
    let name = directory.file_name().and_then(OsStr::to_str).ok_or_else(invalid)?;
    let suffix = name.strip_prefix("electron-cordis-dev.").ok_or_else(invalid)?;
    if suffix.len() != 6
        || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || directory.parent().ok_or_else(invalid)?.canonicalize()? != root
    {
        return Err(invalid());
    }
    let directory = root.join(name);
    if !fs::symlink_metadata(&directory)?.is_dir()
        || directory.canonicalize()? != directory
        || !fs::symlink_metadata(directory.join("load-plan.json"))?.is_file()
    {
        return Err(invalid());
    }
    Ok(directory)
}

async fn run_managed(
    cfg: &SupervisorConfig,
) -> Result<SupervisorReport, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    let mut console_interrupt = tokio::signal::windows::ctrl_c()?;
    #[cfg(windows)]
    windows_job::contain_process_tree()
        .map_err(|error| format!("initialize Cordis process job: {error}"))?;

    let (finished, stdin_closed) = oneshot::channel();
    // The owner can keep stdin open through a normal window exit. A detached reader
    // lets the supervisor exit without waiting on a blocking stdin operation.
    thread::Builder::new().name("cordis-launcher-stdin".into()).spawn(move || {
        let result = io::copy(&mut io::stdin().lock(), &mut io::sink()).map(|_| ());
        let _ = finished.send(result);
    })?;
    let stdin_closed = async { stdin_closed.await.map_err(io::Error::other)? };

    #[cfg(windows)]
    let interrupt = async {
        console_interrupt.recv().await.ok_or_else(|| io::Error::other("Ctrl+C listener closed"))
    };
    #[cfg(not(windows))]
    let interrupt = tokio::signal::ctrl_c();

    Ok(run_supervised_until(cfg, async {
        tokio::select! {
            result = stdin_closed => result,
            result = interrupt => result,
        }
    })
    .await?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (cfg, stop_on_stdin_eof, cleanup_dev_session) = parse_args()?;
    let session =
        cleanup_dev_session.then(|| dev_session_directory(cfg.load_plan.as_deref())).transpose()?;
    let result = if stop_on_stdin_eof {
        run_managed(&cfg).await
    } else {
        run_supervised(&cfg).await.map_err(Into::into)
    };
    if let Some(directory) = session {
        if let Err(error) = fs::remove_dir_all(&directory) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("clean Cordis dev session {}: {error}", directory.display());
            }
        }
    }
    let report = result?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.clean_shutdown {
        std::process::exit(1);
    }
    Ok(())
}
