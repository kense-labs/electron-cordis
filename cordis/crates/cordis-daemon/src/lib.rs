//! Stage A local microkernel daemon library (`electron-cordis-daemon`).
//!
//! Wires:
//! - `RealmTopology` (`HostRealm` / `UiRealm` / `CoreRealm` / `UntrustedPluginRealm`)
//! - All M3 Class-A Rust capability plugins (Batch 1, Batch 2, Batch 3) in `HostRealm` / `CoreRealm`
//! - Dual Chokepoints built on `cordis-core` v3 event waterfalls:
//!   1. Tool Gateway (`ToolInvokeEvent` + `Around` approval guardrail + trajectory)
//!   2. LLM Gateway (`LlmRequestEvent` + `Mapper` secret redaction + `Around` model guardrail + trajectory)
//! - Four optional harness protocol listeners:
//!   - `bridge-mcp` (`--mcp-socket`, `/run/krun/dsh-mcp.sock`)
//!   - `bridge-inkstone-tools` (`--native-tools-socket`, `/run/krun/host-tools.sock`)
//!   - `bridge-acp` (`--acp-socket`, `/run/krun/dsh-rpc.sock`)
//!   - `llm-gateway` (`--llm-gateway-socket`, `/run/krun/llm-gateway.sock`)
//! - `ShadowFiber` live bidirectional host bridge (`--shadow-socket` <-> `@dsh/cordis-bridge`)

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis_core::event::{Next, around, mapper_sync};
use cordis_core::{Context, PreparedPlugin, Routing};
use electron_cordis_cap_clipboard::{
    ClipboardConfig, ClipboardPlugin, ClipboardService, PLUGIN_ID as CLIPBOARD_PLUGIN_ID,
};
use electron_cordis_cap_desktop_ui::{
    NOTIFICATION_PLUGIN_ID, NotificationConfig, NotificationPlugin, NotificationService,
    OS_INTEGRATION_PLUGIN_ID, OsIntegrationConfig, OsIntegrationPlugin, OsIntegrationService,
    TRAY_PLUGIN_ID, TrayMenuConfig, TrayMenuPlugin, TrayMenuService,
};
use electron_cordis_cap_dialog::{
    DialogConfig, DialogPlugin, DialogService, PLUGIN_ID as DIALOG_PLUGIN_ID,
};
use electron_cordis_cap_os_shell::{
    OsShellConfig, OsShellPlugin, OsShellService, PLUGIN_ID as OS_SHELL_PLUGIN_ID,
};
use electron_cordis_cap_platform::{
    CAPTURE_PLUGIN_ID, DesktopCaptureConfig, DesktopCapturePlugin, DesktopCaptureService,
    INSTALLER_PLUGIN_ID, NODE_HOST_PLUGIN_ID, NodeHostConfig, PluginInstallerConfig,
    PluginInstallerPlugin, PluginInstallerService, RuntimeNodeHostPlugin, UPDATER_PLUGIN_ID,
    UpdaterConfig, UpdaterPlugin, UpdaterService,
};
use electron_cordis_cap_safe_storage::{
    PLUGIN_ID as SAFE_STORAGE_PLUGIN_ID, SafeStorageConfig, SafeStoragePlugin, SafeStorageService,
};
use electron_cordis_kernel::{
    BootPhase, FiberRoster, LlmRequestEnvelope, LlmRequestEvent, LlmResponseEnvelope,
    LoadPlanManifest, RealmTopology, RegisteredTool, ToolCallEnvelope, ToolCallResult,
    ToolCatalog, ToolInvokeEvent, TrajectoryRecord, TwoPhaseBootState,
};
use electron_cordis_wire::{
    AcpPromptRequest, AcpSessionCreate, HarnessProjection, INKSTONE_NATIVE_CONTRACT_PROTOCOL,
    INKSTONE_NATIVE_CONTRACT_SHA256, INKSTONE_NATIVE_MAX_FRAME_BYTES, McpCallToolResult,
    McpContentBlock, McpToolDescriptor, NativeContractToolCall, ShadowFiberMessage, WIRE_SCHEMA,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Registered approval policy evaluated by the unified `ToolInvokeEvent` `Around` middleware.
#[derive(Debug, Clone, Default)]
pub struct ApprovalPolicyState {
    pub denied_tools: Vec<String>,
    pub enforce_requires_approval: bool,
    pub approved_tools: Vec<String>,
}

/// Policy evaluated by the `LlmRequestEvent` second chokepoint (`Around` guardrail).
#[derive(Debug, Clone, Default)]
pub struct LlmPolicyState {
    /// If non-empty, only listed models are admitted (FailClosed otherwise).
    pub allowed_models: Vec<String>,
    /// Explicitly blocked models.
    pub blocked_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowFiberRecord {
    pub fiber_id: String,
    pub generation: u64,
    pub plugin_id: String,
    pub registered_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcpSessionRecord {
    session_id: String,
    harness_id: String,
    cwd: Option<String>,
    cancelled: bool,
}

pub type BoxedToolFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
pub type RemoteToolExecutor =
    Arc<dyn Fn(String, String, Value) -> BoxedToolFuture + Send + Sync>;

struct LiveShadowPeer {
    outbound: mpsc::UnboundedSender<ShadowFiberMessage>,
    pending: Mutex<BTreeMap<String, oneshot::Sender<Result<Value, String>>>>,
    seq: AtomicU64,
}

impl LiveShadowPeer {
    fn new(outbound: mpsc::UnboundedSender<ShadowFiberMessage>) -> Arc<Self> {
        Arc::new(Self {
            outbound,
            pending: Mutex::new(BTreeMap::new()),
            seq: AtomicU64::new(1),
        })
    }

    fn send(&self, msg: ShadowFiberMessage) -> Result<(), String> {
        self.outbound
            .send(msg)
            .map_err(|_| "shadow peer disconnected".to_string())
    }

    async fn invoke_remote_tool(
        &self,
        fiber_id: String,
        tool_name: String,
        arguments: Value,
    ) -> Result<Value, String> {
        let req_id = format!("host-call-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("shadow pending lock")
            .insert(req_id.clone(), tx);

        if let Err(e) = self.send(ShadowFiberMessage::ToolInvoke {
            request_id: req_id.clone(),
            fiber_id,
            tool_name,
            arguments,
        }) {
            self.pending.lock().expect("shadow pending lock").remove(&req_id);
            return Err(e);
        }

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("shadow peer closed before replying".into()),
            Err(_) => {
                self.pending.lock().expect("shadow pending lock").remove(&req_id);
                Err("shadow tool invocation timed out".into())
            }
        }
    }

    fn deliver_reply(
        &self,
        request_id: &str,
        ok: bool,
        result: Option<Value>,
        error: Option<String>,
    ) {
        if let Some(tx) = self
            .pending
            .lock()
            .expect("shadow pending lock")
            .remove(request_id)
        {
            let outcome = if ok {
                Ok(result.unwrap_or(Value::Null))
            } else {
                Err(error.unwrap_or_else(|| "remote shadow tool error".into()))
            };
            let _ = tx.send(outcome);
        }
    }
}

pub struct DaemonState {
    plan: Option<LoadPlanManifest>,
    boot: Mutex<TwoPhaseBootState>,
    tools: ToolCatalog,
    topology: RealmTopology,
    roster: tokio::sync::Mutex<FiberRoster>,
    approval: Arc<Mutex<ApprovalPolicyState>>,
    llm_policy: Arc<Mutex<LlmPolicyState>>,
    trajectory: Arc<Mutex<Vec<TrajectoryRecord>>>,
    shadow_fibers: Mutex<BTreeMap<String, ShadowFiberRecord>>,
    remote_executors: Mutex<BTreeMap<String, RemoteToolExecutor>>,
    shadow_peers: Mutex<BTreeMap<String, Arc<LiveShadowPeer>>>,
    acp_sessions: Mutex<BTreeMap<String, AcpSessionRecord>>,
    acp_seq: AtomicU64,
    _tool_around_guard: cordis_core::event::ListenerRegistration,
    _llm_mapper_guard: cordis_core::event::ListenerRegistration,
    _llm_around_guard: cordis_core::event::ListenerRegistration,
}

fn redact_secret_text(input: &str) -> (String, bool) {
    let mut out = input.to_string();
    let mut changed = false;
    for prefix in ["sk-", "Bearer ", "SAFE_STORAGE_SECRET:"] {
        while let Some(idx) = out.find(prefix) {
            let value_start = idx + prefix.len();
            let end = out[value_start..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .map(|rel| value_start + rel)
                .unwrap_or(out.len());
            out.replace_range(idx..end, "[REDACTED]");
            changed = true;
        }
    }
    (out, changed)
}

impl DaemonState {
    /// Initializes the Cordis v3 microkernel, spawns enabled plugins per `LoadPlanManifest`
    /// into their designated `ServiceRealm`s, and installs BOTH chokepoint pipelines
    /// (Tool Gateway `Around` + LLM Gateway `Mapper`/`Around`).
    pub async fn start(plan: Option<LoadPlanManifest>) -> Result<Self, String> {
        if let Some(plan) = &plan {
            if plan.entries.iter().any(|entry| {
                entry.id == "@dsh/krun-runtime"
                    || entry.inject.iter().any(|service| service == "krun.runtime")
            }) {
                return Err("load plan requires an embedded KRun runtime, which is unavailable; regenerate it with a standalone profile".into());
            }
            if plan.entries.iter().any(|entry| {
                entry.id == NODE_HOST_PLUGIN_ID
                    && !matches!(entry.isolation.as_str(), "in-process" | "trusted-host")
            }) {
                return Err("runtime-node-host requires a trusted local process; configure external isolation separately".into());
            }
        }
        let root = Context::new();
        let topology = RealmTopology::new(root).map_err(|e| e.to_string())?;
        let tools = ToolCatalog::default();
        let mut roster = FiberRoster::default();

        let should_load = |id: &str| match &plan {
            Some(p) => p.has_plugin(id),
            None => true,
        };

        if should_load(NODE_HOST_PLUGIN_ID) {
            let handle = topology
                .core
                .spawn(PreparedPlugin::from_input(
                    RuntimeNodeHostPlugin,
                    NodeHostConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {NODE_HOST_PLUGIN_ID}: {e}"))?;
            roster.record(NODE_HOST_PLUGIN_ID, handle);
        }

        // L2 HostRealm: M3 Batch 1 Class-A Rust capability plugins
        if should_load(SAFE_STORAGE_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    SafeStoragePlugin,
                    SafeStorageConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {SAFE_STORAGE_PLUGIN_ID}: {e}"))?;
            roster.record(SAFE_STORAGE_PLUGIN_ID, handle);
        }

        if should_load(CLIPBOARD_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    ClipboardPlugin::new(Some(tools.clone())),
                    ClipboardConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {CLIPBOARD_PLUGIN_ID}: {e}"))?;
            roster.record(CLIPBOARD_PLUGIN_ID, handle);
        }

        if should_load(DIALOG_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    DialogPlugin,
                    DialogConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {DIALOG_PLUGIN_ID}: {e}"))?;
            roster.record(DIALOG_PLUGIN_ID, handle);
        }

        if should_load(OS_SHELL_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    OsShellPlugin,
                    OsShellConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {OS_SHELL_PLUGIN_ID}: {e}"))?;
            roster.record(OS_SHELL_PLUGIN_ID, handle);
        }

        // L2 HostRealm: M3 Batch 2 Desktop UI plugins
        if should_load(TRAY_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    TrayMenuPlugin,
                    TrayMenuConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {TRAY_PLUGIN_ID}: {e}"))?;
            roster.record(TRAY_PLUGIN_ID, handle);
        }

        if should_load(NOTIFICATION_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    NotificationPlugin,
                    NotificationConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {NOTIFICATION_PLUGIN_ID}: {e}"))?;
            roster.record(NOTIFICATION_PLUGIN_ID, handle);
        }

        if should_load(OS_INTEGRATION_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    OsIntegrationPlugin,
                    OsIntegrationConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {OS_INTEGRATION_PLUGIN_ID}: {e}"))?;
            roster.record(OS_INTEGRATION_PLUGIN_ID, handle);
        }

        // L2 HostRealm: M3 Batch 3 Platform & Signed Installer plugins
        if should_load(CAPTURE_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    DesktopCapturePlugin,
                    DesktopCaptureConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {CAPTURE_PLUGIN_ID}: {e}"))?;
            roster.record(CAPTURE_PLUGIN_ID, handle);
        }

        if should_load(UPDATER_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    UpdaterPlugin,
                    UpdaterConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {UPDATER_PLUGIN_ID}: {e}"))?;
            roster.record(UPDATER_PLUGIN_ID, handle);
        }

        if should_load(INSTALLER_PLUGIN_ID) {
            let handle = topology
                .host
                .spawn(PreparedPlugin::from_input(
                    PluginInstallerPlugin,
                    PluginInstallerConfig::default(),
                ))
                .await
                .map_err(|e| format!("spawn {INSTALLER_PLUGIN_ID}: {e}"))?;
            roster.record(INSTALLER_PLUGIN_ID, handle);
        }

        // Chokepoint 1: Tool Gateway `Around` approval & trajectory middleware on `ToolInvokeEvent`.
        let approval = Arc::new(Mutex::new(ApprovalPolicyState::default()));
        let trajectory = Arc::new(Mutex::new(Vec::new()));
        let catalog_for_around = tools.clone();
        let approval_for_around = Arc::clone(&approval);
        let traj_for_around = Arc::clone(&trajectory);

        let tool_around_guard = topology
            .root
            .on::<ToolInvokeEvent, _>(around(
                move |_ctx, envelope: ToolCallEnvelope, next: Next<ToolInvokeEvent>| {
                    let catalog = catalog_for_around.clone();
                    let policy_arc = Arc::clone(&approval_for_around);
                    let traj_arc = Arc::clone(&traj_for_around);
                    async move {
                        let tool_meta = catalog.get(&envelope.tool_name);
                        let origin_plugin = tool_meta
                            .as_ref()
                            .map(|t| t.origin_plugin.clone())
                            .unwrap_or_else(|| "unknown".into());

                        let veto_reason = {
                            let policy = policy_arc.lock().expect("approval lock");
                            if policy.denied_tools.iter().any(|t| t == &envelope.tool_name) {
                                Some(format!(
                                    "APPROVAL_DENIED: tool `{}` blocked by Around guardrail",
                                    envelope.tool_name
                                ))
                            } else if policy.enforce_requires_approval
                                && tool_meta.as_ref().is_some_and(|t| t.requires_approval)
                                && !policy
                                    .approved_tools
                                    .iter()
                                    .any(|t| t == &envelope.tool_name)
                            {
                                Some(format!(
                                    "APPROVAL_REQUIRED: tool `{}` requires explicit approval",
                                    envelope.tool_name
                                ))
                            } else {
                                None
                            }
                        };

                        if let Some(reason) = veto_reason {
                            traj_arc.lock().expect("trajectory lock").push(TrajectoryRecord {
                                session_id: envelope.session_id.clone(),
                                origin_plugin,
                                harness_id: envelope.harness_id.clone(),
                                kind: "tool.denied".into(),
                                payload: json!({
                                    "tool": envelope.tool_name,
                                    "reason": reason,
                                }),
                            });
                            return Ok::<ToolCallResult, Infallible>(ToolCallResult {
                                ok: false,
                                output: Value::Null,
                                error: Some(reason),
                            });
                        }

                        let session_id = envelope.session_id.clone();
                        let harness_id = envelope.harness_id.clone();
                        let tool_name = envelope.tool_name.clone();
                        let result = match next.call(envelope).await {
                            Ok(res) => res,
                            Err(e) => ToolCallResult {
                                ok: false,
                                output: Value::Null,
                                error: Some(e.to_string()),
                            },
                        };

                        traj_arc.lock().expect("trajectory lock").push(TrajectoryRecord {
                            session_id,
                            origin_plugin,
                            harness_id,
                            kind: if result.ok {
                                "tool.executed".into()
                            } else {
                                "tool.error".into()
                            },
                            payload: json!({
                                "tool": tool_name,
                                "ok": result.ok,
                            }),
                        });

                        Ok::<ToolCallResult, Infallible>(result)
                    }
                },
            ))
            .map_err(|e| format!("install tool around guardrail: {e}"))?;

        // Chokepoint 2: LLM Gateway `Mapper` (secret redaction) + `Around` (model policy & trajectory).
        let llm_policy = Arc::new(Mutex::new(LlmPolicyState::default()));
        let llm_mapper_guard = topology
            .root
            .on::<LlmRequestEvent, _>(mapper_sync(|_ctx, mut req: LlmRequestEnvelope| {
                for msg in &mut req.messages {
                    if let Some(content) = msg.get("content").and_then(Value::as_str) {
                        let (clean, changed) = redact_secret_text(content);
                        if changed {
                            msg["content"] = Value::String(clean);
                            msg["_redacted"] = Value::Bool(true);
                        }
                    }
                }
                Ok::<LlmRequestEnvelope, Infallible>(req)
            }))
            .map_err(|e| format!("install llm mapper: {e}"))?;

        let llm_policy_for_around = Arc::clone(&llm_policy);
        let traj_for_llm = Arc::clone(&trajectory);
        let llm_around_guard = topology
            .root
            .on::<LlmRequestEvent, _>(around(
                move |_ctx, req: LlmRequestEnvelope, next: Next<LlmRequestEvent>| {
                    let pol = Arc::clone(&llm_policy_for_around);
                    let traj = Arc::clone(&traj_for_llm);
                    async move {
                        let veto = {
                            let guard = pol.lock().expect("llm policy lock");
                            if guard.blocked_models.iter().any(|m| m == &req.model) {
                                Some(format!("LLM_MODEL_BLOCKED: `{}` is denied", req.model))
                            } else if !guard.allowed_models.is_empty()
                                && !guard.allowed_models.iter().any(|m| m == &req.model)
                            {
                                Some(format!(
                                    "LLM_MODEL_NOT_ALLOWED: `{}` not in allowlist",
                                    req.model
                                ))
                            } else {
                                None
                            }
                        };

                        if let Some(reason) = veto {
                            traj.lock().expect("trajectory lock").push(TrajectoryRecord {
                                session_id: req.session_id.clone(),
                                origin_plugin: "@dsh/llm-gateway".into(),
                                harness_id: req.harness_id.clone(),
                                kind: "llm.denied".into(),
                                payload: json!({ "model": req.model, "reason": reason }),
                            });
                            return Ok::<LlmResponseEnvelope, Infallible>(LlmResponseEnvelope {
                                ok: false,
                                model: req.model,
                                content: String::new(),
                                prompt_tokens: 0,
                                completion_tokens: 0,
                                redacted_fields: Vec::new(),
                                error: Some(reason),
                            });
                        }

                        let session_id = req.session_id.clone();
                        let harness_id = req.harness_id.clone();
                        let model = req.model.clone();
                        let resp = match next.call(req).await {
                            Ok(r) => r,
                            Err(e) => LlmResponseEnvelope {
                                ok: false,
                                model: model.clone(),
                                content: String::new(),
                                prompt_tokens: 0,
                                completion_tokens: 0,
                                redacted_fields: Vec::new(),
                                error: Some(e.to_string()),
                            },
                        };

                        traj.lock().expect("trajectory lock").push(TrajectoryRecord {
                            session_id,
                            origin_plugin: "@dsh/llm-gateway".into(),
                            harness_id,
                            kind: if resp.ok {
                                "llm.completed".into()
                            } else {
                                "llm.error".into()
                            },
                            payload: json!({
                                "model": model,
                                "prompt_tokens": resp.prompt_tokens,
                                "completion_tokens": resp.completion_tokens,
                                "redacted_count": resp.redacted_fields.len(),
                            }),
                        });

                        Ok::<LlmResponseEnvelope, Infallible>(resp)
                    }
                },
            ))
            .map_err(|e| format!("install llm around guardrail: {e}"))?;

        Ok(Self {
            plan,
            boot: Mutex::new(TwoPhaseBootState::default()),
            tools,
            topology,
            roster: tokio::sync::Mutex::new(roster),
            approval,
            llm_policy,
            trajectory,
            shadow_fibers: Mutex::new(BTreeMap::new()),
            remote_executors: Mutex::new(BTreeMap::new()),
            shadow_peers: Mutex::new(BTreeMap::new()),
            acp_sessions: Mutex::new(BTreeMap::new()),
            acp_seq: AtomicU64::new(1),
            _tool_around_guard: tool_around_guard,
            _llm_mapper_guard: llm_mapper_guard,
            _llm_around_guard: llm_around_guard,
        })
    }

    pub fn topology(&self) -> &RealmTopology {
        &self.topology
    }

    pub fn tool_catalog(&self) -> &ToolCatalog {
        &self.tools
    }

    pub fn approval_policy(&self) -> Arc<Mutex<ApprovalPolicyState>> {
        Arc::clone(&self.approval)
    }

    pub fn trajectory_snapshot(&self) -> Vec<TrajectoryRecord> {
        self.trajectory.lock().expect("trajectory lock").clone()
    }

    pub fn attach_shadow_executor(&self, fiber_id: impl Into<String>, exec: RemoteToolExecutor) {
        self.remote_executors
            .lock()
            .expect("remote executors lock")
            .insert(fiber_id.into(), exec);
    }

    pub async fn handle_shadow_message(&self, msg: ShadowFiberMessage) -> Result<Value, String> {
        self.handle_shadow_message_from_peer(msg, None).await
    }

    async fn handle_shadow_message_from_peer(
        &self,
        msg: ShadowFiberMessage,
        peer: Option<Arc<LiveShadowPeer>>,
    ) -> Result<Value, String> {
        match msg {
            ShadowFiberMessage::Apply {
                fiber_id,
                generation,
                plugin_id,
                ..
            } => {
                let mut fibers = self.shadow_fibers.lock().expect("shadow fibers lock");
                fibers.insert(
                    fiber_id.clone(),
                    ShadowFiberRecord {
                        fiber_id: fiber_id.clone(),
                        generation,
                        plugin_id: plugin_id.clone(),
                        registered_tools: Vec::new(),
                    },
                );
                if let Some(p) = peer {
                    self.bind_peer_to_fiber(&fiber_id, p);
                }
                Ok(json!({ "applied": true, "fiber_id": fiber_id, "plugin_id": plugin_id }))
            }
            ShadowFiberMessage::Update {
                fiber_id,
                generation,
                ..
            } => {
                let mut fibers = self.shadow_fibers.lock().expect("shadow fibers lock");
                let Some(rec) = fibers.get_mut(&fiber_id) else {
                    return Err(format!("unknown shadow fiber `{fiber_id}`"));
                };
                rec.generation = generation;
                Ok(json!({ "updated": true, "fiber_id": fiber_id, "generation": generation }))
            }
            ShadowFiberMessage::RegisterTool {
                fiber_id,
                generation,
                tool,
            } => {
                let reg = RegisteredTool {
                    name: tool.name.clone(),
                    version: tool.version,
                    origin_plugin: tool.origin_plugin.clone(),
                    description: tool.description,
                    input_schema: tool.input_schema,
                    requires_approval: tool.requires_approval,
                };
                {
                    let mut fibers = self.shadow_fibers.lock().expect("shadow fibers lock");
                    let entry = fibers.entry(fiber_id.clone()).or_insert_with(|| ShadowFiberRecord {
                        fiber_id: fiber_id.clone(),
                        generation,
                        plugin_id: tool.origin_plugin.clone(),
                        registered_tools: Vec::new(),
                    });
                    if !entry.registered_tools.contains(&tool.name) {
                        entry.registered_tools.push(tool.name.clone());
                    }
                }
                if let Some(p) = peer {
                    self.bind_peer_to_fiber(&fiber_id, p);
                }
                // A listed tool must already have its fiber and executor wired.
                self.tools.register(reg);
                Ok(json!({ "registered": tool.name, "fiber_id": fiber_id }))
            }
            ShadowFiberMessage::Dispose {
                fiber_id,
                generation,
            } => {
                if peer.is_none() {
                    if let Some(live_peer) = self
                        .shadow_peers
                        .lock()
                        .expect("shadow peers lock")
                        .remove(&fiber_id)
                    {
                        let _ = live_peer.send(ShadowFiberMessage::Dispose {
                            fiber_id: fiber_id.clone(),
                            generation,
                        });
                    }
                } else {
                    self.shadow_peers
                        .lock()
                        .expect("shadow peers lock")
                        .remove(&fiber_id);
                }

                let removed_rec = self
                    .shadow_fibers
                    .lock()
                    .expect("shadow fibers lock")
                    .remove(&fiber_id);
                self.remote_executors
                    .lock()
                    .expect("remote executors lock")
                    .remove(&fiber_id);
                let removed_tools = if let Some(rec) = removed_rec {
                    self.tools.unregister_by_plugin(&rec.plugin_id)
                } else {
                    0
                };
                Ok(json!({
                    "disposed": true,
                    "fiber_id": fiber_id,
                    "removed_tools": removed_tools,
                }))
            }
            ShadowFiberMessage::ServiceInvoke {
                request_id,
                service,
                method,
                params,
                ..
            } => {
                // Shadow Fiber runs in UntrustedPluginRealm: never allowed to call safeStorage!
                let res = self
                    .dispatch_compat("untrusted-plugin", &service, &method, &params)
                    .await;
                let reply = match res {
                    Ok(value) => ShadowFiberMessage::RpcReply {
                        request_id: request_id.clone(),
                        ok: true,
                        result: Some(value),
                        error: None,
                    },
                    Err((_code, message)) => ShadowFiberMessage::RpcReply {
                        request_id: request_id.clone(),
                        ok: false,
                        result: None,
                        error: Some(message),
                    },
                };
                if let Some(p) = peer {
                    let _ = p.send(reply.clone());
                }
                Ok(serde_json::to_value(reply).unwrap_or_else(|_| json!({})))
            }
            ShadowFiberMessage::ToolInvoke {
                request_id,
                tool_name,
                arguments,
                ..
            } => {
                let res = self
                    .invoke_plane_b_tool(ToolCallEnvelope {
                        session_id: "shadow-rpc".into(),
                        harness_id: "shadow-fiber".into(),
                        tool_name,
                        arguments,
                    })
                    .await;
                let reply = ShadowFiberMessage::RpcReply {
                    request_id,
                    ok: res.ok,
                    result: Some(res.output),
                    error: res.error,
                };
                if let Some(p) = peer {
                    let _ = p.send(reply.clone());
                }
                Ok(serde_json::to_value(reply).unwrap_or_else(|_| json!({})))
            }
            ShadowFiberMessage::RpcReply {
                request_id,
                ok,
                result,
                error,
            } => {
                if let Some(p) = peer {
                    p.deliver_reply(&request_id, ok, result, error);
                }
                Ok(json!({ "ack": request_id }))
            }
        }
    }

    fn bind_peer_to_fiber(&self, fiber_id: &str, peer: Arc<LiveShadowPeer>) {
        self.shadow_peers
            .lock()
            .expect("shadow peers lock")
            .insert(fiber_id.to_string(), Arc::clone(&peer));

        let peer_for_exec = Arc::clone(&peer);
        let exec: RemoteToolExecutor = Arc::new(move |fid, tool_name, arguments| {
            let p = Arc::clone(&peer_for_exec);
            Box::pin(async move { p.invoke_remote_tool(fid, tool_name, arguments).await })
        });
        self.attach_shadow_executor(fiber_id, exec);
    }

    /// Invokes the LLM Gateway second chokepoint (`LlmRequestEvent` waterfall).
    pub async fn invoke_llm_gateway(&self, req: LlmRequestEnvelope) -> LlmResponseEnvelope {
        let model_fallback = req.model.clone();
        let outcome = self
            .topology
            .root
            .waterfall::<LlmRequestEvent, _, _, Infallible>(
                Routing::Unscoped,
                req,
                |sanitized| async move {
                    let mut redacted_fields = Vec::new();
                    let mut char_count: u64 = 0;
                    let mut last_user = String::new();
                    for (idx, m) in sanitized.messages.iter().enumerate() {
                        if m.get("_redacted").and_then(Value::as_bool) == Some(true) {
                            redacted_fields.push(format!("messages[{idx}].content"));
                        }
                        if let Some(c) = m.get("content").and_then(Value::as_str) {
                            char_count += c.len() as u64;
                            last_user = c.to_string();
                        }
                    }
                    let reply = format!(
                        "[llm-gateway:{}] echo({})",
                        sanitized.model, last_user
                    );
                    Ok(LlmResponseEnvelope {
                        ok: true,
                        model: sanitized.model,
                        content: reply,
                        prompt_tokens: char_count.div_ceil(4).max(1),
                        completion_tokens: 8,
                        redacted_fields,
                        error: None,
                    })
                },
            )
            .await;

        match outcome {
            Ok(resp) => resp,
            Err(e) => LlmResponseEnvelope {
                ok: false,
                model: model_fallback,
                content: String::new(),
                prompt_tokens: 0,
                completion_tokens: 0,
                redacted_fields: Vec::new(),
                error: Some(e.to_string()),
            },
        }
    }

    /// Invokes any Plane B registered tool through the unified `ToolInvokeEvent` waterfall.
    pub async fn invoke_plane_b_tool(&self, envelope: ToolCallEnvelope) -> ToolCallResult {
        let catalog = self.tools.clone();
        let host_ctx = self.topology.host.clone();
        let shadow_map: Vec<(String, String, Vec<String>)> = self
            .shadow_fibers
            .lock()
            .expect("shadow fibers lock")
            .values()
            .map(|r| {
                (
                    r.fiber_id.clone(),
                    r.plugin_id.clone(),
                    r.registered_tools.clone(),
                )
            })
            .collect();
        let remote_executors: BTreeMap<String, RemoteToolExecutor> = self
            .remote_executors
            .lock()
            .expect("remote executors lock")
            .clone();

        let outcome = self
            .topology
            .root
            .waterfall::<ToolInvokeEvent, _, _, Infallible>(
                Routing::Unscoped,
                envelope,
                move |env| {
                    let catalog = catalog.clone();
                    let host_ctx = host_ctx.clone();
                    let shadow_map = shadow_map.clone();
                    let remote_executors = remote_executors.clone();
                    async move {
                        let Some(meta) = catalog.get(&env.tool_name) else {
                            return Ok(ToolCallResult {
                                ok: false,
                                output: Value::Null,
                                error: Some(format!("TOOL_NOT_FOUND: `{}`", env.tool_name)),
                            });
                        };

                        if meta.origin_plugin == CLIPBOARD_PLUGIN_ID {
                            match host_ctx.try_service::<ClipboardService>() {
                                Ok(svc) => {
                                    match svc.invoke_dsh_tool(&env.tool_name, &env.arguments) {
                                        Ok(val) => {
                                            return Ok(ToolCallResult {
                                                ok: true,
                                                output: val,
                                                error: None,
                                            });
                                        }
                                        Err(e) => {
                                            return Ok(ToolCallResult {
                                                ok: false,
                                                output: Value::Null,
                                                error: Some(e.to_string()),
                                            });
                                        }
                                    }
                                }
                                Err(e) => {
                                    return Ok(ToolCallResult {
                                        ok: false,
                                        output: Value::Null,
                                        error: Some(format!("clipboard unavailable: {e}")),
                                    });
                                }
                            }
                        }

                        for (fiber_id, plugin_id, tool_names) in &shadow_map {
                            if *plugin_id == meta.origin_plugin
                                && tool_names.iter().any(|n| n == &env.tool_name)
                            {
                                if let Some(exec) = remote_executors.get(fiber_id) {
                                    match exec(
                                        fiber_id.clone(),
                                        env.tool_name.clone(),
                                        env.arguments.clone(),
                                    )
                                    .await
                                    {
                                        Ok(val) => {
                                            return Ok(ToolCallResult {
                                                ok: true,
                                                output: val,
                                                error: None,
                                            });
                                        }
                                        Err(err) => {
                                            return Ok(ToolCallResult {
                                                ok: false,
                                                output: Value::Null,
                                                error: Some(err),
                                            });
                                        }
                                    }
                                }
                            }
                        }

                        Ok(ToolCallResult {
                            ok: false,
                            output: Value::Null,
                            error: Some(format!(
                                "TOOL_PROVIDER_UNBOUND: no active executor for `{}` ({})",
                                env.tool_name, meta.origin_plugin
                            )),
                        })
                    }
                },
            )
            .await;

        match outcome {
            Ok(res) => res,
            Err(e) => ToolCallResult {
                ok: false,
                output: Value::Null,
                error: Some(e.to_string()),
            },
        }
    }

    async fn dispatch_compat(
        &self,
        caller_realm: &str,
        capability: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, (i32, String)> {
        let not_loaded = |name: &str| {
            (
                -32004,
                format!("CAPABILITY_NOT_LOADED: `{name}` has no active provider in HostRealm"),
            )
        };
        match capability {
            "clipboard" => self
                .topology
                .host
                .try_service::<ClipboardService>()
                .map_err(|_| not_loaded("clipboard"))?
                .call_compat(method, args)
                .map_err(|e| (-32010, e.to_string())),
            "safeStorage" => self
                .topology
                .host
                .try_service::<SafeStorageService>()
                .map_err(|_| not_loaded("safeStorage"))?
                .call_compat(caller_realm, method, args)
                .map_err(|e| (-32003, e.to_string())),
            "dialog" => self
                .topology
                .host
                .try_service::<DialogService>()
                .map_err(|_| not_loaded("dialog"))?
                .call_compat(method, args)
                .await
                .map_err(|e| (-32011, e.to_string())),
            "shell" => self
                .topology
                .host
                .try_service::<OsShellService>()
                .map_err(|_| not_loaded("shell"))?
                .call_compat(method, args)
                .await
                .map_err(|e| (-32012, e.to_string())),
            "trayMenu" => self
                .topology
                .host
                .try_service::<TrayMenuService>()
                .map_err(|_| not_loaded("trayMenu"))?
                .call_compat(method, args)
                .map_err(|e| (-32013, e.to_string())),
            "notification" => self
                .topology
                .host
                .try_service::<NotificationService>()
                .map_err(|_| not_loaded("notification"))?
                .call_compat(method, args)
                .await
                .map_err(|e| (-32014, e.to_string())),
            "osIntegration" => self
                .topology
                .host
                .try_service::<OsIntegrationService>()
                .map_err(|_| not_loaded("osIntegration"))?
                .call_compat(method, args)
                .map_err(|e| (-32015, e.to_string())),
            "desktopCapture" => self
                .topology
                .host
                .try_service::<DesktopCaptureService>()
                .map_err(|_| not_loaded("desktopCapture"))?
                .call_compat(method, args)
                .map_err(|e| (-32016, e.to_string())),
            "updater" => self
                .topology
                .host
                .try_service::<UpdaterService>()
                .map_err(|_| not_loaded("updater"))?
                .call_compat(method, args)
                .map_err(|e| (-32017, e.to_string())),
            "pluginInstaller" => self
                .topology
                .host
                .try_service::<PluginInstallerService>()
                .map_err(|_| not_loaded("pluginInstaller"))?
                .call_compat(method, args)
                .map_err(|e| (-32018, e.to_string())),
            other => Err(not_loaded(other)),
        }
    }

    pub async fn dispatch(&self, req: RpcRequest) -> (RpcResponse, bool) {
        let id = req.id.unwrap_or(Value::Null);
        let ok = |result: Value| RpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: Some(result),
            error: None,
        };
        let err = |code: i32, message: String| RpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: None,
            error: Some(RpcError { code, message }),
        };

        match req.method.as_str() {
            "cordis.health" => {
                let boot = self.boot.lock().expect("boot lock").clone();
                let (profile, bundled, on_demand) = match &self.plan {
                    Some(p) => (p.profile.as_str(), p.bundled_count(), p.on_demand_count()),
                    None => ("unconfigured", 0, 0),
                };
                (
                    ok(json!({
                        "schema": WIRE_SCHEMA,
                        "profile": profile,
                        "phase": boot.phase,
                        "ui_ready": boot.ui_ready,
                        "auth_generation": boot.auth_generation,
                        "bundled_plugins": bundled,
                        "on_demand_plugins": on_demand,
                        "registered_tools": self.tools.list().len(),
                        "krun_backends": [],
                        "projections": [
                            HarnessProjection::Mcp.default_guest_socket(),
                            HarnessProjection::InkstoneToolsV1.default_guest_socket(),
                            HarnessProjection::AcpJsonRpc.default_guest_socket(),
                            HarnessProjection::LlmGateway.default_guest_socket(),
                        ],
                    })),
                    false,
                )
            }
            "cordis.ui_ready" => {
                let (phase, ui_ready) = {
                    let mut boot = self.boot.lock().expect("boot lock");
                    boot.mark_ui_ready();
                    (boot.phase, boot.ui_ready)
                };
                // Flush any startup intents queued prior to first paint in strict FIFO order.
                let startup_intents = self
                    .topology
                    .host
                    .try_service::<OsIntegrationService>()
                    .map(|svc| svc.drain_startup_intents())
                    .unwrap_or_default();
                (
                    ok(json!({
                        "phase": phase,
                        "ui_ready": ui_ready,
                        "startup_intents": startup_intents,
                    })),
                    false,
                )
            }
            "cordis.auth.swap" => {
                let mut boot = self.boot.lock().expect("boot lock");
                let generation = boot.swap_auth_generation();
                (
                    ok(json!({
                        "phase": boot.phase,
                        "auth_generation": generation,
                    })),
                    false,
                )
            }
            "cordis.tool.register" => match serde_json::from_value::<RegisteredTool>(req.params) {
                Ok(tool) => {
                    let name = tool.name.clone();
                    self.tools.register(tool);
                    (ok(json!({ "registered": name })), false)
                }
                Err(e) => (err(-32602, format!("invalid tool spec: {e}")), false),
            },
            "cordis.tool.unregister_plugin" => {
                let Some(origin) = req.params.get("origin_plugin").and_then(Value::as_str) else {
                    return (err(-32602, "missing origin_plugin".into()), false);
                };
                let removed = self.tools.unregister_by_plugin(origin);
                (ok(json!({ "removed": removed })), false)
            }
            "cordis.tool.list" => (ok(json!({ "tools": self.tools.list() })), false),
            "cordis.policy.set" => {
                let mut policy = self.approval.lock().expect("approval lock");
                if let Some(arr) = req.params.get("denied_tools").and_then(Value::as_array) {
                    policy.denied_tools = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect();
                }
                if let Some(flag) = req
                    .params
                    .get("enforce_requires_approval")
                    .and_then(Value::as_bool)
                {
                    policy.enforce_requires_approval = flag;
                }
                if let Some(arr) = req.params.get("approved_tools").and_then(Value::as_array) {
                    policy.approved_tools = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect();
                }
                (
                    ok(json!({
                        "denied_tools": policy.denied_tools,
                        "enforce_requires_approval": policy.enforce_requires_approval,
                        "approved_tools": policy.approved_tools,
                    })),
                    false,
                )
            }
            "cordis.llm_policy.set" => {
                let mut policy = self.llm_policy.lock().expect("llm policy lock");
                if let Some(arr) = req.params.get("allowed_models").and_then(Value::as_array) {
                    policy.allowed_models = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect();
                }
                if let Some(arr) = req.params.get("blocked_models").and_then(Value::as_array) {
                    policy.blocked_models = arr
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect();
                }
                (
                    ok(json!({
                        "allowed_models": policy.allowed_models,
                        "blocked_models": policy.blocked_models,
                    })),
                    false,
                )
            }
            "cordis.trajectory.list" => (
                ok(json!({ "records": self.trajectory_snapshot() })),
                false,
            ),
            "cordis.shadow.message" => {
                match serde_json::from_value::<ShadowFiberMessage>(req.params) {
                    Ok(msg) => match self.handle_shadow_message(msg).await {
                        Ok(res) => (ok(res), false),
                        Err(e) => (err(-32020, e), false),
                    },
                    Err(e) => (err(-32602, format!("invalid shadow message: {e}")), false),
                }
            }
            // Projection 1: Standard MCP (`bridge-mcp` / `/run/krun/dsh-mcp.sock`)
            "initialize" => (
                ok(json!({
                    "protocolVersion": "2025-03-26",
                    "serverInfo": {
                        "name": "electron-cordis-bridge-mcp",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "tools": { "listChanged": true }
                    }
                })),
                false,
            ),
            "notifications/initialized" => (ok(json!({ "ack": true })), false),
            "tools/list" | "mcp.tools.list" => {
                let mcp_tools: Vec<McpToolDescriptor> = self
                    .tools
                    .list()
                    .into_iter()
                    .map(|t| McpToolDescriptor {
                        name: t.name,
                        description: t.description,
                        input_schema: t.input_schema,
                    })
                    .collect();
                (ok(json!({ "tools": mcp_tools })), false)
            }
            "tools/call" | "mcp.tools.call" => {
                let Some(name) = req.params.get("name").and_then(Value::as_str) else {
                    return (err(-32602, "missing MCP tool `name`".into()), false);
                };
                let arguments = req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let session_id = req
                    .params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("mcp-default")
                    .to_string();
                let res = self
                    .invoke_plane_b_tool(ToolCallEnvelope {
                        session_id,
                        harness_id: "mcp-client".into(),
                        tool_name: name.to_string(),
                        arguments,
                    })
                    .await;
                let text = if res.ok {
                    serde_json::to_string(&res.output).unwrap_or_default()
                } else {
                    res.error.clone().unwrap_or_else(|| "tool failed".into())
                };
                let mcp_result = McpCallToolResult {
                    content: vec![McpContentBlock {
                        kind: "text".into(),
                        text,
                    }],
                    is_error: !res.ok,
                    structured_content: if res.ok { Some(res.output) } else { None },
                };
                (
                    ok(serde_json::to_value(mcp_result).unwrap_or_else(|_| json!({}))),
                    false,
                )
            }
            // Projection 2: Inkstone Host Native Contract (`bridge-inkstone-tools` / `/run/krun/host-tools.sock`)
            "inkstone.contract.info" => (
                ok(json!({
                    "protocol": INKSTONE_NATIVE_CONTRACT_PROTOCOL,
                    "contractSha256": INKSTONE_NATIVE_CONTRACT_SHA256,
                    "maxFrameBytes": INKSTONE_NATIVE_MAX_FRAME_BYTES,
                })),
                false,
            ),
            "inkstone.tools.list" => (
                ok(json!({
                    "schema": "inkstone.tools.v1",
                    "protocol": INKSTONE_NATIVE_CONTRACT_PROTOCOL,
                    "contractSha256": INKSTONE_NATIVE_CONTRACT_SHA256,
                    "tools": self.tools.list(),
                })),
                false,
            ),
            "inkstone.tools.call" => {
                match serde_json::from_value::<NativeContractToolCall>(req.params) {
                    Ok(call) => {
                        let res = self
                            .invoke_plane_b_tool(ToolCallEnvelope {
                                session_id: call.session_id,
                                harness_id: call.harness_id,
                                tool_name: call.tool,
                                arguments: call.input,
                            })
                            .await;
                        (
                            ok(serde_json::to_value(res).unwrap_or_else(|_| json!({}))),
                            false,
                        )
                    }
                    Err(e) => (
                        err(-32602, format!("invalid inkstone.tools.call params: {e}")),
                        false,
                    ),
                }
            }
            // Projection 3: ACP / dsh-sdk-protocol (`bridge-acp` / `/run/krun/dsh-rpc.sock`)
            "session/new" | "acp.session.new" => {
                let create: AcpSessionCreate = serde_json::from_value(req.params)
                    .unwrap_or(AcpSessionCreate {
                        harness_id: "acp-client".into(),
                        cwd: None,
                    });
                let sid = format!("acp-sess-{}", self.acp_seq.fetch_add(1, Ordering::Relaxed));
                let rec = AcpSessionRecord {
                    session_id: sid.clone(),
                    harness_id: create.harness_id,
                    cwd: create.cwd,
                    cancelled: false,
                };
                self.acp_sessions
                    .lock()
                    .expect("acp sessions lock")
                    .insert(sid.clone(), rec);
                (
                    ok(json!({
                        "sessionId": sid,
                        "tools": self.tools.list(),
                    })),
                    false,
                )
            }
            "session/prompt" | "acp.session.prompt" => {
                match serde_json::from_value::<AcpPromptRequest>(req.params) {
                    Ok(prompt_req) => {
                        let session = self
                            .acp_sessions
                            .lock()
                            .expect("acp sessions lock")
                            .get(&prompt_req.session_id)
                            .cloned();
                        let Some(sess) = session else {
                            return (
                                err(
                                    -32030,
                                    format!("ACP_SESSION_NOT_FOUND: `{}`", prompt_req.session_id),
                                ),
                                false,
                            );
                        };
                        if sess.cancelled {
                            return (
                                err(
                                    -32031,
                                    format!("ACP_SESSION_CANCELLED: `{}`", prompt_req.session_id),
                                ),
                                false,
                            );
                        }
                        let tool_outcome = if let Some(tc) = prompt_req.tool_call {
                            Some(
                                self.invoke_plane_b_tool(ToolCallEnvelope {
                                    session_id: sess.session_id.clone(),
                                    harness_id: sess.harness_id.clone(),
                                    tool_name: tc.name,
                                    arguments: tc.arguments,
                                })
                                .await,
                            )
                        } else {
                            None
                        };
                        (
                            ok(json!({
                                "sessionId": sess.session_id,
                                "stopReason": "end_turn",
                                "prompt": prompt_req.prompt,
                                "toolResult": tool_outcome,
                            })),
                            false,
                        )
                    }
                    Err(e) => (err(-32602, format!("invalid acp prompt: {e}")), false),
                }
            }
            "session/cancel" | "acp.session.cancel" => {
                let Some(sid) = req.params.get("session_id").and_then(Value::as_str) else {
                    return (err(-32602, "missing session_id".into()), false);
                };
                if let Some(s) = self
                    .acp_sessions
                    .lock()
                    .expect("acp sessions lock")
                    .get_mut(sid)
                {
                    s.cancelled = true;
                    (ok(json!({ "cancelled": true, "sessionId": sid })), false)
                } else {
                    (err(-32030, format!("ACP_SESSION_NOT_FOUND: `{sid}`")), false)
                }
            }
            // Projection 4: LLM Gateway second chokepoint (`llm-gateway` / `/run/krun/llm-gateway.sock`)
            "llm.chat.completions" | "llm/complete" => {
                match serde_json::from_value::<LlmRequestEnvelope>(req.params) {
                    Ok(llm_req) => {
                        let resp = self.invoke_llm_gateway(llm_req).await;
                        (
                            ok(serde_json::to_value(resp).unwrap_or_else(|_| json!({}))),
                            false,
                        )
                    }
                    Err(e) => (err(-32602, format!("invalid llm request: {e}")), false),
                }
            }
            "cordis.compat.call" => {
                let cap = req
                    .params
                    .get("capability")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let method = req
                    .params
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = req
                    .params
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let caller_realm = req
                    .params
                    .get("realm")
                    .and_then(Value::as_str)
                    .unwrap_or("host");
                match self.dispatch_compat(caller_realm, cap, method, &args).await {
                    Ok(value) => (ok(value), false),
                    Err((code, msg)) => (err(code, msg), false),
                }
            }
            "cordis.shutdown" => {
                {
                    let mut boot = self.boot.lock().expect("boot lock");
                    boot.phase = BootPhase::ShuttingDown;
                }
                let shadow_ids: Vec<String> = self
                    .shadow_fibers
                    .lock()
                    .expect("shadow fibers lock")
                    .keys()
                    .cloned()
                    .collect();
                for fid in shadow_ids.into_iter().rev() {
                    let _ = self
                        .handle_shadow_message(ShadowFiberMessage::Dispose {
                            fiber_id: fid,
                            generation: 0,
                        })
                        .await;
                }
                let roster = {
                    let mut guard = self.roster.lock().await;
                    std::mem::take(&mut *guard)
                };
                let teardown_errors = roster.drain_reverse().await;
                let msgs: Vec<String> = teardown_errors.into_iter().map(|e| e.to_string()).collect();
                (ok(json!({ "drained": true, "errors": msgs })), true)
            }
            other => (err(-32601, format!("unknown method: {other}")), false),
        }
    }

    #[cfg(unix)]
    pub fn spawn_jsonrpc_unix_listener(
        self: &Arc<Self>,
        socket_path: std::path::PathBuf,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path)?;
        let state = Arc::clone(self);
        Ok(tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let st = Arc::clone(&state);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut lines = BufReader::new(read_half).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let (resp, _) = match serde_json::from_str::<RpcRequest>(trimmed) {
                            Ok(req) => st.dispatch(req).await,
                            Err(e) => (
                                RpcResponse {
                                    jsonrpc: "2.0".to_string(),
                                    id: Value::Null,
                                    result: None,
                                    error: Some(RpcError {
                                        code: -32700,
                                        message: format!("invalid json: {e}"),
                                    }),
                                },
                                false,
                            ),
                        };
                        if let Ok(encoded) = serde_json::to_string(&resp) {
                            if write_half.write_all(encoded.as_bytes()).await.is_err()
                                || write_half.write_all(b"\n").await.is_err()
                                || write_half.flush().await.is_err()
                            {
                                break;
                            }
                        }
                    }
                });
            }
        }))
    }

    #[cfg(unix)]
    pub fn spawn_shadow_unix_listener(
        self: &Arc<Self>,
        socket_path: std::path::PathBuf,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path)?;
        let state = Arc::clone(self);
        Ok(tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let st = Arc::clone(&state);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let (tx, mut rx) = mpsc::unbounded_channel::<ShadowFiberMessage>();
                    let peer = LiveShadowPeer::new(tx);
                    let owned_fibers = Arc::new(Mutex::new(Vec::<String>::new()));

                    let writer = tokio::spawn(async move {
                        while let Some(msg) = rx.recv().await {
                            if let Ok(encoded) = serde_json::to_string(&msg) {
                                if write_half.write_all(encoded.as_bytes()).await.is_err()
                                    || write_half.write_all(b"\n").await.is_err()
                                    || write_half.flush().await.is_err()
                                {
                                    break;
                                }
                            }
                        }
                    });

                    let mut lines = BufReader::new(read_half).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(msg) = serde_json::from_str::<ShadowFiberMessage>(trimmed) {
                            if let ShadowFiberMessage::Apply { ref fiber_id, .. }
                            | ShadowFiberMessage::RegisterTool { ref fiber_id, .. } = msg
                            {
                                let mut owned = owned_fibers.lock().expect("owned fibers lock");
                                if !owned.contains(fiber_id) {
                                    owned.push(fiber_id.clone());
                                }
                            }
                            let st_task = Arc::clone(&st);
                            let peer_task = Arc::clone(&peer);
                            tokio::spawn(async move {
                                let _ = st_task
                                    .handle_shadow_message_from_peer(msg, Some(peer_task))
                                    .await;
                            });
                        }
                    }

                    let leftovers: Vec<String> = {
                        let mut guard = owned_fibers.lock().expect("owned fibers lock");
                        std::mem::take(&mut *guard)
                    };
                    for fid in leftovers.into_iter().rev() {
                        let _ = st
                            .handle_shadow_message_from_peer(
                                ShadowFiberMessage::Dispose {
                                    fiber_id: fid,
                                    generation: 0,
                                },
                                Some(Arc::clone(&peer)),
                            )
                            .await;
                    }
                    writer.abort();
                });
            }
        }))
    }
}
