#!/usr/bin/env python3
"""Generate GN args, Cargo feature flags, and frozen Cordis LoadPlan from a
declarative Electron-Cordis profile (*.cordis.json).

Fail-closed by design:
- Unknown plugin IDs or illegal isolation declarations abort immediately.
- Chromium PDF Viewer dependency invariant (pdf_viewer => pdf && extensions) is
  resolved explicitly rather than silently ignored.
- Headless profiles emit `build_chromium = false` so CI skips Electron builds.

Desktop profiles use upstream Electron build settings by default.
Pass --electron-lite to generate the optional reduced build settings.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys
from typing import Any

KNOWN_PLUGINS: dict[str, dict[str, Any]] = {
    # Wire projections and application integrations; these are not daemon plugin crates.
    "@dsh/bridge-mcp": {"class": "external"},
    "@dsh/bridge-inkstone-tools": {"class": "external"},
    "@dsh/bridge-acp": {"class": "external"},
    "@dsh/llm-gateway": {"class": "external"},
    "@dsh/inkstone-harness": {"class": "external"},
    "@dsh/core-dsh-ts": {"class": "external"},
    "@dsh/runtime-node-host": {"class": "rust", "cargo_feature": "runtime-node-host"},
    # Class-A OS capabilities (Rust DSH plugins, Chromium-free)
    "@dsh/cap-safe-storage": {"class": "rust", "cargo_feature": "cap-safe-storage"},
    "@dsh/cap-clipboard": {"class": "rust", "cargo_feature": "cap-clipboard"},
    "@dsh/cap-dialog": {"class": "rust", "cargo_feature": "cap-dialog"},
    "@dsh/cap-tray-menu": {"class": "rust", "cargo_feature": "cap-tray-menu"},
    "@dsh/cap-notification": {"class": "rust", "cargo_feature": "cap-notification"},
    "@dsh/cap-desktop-capture": {"class": "rust", "cargo_feature": "cap-desktop-capture"},
    "@dsh/cap-os-integration": {"class": "rust", "cargo_feature": "cap-os-integration"},
    "@dsh/cap-os-shell": {"class": "rust", "cargo_feature": "cap-os-shell"},
    "@dsh/cap-updater": {"class": "rust", "cargo_feature": "cap-updater"},
    "@dsh/plugin-installer": {"class": "rust", "cargo_feature": "plugin-installer"},
    "@dsh/store-sqlite": {"class": "external"},
    # Heavy optional domain plugins (default delivery = "on-demand" for slim installers)
    "@dsh/plugin-science-pack": {"class": "pack", "cargo_feature": None},
    "@dsh/plugin-feishu": {"class": "pack", "cargo_feature": None},
    "@dsh/plugin-ssh-remote": {"class": "pack", "cargo_feature": None},
    # Class-B Electron Chromium surface & optional GN subsystems
    "@dsh/electron-web-surface": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-pdf-viewer": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-net": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-printing": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-extensions": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-spellcheck": {"class": "electron", "gn_plugin": True},
    # Optional Electron APIs with independent Lite build flags.
    "@dsh/electron-browser-view": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-touch-bar": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-in-app-purchase": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-push-notifications": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-content-tracing": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-net-log": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-share-menu": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-service-worker-main": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-shared-texture": {"class": "electron", "gn_plugin": True},
    # Transitional P0-P2 compat bindings & JS shim
    "@dsh/electron-compat": {"class": "js", "cargo_feature": None},
    "@dsh/electron-legacy-safe-storage": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-clipboard": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-dialog": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-tray-menu": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-notification": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-os-integration": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-shell": {"class": "electron", "gn_plugin": True},
    "@dsh/electron-legacy-updater": {"class": "electron", "gn_plugin": True},
}

VALID_ISOLATIONS = {
    "in-process",
    "trusted-host",
    "electron-utility",
    "os-sandbox",
    "wasm",
    "microvm",
}

VALID_DELIVERIES = {"bundled", "on-demand"}


def fail(msg: str) -> None:
    print(f"[gen-cordis-build] FAIL-CLOSED: {msg}", file=sys.stderr)
    sys.exit(2)


def resolve_profile(profile_path: pathlib.Path, *, electron_lite: bool = False) -> dict[str, Any]:
    raw = profile_path.read_text(encoding="utf-8")
    data = json.loads(raw)

    if data.get("schema") != "electron-cordis.profile.v1":
        fail(f"unsupported schema: {data.get('schema')!r}")

    name = data.get("name", "").strip()
    if not name:
        fail("profile name is required")

    plugins = data.get("plugins")
    if not isinstance(plugins, list) or not plugins:
        fail("profile must declare a non-empty plugins array")

    electron_plugins: list[str] = []
    cargo_features: list[str] = []
    bundled_plugins: list[str] = []
    on_demand_plugins: list[str] = []
    entries: list[dict[str, Any]] = []
    seen_ids: set[str] = set()

    for idx, item in enumerate(plugins):
        if not isinstance(item, dict):
            fail(f"plugins[{idx}] must be an object")
        pid = item.get("id", "")
        if not isinstance(pid, str) or not pid:
            fail(f"plugins[{idx}].id is required")
        if pid in seen_ids:
            fail(f"duplicate plugin id: {pid}")
        seen_ids.add(pid)

        isolation = item.get("isolation", "in-process")
        if isolation not in VALID_ISOLATIONS:
            fail(f"plugin {pid} has illegal isolation {isolation!r}")

        delivery = item.get("delivery", "bundled")
        if delivery not in VALID_DELIVERIES:
            fail(f"plugin {pid} has illegal delivery {delivery!r}")

        runtime = item.get("runtime", "rust")
        meta = KNOWN_PLUGINS.get(pid)
        if meta is None:
            # Allow external @deepseek-ai/dsh-* or community node-dsh/wasm plugins
            if runtime not in ("node-dsh", "wasm", "acp-proc"):
                fail(f"unknown builtin plugin id {pid!r} for runtime {runtime!r}")
        else:
            if meta.get("cargo_feature"):
                if runtime != "rust" or isolation not in {"in-process", "trusted-host"}:
                    fail(f"builtin plugin {pid} requires the Rust runtime and trusted local isolation")
                realm = "core" if pid == "@dsh/runtime-node-host" else "host"
                if item.get("realm", "default") not in {"default", realm}:
                    fail(f"builtin plugin {pid} requires the {realm} realm")
            if delivery == "bundled":
                if meta.get("gn_plugin"):
                    electron_plugins.append(pid)
                cf = meta.get("cargo_feature")
                if cf and cf not in cargo_features:
                    cargo_features.append(cf)

        if delivery == "bundled":
            bundled_plugins.append(pid)
        else:
            on_demand_plugins.append(pid)

        entries.append(
            {
                "id": pid,
                "runtime": runtime,
                "isolation": isolation,
                "delivery": delivery,
                "realm": item.get("realm", "default"),
                "inject": item.get("inject", []),
                "config": item.get("config", {}),
            }
        )

    has_web_surface = "@dsh/electron-web-surface" in electron_plugins
    if electron_plugins and not has_web_surface:
        fail("Electron GN plugins require @dsh/electron-web-surface")

    use_upstream_electron = has_web_surface and not electron_lite
    gn_args: dict[str, Any] = {
        "electron_cordis_mode": not use_upstream_electron,
        "electron_cordis_profile": name,
        "electron_cordis_plugins": sorted(electron_plugins),
    }
    if not use_upstream_electron:
        has_pdf = "@dsh/electron-pdf-viewer" in electron_plugins
        # Chromium WebUI and test targets require the stock extensions and printing flags.
        # The Electron PDF viewer also needs the Electron extensions integration.
        effective_extensions = has_pdf or "@dsh/electron-extensions" in electron_plugins
        gn_args.update({
            "enable_pdf_viewer": has_pdf,
            "enable_electron_extensions": effective_extensions,
            "enable_builtin_spellchecker": "@dsh/electron-spellcheck" in electron_plugins,
            "enable_prompt_api": False,
        })

    digest = hashlib.sha256(
        json.dumps(
            {"name": name, "gn_args": gn_args, "cargo_features": sorted(cargo_features), "entries": entries},
            sort_keys=True,
        ).encode("utf-8")
    ).hexdigest()

    return {
        "schema": "electron-cordis.build-plan.v1",
        "profile": name,
        "build_chromium": has_web_surface,
        "compatibility_digest": digest,
        "cargo_features": sorted(cargo_features),
        "bundled_plugins": sorted(bundled_plugins),
        "on_demand_plugins": sorted(on_demand_plugins),
        "gn_args": gn_args,
        "load_plan": {
            "schema": "cordis.load-plan.v1",
            "profile": name,
            "entries": entries,
        },
    }


def render_gn(gn_args: dict[str, Any]) -> str:
    lines = [
        "# Auto-generated by script/gen-cordis-build.py — DO NOT EDIT.",
        "",
    ]
    for key, val in gn_args.items():
        if isinstance(val, bool):
            lines.append(f"{key} = {'true' if val else 'false'}")
        elif isinstance(val, str):
            lines.append(f'{key} = "{val}"')
        elif isinstance(val, list):
            rendered = ", ".join(f'"{v}"' for v in val)
            lines.append(f"{key} = [ {rendered} ]")
        else:
            fail(f"unsupported GN arg type for {key}: {type(val).__name__}")
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description="Compile an Electron-Cordis profile into GN/Cargo/LoadPlan artifacts.")
    parser.add_argument("profile", type=pathlib.Path, help="Path to *.cordis.json profile")
    parser.add_argument("--out-dir", type=pathlib.Path, required=True, help="Output directory")
    parser.add_argument("--electron-lite", action="store_true", help="Enable module pruning for desktop builds")
    args = parser.parse_args()

    plan = resolve_profile(args.profile, electron_lite=args.electron_lite)
    args.out_dir.mkdir(parents=True, exist_ok=True)

    (args.out_dir / "build-plan.json").write_text(
        json.dumps(plan, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    (args.out_dir / "load-plan.json").write_text(
        json.dumps(plan["load_plan"], indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    if plan["build_chromium"]:
        (args.out_dir / "cordis.args.gn").write_text(render_gn(plan["gn_args"]), encoding="utf-8")
    else:
        (args.out_dir / "cordis.args.gn").unlink(missing_ok=True)

    print(
        f"[gen-cordis-build] profile={plan['profile']} "
        f"build_chromium={plan['build_chromium']} "
        f"cargo_features={','.join(plan['cargo_features'])} "
        f"digest={plan['compatibility_digest'][:12]}"
    )


if __name__ == "__main__":
    main()
