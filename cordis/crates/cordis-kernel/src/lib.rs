//! Electron-Cordis microkernel foundation built on internalized `cordis-core` v3.
//!
//! Enforces orthogonal isolation across four canonical `ServiceRealm` boundaries:
//! - `HostRealm`: sole owner of OS secrets (`SafeStorage`, `AskpassVault`, updater, `app.db`)
//! - `UiRealm`: stripped Electron-Lite web surface + TIPC bridge (zero KRun / secret access)
//! - `CoreRealm`: trusted VM core engine + single-writer private volume + Plane A/B gateways
//! - `UntrustedPluginRealm`: third-party DSH JS/Wasm plugins (zero fallback to privileged realms)

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use cordis_core::{Context, Event, FiberHandle, FiberState, ServiceRealm};
use cordis_loader::outcome::EntryOutcome;
use cordis_loader::plan::InjectEntry;
use cordis_loader::{LoadPlanBuilder, PluginEntry, PluginResolver};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LOAD_PLAN_SCHEMA: &str = "cordis.load-plan.v1";

/// Canonical semantic service names shared across Electron-Cordis and KRun planes.
pub mod service_names {
    // HostRealm (never mapped into CoreRealm or UntrustedPluginRealm)
    pub const SAFE_STORAGE: &str = "host.safe_storage";
    pub const ASKPASS_VAULT: &str = "host.askpass_vault";
    pub const APP_DB: &str = "host.app_db";
    pub const UPDATER: &str = "host.updater";
    pub const PLUGIN_INSTALLER: &str = "host.plugin_installer";

    // UiRealm
    pub const UI_WEB_SURFACE: &str = "ui.web_surface";
    pub const UI_READY: &str = "ui.ready";
    pub const AUTH_SESSION: &str = "auth.session";

    // CoreRealm / KRun Planes A, B, C
    pub const KRUN_RUNTIME: &str = "krun.runtime";
    pub const PRIVATE_VOLUME: &str = "krun.private_volume";
    pub const PLANE_A_HOST_TOOLS: &str = "plane_a.host_tools";
    pub const PLANE_B_TOOL_CATALOG: &str = "plane_b.tool_catalog";
    pub const LLM_GATEWAY: &str = "gateway.llm";
}

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("service realm wiring failed: {0}")]
    RealmWiring(String),
    #[error("load plan validation failed (fail-closed): {0}")]
    InvalidLoadPlan(String),
    #[error("teardown error on fiber `{id}`: {message}")]
    Teardown { id: String, message: String },
}

/// Delivery policy for a plugin entry (`bundled` in installer vs `on-demand` signed download).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PluginDelivery {
    #[default]
    Bundled,
    OnDemand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadPlanEntry {
    pub id: String,
    pub runtime: String,
    pub isolation: String,
    #[serde(default)]
    pub delivery: PluginDelivery,
    #[serde(default = "default_realm_name")]
    pub realm: String,
    #[serde(default)]
    pub inject: Vec<String>,
    #[serde(default)]
    pub config: serde_json::Value,
}

fn default_realm_name() -> String {
    "default".to_string()
}

/// Immutable declarative LoadPlan produced by `script/gen-cordis-build.py`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadPlanManifest {
    pub schema: String,
    pub profile: String,
    pub entries: Vec<LoadPlanEntry>,
}

const VALID_ISOLATIONS: &[&str] = &[
    "in-process",
    "trusted-host",
    "electron-utility",
    "os-sandbox",
    "wasm",
    "microvm",
];

const VALID_REALMS: &[&str] = &["default", "host", "ui", "core", "untrusted-plugin"];

const PRIVILEGED_SERVICES_DENIED_TO_UNTRUSTED: &[&str] = &[
    service_names::SAFE_STORAGE,
    service_names::ASKPASS_VAULT,
    service_names::APP_DB,
    service_names::PRIVATE_VOLUME,
    service_names::KRUN_RUNTIME,
];

impl LoadPlanManifest {
    pub fn from_json_str(raw: &str) -> Result<Self, KernelError> {
        let plan: Self = serde_json::from_str(raw)
            .map_err(|e| KernelError::InvalidLoadPlan(format!("json parse: {e}")))?;
        if plan.schema != LOAD_PLAN_SCHEMA {
            return Err(KernelError::InvalidLoadPlan(format!(
                "expected schema `{LOAD_PLAN_SCHEMA}`, got `{}`",
                plan.schema
            )));
        }
        if plan.profile.trim().is_empty() {
            return Err(KernelError::InvalidLoadPlan("profile name required".into()));
        }
        if plan.entries.is_empty() {
            return Err(KernelError::InvalidLoadPlan(
                "load plan must declare at least one entry".into(),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        for entry in &plan.entries {
            if entry.id.trim().is_empty() {
                return Err(KernelError::InvalidLoadPlan("entry id required".into()));
            }
            if !seen.insert(entry.id.as_str()) {
                return Err(KernelError::InvalidLoadPlan(format!(
                    "duplicate plugin id `{}`",
                    entry.id
                )));
            }
            if !VALID_ISOLATIONS.contains(&entry.isolation.as_str()) {
                return Err(KernelError::InvalidLoadPlan(format!(
                    "plugin `{}` has illegal isolation `{}`",
                    entry.id, entry.isolation
                )));
            }
            if !VALID_REALMS.contains(&entry.realm.as_str()) {
                return Err(KernelError::InvalidLoadPlan(format!(
                    "plugin `{}` has illegal realm `{}`",
                    entry.id, entry.realm
                )));
            }
            if entry.realm == "untrusted-plugin" {
                for dep in &entry.inject {
                    if PRIVILEGED_SERVICES_DENIED_TO_UNTRUSTED.contains(&dep.as_str())
                        && dep != service_names::KRUN_RUNTIME
                    {
                        return Err(KernelError::InvalidLoadPlan(format!(
                            "untrusted plugin `{}` cannot declare privileged inject `{dep}`",
                            entry.id
                        )));
                    }
                }
            }
        }
        Ok(plan)
    }

    pub fn has_plugin(&self, id: &str) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    pub fn bundled_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.delivery == PluginDelivery::Bundled)
            .count()
    }

    pub fn on_demand_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.delivery == PluginDelivery::OnDemand)
            .count()
    }
}

/// Four isolated `Context` views derived from one `cordis-core` root runtime.
///
/// Because Cordis v3 resolves `(Service, ServiceRealm)` with **zero fallback**,
/// untrusted plugins spawned in `untrusted_plugin` are structurally incapable
/// of resolving `SAFE_STORAGE`, `ASKPASS_VAULT`, or `PRIVATE_VOLUME`.
#[derive(Clone)]
pub struct RealmTopology {
    pub root: Context,
    pub host: Context,
    pub ui: Context,
    pub core: Context,
    pub untrusted_plugin: Context,
    pub realms: RealmHandles,
}

#[derive(Clone)]
pub struct RealmHandles {
    pub host_secrets: ServiceRealm,
    pub core_private_volume: ServiceRealm,
    pub shared_plane_b_catalog: ServiceRealm,
    pub untrusted_sandbox: ServiceRealm,
}

impl RealmTopology {
    pub fn new(root: Context) -> Result<Self, KernelError> {
        let host_secrets = root.new_service_realm();
        let core_private_volume = root.new_service_realm();
        let shared_plane_b_catalog = root.new_service_realm();
        let untrusted_sandbox = root.new_service_realm();

        let host = root
            .with_service_realms([
                (service_names::SAFE_STORAGE, host_secrets.clone()),
                (service_names::ASKPASS_VAULT, host_secrets.clone()),
                (
                    service_names::PLANE_B_TOOL_CATALOG,
                    shared_plane_b_catalog.clone(),
                ),
            ])
            .map_err(|e| KernelError::RealmWiring(e.to_string()))?;

        // UI realm gets isolated empty slots for host secrets and VM private volume.
        let ui = root
            .with_isolated_service(service_names::SAFE_STORAGE)
            .with_isolated_service(service_names::ASKPASS_VAULT)
            .with_isolated_service(service_names::PRIVATE_VOLUME)
            .with_isolated_service(service_names::KRUN_RUNTIME);

        // Trusted Core realm sees private volume and shared Plane B catalog, never raw host secrets.
        let core = root
            .with_isolated_service(service_names::SAFE_STORAGE)
            .with_isolated_service(service_names::ASKPASS_VAULT)
            .with_service_realms([
                (service_names::PRIVATE_VOLUME, core_private_volume.clone()),
                (
                    service_names::PLANE_B_TOOL_CATALOG,
                    shared_plane_b_catalog.clone(),
                ),
            ])
            .map_err(|e| KernelError::RealmWiring(e.to_string()))?;

        // Untrusted plugin realm has private empty slots for every privileged service.
        let untrusted_plugin = root
            .with_isolated_service(service_names::SAFE_STORAGE)
            .with_isolated_service(service_names::ASKPASS_VAULT)
            .with_isolated_service(service_names::APP_DB)
            .with_isolated_service(service_names::PRIVATE_VOLUME)
            .with_isolated_service(service_names::KRUN_RUNTIME)
            .with_service_realms([(
                service_names::PLANE_B_TOOL_CATALOG,
                shared_plane_b_catalog.clone(),
            )])
            .map_err(|e| KernelError::RealmWiring(e.to_string()))?;

        Ok(Self {
            root,
            host,
            ui,
            core,
            untrusted_plugin,
            realms: RealmHandles {
                host_secrets,
                core_private_volume,
                shared_plane_b_catalog,
                untrusted_sandbox,
            },
        })
    }
}

/// Two-phase interactive boot state preserving Inkstone's first-paint priority:
/// Core/KRun/Science fibers remain Pending until `ui_ready` and `auth_generation > 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPhase {
    PreUi,
    UiReady,
    CoreActive,
    ShuttingDown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoPhaseBootState {
    pub phase: BootPhase,
    pub ui_ready: bool,
    pub auth_generation: u64,
}

impl Default for TwoPhaseBootState {
    fn default() -> Self {
        Self {
            phase: BootPhase::PreUi,
            ui_ready: false,
            auth_generation: 0,
        }
    }
}

impl TwoPhaseBootState {
    pub fn mark_ui_ready(&mut self) {
        self.ui_ready = true;
        if self.phase == BootPhase::PreUi {
            self.phase = if self.auth_generation > 0 {
                BootPhase::CoreActive
            } else {
                BootPhase::UiReady
            };
        }
    }

    /// Advances account generation (`era_swap` boundary for profile-scoped fibers).
    pub fn swap_auth_generation(&mut self) -> u64 {
        self.auth_generation = self.auth_generation.saturating_add(1);
        if self.ui_ready && self.phase != BootPhase::ShuttingDown {
            self.phase = BootPhase::CoreActive;
        }
        self.auth_generation
    }
}

/// Plane B unified tool metadata shared across every KRun harness projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredTool {
    pub name: String,
    pub version: String,
    pub origin_plugin: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub requires_approval: bool,
}

/// Thread-safe Plane B tool catalog (single authority; never forked per harness).
#[derive(Clone, Default)]
pub struct ToolCatalog {
    inner: Arc<RwLock<BTreeMap<String, RegisteredTool>>>,
}

impl ToolCatalog {
    pub fn register(&self, tool: RegisteredTool) {
        let mut guard = self.inner.write().expect("tool catalog lock");
        guard.insert(tool.name.clone(), tool);
    }

    pub fn unregister_by_plugin(&self, origin_plugin: &str) -> usize {
        let mut guard = self.inner.write().expect("tool catalog lock");
        let before = guard.len();
        guard.retain(|_, t| t.origin_plugin != origin_plugin);
        before - guard.len()
    }

    pub fn list(&self) -> Vec<RegisteredTool> {
        let guard = self.inner.read().expect("tool catalog lock");
        guard.values().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<RegisteredTool> {
        let guard = self.inner.read().expect("tool catalog lock");
        guard.get(name).cloned()
    }
}

/// Unified event-sourced trajectory record emitted by both Tool Gateway and LLM Gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRecord {
    pub session_id: String,
    pub origin_plugin: String,
    pub harness_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Event contract for unified execution trajectory (compatible with DSH trajectory plugins).
pub struct TrajectoryEvent;

impl Event for TrajectoryEvent {
    const NAME: &'static str = "dsh.trajectory";
    type Args = TrajectoryRecord;
    type Output = ();
}

/// Tool call envelope passing through the Tool Gateway chokepoint (Around/Mapper/Responder).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEnvelope {
    pub session_id: String,
    pub harness_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub ok: bool,
    pub output: serde_json::Value,
    pub error: Option<String>,
}

/// Event contract for cross-harness tool invocation (supports DSH `Around` approval middleware).
pub struct ToolInvokeEvent;

impl Event for ToolInvokeEvent {
    const NAME: &'static str = "dsh.tool.invoke";
    type Args = ToolCallEnvelope;
    type Output = ToolCallResult;
}

/// LLM Gateway request envelope passing through the second chokepoint (`Mapper` redaction + `Around` guardrail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequestEnvelope {
    pub session_id: String,
    pub harness_id: String,
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponseEnvelope {
    pub ok: bool,
    pub model: String,
    pub content: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub redacted_fields: Vec<String>,
    pub error: Option<String>,
}

/// Event contract for the unified LLM / Model Proxy Gateway chokepoint.
pub struct LlmRequestEvent;

impl Event for LlmRequestEvent {
    const NAME: &'static str = "dsh.llm.request";
    type Args = LlmRequestEnvelope;
    type Output = LlmResponseEnvelope;
}

impl RealmTopology {
    /// Returns the isolated `Context` for a canonical realm name from `LoadPlanManifest`.
    pub fn context_for_realm(&self, realm: &str) -> Result<&Context, KernelError> {
        match realm {
            "host" => Ok(&self.host),
            "ui" => Ok(&self.ui),
            "core" => Ok(&self.core),
            "untrusted-plugin" => Ok(&self.untrusted_plugin),
            "default" => Ok(&self.root),
            other => Err(KernelError::RealmWiring(format!("unknown realm `{other}`"))),
        }
    }

    /// Compiles the entries targeting `realm_name` in `manifest` into a frozen `cordis-loader`
    /// `LoadPlan`, executes it through `resolver` with transactional FailClosed semantics, and
    /// rolls back every spawned fiber in reverse order if any entry fails.
    pub async fn load_realm_with_loader<R: PluginResolver>(
        &self,
        manifest: &LoadPlanManifest,
        realm_name: &str,
        allowed_ids: Option<&[&str]>,
        resolver: &R,
    ) -> Result<Vec<(String, FiberHandle)>, KernelError> {
        let ctx = self.context_for_realm(realm_name)?;
        let mut builder = LoadPlanBuilder::new();
        let mut has_entries = false;

        for entry in &manifest.entries {
            if entry.realm != realm_name || entry.delivery != PluginDelivery::Bundled {
                continue;
            }
            if let Some(filter) = allowed_ids {
                if !filter.contains(&entry.id.as_str()) {
                    continue;
                }
            }
            let mut pe = PluginEntry::new(entry.config.clone());
            pe.key = Some(entry.id.clone());
            pe.name = Some(entry.id.clone());
            pe.inject = entry
                .inject
                .iter()
                .cloned()
                .map(InjectEntry::Required)
                .collect();
            builder
                .add_plugin(None, pe)
                .map_err(|e| KernelError::InvalidLoadPlan(format!("loader builder: {e}")))?;
            has_entries = true;
        }

        if !has_entries {
            return Ok(Vec::new());
        }

        let plan = builder
            .finish()
            .map_err(|e| KernelError::InvalidLoadPlan(format!("loader finish: {e}")))?;
        let outcome = plan.load(ctx, resolver).await;

        let mut spawned: Vec<(String, FiberHandle)> = Vec::new();
        let mut failure_msg: Option<String> = None;

        for entry_out in outcome.entries() {
            match entry_out {
                EntryOutcome::Spawned {
                    resolve_key,
                    fiber_handle,
                    ..
                } => {
                    spawned.push((resolve_key.clone(), fiber_handle.clone()));
                }
                EntryOutcome::Failed {
                    resolve_key,
                    failure,
                    ..
                } => {
                    failure_msg = Some(format!("plugin `{resolve_key}` load failed: {failure}"));
                }
                _ => {}
            }
        }

        if let Some(msg) = failure_msg {
            // Transactional rollback in reverse spawn order.
            for (_, handle) in spawned.into_iter().rev() {
                let _ = handle.dispose().await;
            }
            return Err(KernelError::InvalidLoadPlan(msg));
        }

        Ok(spawned)
    }
}

/// Explicit reverse-spawn-order Fiber teardown roster (enforces KRun revocation barrier).
#[derive(Default)]
pub struct FiberRoster {
    entries: Vec<(String, FiberHandle)>,
}

impl FiberRoster {
    pub fn record(&mut self, id: impl Into<String>, handle: FiberHandle) {
        self.entries.push((id.into(), handle));
    }

    pub fn active_plugin_ids(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, handle)| handle.state() == FiberState::Active)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Disposes all delivered fibers in reverse spawn order, attempting every entry
    /// so cleanup barriers drain deterministically even if one fiber fails.
    pub async fn drain_reverse(mut self) -> Vec<KernelError> {
        let mut errors = Vec::new();
        while let Some((id, handle)) = self.entries.pop() {
            if let Err(err) = handle.dispose().await {
                errors.push(KernelError::Teardown {
                    id,
                    message: err.to_string(),
                });
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;

    use cordis_core::{Context, FiberState, InjectSpec, Plugin, PreparedPlugin, Service};

    use super::*;

    struct SafeStorageStub;
    impl Service for SafeStorageStub {
        const NAME: &'static str = service_names::SAFE_STORAGE;
    }

    struct PrivateVolumeStub;
    impl Service for PrivateVolumeStub {
        const NAME: &'static str = service_names::PRIVATE_VOLUME;
    }

    struct AskpassVaultStub;
    impl Service for AskpassVaultStub {
        const NAME: &'static str = service_names::ASKPASS_VAULT;
    }

    struct SharedCatalogStub;
    impl Service for SharedCatalogStub {
        const NAME: &'static str = service_names::PLANE_B_TOOL_CATALOG;
    }

    #[derive(Clone, Copy)]
    struct UntrustedSecretProber;

    impl Plugin for UntrustedSecretProber {
        type Config = ();
        type Input = ();
        type PrepareError = Infallible;
        type ApplyError = Infallible;

        fn inject(&self) -> InjectSpec {
            InjectSpec::none().require(service_names::SAFE_STORAGE)
        }

        fn prepare(&self, (): ()) -> Result<(), Infallible> {
            Ok(())
        }

        async fn apply(&self, _ctx: Context, (): &()) -> Result<(), Infallible> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn untrusted_plugin_realm_has_zero_fallback_for_privileged_services() {
        let root = Context::new();
        let topo = RealmTopology::new(root.clone()).expect("realm topology");

        // Publish privileged services in their authoritative realms and shared catalog in Plane B.
        let _safe_pub = topo.host.provide(Arc::new(SafeStorageStub)).expect("provide safe_storage");
        let _askpass_pub = topo.host.provide(Arc::new(AskpassVaultStub)).expect("provide askpass");
        let _vol_pub = topo
            .core
            .provide(Arc::new(PrivateVolumeStub))
            .expect("provide private_volume");
        let _cat_pub = topo
            .host
            .provide(Arc::new(SharedCatalogStub))
            .expect("provide plane_b catalog");

        // Authoritative realms see their own services.
        assert!(topo.host.try_service::<SafeStorageStub>().is_ok());
        assert!(topo.host.try_service::<AskpassVaultStub>().is_ok());
        assert!(topo.host.try_service::<SharedCatalogStub>().is_ok());
        assert!(topo.core.try_service::<PrivateVolumeStub>().is_ok());
        assert!(topo.core.try_service::<SharedCatalogStub>().is_ok());

        // Cross-realm isolation: Core cannot read Host secrets; UI cannot read secrets or private volume.
        assert!(topo.core.try_service::<SafeStorageStub>().is_err());
        assert!(topo.core.try_service::<AskpassVaultStub>().is_err());
        assert!(topo.ui.try_service::<SafeStorageStub>().is_err());
        assert!(topo.ui.try_service::<PrivateVolumeStub>().is_err());

        // UntrustedPluginRealm: zero fallback for SAFE_STORAGE / ASKPASS_VAULT / PRIVATE_VOLUME.
        assert!(topo.untrusted_plugin.try_service::<SafeStorageStub>().is_err());
        assert!(topo.untrusted_plugin.try_service::<AskpassVaultStub>().is_err());
        assert!(topo.untrusted_plugin.try_service::<PrivateVolumeStub>().is_err());
        // Shared Plane B tool catalog IS explicitly bridged into UntrustedPluginRealm.
        assert!(topo.untrusted_plugin.try_service::<SharedCatalogStub>().is_ok());

        // Even a Fiber spawned in UntrustedPluginRealm requiring SAFE_STORAGE stays Pending forever.
        let handle = topo
            .untrusted_plugin
            .spawn(PreparedPlugin::from_input(UntrustedSecretProber, ()))
            .await
            .expect("spawn prober");
        assert_eq!(handle.state(), FiberState::Pending);
        handle.dispose().await.expect("dispose prober");
    }

    #[test]
    fn two_phase_boot_state_converges_in_both_orders_and_locks_shutdown() {
        // Order 1: UI first, then Auth swap.
        let mut boot = TwoPhaseBootState::default();
        assert_eq!(boot.phase, BootPhase::PreUi);
        assert!(!boot.ui_ready);
        assert_eq!(boot.auth_generation, 0);

        boot.mark_ui_ready();
        assert_eq!(boot.phase, BootPhase::UiReady);
        assert!(boot.ui_ready);

        assert_eq!(boot.swap_auth_generation(), 1);
        assert_eq!(boot.phase, BootPhase::CoreActive);
        assert_eq!(boot.swap_auth_generation(), 2);
        assert_eq!(boot.phase, BootPhase::CoreActive);

        // ShuttingDown never regresses to CoreActive.
        boot.phase = BootPhase::ShuttingDown;
        assert_eq!(boot.swap_auth_generation(), 3);
        assert_eq!(boot.phase, BootPhase::ShuttingDown);

        // Order 2: Auth ready before first paint still waits for UiReady.
        let mut boot2 = TwoPhaseBootState::default();
        assert_eq!(boot2.swap_auth_generation(), 1);
        assert_eq!(boot2.phase, BootPhase::PreUi);
        boot2.mark_ui_ready();
        assert_eq!(boot2.phase, BootPhase::CoreActive);
    }

    #[test]
    fn load_plan_manifest_fail_closed_validation() {
        // Bad JSON
        assert!(LoadPlanManifest::from_json_str("{not json").is_err());

        // Wrong schema
        let bad_schema = r#"{"schema":"cordis.load-plan.v999","profile":"p","entries":[]}"#;
        assert!(matches!(
            LoadPlanManifest::from_json_str(bad_schema),
            Err(KernelError::InvalidLoadPlan(_))
        ));

        // Empty profile
        let empty_profile = r#"{"schema":"cordis.load-plan.v1","profile":"   ","entries":[{"id":"a","runtime":"rust","isolation":"in-process"}]}"#;
        assert!(LoadPlanManifest::from_json_str(empty_profile).is_err());

        // Empty entries
        let empty_entries = r#"{"schema":"cordis.load-plan.v1","profile":"inkstone","entries":[]}"#;
        assert!(LoadPlanManifest::from_json_str(empty_entries).is_err());

        // Duplicate ID
        let dup_id = r#"{
            "schema":"cordis.load-plan.v1",
            "profile":"inkstone",
            "entries":[
                {"id":"@dsh/cap-clipboard","runtime":"rust","isolation":"trusted-host","realm":"host"},
                {"id":"@dsh/cap-clipboard","runtime":"rust","isolation":"trusted-host","realm":"host"}
            ]
        }"#;
        assert!(LoadPlanManifest::from_json_str(dup_id).is_err());

        // Illegal isolation
        let bad_iso = r#"{
            "schema":"cordis.load-plan.v1",
            "profile":"inkstone",
            "entries":[{"id":"@dsh/x","runtime":"rust","isolation":"root-shell","realm":"host"}]
        }"#;
        assert!(LoadPlanManifest::from_json_str(bad_iso).is_err());

        // Untrusted plugin requesting privileged host secret fails closed
        let untrusted_priv = r#"{
            "schema":"cordis.load-plan.v1",
            "profile":"inkstone",
            "entries":[{
                "id":"@ext/evil",
                "runtime":"node-dsh",
                "isolation":"os-sandbox",
                "realm":"untrusted-plugin",
                "inject":["host.safe_storage"]
            }]
        }"#;
        assert!(LoadPlanManifest::from_json_str(untrusted_priv).is_err());

        // Valid plan with bundled + on-demand split passes
        let valid = r#"{
            "schema":"cordis.load-plan.v1",
            "profile":"inkstone-desktop",
            "entries":[
                {"id":"@dsh/cap-clipboard","runtime":"rust","isolation":"trusted-host","delivery":"bundled","realm":"host"},
                {"id":"@dsh/plugin-science-pack","runtime":"acp-proc","isolation":"os-sandbox","delivery":"on-demand","realm":"untrusted-plugin","inject":["krun.runtime"]}
            ]
        }"#;
        let plan = LoadPlanManifest::from_json_str(valid).expect("valid load plan");
        assert_eq!(plan.bundled_count(), 1);
        assert_eq!(plan.on_demand_count(), 1);
        assert!(plan.has_plugin("@dsh/cap-clipboard"));
    }

    #[tokio::test]
    async fn cordis_loader_transactional_realm_load_rolls_back_on_failure() {
        use std::sync::Mutex;

        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");
        let log = Arc::new(Mutex::new(Vec::<String>::new()));

        #[derive(Clone)]
        struct GoodPlugin {
            label: String,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl Plugin for GoodPlugin {
            type Config = ();
            type Input = ();
            type PrepareError = Infallible;
            type ApplyError = Infallible;
            fn prepare(&self, (): ()) -> Result<(), Infallible> {
                Ok(())
            }
            async fn apply(&self, ctx: Context, (): &()) -> Result<(), Infallible> {
                self.log.lock().unwrap().push(format!("up:{}", self.label));
                let l = Arc::clone(&self.log);
                let label = self.label.clone();
                ctx.effect_sync(move || {
                    l.lock().unwrap().push(format!("down:{label}"));
                })
                .unwrap();
                Ok(())
            }
        }

        let manifest = LoadPlanManifest::from_json_str(
            r#"{
                "schema":"cordis.load-plan.v1",
                "profile":"loader-test",
                "entries":[
                    {"id":"@dsh/a","runtime":"rust","isolation":"trusted-host","realm":"host"},
                    {"id":"@dsh/b","runtime":"rust","isolation":"trusted-host","realm":"host"},
                    {"id":"@dsh/missing","runtime":"rust","isolation":"trusted-host","realm":"host"}
                ]
            }"#,
        )
        .unwrap();

        let log_res = Arc::clone(&log);
        let resolver = move |req: cordis_loader::resolver::PluginRequest<'_>| -> Result<Option<PreparedPlugin>, std::io::Error> {
            match req.resolve_key() {
                "@dsh/a" => Ok(Some(PreparedPlugin::from_input(
                    GoodPlugin { label: "a".into(), log: Arc::clone(&log_res) },
                    (),
                ))),
                "@dsh/b" => Ok(Some(PreparedPlugin::from_input(
                    GoodPlugin { label: "b".into(), log: Arc::clone(&log_res) },
                    (),
                ))),
                _ => Ok(None),
            }
        };

        // Failure on `@dsh/missing` rolls back `@dsh/b` then `@dsh/a` in reverse order!
        let err = topo
            .load_realm_with_loader(&manifest, "host", None, &resolver)
            .await
            .expect_err("missing plugin must fail closed and rollback");
        assert!(err.to_string().contains("@dsh/missing"));
        assert_eq!(
            *log.lock().unwrap(),
            vec!["up:a", "up:b", "down:b", "down:a"]
        );

        // Filtered load of only known entries succeeds cleanly.
        log.lock().unwrap().clear();
        let handles = topo
            .load_realm_with_loader(&manifest, "host", Some(&["@dsh/a", "@dsh/b"]), &resolver)
            .await
            .expect("filtered load succeeds");
        assert_eq!(handles.len(), 2);
        let mut roster = FiberRoster::default();
        for (id, h) in handles {
            roster.record(id, h);
        }
        assert!(roster.drain_reverse().await.is_empty());
        assert_eq!(
            *log.lock().unwrap(),
            vec!["up:a", "up:b", "down:b", "down:a"]
        );
    }
}
