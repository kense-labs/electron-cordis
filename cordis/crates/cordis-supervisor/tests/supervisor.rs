//! M5 Stage B Supervisor integration test:
//! 1. Headless mode starts ZERO UI/Chromium and cleanly supervises `electron-cordis-daemon`.
//! 2. Desktop mode survives a crashing UI child process (restarts UI automatically) WITHOUT
//!    restarting `electron-cordis-daemon` or dropping the disk-backed `PrivateVolumeWriterSeal`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use electron_cordis_supervisor::{SupervisorConfig, run_supervised};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const SUPERVISOR_BIN: &str = env!("CARGO_BIN_EXE_electron-cordis-supervisor");

fn daemon_bin_path() -> PathBuf {
    Path::new(SUPERVISOR_BIN)
        .with_file_name(format!("electron-cordis-daemon{}", std::env::consts::EXE_SUFFIX))
}

#[tokio::test]
async fn stage_b_supervisor_runs_zero_chromium_headless_and_survives_ui_crash_without_dropping_seal()
 {
    let daemon_bin = daemon_bin_path();
    assert!(
        daemon_bin.exists(),
        "daemon binary not found at {}; run cargo build -p electron-cordis-daemon",
        daemon_bin.display()
    );

    // 1. Zero-Chromium Headless supervision
    let headless_report = run_supervised(&SupervisorConfig {
        daemon_bin: daemon_bin.clone(),
        load_plan: None,
        private_volume_dir: None,
        ui_bin: None,
        ui_args: Vec::new(),
        max_ui_restarts: 0,
        extra_daemon_args: Vec::new(),
    })
    .await
    .expect("headless supervise");
    assert!(headless_report.headless);
    assert_eq!(headless_report.ui_launches, 0);
    assert!(headless_report.clean_shutdown);

    // 2. Desktop UI crash + automatic restart with uninterrupted daemon & private volume seal!
    let tmp_vol = std::env::temp_dir().join(format!("cordis-sup-vol-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_vol);

    // Node UI stub: crashes with exit code 42 on attempt 1, exits 0 cleanly on attempt 2!
    let report = run_supervised(&SupervisorConfig {
        daemon_bin,
        load_plan: None,
        private_volume_dir: Some(tmp_vol.clone()),
        ui_bin: Some(PathBuf::from("node")),
        ui_args: vec![
            "-e".into(),
            "const a = Number(process.env.ELECTRON_CORDIS_UI_ATTEMPT || '1'); process.exit(a === 1 ? 42 : 0);".into(),
        ],
        max_ui_restarts: 2,
        extra_daemon_args: Vec::new(),
    })
    .await
    .expect("desktop supervise with ui crash recovery");

    assert!(!report.headless);
    assert_eq!(
        report.ui_launches, 2,
        "UI must crash once and relaunch once"
    );
    assert_eq!(report.ui_crash_restarts, 1);
    assert!(
        !report.daemon_restarted,
        "Core/KRun daemon must NOT restart when UI crashes"
    );
    assert!(
        report.writer_seal_preserved_across_ui_restart,
        "Single-writer private volume seal must remain continuously held across UI crash/restart"
    );
    assert!(report.clean_shutdown);
    assert!(
        !tmp_vol.join(".writer-seal.lock").exists(),
        "Writer seal must be cleaned up after supervisor exits"
    );
    let _ = fs::remove_dir_all(&tmp_vol);
}

#[tokio::test]
async fn supervisor_surfaces_ui_failure_after_draining_the_daemon() {
    let result = run_supervised(&SupervisorConfig {
        daemon_bin: daemon_bin_path(),
        load_plan: None,
        private_volume_dir: None,
        ui_bin: Some(PathBuf::from("node")),
        ui_args: vec!["-e".into(), "process.exit(42);".into()],
        max_ui_restarts: 0,
        extra_daemon_args: Vec::new(),
    })
    .await;

    let error = result.expect_err("UI failure should reach the launcher");
    assert!(
        error.contains("Electron UI exited unsuccessfully"),
        "{error}"
    );
    assert!(error.ends_with("42"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn supervisor_drains_daemon_if_ui_cannot_spawn() {
    let volume = std::env::temp_dir().join(format!("cordis-spawn-{}", std::process::id()));
    let _ = fs::remove_dir_all(&volume);
    let socket = volume.join("cordis.sock");
    let result = run_supervised(&SupervisorConfig {
        daemon_bin: daemon_bin_path(),
        load_plan: None,
        private_volume_dir: Some(volume.clone()),
        ui_bin: Some(volume.join("missing-electron")),
        ui_args: Vec::new(),
        max_ui_restarts: 0,
        extra_daemon_args: vec!["--socket".into(), socket.to_string_lossy().into_owned()],
    })
    .await;

    let socket_exists = socket.exists();
    let seal_exists = volume.join(".writer-seal.lock").exists();
    fs::remove_dir_all(volume).expect("remove test volume");
    let error = result.expect_err("missing UI executable should reach the launcher");
    assert!(error.contains("spawn UI"), "{error}");
    assert!(!socket_exists, "daemon socket must be cleaned up");
    assert!(!seal_exists, "writer seal must be cleaned up");
}

fn managed_supervisor(ui_script: &str, session_plan: Option<&Path>) -> Child {
    let mut command = Command::new(SUPERVISOR_BIN);
    command
        .arg("--stop-on-stdin-eof")
        .arg("--daemon-bin")
        .arg(daemon_bin_path())
        .args(["--ui-bin", "node", "--ui-arg", "-e", "--ui-arg", ui_script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(plan) = session_plan {
        command.args(["--cleanup-dev-session", "--load-plan"]).arg(plan);
    }
    command.spawn().expect("start managed supervisor")
}

async fn read_ui_start(supervisor: &mut Child) -> String {
    let mut lines = BufReader::new(supervisor.stderr.as_mut().expect("supervisor stderr")).lines();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = lines.next_line().await.expect("read UI startup") {
            if let Some(value) = line.strip_prefix("ready ") {
                return value.to_owned();
            }
        }
        panic!("UI exited before announcing startup");
    })
    .await
    .expect("UI starts")
}

async fn assert_clean_shutdown(supervisor: Child) {
    let output = tokio::time::timeout(Duration::from_secs(20), supervisor.wait_with_output())
        .await
        .expect("supervisor exits")
        .expect("read supervisor output");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("supervisor report");
    assert_eq!(report["ui_launches"], 1);
    assert_eq!(report["clean_shutdown"], true);
}

#[tokio::test]
async fn managed_supervisor_drains_on_launcher_eof() {
    let out = Command::new("node")
        .args([
            "-e",
            "const { mkdtempSync } = require('node:fs');\
             const { join } = require('node:path');\
             const tmp = process.platform === 'win32' ? require('node:os').tmpdir() : '/tmp';\
             process.stdout.write(mkdtempSync(join(tmp, 'electron-cordis-dev.')));",
        ])
        .output()
        .await
        .expect("create dev session directory");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let directory = PathBuf::from(String::from_utf8(out.stdout).expect("dev session path"));
    let plan = directory.join("load-plan.json");
    fs::write(
        &plan,
        r#"{"schema":"cordis.load-plan.v1","profile":"test","entries":[
            {"id":"@test/deferred","runtime":"rust","isolation":"in-process","delivery":"on-demand"}
        ]}"#,
    )
    .expect("write dev session plan");
    let mut supervisor = managed_supervisor(
        "console.error('ready ui'); setTimeout(() => process.exit(90), 20000);",
        Some(&plan),
    );
    let launcher = supervisor.stdin.take().expect("control pipe");
    assert_eq!(read_ui_start(&mut supervisor).await, "ui");
    drop(launcher);
    assert_clean_shutdown(supervisor).await;
    assert!(!directory.exists(), "supervisor must remove the dev session after launcher EOF");
}

#[tokio::test]
async fn managed_supervisor_exits_with_launcher_pipe_still_open() {
    let mut supervisor = managed_supervisor("process.exit(0);", None);
    let launcher = supervisor.stdin.take().expect("control pipe");
    assert_clean_shutdown(supervisor).await;
    drop(launcher);
}

#[cfg(windows)]
#[tokio::test]
async fn managed_windows_job_reaps_children_when_supervisor_is_terminated() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

    let mut supervisor = managed_supervisor(
        "const child = require('node:child_process').spawn(process.execPath,\
         ['-e', 'setTimeout(() => process.exit(91), 20000)'], {stdio: 'ignore'});\
         console.error('ready ' + JSON.stringify([Number(process.env.ELECTRON_CORDIS_DAEMON_PID),\
         process.pid, child.pid])); setTimeout(() => process.exit(90), 20000);",
        None,
    );
    let launcher = supervisor.stdin.take().expect("control pipe");
    let children: [u32; 3] = serde_json::from_str(&read_ui_start(&mut supervisor).await)
        .expect("daemon, UI, and grandchild process IDs");
    let handles = children.map(|pid| {
        // SAFETY: OpenProcess receives only PIDs reported by our fixture and requests wait access.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null(), "open PID {pid}: {}", std::io::Error::last_os_error());
        // SAFETY: The successful call returned an owned handle.
        unsafe { OwnedHandle::from_raw_handle(handle) }
    });
    supervisor.kill().await.expect("force supervisor exit");
    for (pid, handle) in children.into_iter().zip(handles) {
        // SAFETY: The handle is held through the wait, even if the PID has exited.
        assert_eq!(unsafe { WaitForSingleObject(handle.as_raw_handle(), 5000) }, WAIT_OBJECT_0,
                   "managed PID {pid} survived supervisor exit");
    }
    drop(launcher);
}
