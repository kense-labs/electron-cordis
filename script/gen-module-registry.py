#!/usr/bin/env python3
"""Generate Electron-Cordis C++ linked-binding gate header and TypeScript
browser module-list from a compiled build-plan.json.

Class-C redundant modules (BrowserView, TouchBar, inAppPurchase, pushNotifications,
contentTracing, netLog, ShareMenu, ServiceWorkerMain, sharedTexture) are excluded
entirely in Cordis mode. Class-A OS modules route either to legacy Electron bindings
(during P0-P1) or to `@dsh/electron-compat` (once offloaded to Rust DSH plugins).
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys

# Minimal Class-B web-surface browser modules required by Inkstone Desktop.
CLASS_B_CORE_MODULES: list[tuple[str, str]] = [
    ("app", "() => require('./app')"),
    ("BaseWindow", "() => require('./base-window')"),
    ("BrowserWindow", "() => require('./browser-window')"),
    ("ipcMain", "() => require('./ipc-main')"),
    ("MessageChannelMain", "() => process._linkedBinding('electron_browser_message_port').MessageChannelMain"),
    ("protocol", "() => require('./protocol')"),
    ("session", "() => require('./session')"),
    ("View", "() => process._linkedBinding('electron_browser_view').View"),
    ("webContents", "() => require('./web-contents')"),
    ("WebContentsView", "() => process._linkedBinding('electron_browser_web_contents_view').WebContentsView"),
    ("webFrameMain", "() => require('./web-frame-main')"),
]

OPTIONAL_CLASS_B: dict[str, list[tuple[str, str]]] = {
    "@dsh/electron-net": [
        ("net", "() => require('./net')"),
    ],
}

# Class-A OS capabilities: (module_name, legacy_plugin_id, rust_plugin_id, legacy_loader, compat_export)
CLASS_A_MODULES: list[tuple[str, str, str, str, str]] = [
    ("safeStorage", "@dsh/electron-legacy-safe-storage", "@dsh/cap-safe-storage", "() => require('./safe-storage')", "safeStorage"),
    ("clipboard", "@dsh/electron-legacy-clipboard", "@dsh/cap-clipboard", "() => require('./clipboard')", "clipboard"),
    ("dialog", "@dsh/electron-legacy-dialog", "@dsh/cap-dialog", "() => require('./dialog')", "dialog"),
    ("Menu", "@dsh/electron-legacy-tray-menu", "@dsh/cap-tray-menu", "() => require('./menu')", "Menu"),
    ("MenuItem", "@dsh/electron-legacy-tray-menu", "@dsh/cap-tray-menu", "() => require('./menu-item')", "MenuItem"),
    ("Tray", "@dsh/electron-legacy-tray-menu", "@dsh/cap-tray-menu", "() => require('./tray')", "Tray"),
    ("Notification", "@dsh/electron-legacy-notification", "@dsh/cap-notification", "() => process._linkedBinding('electron_browser_notification').Notification", "Notification"),
    ("nativeTheme", "@dsh/electron-legacy-os-integration", "@dsh/cap-os-integration", "() => require('./native-theme')", "nativeTheme"),
    ("screen", "@dsh/electron-legacy-os-integration", "@dsh/cap-os-integration", "() => require('./screen')", "screen"),
    ("globalShortcut", "@dsh/electron-legacy-os-integration", "@dsh/cap-os-integration", "() => require('./global-shortcut')", "globalShortcut"),
    ("powerMonitor", "@dsh/electron-legacy-os-integration", "@dsh/cap-os-integration", "() => require('./power-monitor')", "powerMonitor"),
    ("powerSaveBlocker", "@dsh/electron-legacy-os-integration", "@dsh/cap-os-integration", "() => require('./power-save-blocker')", "powerSaveBlocker"),
    ("systemPreferences", "@dsh/electron-legacy-os-integration", "@dsh/cap-os-integration", "() => require('./system-preferences')", "systemPreferences"),
    ("autoUpdater", "@dsh/electron-legacy-updater", "@dsh/cap-updater", "() => require('./auto-updater')", "autoUpdater"),
]

# Explicitly pruned Class-C modules (emitted as 0 in generated C++ header).
PRUNED_CLASS_C: list[str] = [
    "BROWSER_VIEW",
    "TOUCH_BAR",
    "IN_APP_PURCHASE",
    "PUSH_NOTIFICATIONS",
    "CONTENT_TRACING",
    "NET_LOG",
    "SHARE_MENU",
    "SERVICE_WORKER_MAIN",
    "SHARED_TEXTURE",
]


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate Cordis Electron module registry from build-plan.json.")
    parser.add_argument("build_plan", type=pathlib.Path, help="Path to build-plan.json")
    parser.add_argument("--out-dir", type=pathlib.Path, required=True, help="Output directory")
    args = parser.parse_args()

    plan = json.loads(args.build_plan.read_text(encoding="utf-8"))
    if not plan.get("build_chromium"):
        print("[gen-module-registry] Profile skips Chromium; no Electron module registry generated.")
        sys.exit(0)

    bundled = set(plan.get("bundled_plugins", []))
    gn_plugins = set(plan.get("gn_args", {}).get("electron_cordis_plugins", []))
    has_compat = "@dsh/electron-compat" in bundled

    ts_entries: list[tuple[str, str]] = list(CLASS_B_CORE_MODULES)
    for pid, mods in OPTIONAL_CLASS_B.items():
        if pid in gn_plugins:
            ts_entries.extend(mods)

    cpp_flags: dict[str, int] = {f"CLASS_C_{name}": 0 for name in PRUNED_CLASS_C}
    cpp_flags["ELECTRON_NET"] = 1 if "@dsh/electron-net" in gn_plugins else 0

    for mod_name, legacy_pid, rust_pid, legacy_loader, compat_export in CLASS_A_MODULES:
        macro = mod_name.upper()
        if legacy_pid in gn_plugins:
            ts_entries.append((mod_name, legacy_loader))
            cpp_flags[f"LEGACY_{macro}"] = 1
        elif rust_pid in bundled and has_compat:
            ts_entries.append((mod_name, f"() => require('@dsh/electron-compat').{compat_export}"))
            cpp_flags[f"LEGACY_{macro}"] = 0
        else:
            cpp_flags[f"LEGACY_{macro}"] = 0

    ts_entries.sort(key=lambda pair: pair[0].lower())
    args.out_dir.mkdir(parents=True, exist_ok=True)

    ts_lines = [
        "// Auto-generated by script/gen-module-registry.py — DO NOT EDIT.",
        f"// Profile: {plan['profile']} (digest: {plan['compatibility_digest'][:12]})",
        "",
        "export const browserModuleList: ElectronInternal.ModuleEntry[] = [",
    ]
    for name, loader in ts_entries:
        ts_lines.append(f"  {{ name: '{name}', loader: {loader} }},")
    ts_lines.append("];\n")
    (args.out_dir / "cordis_module_list.ts").write_text("\n".join(ts_lines), encoding="utf-8")

    h_lines = [
        "// Auto-generated by script/gen-module-registry.py — DO NOT EDIT.",
        "#ifndef ELECTRON_CORDIS_LINKED_BINDINGS_H_",
        "#define ELECTRON_CORDIS_LINKED_BINDINGS_H_",
        "",
    ]
    for flag, val in sorted(cpp_flags.items()):
        h_lines.append(f"#define ELECTRON_CORDIS_ENABLE_{flag} {val}")
    h_lines.extend(["", "#endif  // ELECTRON_CORDIS_LINKED_BINDINGS_H_", ""])
    (args.out_dir / "cordis_linked_bindings.h").write_text("\n".join(h_lines), encoding="utf-8")

    print(
        f"[gen-module-registry] profile={plan['profile']} "
        f"modules={len(ts_entries)} pruned_class_c={len(PRUNED_CLASS_C)}"
    )


if __name__ == "__main__":
    main()
