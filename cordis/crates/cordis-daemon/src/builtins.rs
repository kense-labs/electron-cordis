//! Registry of the native plugins that the daemon can load into Cordis realms.

use std::io;

use cordis_core::{InjectSpec, PreparedPlugin};
use cordis_loader::resolver::PluginRequest;
use electron_cordis_kernel::{
    FiberRoster, LoadPlanEntry, LoadPlanManifest, PluginDelivery, RealmTopology, ToolCatalog,
    LOAD_PLAN_SCHEMA,
};
use serde_json::Value;

type Prepare = fn(&PluginRequest<'_>, &ToolCatalog) -> io::Result<PreparedPlugin>;

struct Builtin {
    id: &'static str,
    realm: &'static str,
    feature: &'static str,
    prepare: Option<Prepare>,
}

macro_rules! builtin {
    ($id:literal, $realm:literal, $feature:literal, |$tools:ident| $plugin:expr) => {
        Builtin {
            id: $id,
            realm: $realm,
            feature: $feature,
            prepare: {
                #[cfg(feature = $feature)]
                {
                    Some(|request, $tools| prepare_plugin($plugin, request.config()))
                }
                #[cfg(not(feature = $feature))]
                {
                    None
                }
            },
        }
    };
}

const BUILTINS: &[Builtin] = &[
    builtin!("@dsh/runtime-node-host", "core", "runtime-node-host", |_tools| {
        electron_cordis_cap_platform::RuntimeNodeHostPlugin
    }),
    builtin!("@dsh/cap-safe-storage", "host", "cap-safe-storage", |_tools| {
        electron_cordis_cap_safe_storage::SafeStoragePlugin
    }),
    builtin!("@dsh/cap-clipboard", "host", "cap-clipboard", |tools| {
        electron_cordis_cap_clipboard::ClipboardPlugin::new(Some(tools.clone()))
    }),
    builtin!("@dsh/cap-dialog", "host", "cap-dialog", |_tools| {
        electron_cordis_cap_dialog::DialogPlugin
    }),
    builtin!("@dsh/cap-os-shell", "host", "cap-os-shell", |_tools| {
        electron_cordis_cap_os_shell::OsShellPlugin
    }),
    builtin!("@dsh/cap-tray-menu", "host", "cap-tray-menu", |_tools| {
        electron_cordis_cap_desktop_ui::TrayMenuPlugin
    }),
    builtin!("@dsh/cap-notification", "host", "cap-notification", |_tools| {
        electron_cordis_cap_desktop_ui::NotificationPlugin
    }),
    builtin!("@dsh/cap-os-integration", "host", "cap-os-integration", |_tools| {
        electron_cordis_cap_desktop_ui::OsIntegrationPlugin
    }),
    builtin!("@dsh/cap-desktop-capture", "host", "cap-desktop-capture", |_tools| {
        electron_cordis_cap_platform::DesktopCapturePlugin
    }),
    builtin!("@dsh/cap-updater", "host", "cap-updater", |_tools| {
        electron_cordis_cap_platform::UpdaterPlugin
    }),
    builtin!("@dsh/plugin-installer", "host", "plugin-installer", |_tools| {
        electron_cordis_cap_platform::PluginInstallerPlugin
    }),
];

#[cfg(any(
    feature = "runtime-node-host",
    feature = "cap-safe-storage",
    feature = "cap-clipboard",
    feature = "cap-dialog",
    feature = "cap-os-shell",
    feature = "cap-tray-menu",
    feature = "cap-notification",
    feature = "cap-os-integration",
    feature = "cap-desktop-capture",
    feature = "cap-updater",
    feature = "plugin-installer",
))]
fn prepare_plugin<P>(plugin: P, config: &Value) -> io::Result<PreparedPlugin>
where
    P: cordis_core::Plugin,
    P::Config: Default + serde::Serialize + serde::de::DeserializeOwned,
{
    let mut config_with_defaults = serde_json::to_value(P::Config::default())?;
    if !config.is_null() {
        let fields = config
            .as_object()
            .ok_or_else(|| io::Error::other("builtin plugin config must be an object"))?;
        config_with_defaults
            .as_object_mut()
            .expect("builtin plugin config serializes as an object")
            .extend(fields.clone());
    }
    cordis_loader::resolver::prepare_plugin_json(plugin, &config_with_defaults)
        .map_err(|error| io::Error::other(error.to_string()))
}

pub(super) async fn load(
    topology: &RealmTopology,
    tools: &ToolCatalog,
    plan: Option<&LoadPlanManifest>,
) -> Result<FiberRoster, String> {
    let mut builtins_plan = LoadPlanManifest {
        schema: LOAD_PLAN_SCHEMA.into(),
        profile: plan.map_or("unconfigured", |p| p.profile.as_str()).into(),
        entries: Vec::new(),
    };
    if let Some(plan) = plan {
        for entry in &plan.entries {
            if entry.id == "@dsh/krun-runtime" || entry.inject.iter().any(|s| s == "krun.runtime") {
                return Err("load plan requires an embedded KRun runtime, which is unavailable; regenerate it with a standalone profile".into());
            }
            let Some(builtin) = BUILTINS.iter().find(|b| b.id == entry.id) else {
                continue;
            };
            if entry.delivery != PluginDelivery::Bundled {
                continue;
            }
            if entry.runtime != "rust"
                || !matches!(entry.isolation.as_str(), "in-process" | "trusted-host")
                || (entry.realm != builtin.realm && entry.realm != "default")
            {
                return Err(format!(
                    "builtin `{}` requires a trusted local process in the {} realm with the Rust runtime",
                    entry.id, builtin.realm
                ));
            }
            if builtin.prepare.is_none() {
                return Err(format!(
                    "builtin `{}` was requested but Cargo feature `{}` is unavailable",
                    entry.id, builtin.feature
                ));
            }
            let mut entry = entry.clone();
            entry.realm = builtin.realm.into();
            builtins_plan.entries.push(entry);
        }
    } else {
        builtins_plan.entries.extend(BUILTINS.iter().filter(|b| b.prepare.is_some()).map(|b| {
            LoadPlanEntry {
                id: b.id.into(),
                runtime: "rust".into(),
                isolation: "trusted-host".into(),
                delivery: PluginDelivery::Bundled,
                realm: b.realm.into(),
                inject: Vec::new(),
                config: Value::Null,
            }
        }));
    }

    let resolver = |request: PluginRequest<'_>| -> io::Result<Option<PreparedPlugin>> {
        let Some(prepare) =
            BUILTINS.iter().find(|b| b.id == request.resolve_key()).and_then(|b| b.prepare)
        else {
            return Ok(None);
        };
        let mut inject = InjectSpec::none();
        for entry in request.inject() {
            match entry {
                cordis_loader::plan::InjectEntry::Required(service) => {
                    inject = inject.require(service.clone());
                }
                _ => return Err(io::Error::other("configured inject requires a typed adapter")),
            }
        }
        Ok(Some(prepare(&request, tools)?.with_inject_overlay(inject)))
    };

    let mut roster = FiberRoster::default();
    for realm in ["core", "host"] {
        match topology.load_realm_with_loader(&builtins_plan, realm, None, &resolver).await {
            Ok(entries) => {
                for (id, handle) in entries {
                    roster.record(id, handle);
                }
            }
            Err(error) => {
                let cleanup_errors = roster.drain_reverse().await;
                return Err(if cleanup_errors.is_empty() {
                    error.to_string()
                } else {
                    format!("{error}; rollback failed: {cleanup_errors:?}")
                });
            }
        }
    }
    Ok(roster)
}
