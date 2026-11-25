//! M5 Stage B Supervisor integration test:
//! 1. Headless mode starts ZERO UI/Chromium and cleanly supervises `electron-cordis-daemon`.
//! 2. Desktop mode survives a crashing UI child process (restarts UI automatically) WITHOUT
//!    restarting `electron-cordis-daemon` or dropping the disk-backed `PrivateVolumeWriterSeal`.

use std::fs;
use std::path::PathBuf;

use electron_cordis_supervisor::{SupervisorConfig, run_supervised};

fn daemon_bin_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let target_debug = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/debug dir");
    let name = if cfg!(windows) {
        "electron-cordis-daemon.exe"
    } else {
        "electron-cordis-daemon"
    };
    target_debug.join(name)
}

#[tokio::test]
async fn stage_b_supervisor_runs_zero_chromium_headless_and_survives_ui_crash_without_dropping_seal() {
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
    assert_eq!(report.ui_launches, 2, "UI must crash once and relaunch once");
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
