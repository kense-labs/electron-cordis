//! M3 Batch 2 Class-A Rust DSH desktop capability plugins (Chromium-free):
//! - `@dsh/cap-tray-menu`: bilingual hot-rebuilt tray & application menu model
//! - `@dsh/cap-notification`: permission-gated non-blocking desktop notifications
//! - `@dsh/cap-os-integration`: Startup Intent FIFO buffer (Pre-UiReady -> UiReady flush),
//!   bilingual locale state, and isolated side-by-side protocol registration (`inkstone-cordis://`)

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use cordis_core::{Context, Plugin, Service};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const TRAY_PLUGIN_ID: &str = "@dsh/cap-tray-menu";
pub const NOTIFICATION_PLUGIN_ID: &str = "@dsh/cap-notification";
pub const OS_INTEGRATION_PLUGIN_ID: &str = "@dsh/cap-os-integration";

pub const TRAY_SERVICE_NAME: &str = "cap.tray-menu";
pub const NOTIFICATION_SERVICE_NAME: &str = "cap.notification";
pub const OS_INTEGRATION_SERVICE_NAME: &str = "cap.os-integration";

#[derive(Debug, Error)]
pub enum DesktopUiError {
    #[error("PERMISSION_DENIED (fail-closed): {0}")]
    PermissionDenied(String),
    #[error("invalid desktop-ui arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown desktop-ui method: {0}")]
    UnknownMethod(String),
    #[error("desktop-ui runtime error: {0}")]
    Runtime(String),
}

// ---------------------------------------------------------------------------
// 1. @dsh/cap-tray-menu
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrayMenuItem {
    pub id: String,
    pub label: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub separator: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayState {
    pub locale: String,
    pub tooltip: String,
    pub items: Vec<TrayMenuItem>,
}

impl Default for TrayState {
    fn default() -> Self {
        Self::for_locale("zh-CN")
    }
}

impl TrayState {
    pub fn for_locale(locale: &str) -> Self {
        let is_zh = locale.to_ascii_lowercase().starts_with("zh");
        Self {
            locale: locale.to_string(),
            tooltip: if is_zh {
                "书生·端砚 (Cordis)".into()
            } else {
                "Inkstone (Cordis)".into()
            },
            items: vec![
                TrayMenuItem {
                    id: "open-window".into(),
                    label: if is_zh { "打开主窗口".into() } else { "Open Main Window".into() },
                    enabled: true,
                    separator: false,
                },
                TrayMenuItem {
                    id: "quit".into(),
                    label: if is_zh { "退出".into() } else { "Quit".into() },
                    enabled: true,
                    separator: false,
                },
            ],
        }
    }
}

#[derive(Clone, Default)]
pub struct TrayMenuService {
    state: Arc<Mutex<TrayState>>,
}

impl Service for TrayMenuService {
    const NAME: &'static str = TRAY_SERVICE_NAME;
}

impl TrayMenuService {
    pub fn new(locale: &str) -> Self {
        Self {
            state: Arc::new(Mutex::new(TrayState::for_locale(locale))),
        }
    }

    pub fn set_locale(&self, locale: &str) -> Result<TrayState, DesktopUiError> {
        if locale.trim().is_empty() {
            return Err(DesktopUiError::InvalidArgs("locale required".into()));
        }
        let next = TrayState::for_locale(locale);
        let mut guard = self.state.lock().expect("tray lock");
        *guard = next.clone();
        Ok(next)
    }

    pub fn set_menu(&self, items: Vec<TrayMenuItem>) -> Result<TrayState, DesktopUiError> {
        for item in &items {
            if item.id.trim().is_empty() {
                return Err(DesktopUiError::InvalidArgs("menu item id required".into()));
            }
        }
        let mut guard = self.state.lock().expect("tray lock");
        guard.items = items;
        Ok(guard.clone())
    }

    pub fn snapshot(&self) -> TrayState {
        self.state.lock().expect("tray lock").clone()
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, DesktopUiError> {
        match method {
            "getMenu" => Ok(serde_json::to_value(self.snapshot()).unwrap()),
            "setLocale" => {
                let locale = args
                    .get("locale")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DesktopUiError::InvalidArgs("missing string `locale`".into()))?;
                Ok(serde_json::to_value(self.set_locale(locale)?).unwrap())
            }
            "setMenu" => {
                let items: Vec<TrayMenuItem> = serde_json::from_value(
                    args.get("items").cloned().unwrap_or_else(|| json!([])),
                )
                .map_err(|e| DesktopUiError::InvalidArgs(e.to_string()))?;
                Ok(serde_json::to_value(self.set_menu(items)?).unwrap())
            }
            "setTooltip" => {
                let tooltip = args
                    .get("tooltip")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        DesktopUiError::InvalidArgs("missing string `tooltip`".into())
                    })?;
                let mut guard = self.state.lock().expect("tray lock");
                guard.tooltip = tooltip.to_string();
                Ok(json!({ "tooltip": guard.tooltip }))
            }
            other => Err(DesktopUiError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayMenuConfig {
    #[serde(default = "default_locale")]
    pub locale: String,
}

impl Default for TrayMenuConfig {
    fn default() -> Self {
        Self {
            locale: default_locale(),
        }
    }
}

fn default_locale() -> String {
    "zh-CN".to_string()
}

#[derive(Clone, Default)]
pub struct TrayMenuPlugin;

impl Plugin for TrayMenuPlugin {
    type Config = TrayMenuConfig;
    type Input = TrayMenuConfig;
    type PrepareError = Infallible;
    type ApplyError = DesktopUiError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(TrayMenuService::new(&input.locale));
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| DesktopUiError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| DesktopUiError::Runtime(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 2. @dsh/cap-notification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRecord {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub silent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub permitted: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { permitted: true }
    }
}

#[derive(Clone)]
pub struct NotificationService {
    permitted: Arc<Mutex<bool>>,
    sent: Arc<Mutex<Vec<NotificationRecord>>>,
}

impl Service for NotificationService {
    const NAME: &'static str = NOTIFICATION_SERVICE_NAME;
}

impl NotificationService {
    pub fn new(cfg: NotificationConfig) -> Self {
        Self {
            permitted: Arc::new(Mutex::new(cfg.permitted)),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn show(
        &self,
        title: String,
        body: String,
        silent: bool,
    ) -> Result<NotificationRecord, DesktopUiError> {
        if !*self.permitted.lock().expect("perm lock") {
            return Err(DesktopUiError::PermissionDenied(
                "desktop notification permission denied".into(),
            ));
        }
        if title.trim().is_empty() || title.contains('\0') || body.contains('\0') {
            return Err(DesktopUiError::InvalidArgs(
                "valid non-empty title and NUL-free body required".into(),
            ));
        }
        let sent_arc = Arc::clone(&self.sent);
        tokio::task::spawn_blocking(move || {
            let mut guard = sent_arc.lock().expect("sent lock");
            let id = (guard.len() as u64) + 1;
            let rec = NotificationRecord {
                id,
                title,
                body,
                silent,
            };
            guard.push(rec.clone());
            rec
        })
        .await
        .map_err(|e| DesktopUiError::Runtime(e.to_string()))
    }

    pub async fn call_compat(&self, method: &str, args: &Value) -> Result<Value, DesktopUiError> {
        match method {
            "isSupported" => Ok(json!(true)),
            "setPermission" => {
                let allow = args
                    .get("permitted")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        DesktopUiError::InvalidArgs("missing bool `permitted`".into())
                    })?;
                *self.permitted.lock().expect("perm lock") = allow;
                Ok(json!({ "permitted": allow }))
            }
            "show" => {
                let title = args
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let body = args
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let silent = args.get("silent").and_then(Value::as_bool).unwrap_or(false);
                let rec = self.show(title, body, silent).await?;
                Ok(serde_json::to_value(rec).unwrap())
            }
            "listSent" => {
                let list = self.sent.lock().expect("sent lock").clone();
                Ok(json!({ "notifications": list }))
            }
            other => Err(DesktopUiError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct NotificationPlugin;

impl Plugin for NotificationPlugin {
    type Config = NotificationConfig;
    type Input = NotificationConfig;
    type PrepareError = Infallible;
    type ApplyError = DesktopUiError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(NotificationService::new(input.clone()));
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| DesktopUiError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| DesktopUiError::Runtime(e.to_string()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 3. @dsh/cap-os-integration (Startup Intent FIFO + Bilingual Locale + Canary Scheme)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartupIntent {
    pub kind: String,
    pub payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsIntegrationConfig {
    #[serde(default = "default_locale")]
    pub locale: String,
    /// Side-by-side canary scheme (isolated from native Electron `inkstone://`).
    #[serde(default = "default_scheme")]
    pub protocol_scheme: String,
}

fn default_scheme() -> String {
    "inkstone-cordis".to_string()
}

impl Default for OsIntegrationConfig {
    fn default() -> Self {
        Self {
            locale: default_locale(),
            protocol_scheme: default_scheme(),
        }
    }
}

struct OsIntegrationState {
    locale: String,
    protocol_scheme: String,
    open_at_login: bool,
    startup_intents: VecDeque<StartupIntent>,
}

#[derive(Clone)]
pub struct OsIntegrationService {
    state: Arc<Mutex<OsIntegrationState>>,
}

impl Service for OsIntegrationService {
    const NAME: &'static str = OS_INTEGRATION_SERVICE_NAME;
}

impl OsIntegrationService {
    pub fn new(cfg: OsIntegrationConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(OsIntegrationState {
                locale: cfg.locale,
                protocol_scheme: cfg.protocol_scheme,
                open_at_login: false,
                startup_intents: VecDeque::new(),
            })),
        }
    }

    pub fn enqueue_startup_intent(&self, intent: StartupIntent) -> Result<usize, DesktopUiError> {
        if intent.kind.trim().is_empty() || intent.payload.contains('\0') {
            return Err(DesktopUiError::InvalidArgs("invalid startup intent".into()));
        }
        let mut guard = self.state.lock().expect("os-int lock");
        guard.startup_intents.push_back(intent);
        Ok(guard.startup_intents.len())
    }

    /// Drains all buffered startup intents in strict FIFO order (called on `UiReady`).
    pub fn drain_startup_intents(&self) -> Vec<StartupIntent> {
        let mut guard = self.state.lock().expect("os-int lock");
        guard.startup_intents.drain(..).collect()
    }

    pub fn call_compat(&self, method: &str, args: &Value) -> Result<Value, DesktopUiError> {
        match method {
            "getLocale" => {
                let guard = self.state.lock().expect("os-int lock");
                Ok(json!({ "locale": guard.locale }))
            }
            "setLocale" => {
                let locale = args
                    .get("locale")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DesktopUiError::InvalidArgs("missing string `locale`".into()))?;
                let mut guard = self.state.lock().expect("os-int lock");
                guard.locale = locale.to_string();
                Ok(json!({ "locale": guard.locale }))
            }
            "enqueueStartupIntent" => {
                let intent: StartupIntent = serde_json::from_value(args.clone())
                    .map_err(|e| DesktopUiError::InvalidArgs(e.to_string()))?;
                let queued = self.enqueue_startup_intent(intent)?;
                Ok(json!({ "queued": queued }))
            }
            "drainStartupIntents" => {
                let intents = self.drain_startup_intents();
                Ok(json!({ "intents": intents }))
            }
            "getProtocolScheme" => {
                let guard = self.state.lock().expect("os-int lock");
                Ok(json!({ "scheme": guard.protocol_scheme }))
            }
            "setLoginItemSettings" => {
                let open = args
                    .get("openAtLogin")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut guard = self.state.lock().expect("os-int lock");
                guard.open_at_login = open;
                Ok(json!({ "openAtLogin": guard.open_at_login }))
            }
            other => Err(DesktopUiError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct OsIntegrationPlugin;

impl Plugin for OsIntegrationPlugin {
    type Config = OsIntegrationConfig;
    type Input = OsIntegrationConfig;
    type PrepareError = Infallible;
    type ApplyError = DesktopUiError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(OsIntegrationService::new(input.clone()));
        let pub_handle = ctx
            .provide(svc)
            .map_err(|e| DesktopUiError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = pub_handle.remove();
        })
        .map_err(|e| DesktopUiError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::PreparedPlugin;
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    #[tokio::test]
    async fn batch2_tray_notification_and_startup_intent_fifo_work_cleanly() {
        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");

        let h_tray = topo
            .host
            .spawn(PreparedPlugin::from_input(
                TrayMenuPlugin,
                TrayMenuConfig::default(),
            ))
            .await
            .unwrap();
        let h_notif = topo
            .host
            .spawn(PreparedPlugin::from_input(
                NotificationPlugin,
                NotificationConfig::default(),
            ))
            .await
            .unwrap();
        let h_os = topo
            .host
            .spawn(PreparedPlugin::from_input(
                OsIntegrationPlugin,
                OsIntegrationConfig::default(),
            ))
            .await
            .unwrap();

        // Bilingual tray hot-switch
        let tray = topo.host.try_service::<TrayMenuService>().unwrap();
        assert_eq!(tray.snapshot().items[0].label, "打开主窗口");
        tray.call_compat("setLocale", &json!({ "locale": "en-US" }))
            .unwrap();
        assert_eq!(tray.snapshot().items[0].label, "Open Main Window");

        // Notification permission gate
        let notif = topo.host.try_service::<NotificationService>().unwrap();
        let shown = notif
            .call_compat("show", &json!({ "title": "Done", "body": "Task finished" }))
            .await
            .unwrap();
        assert_eq!(shown["id"], json!(1));
        notif
            .call_compat("setPermission", &json!({ "permitted": false }))
            .await
            .unwrap();
        assert!(matches!(
            notif
                .call_compat("show", &json!({ "title": "Blocked", "body": "x" }))
                .await,
            Err(DesktopUiError::PermissionDenied(_))
        ));

        // Startup Intent FIFO queue
        let os_int = topo.host.try_service::<OsIntegrationService>().unwrap();
        assert_eq!(
            os_int.call_compat("getProtocolScheme", &json!({})).unwrap()["scheme"],
            json!("inkstone-cordis")
        );
        os_int
            .call_compat(
                "enqueueStartupIntent",
                &json!({ "kind": "deep-link", "payload": "inkstone-cordis://chat/1" }),
            )
            .unwrap();
        os_int
            .call_compat(
                "enqueueStartupIntent",
                &json!({ "kind": "open-file", "payload": "/tmp/paper.pdf" }),
            )
            .unwrap();
        let drained = os_int.drain_startup_intents();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].kind, "deep-link");
        assert_eq!(drained[1].kind, "open-file");
        assert!(os_int.drain_startup_intents().is_empty());

        h_os.dispose().await.unwrap();
        h_notif.dispose().await.unwrap();
        h_tray.dispose().await.unwrap();
    }
}
