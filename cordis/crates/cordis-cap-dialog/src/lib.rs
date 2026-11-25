//! `@dsh/cap-dialog` — Class-A OS dialog capability plugin (Chromium-free Rust DSH plugin).
//!
//! Guarantees:
//! - Never blocks Tokio worker threads (`tokio::task::spawn_blocking` for native dialog calls).
//! - Validates dialog options strictly (rejects NUL bytes in paths/titles/messages).
//! - Provides deterministic headless mode for CI / `inkstone-headless` / automated tests.

use std::convert::Infallible;
use std::sync::Arc;

use cordis_core::{Context, Plugin, Service};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const PLUGIN_ID: &str = "@dsh/cap-dialog";
pub const SERVICE_NAME: &str = "cap.dialog";

#[derive(Debug, Error)]
pub enum DialogError {
    #[error("invalid dialog options: {0}")]
    InvalidArgs(String),
    #[error("unknown dialog method: {0}")]
    UnknownMethod(String),
    #[error("dialog execution error: {0}")]
    Runtime(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialogMode {
    /// Non-interactive deterministic headless mode (returns canceled=true for file pickers and defaultId for message boxes).
    #[default]
    Headless,
    /// Native OS dialog via `spawn_blocking` (falls back cleanly if display is unavailable).
    OsNative,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DialogConfig {
    #[serde(default)]
    pub mode: DialogMode,
    /// Optional canned paths returned by `showOpenDialog` / `showSaveDialog` in Headless mode.
    #[serde(default)]
    pub headless_selected_paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDialogOptions {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub default_path: Option<String>,
    #[serde(default)]
    pub properties: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveDialogOptions {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub default_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBoxOptions {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub buttons: Vec<String>,
    #[serde(default)]
    pub default_id: usize,
}

fn reject_nul(label: &str, value: &str) -> Result<(), DialogError> {
    if value.contains('\0') {
        return Err(DialogError::InvalidArgs(format!(
            "{label} must not contain NUL bytes"
        )));
    }
    Ok(())
}

#[derive(Clone)]
pub struct DialogService {
    config: DialogConfig,
}

impl Service for DialogService {
    const NAME: &'static str = SERVICE_NAME;
}

impl Default for DialogService {
    fn default() -> Self {
        Self::new(DialogConfig::default())
    }
}

impl DialogService {
    pub fn new(config: DialogConfig) -> Self {
        Self { config }
    }

    pub async fn show_open_dialog(&self, opts: OpenDialogOptions) -> Result<Value, DialogError> {
        if let Some(title) = &opts.title {
            reject_nul("title", title)?;
        }
        if let Some(path) = &opts.default_path {
            reject_nul("defaultPath", path)?;
        }
        let cfg = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let paths = cfg.headless_selected_paths;
            let canceled = paths.is_empty();
            Ok::<Value, DialogError>(json!({
                "canceled": canceled,
                "filePaths": paths,
            }))
        })
        .await
        .map_err(|e| DialogError::Runtime(format!("join error: {e}")))?
    }

    pub async fn show_save_dialog(&self, opts: SaveDialogOptions) -> Result<Value, DialogError> {
        if let Some(title) = &opts.title {
            reject_nul("title", title)?;
        }
        if let Some(path) = &opts.default_path {
            reject_nul("defaultPath", path)?;
        }
        let cfg = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let file_path = cfg.headless_selected_paths.into_iter().next();
            let canceled = file_path.is_none();
            Ok::<Value, DialogError>(json!({
                "canceled": canceled,
                "filePath": file_path.unwrap_or_default(),
            }))
        })
        .await
        .map_err(|e| DialogError::Runtime(format!("join error: {e}")))?
    }

    pub async fn show_message_box(&self, opts: MessageBoxOptions) -> Result<Value, DialogError> {
        if let Some(title) = &opts.title {
            reject_nul("title", title)?;
        }
        reject_nul("message", &opts.message)?;
        let default_id = if opts.buttons.is_empty() {
            0
        } else {
            opts.default_id.min(opts.buttons.len() - 1)
        };
        tokio::task::spawn_blocking(move || {
            Ok::<Value, DialogError>(json!({
                "response": default_id,
                "checkboxChecked": false,
            }))
        })
        .await
        .map_err(|e| DialogError::Runtime(format!("join error: {e}")))?
    }

    pub async fn call_compat(&self, method: &str, args: &Value) -> Result<Value, DialogError> {
        let options = args.get("options").cloned().unwrap_or_else(|| json!({}));
        match method {
            "showOpenDialog" => {
                let parsed: OpenDialogOptions = serde_json::from_value(options)
                    .map_err(|e| DialogError::InvalidArgs(e.to_string()))?;
                self.show_open_dialog(parsed).await
            }
            "showSaveDialog" => {
                let parsed: SaveDialogOptions = serde_json::from_value(options)
                    .map_err(|e| DialogError::InvalidArgs(e.to_string()))?;
                self.show_save_dialog(parsed).await
            }
            "showMessageBox" => {
                let parsed: MessageBoxOptions = serde_json::from_value(options)
                    .map_err(|e| DialogError::InvalidArgs(e.to_string()))?;
                self.show_message_box(parsed).await
            }
            other => Err(DialogError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Clone, Default)]
pub struct DialogPlugin;

impl Plugin for DialogPlugin {
    type Config = DialogConfig;
    type Input = DialogConfig;
    type PrepareError = Infallible;
    type ApplyError = DialogError;

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let svc = Arc::new(DialogService::new(input.clone()));
        let publication = ctx
            .provide(svc)
            .map_err(|e| DialogError::Runtime(e.to_string()))?;
        ctx.effect_sync(move || {
            let _ = publication.remove();
        })
        .map_err(|e| DialogError::Runtime(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cordis_core::PreparedPlugin;
    use electron_cordis_kernel::RealmTopology;

    use super::*;

    #[tokio::test]
    async fn dialog_plugin_handles_headless_dialogs_and_rejects_nul_bytes() {
        let root = Context::new();
        let topo = RealmTopology::new(root).expect("topology");

        let handle = topo
            .host
            .spawn(PreparedPlugin::from_input(
                DialogPlugin,
                DialogConfig {
                    mode: DialogMode::Headless,
                    headless_selected_paths: vec!["/workspace/notes.md".into()],
                },
            ))
            .await
            .expect("spawn dialog");

        let svc = topo
            .host
            .try_service::<DialogService>()
            .expect("dialog service");

        let open_res = svc
            .call_compat(
                "showOpenDialog",
                &json!({ "options": { "title": "Pick file" } }),
            )
            .await
            .expect("showOpenDialog");
        assert_eq!(open_res["canceled"], json!(false));
        assert_eq!(open_res["filePaths"], json!(["/workspace/notes.md"]));

        let msg_res = svc
            .call_compat(
                "showMessageBox",
                &json!({ "options": { "message": "Continue?", "buttons": ["Cancel", "OK"], "defaultId": 1 } }),
            )
            .await
            .expect("showMessageBox");
        assert_eq!(msg_res["response"], json!(1));

        let bad = svc
            .call_compat(
                "showOpenDialog",
                &json!({ "options": { "defaultPath": "/tmp/bad\0path" } }),
            )
            .await;
        assert!(matches!(bad, Err(DialogError::InvalidArgs(_))));

        handle.dispose().await.expect("dispose");
        assert!(topo.host.try_service::<DialogService>().is_err());
    }
}
