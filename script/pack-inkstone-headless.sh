#!/usr/bin/env bash
# M5 Zero-Chromium `inkstone-headless` release distribution packager & smoke verifier.
# Outputs exclusively into `electron-cordis/dist/cordis-headless/` (zero intrusion into ../web).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
CORDIS_DIR="${ROOT_DIR}/cordis"
OUT_DIR="${1:-${ROOT_DIR}/dist/cordis-headless}"

echo "[pack-inkstone-headless] 1/4 Building release Rust microkernel daemon & Stage B supervisor..."
(
  cd "${CORDIS_DIR}"
  cargo build --release -p electron-cordis-daemon -p electron-cordis-supervisor
)

echo "[pack-inkstone-headless] 2/4 Compiling frozen headless LoadPlan..."
rm -rf "${OUT_DIR}"
mkdir -p "${OUT_DIR}/bin" "${OUT_DIR}/config" "${OUT_DIR}/js-packages"

python3 "${ROOT_DIR}/script/gen-cordis-build.py" \
  "${ROOT_DIR}/profiles/inkstone-headless.cordis.json" \
  --out-dir "${OUT_DIR}/config"

# Hard gate: headless profile MUST NOT require Chromium
python3 - "${OUT_DIR}/config/build-plan.json" <<'PY'
import json, pathlib, sys
plan = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
assert plan["build_chromium"] is False, "HEADLESS FAIL-CLOSED: build_chromium must be false"
assert not (pathlib.Path(sys.argv[1]).parent / "cordis.args.gn").exists(), "cordis.args.gn must not exist"
print("[pack-inkstone-headless] Verified build_chromium=false (zero Chromium).")
PY

echo "[pack-inkstone-headless] 3/4 Staging zero-Chromium distribution into ${OUT_DIR}..."
cp "${CORDIS_DIR}/target/release/electron-cordis-daemon" "${OUT_DIR}/bin/electron-cordis-daemon"
cp "${CORDIS_DIR}/target/release/electron-cordis-supervisor" "${OUT_DIR}/bin/electron-cordis-supervisor"
cp -R "${ROOT_DIR}/js-packages/dsh-cordis-bridge" "${OUT_DIR}/js-packages/"
cp -R "${ROOT_DIR}/js-packages/electron-compat-shim" "${OUT_DIR}/js-packages/"
rm -rf "${OUT_DIR}/js-packages/dsh-cordis-bridge/test" "${OUT_DIR}/js-packages/electron-compat-shim/test"

# Generate SHA-256 manifest + assert zero Chromium/Electron binaries exist
python3 - "${OUT_DIR}" <<'PY'
import hashlib, json, pathlib, sys

out = pathlib.Path(sys.argv[1])
files = {}
total_bytes = 0
for p in sorted(out.rglob("*")):
    if p.is_file() and p.name != "MANIFEST.json":
        rel = p.relative_to(out).as_posix()
        lower = rel.lower()
        assert "chromium" not in lower and "electron.app" not in lower and "libffmpeg" not in lower, (
            f"unexpected Chromium/Electron binary in headless bundle: {rel}"
        )
        data = p.read_bytes()
        total_bytes += len(data)
        files[rel] = {
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }

manifest = {
    "schema": "electron-cordis.headless-dist.v1",
    "profile": "inkstone-headless",
    "zero_chromium": True,
    "total_bytes": total_bytes,
    "total_mib": round(total_bytes / (1024 * 1024), 2),
    "files": files,
}
(out / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
print(f"[pack-inkstone-headless] Packaged {len(files)} files, total={manifest['total_mib']} MiB ({total_bytes} bytes), zero_chromium=True")
PY

echo "[pack-inkstone-headless] 4/4 Running live smoke test via packaged Stage B supervisor (--headless)..."
REPORT="$("${OUT_DIR}/bin/electron-cordis-supervisor" \
  --daemon-bin "${OUT_DIR}/bin/electron-cordis-daemon" \
  --load-plan "${OUT_DIR}/config/load-plan.json" \
  --headless)"

python3 - "${REPORT}" <<'PY'
import json, sys
r = json.loads(sys.argv[1])
assert r["headless"] is True, r
assert r["ui_launches"] == 0, r
assert r["clean_shutdown"] is True, r
assert r["health_after_ui_restart"]["profile"] == "inkstone-headless", r
print(
    f"[pack-inkstone-headless] SMOKE PASS: profile={r['health_after_ui_restart']['profile']} "
    f"daemon_pid={r['daemon_pid']} ui_launches={r['ui_launches']} clean_shutdown={r['clean_shutdown']}"
)
PY
