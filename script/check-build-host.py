#!/usr/bin/env python3
"""Preflight build-host capability checker for Electron-Cordis build tiers.

Tiers:
- headless:      Tier 1 (Rust microkernel + JS bridge + zero-Chromium headless dist)
- canary-pack:   Tier 2 (Isolated side-by-side desktop candidate via Prebuilt Base)
- chromium-base: Tier 3 (Full Chromium/Electron C++ source build for Prebuilt Base)

Exit codes:
- 0: All required checks for the requested tier passed.
- 2: One or more hard requirements are missing on this host.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
from typing import Any


MIN_DISK_GIB = {
    "headless": 5.0,
    "canary-pack": 15.0,
    "chromium-base": 200.0,
}

MIN_RAM_GIB = {
    "headless": 4.0,
    "canary-pack": 8.0,
    "chromium-base": 16.0,
}

RECOMMENDED_RAM_GIB = {
    "headless": 8.0,
    "canary-pack": 16.0,
    "chromium-base": 32.0,
}


def detect_ram_gib() -> float | None:
    system = platform.system()
    try:
        if system == "Darwin":
            out = subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True).strip()
            return int(out) / (1024**3)
        if system == "Linux":
            with open("/proc/meminfo", encoding="utf-8") as fh:
                for line in fh:
                    if line.startswith("MemTotal:"):
                        kb = int(line.split()[1])
                        return kb / (1024**2)
    except Exception:
        return None
    return None


def detect_free_disk_gib(target: pathlib.Path) -> float:
    probe = target
    while not probe.exists() and probe.parent != probe:
        probe = probe.parent
    usage = shutil.disk_usage(probe)
    return usage.free / (1024**3)


def check_cmd(name: str) -> str | None:
    return shutil.which(name)


def check_full_xcode() -> tuple[bool, str]:
    try:
        dev_dir = subprocess.check_output(["xcode-select", "-p"], text=True, stderr=subprocess.STDOUT).strip()
    except Exception as exc:
        return False, f"xcode-select failed: {exc}"
    if "CommandLineTools" in dev_dir:
        return False, f"active developer dir is `{dev_dir}` (full /Applications/Xcode.app required)"
    try:
        ver = subprocess.check_output(["xcodebuild", "-version"], text=True, stderr=subprocess.STDOUT).strip()
        first_line = ver.splitlines()[0] if ver else "Xcode ok"
        return True, f"{first_line} ({dev_dir})"
    except Exception as exc:
        return False, f"xcodebuild unavailable: {exc}"


def evaluate(tier: str, workspace_dir: pathlib.Path) -> dict[str, Any]:
    checks: list[dict[str, Any]] = []

    def add(name: str, ok: bool, detail: str, required: bool = True, fix: str | None = None) -> None:
        checks.append(
            {
                "name": name,
                "ok": ok,
                "required": required,
                "detail": detail,
                "fix": fix,
            }
        )

    # 1. Disk space
    free_gib = detect_free_disk_gib(workspace_dir)
    min_disk = MIN_DISK_GIB[tier]
    add(
        "disk_free_gib",
        free_gib >= min_disk,
        f"{free_gib:.1f} GiB free at {workspace_dir} (required >= {min_disk:.0f} GiB)",
        fix=f"Free space or pass --workspace-dir on a volume with >= {min_disk:.0f} GiB free",
    )

    # 2. RAM
    ram_gib = detect_ram_gib()
    min_ram = MIN_RAM_GIB[tier]
    rec_ram = RECOMMENDED_RAM_GIB[tier]
    if ram_gib is not None:
        add(
            "ram_gib",
            ram_gib >= min_ram,
            f"{ram_gib:.1f} GiB RAM (min >= {min_ram:.0f} GiB, recommended >= {rec_ram:.0f} GiB)",
            fix=f"Use a build host with >= {rec_ram:.0f} GiB RAM",
        )

    # 3. Tier 1 core tools (required for all tiers)
    for tool in ("cargo", "rustc", "node", "python3", "git"):
        found = check_cmd(tool)
        add(
            f"tool:{tool}",
            found is not None,
            found or "MISSING",
            fix=f"Install `{tool}` and ensure it is on PATH",
        )

    # 4. Tier 2 canary pack tools
    if tier in ("canary-pack", "chromium-base"):
        pnpm = check_cmd("pnpm")
        add("tool:pnpm", pnpm is not None, pnpm or "MISSING", fix="Install pnpm (corepack enable)")

    if tier == "canary-pack":
        dist = os.environ.get("ELECTRON_CORDIS_DIST", "").strip()
        allow_dryrun = os.environ.get("ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN") == "1"
        ok = (bool(dist) and pathlib.Path(dist).exists()) or allow_dryrun
        add(
            "env:ELECTRON_CORDIS_DIST",
            ok,
            dist if dist else ("dry-run allowed" if allow_dryrun else "unset"),
            required=False,
            fix="Set ELECTRON_CORDIS_DIST=/path/to/prebuilt-electron-cordis (or ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN=1 for config dry-run)",
        )

    # 5. Tier 3 full Chromium/Electron C++ source build requirements
    if tier == "chromium-base":
        if platform.system() == "Darwin":
            xcode_ok, xcode_detail = check_full_xcode()
            add(
                "macos:full_xcode",
                xcode_ok,
                xcode_detail,
                fix="Install /Applications/Xcode.app and run: sudo xcode-select -s /Applications/Xcode.app/Contents/Developer && sudo xcodebuild -license accept",
            )

        has_e = check_cmd("e")
        has_gclient = check_cmd("gclient")
        has_gn = check_cmd("gn")
        has_ninja = check_cmd("ninja")
        add(
            "chromium:depot_tools_or_build_tools",
            bool(has_e or (has_gclient and has_gn and has_ninja)),
            f"e={has_e or 'MISSING'}, gclient={has_gclient or 'MISSING'}, gn={has_gn or 'MISSING'}, ninja={has_ninja or 'MISSING'}",
            fix="Install depot_tools (gclient/gn/ninja) or @electron/build-tools (`e`)",
        )

    failed_required = [c for c in checks if c["required"] and not c["ok"]]
    return {
        "tier": tier,
        "host": {
            "os": platform.system(),
            "arch": platform.machine(),
            "cpus": os.cpu_count(),
            "workspace_dir": str(workspace_dir),
        },
        "ready": len(failed_required) == 0,
        "failed_count": len(failed_required),
        "checks": checks,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Check local host readiness for an Electron-Cordis build tier.")
    parser.add_argument(
        "--tier",
        choices=("headless", "canary-pack", "chromium-base"),
        default="chromium-base",
        help="Target build tier to validate",
    )
    parser.add_argument(
        "--workspace-dir",
        type=pathlib.Path,
        default=pathlib.Path.cwd(),
        help="Directory/volume where build artifacts will live",
    )
    parser.add_argument("--json", action="store_true", help="Emit JSON report")
    args = parser.parse_args()

    report = evaluate(args.tier, args.workspace_dir.resolve())
    if args.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        status = "READY" if report["ready"] else "NOT_READY"
        h = report["host"]
        print(f"[check-build-host] tier={report['tier']} status={status} host={h['os']}/{h['arch']} cpus={h['cpus']}")
        for c in report["checks"]:
            tag = "PASS" if c["ok"] else ("FAIL" if c["required"] else "WARN")
            print(f"  [{tag}] {c['name']}: {c['detail']}")
            if not c["ok"] and c["fix"]:
                print(f"         -> {c['fix']}")

    sys.exit(0 if report["ready"] else 2)


if __name__ == "__main__":
    main()
