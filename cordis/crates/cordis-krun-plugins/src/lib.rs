//! Bridges `../../krun/crates/*` into `cordis-rs` plugins without duplicating
//! KRun's cross-platform backend selection, FailClosed guarantees, or Plane A/B/C contracts.
//!
//! Includes:
//! - Conditional compilation wiring for ALL sibling KRun backends:
//!   - `backend-kiri-vz` (macOS Virtualization.framework)
//!   - `backend-kiri-win` (Windows WSL2 logical ports)
//!   - `backend-vm` (libkrun microVM)
//!   - `backend-os-sandbox` (macOS Seatbelt / Linux bwrap)
//!   - `backend-wasm` (Wasm/WASI sandbox, strictly FailClosed until runtime linked)
//!   - `backend-fake` (offline tests)
//! - `KrunRuntimePlugin` publishing `KrunRuntimeService` into `CoreRealm`.
//! - `PrivateVolumeWriterSeal`: filesystem-backed atomic exclusive lockfile (`create_new`)
//!   physically preventing dual-write across processes or overlapping eras.
//! - `KrunSessionPlugin` demonstrating generation-scoped `ctx.effect` lifecycle:
//!   - `FiberHandle::update()` hot-updates `/guest-shares` WITHOUT restarting the VM.
//!   - `FiberHandle::era_swap()` and `FiberHandle::dispose()` enforce a strict reverse-order
//!     revocation barrier before a successor era may acquire the single-writer private volume.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cordis_core::{Context, Plugin, Service};
use electron_cordis_kernel::service_names;
use krun_runtime::{
    Backend, BackendId, DoctorReport, ExecRequest, ExecResult, HostFacts, Manager, Spec, Strength,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KrunPluginError {
    #[error("krun backend unavailable (fail-closed): {0}")]
    FailClosed(String),
    #[error("krun cordis plugin error: {0}")]
    Runtime(String),
}

/// Isolation tier requested by a DSH plugin manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationTier {
    InProcess,
    TrustedHost,
    ElectronUtility,
    OsSandbox,
    Wasm,
    Microvm,
}

impl IsolationTier {
    /// Minimum KRun backend `Strength` required to satisfy this tier (never silently downgraded).
    pub fn min_strength(self) -> Strength {
        match self {
            Self::InProcess | Self::TrustedHost | Self::ElectronUtility => Strength::None,
            Self::Wasm => Strength::Wasm,
            Self::OsSandbox => Strength::Os,
            Self::Microvm => Strength::Vm,
        }
    }
}

/// Configuration for assembling platform KRun backends into `KrunRuntimeService`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KrunBackendConfig {
    pub enable_kiri_vz: bool,
    pub enable_kiri_win: bool,
    pub enable_vm: bool,
    pub enable_os_sandbox: bool,
    pub enable_wasm: bool,
    pub enable_fake: bool,
}

impl Default for KrunBackendConfig {
    fn default() -> Self {
        Self {
            enable_kiri_vz: cfg!(all(target_os = "macos", feature = "kiri-vz")),
            enable_kiri_win: cfg!(feature = "kiri-win"),
            enable_vm: cfg!(feature = "vm"),
            enable_os_sandbox: cfg!(feature = "os-sandbox"),
            enable_wasm: cfg!(feature = "wasm"),
            enable_fake: cfg!(feature = "fake"),
        }
    }
}

/// Builds the platform-compiled KRun backends directly from `../../krun/crates/*`.
pub fn assemble_platform_backends(cfg: &KrunBackendConfig) -> Vec<Arc<dyn Backend>> {
    let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
    let facts = HostFacts::probe();

    #[cfg(all(target_os = "macos", feature = "kiri-vz"))]
    if cfg.enable_kiri_vz {
        backends.push(Arc::new(krun_backend_kiri_vz::KiriVzBackend::new(
            facts.clone(),
        )));
    }

    #[cfg(feature = "kiri-win")]
    if cfg.enable_kiri_win {
        backends.push(Arc::new(krun_backend_kiri_win::KiriWinBackend::new(
            facts.clone(),
        )));
    }

    #[cfg(feature = "vm")]
    if cfg.enable_vm {
        backends.push(Arc::new(krun_backend_vm::KrunBackend::new(facts.clone())));
    }

    #[cfg(feature = "os-sandbox")]
    if cfg.enable_os_sandbox {
        backends.push(Arc::new(krun_backend_os_sandbox::OsSandboxBackend));
    }

    #[cfg(feature = "wasm")]
    if cfg.enable_wasm {
        backends.push(Arc::new(krun_backend_wasm::WasmBackend));
    }

    #[cfg(feature = "fake")]
    if cfg.enable_fake {
        backends.push(Arc::new(krun_backend_fake::FakeBackend));
    }

    backends
}

/// Filesystem-backed single-writer private volume seal (atomic `create_new` lockfile).
/// Guarantees at the OS filesystem level that two eras or two processes can NEVER dual-write
/// the same VM private volume simultaneously.
pub struct PrivateVolumeWriterSeal {
    lock_path: PathBuf,
    owner_epoch: u64,
    released: AtomicBool,
}

impl PrivateVolumeWriterSeal {
    pub fn acquire(volume_dir: &Path, owner_epoch: u64) -> Result<Self, KrunPluginError> {
        fs::create_dir_all(volume_dir)
            .map_err(|e| KrunPluginError::Runtime(format!("create volume dir: {e}")))?;
        let lock_path = volume_dir.join(".writer-seal.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|e| {
                KrunPluginError::FailClosed(format!(
                    "PRIVATE_VOLUME_DUAL_WRITE_DENIED: seal `{}` already held ({e})",
                    lock_path.display()
                ))
            })?;
        let _ = writeln!(file, "pid={} epoch={owner_epoch}", std::process::id());
        Ok(Self {
            lock_path,
            owner_epoch,
            released: AtomicBool::new(false),
        })
    }

    pub fn owner_epoch(&self) -> u64 {
        self.owner_epoch
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn release(&self) {
        if !self.released.swap(true, Ordering::SeqCst) {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

impl Drop for PrivateVolumeWriterSeal {
    fn drop(&mut self) {
        self.release();
    }
}

/// Shared service handle published into `CoreRealm` as `krun.runtime`.
#[derive(Clone)]
pub struct KrunRuntimeService {
    manager: Arc<Manager>,
}

impl Service for KrunRuntimeService {
    const NAME: &'static str = service_names::KRUN_RUNTIME;
}

impl KrunRuntimeService {
    pub fn from_backends(backends: Vec<Arc<dyn Backend>>) -> Self {
        Self {
            manager: Arc::new(Manager::new(backends)),
        }
    }

    pub fn from_platform_config(cfg: &KrunBackendConfig) -> Self {
        Self::from_backends(assemble_platform_backends(cfg))
    }

    pub fn doctor(&self) -> DoctorReport {
        self.manager.doctor()
    }

    pub fn registered_backend_ids(&self) -> Vec<BackendId> {
        self.manager.backends().keys().copied().collect()
    }

    /// Selects a backend satisfying `spec` and verifies it meets `required_tier`
    /// with strict FailClosed semantics (never silently downgrades).
    pub fn resolve_backend(
        &self,
        spec: &Spec,
        required_tier: IsolationTier,
    ) -> Result<BackendId, KrunPluginError> {
        let (backend, cap) = krun_runtime::runtime::select_backend(self.manager.backends(), spec)
            .map_err(|e| KrunPluginError::FailClosed(e.to_string()))?;
        if !cap.available {
            return Err(KrunPluginError::FailClosed(format!(
                "backend {} is unavailable: {}",
                backend.id(),
                cap.missing.join("; ")
            )));
        }
        if cap.strength < required_tier.min_strength() {
            return Err(KrunPluginError::FailClosed(format!(
                "backend {} strength {:?} is below required {:?} (no silent downgrade)",
                backend.id(),
                cap.strength,
                required_tier.min_strength()
            )));
        }
        Ok(backend.id())
    }

    /// Executes a one-shot command inside the resolved KRun backend (strictly FailClosed).
    pub fn exec_in_tier(
        &self,
        spec: Spec,
        required_tier: IsolationTier,
        req: ExecRequest,
    ) -> Result<ExecResult, KrunPluginError> {
        let chosen = self.resolve_backend(&spec, required_tier)?;
        let backend = self
            .manager
            .backends()
            .get(&chosen)
            .ok_or_else(|| KrunPluginError::FailClosed(format!("missing backend {chosen}")))?;
        let mut session = backend
            .open(spec)
            .map_err(|e| KrunPluginError::FailClosed(e.to_string()))?;
        let res = session
            .exec(req)
            .map_err(|e| KrunPluginError::Runtime(e.to_string()));
        let _ = session.shutdown();
        res
    }
}

/// Cordis v3 plugin that wires KRun backends into `CoreRealm` as `krun.runtime`.
#[derive(Clone, Default)]
pub struct KrunRuntimePlugin;

impl Plugin for KrunRuntimePlugin {
    type Config = KrunBackendConfig;
    type Input = KrunBackendConfig;
    type PrepareError = Infallible;
    type ApplyError = KrunPluginError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(KrunRuntimeService::from_platform_config(input));
        let publication = ctx
            .provide(svc)
            .map_err(|e| KrunPluginError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = publication.remove();
        })
        .map_err(|e| KrunPluginError::Runtime(e.to_string()))?;
        Ok(())
    }
}

/// Single-writer private volume & VM lifecycle coordinator shared across a Fiber's updates
/// and across `era_swap()` replacements so tests and runtime can enforce single-writer sealing.
#[derive(Default)]
pub struct KrunSessionCoordinator {
    pub vm_boot_count: AtomicUsize,
    pub vm_stop_count: AtomicUsize,
    writer_seal_held: AtomicBool,
    disk_seal: Mutex<Option<PrivateVolumeWriterSeal>>,
    active_shares: Mutex<BTreeSet<String>>,
    pub audit_log: Mutex<Vec<String>>,
}

impl KrunSessionCoordinator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_writer_seal_held(&self) -> bool {
        self.writer_seal_held.load(Ordering::SeqCst)
    }

    pub fn active_shares(&self) -> Vec<String> {
        self.active_shares
            .lock()
            .expect("shares lock")
            .iter()
            .cloned()
            .collect()
    }

    pub fn audit_snapshot(&self) -> Vec<String> {
        self.audit_log.lock().expect("audit lock").clone()
    }

    fn record(&self, event: impl Into<String>) {
        self.audit_log.lock().expect("audit lock").push(event.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrunSessionConfig {
    pub auth_epoch: u64,
    pub host_tools_token: String,
    pub guest_shares: Vec<String>,
    #[serde(default)]
    pub private_volume_dir: Option<PathBuf>,
}

/// Generation-aware KRun VM session plugin:
/// - On `FiberHandle::update()` (with `on_update` around hook), only updates hot-mounted
///   `/guest-shares` without restarting the VM or dropping the single-writer seal.
/// - On `FiberHandle::era_swap()` (new `FiberId`) or `FiberHandle::dispose()`, drains
///   effects in strict reverse order:
///   1. `revoke_token:<token>`
///   2. `unmount_shares`
///   3. `release_writer_seal:<epoch>` (also unlinking `.writer-seal.lock` if disk-backed)
///   4. `stop_vm:<epoch>`
#[derive(Clone)]
pub struct KrunSessionPlugin {
    coordinator: Arc<KrunSessionCoordinator>,
}

impl KrunSessionPlugin {
    pub fn new(coordinator: Arc<KrunSessionCoordinator>) -> Self {
        Self { coordinator }
    }
}

impl Plugin for KrunSessionPlugin {
    type Config = KrunSessionConfig;
    type Input = KrunSessionConfig;
    type PrepareError = Infallible;
    type ApplyError = KrunPluginError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let coord = Arc::clone(&self.coordinator);
        let epoch = input.auth_epoch;

        // Every new era MUST observe single-writer seal already released by the prior era's teardown barrier.
        if coord
            .writer_seal_held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(KrunPluginError::FailClosed(
                "private volume single-writer seal already held (refusing dual-write)".into(),
            ));
        }

        if let Some(dir) = &input.private_volume_dir {
            match PrivateVolumeWriterSeal::acquire(dir, epoch) {
                Ok(seal) => {
                    *coord.disk_seal.lock().expect("disk seal lock") = Some(seal);
                }
                Err(e) => {
                    coord.writer_seal_held.store(false, Ordering::SeqCst);
                    return Err(e);
                }
            }
        }

        // Boot VM once for this Era and arm outermost teardown effects first (so LIFO drains them last).
        coord.vm_boot_count.fetch_add(1, Ordering::SeqCst);
        coord.record(format!("boot_vm:{epoch}"));
        coord.record(format!("acquire_writer_seal:{epoch}"));

        let c_stop = Arc::clone(&coord);
        ctx.effect_sync(move || {
            if let Some(seal) = c_stop.disk_seal.lock().expect("disk seal lock").take() {
                seal.release();
            }
            c_stop.writer_seal_held.store(false, Ordering::SeqCst);
            c_stop.record(format!("release_writer_seal:{epoch}"));
            c_stop.vm_stop_count.fetch_add(1, Ordering::SeqCst);
            c_stop.record(format!("stop_vm:{epoch}"));
        })
        .map_err(|e| KrunPluginError::Runtime(e.to_string()))?;

        // Mount `/guest-shares`.
        {
            let mut shares = coord.active_shares.lock().expect("shares lock");
            shares.clear();
            for s in &input.guest_shares {
                shares.insert(s.clone());
            }
        }
        coord.record(format!(
            "mount_shares:{}",
            input.guest_shares.join(",")
        ));

        let c_shares = Arc::clone(&coord);
        ctx.effect_sync(move || {
            c_shares.active_shares.lock().expect("shares lock").clear();
            c_shares.record("unmount_shares");
        })
        .map_err(|e| KrunPluginError::Runtime(e.to_string()))?;

        // Issue generation-scoped host_tools capability token (registered last -> revoked FIRST on teardown).
        let token = input.host_tools_token.clone();
        coord.record(format!("issue_token:{token}"));
        let c_tok = Arc::clone(&coord);
        ctx.effect_sync(move || {
            c_tok.record(format!("revoke_token:{token}"));
        })
        .map_err(|e| KrunPluginError::Runtime(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::event::{ListenerOptions, around};
    use cordis_core::lifecycle::{FiberState, UpdateNext, UpdateOutcome};
    use cordis_core::{PreparedChange, PreparedPlugin};
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    #[tokio::test]
    async fn krun_runtime_plugin_wires_all_platform_backends_and_enforces_fail_closed() {
        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");

        let cfg = KrunBackendConfig::default();
        let handle = topo
            .core
            .spawn(PreparedPlugin::from_input(KrunRuntimePlugin, cfg))
            .await
            .expect("spawn krun runtime plugin");
        assert_eq!(handle.state(), FiberState::Active);

        let svc = topo
            .core
            .try_service::<KrunRuntimeService>()
            .expect("core sees krun.runtime");
        assert!(topo.untrusted_plugin.try_service::<KrunRuntimeService>().is_err());

        let ids = svc.registered_backend_ids();
        #[cfg(all(target_os = "macos", feature = "kiri-vz"))]
        assert!(ids.contains(&BackendId::KiriVz));
        #[cfg(feature = "kiri-win")]
        assert!(ids.contains(&BackendId::KiriWin));
        #[cfg(feature = "vm")]
        assert!(ids.contains(&BackendId::Krun));
        #[cfg(feature = "os-sandbox")]
        assert!(ids.contains(&BackendId::OsSandbox));
        #[cfg(feature = "wasm")]
        assert!(ids.contains(&BackendId::Wasm));
        #[cfg(feature = "fake")]
        assert!(ids.contains(&BackendId::Fake));

        // Wasm tier MUST fail closed when wasm runtime is not linked (never silently downgrade to Fake).
        #[cfg(all(feature = "wasm", feature = "fake"))]
        {
            let wasm_plus_fake = KrunRuntimeService::from_backends(vec![
                Arc::new(krun_backend_wasm::WasmBackend),
                Arc::new(krun_backend_fake::FakeBackend),
            ]);
            let spec = Spec {
                harness_name: "wasm-plugin".into(),
                work_dir: "/tmp".into(),
                prefer: vec![BackendId::Wasm, BackendId::Fake],
                ..Spec::default()
            };
            let err = wasm_plus_fake
                .resolve_backend(&spec, IsolationTier::Wasm)
                .expect_err("must fail closed when wasm backend unavailable");
            assert!(err.to_string().contains("no silent downgrade"));
        }

        // Real OS sandbox execution on macOS Seatbelt!
        #[cfg(all(target_os = "macos", feature = "os-sandbox"))]
        {
            let cwd = std::env::temp_dir();
            let spec = Spec {
                harness_name: "seatbelt-probe".into(),
                work_dir: cwd.to_string_lossy().into_owned(),
                prefer: vec![BackendId::OsSandbox],
                ..Spec::default()
            };
            if svc.resolve_backend(&spec, IsolationTier::OsSandbox).is_ok() {
                let res = svc
                    .exec_in_tier(
                        spec,
                        IsolationTier::OsSandbox,
                        ExecRequest {
                            argv: vec!["/bin/echo".into(), "krun-seatbelt-ok".into()],
                            env: std::collections::BTreeMap::new(),
                            work_dir: None,
                            timeout: Some(std::time::Duration::from_secs(5)),
                        },
                    )
                    .expect("seatbelt exec");
                assert_eq!(res.exit_code, 0);
                assert_eq!(res.backend, BackendId::OsSandbox);
                assert!(res.stdout.contains("krun-seatbelt-ok"));
            }
        }

        handle.dispose().await.expect("dispose krun runtime");
        assert!(topo.core.try_service::<KrunRuntimeService>().is_err());
    }

    #[tokio::test]
    async fn hot_update_keeps_vm_running_while_era_swap_and_disk_seal_enforce_reverse_barrier() {
        let tmp_vol = std::env::temp_dir().join(format!(
            "cordis-private-vol-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp_vol);

        let root = Context::new();
        let coord = KrunSessionCoordinator::new();
        let plugin = KrunSessionPlugin::new(Arc::clone(&coord));

        let hot_coord = Arc::clone(&coord);
        let _hot_hook = root
            .on_update::<KrunSessionPlugin, _>(
                around::<KrunSessionPlugin, _>(
                    move |_ctx, change: KrunSessionConfig, next: UpdateNext<KrunSessionPlugin>| {
                        let c = Arc::clone(&hot_coord);
                        async move {
                            let mut shares = c.active_shares.lock().expect("shares lock");
                            shares.clear();
                            for s in &change.guest_shares {
                                shares.insert(s.clone());
                            }
                            drop(shares);
                            c.record(format!(
                                "hot_update_shares:{}",
                                change.guest_shares.join(",")
                            ));
                            let _ = next;
                            Ok::<_, Infallible>(change)
                        }
                    },
                ),
                ListenerOptions::default(),
            )
            .expect("register hot update hook");

        let era1 = root
            .spawn(PreparedPlugin::from_input(
                plugin,
                KrunSessionConfig {
                    auth_epoch: 1,
                    host_tools_token: "tok-1".into(),
                    guest_shares: vec!["/workspace/a".into()],
                    private_volume_dir: Some(tmp_vol.clone()),
                },
            ))
            .await
            .expect("spawn era 1");
        let era1_id = era1.id();
        assert_eq!(coord.vm_boot_count.load(Ordering::SeqCst), 1);
        assert!(tmp_vol.join(".writer-seal.lock").exists());

        // Cross-process / second-holder dual-write attempt MUST fail closed while era 1 holds the seal!
        assert!(matches!(
            PrivateVolumeWriterSeal::acquire(&tmp_vol, 99),
            Err(KrunPluginError::FailClosed(_))
        ));

        // 1. Hot update via FiberHandle::update() — VM is NOT restarted, FiberId preserved!
        let outcome = era1
            .update(PreparedChange::from_input::<KrunSessionPlugin>(
                KrunSessionConfig {
                    auth_epoch: 1,
                    host_tools_token: "tok-1".into(),
                    guest_shares: vec!["/workspace/a".into(), "/workspace/b".into()],
                    private_volume_dir: Some(tmp_vol.clone()),
                },
            ))
            .await
            .expect("hot update");
        assert_eq!(outcome, UpdateOutcome::Vetoed);
        assert_eq!(era1.id(), era1_id);
        assert_eq!(coord.vm_boot_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            coord.active_shares(),
            vec!["/workspace/a".to_string(), "/workspace/b".to_string()]
        );

        // 2. Account switch via FiberHandle::era_swap() — MUST run full reverse barrier & release disk seal BEFORE era 2 acquires it!
        let era2 = era1
            .era_swap(PreparedChange::from_input::<KrunSessionPlugin>(
                KrunSessionConfig {
                    auth_epoch: 2,
                    host_tools_token: "tok-2".into(),
                    guest_shares: vec!["/workspace/account2".into()],
                    private_volume_dir: Some(tmp_vol.clone()),
                },
            ))
            .await
            .expect("era swap to account 2");
        assert_eq!(era1.state(), FiberState::Disposed);
        assert_ne!(era2.id(), era1_id);
        assert_eq!(coord.vm_boot_count.load(Ordering::SeqCst), 2);
        assert_eq!(coord.vm_stop_count.load(Ordering::SeqCst), 1);
        assert!(tmp_vol.join(".writer-seal.lock").exists());

        // 3. Final dispose() — reverse cleanup barrier releases disk lockfile & stops VM 2.
        era2.dispose().await.expect("dispose era 2");
        assert_eq!(coord.vm_stop_count.load(Ordering::SeqCst), 2);
        assert!(!coord.is_writer_seal_held());
        assert!(!tmp_vol.join(".writer-seal.lock").exists());

        let log = coord.audit_snapshot();
        assert_eq!(
            log,
            vec![
                "boot_vm:1",
                "acquire_writer_seal:1",
                "mount_shares:/workspace/a",
                "issue_token:tok-1",
                "hot_update_shares:/workspace/a,/workspace/b",
                "revoke_token:tok-1",
                "unmount_shares",
                "release_writer_seal:1",
                "stop_vm:1",
                "boot_vm:2",
                "acquire_writer_seal:2",
                "mount_shares:/workspace/account2",
                "issue_token:tok-2",
                "revoke_token:tok-2",
                "unmount_shares",
                "release_writer_seal:2",
                "stop_vm:2",
            ]
        );
        let _ = fs::remove_dir_all(&tmp_vol);
    }
}
