#!/usr/bin/env python3
"""Parity and safety gate guard for Inkstone <-> Electron-Cordis parallel development.

Verifies four invariants:
1. Preload surface in ../web/src/preload/index.ts exposes ONLY the 4 frozen allowlisted keys.
2. Renderer code in ../web/src/renderer/ contains ZERO KRun / ServiceRealm / Cordis-control leaks.
3. cordis/Cargo.toml has ZERO external crates.io cordis-* dependencies (source-internalized rule).
4. Profiles contain ZERO legacy Go Core references.
"""

from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
INKSTONE_ROOT = ROOT.parent

EXPECTED_PRELOAD_KEYS = {
    "desktopTransport",
    "desktopFiles",
    "observability",
    "__inkstoneDiagnosticsLog",
}

FORBIDDEN_RENDERER_PATTERNS = [
    r"\bKRUN_",
    r"krun\.control",
    r"ServiceRealm",
    r"cordis\.control",
    r"host-tools\.sock",
    r"\bvsock\b",
]


def fail(errors: list[str]) -> None:
    for e in errors:
        print(f"[check-inkstone-parity] FAIL: {e}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    errors: list[str] = []

    # 1. Preload allowlist check
    preload = INKSTONE_ROOT / "web" / "src" / "preload" / "index.ts"
    if preload.is_file():
        text = preload.read_text(encoding="utf-8")
        exposed = set(re.findall(r"exposeInMainWorld\(\s*['\"]([^'\"]+)['\"]", text))
        if exposed != EXPECTED_PRELOAD_KEYS:
            errors.append(
                f"preload exposeInMainWorld mismatch: got {sorted(exposed)}, expected {sorted(EXPECTED_PRELOAD_KEYS)}"
            )
    else:
        errors.append(f"missing sibling preload file: {preload}")

    # 2. Renderer zero-leak scan
    renderer_dir = INKSTONE_ROOT / "web" / "src" / "renderer"
    if renderer_dir.is_dir():
        compiled = [re.compile(pat) for pat in FORBIDDEN_RENDERER_PATTERNS]
        for path in renderer_dir.rglob("*"):
            if path.suffix not in {".ts", ".tsx", ".js", ".jsx"} or not path.is_file():
                continue
            content = path.read_text(encoding="utf-8", errors="ignore")
            for pat in compiled:
                if pat.search(content):
                    errors.append(f"renderer leak ({pat.pattern}) in {path.relative_to(INKSTONE_ROOT)}")

    # 3. Internalized cordis-rs check (no external crates.io cordis-* version specs)
    cargo_toml = ROOT / "cordis" / "Cargo.toml"
    cargo_text = cargo_toml.read_text(encoding="utf-8")
    for dep in ("cordis-core", "cordis-loader", "cordis-timer", "cordis-rs"):
        if re.search(rf'(?m)^{dep}\s*=\s*"', cargo_text):
            errors.append(f"{cargo_toml.name} must internalize `{dep}` via local path, not external crates.io version")

    # 4. Zero Go Core references in profiles
    for prof in (ROOT / "profiles").glob("*.cordis.json"):
        ptxt = prof.read_text(encoding="utf-8")
        if "orbit-go" in ptxt or "orbit_go" in ptxt or "core-go" in ptxt:
            errors.append(f"legacy Go core reference found in {prof.name}")

    if errors:
        fail(errors)

    print("[check-inkstone-parity] PASS: preload allowlist, renderer isolation, internalized cordis-rs, and zero-Go profiles verified.")


if __name__ == "__main__":
    main()
