#!/usr/bin/env bash
# M5 Zero-Chromium `inkstone-headless` release distribution packager & smoke verifier.
# Installs the verified package into dist/cordis-headless by default.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT_DIR="${1:-${ROOT_DIR}/dist/cordis-headless}"
OUT_DIR="$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "${OUT_DIR}")"
OUT_PARENT="$(dirname "${OUT_DIR}")"
if [[ "${OUT_DIR}" == "${OUT_PARENT}" ]]; then
  printf 'Provide a dedicated Headless distribution directory.\n' >&2
  exit 2
fi
mkdir -p "${OUT_PARENT}"
WORK_DIR="$(mktemp -d "${OUT_PARENT}/.cordis-headless.XXXXXXXX")"
PLAN_DIR="${WORK_DIR}/plan"
PACKAGE_DIR="${WORK_DIR}/package"
cleanup() {
  python3 -c 'import shutil, sys; shutil.rmtree(sys.argv[1])' "${WORK_DIR}"
}
trap cleanup EXIT
python3 "${SCRIPT_DIR}/publish-inkstone-headless.py" "${OUT_DIR}" "${WORK_DIR}"

echo "[pack-inkstone-headless] 1/4 Compiling frozen headless LoadPlan..."
python3 "${ROOT_DIR}/script/gen-cordis-build.py" \
  "${ROOT_DIR}/profiles/inkstone-headless.cordis.json" \
  --out-dir "${PLAN_DIR}"

# Verify before replacing any existing distribution.
python3 - "${PLAN_DIR}/build-plan.json" <<'PYTHON'
import json, pathlib, sys
plan_file = pathlib.Path(sys.argv[1])
plan = json.loads(plan_file.read_text(encoding="utf-8"))
if plan["build_chromium"] or (plan_file.parent / "cordis.args.gn").exists():
    sys.exit("HEADLESS FAIL-CLOSED: this profile requires Chromium")
print("[pack-inkstone-headless] Verified build_chromium=false (zero Chromium).")
PYTHON

echo "[pack-inkstone-headless] 2/4 Building the headless daemon and supervisor..."
BIN_DIR="$(python3 "${ROOT_DIR}/script/build-cordis-runtime.py" "${PLAN_DIR}/build-plan.json" --release)"

# Strip local symbols from copies of the macOS arm64 release executables.
if [[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]]; then
  OPTIMIZED_BIN_DIR="${PLAN_DIR}/optimized-bin"
  mkdir -p "${OPTIMIZED_BIN_DIR}"
  for binary in electron-cordis-daemon electron-cordis-supervisor; do
    cp "${BIN_DIR}/${binary}" "${OPTIMIZED_BIN_DIR}/${binary}"
    xcrun strip -x "${OPTIMIZED_BIN_DIR}/${binary}"
    codesign --verify --strict "${OPTIMIZED_BIN_DIR}/${binary}"
  done
  BIN_DIR="${OPTIMIZED_BIN_DIR}"
fi

echo "[pack-inkstone-headless] 3/4 Staging zero-Chromium distribution..."
mkdir -p "${PACKAGE_DIR}/bin" "${PACKAGE_DIR}/config"
cp "${PLAN_DIR}/build-plan.json" "${PLAN_DIR}/load-plan.json" "${PACKAGE_DIR}/config/"
cp "${BIN_DIR}/electron-cordis-daemon" "${PACKAGE_DIR}/bin/"
cp "${BIN_DIR}/electron-cordis-supervisor" "${PACKAGE_DIR}/bin/"
for package in dsh-cordis-bridge electron-compat-shim; do
  source="${ROOT_DIR}/js-packages/${package}"
  destination="${PACKAGE_DIR}/js-packages/${package}"
  mkdir -p "${destination}"
  cp "${source}/package.json" "${destination}/"
  cp -R "${source}/src" "${destination}/"
done

# Generate SHA-256 manifest + assert zero Chromium/Electron binaries exist
python3 - "${PACKAGE_DIR}" <<'PY'
import hashlib, json, pathlib, sys

out = pathlib.Path(sys.argv[1])
files = {}
total_bytes = 0
for p in sorted(out.rglob("*")):
    if p.is_file() and p.name != "MANIFEST.json":
        rel = p.relative_to(out).as_posix()
        lower = rel.lower()
        if any(name in lower for name in ("chromium", "electron.app", "libffmpeg")):
            sys.exit(f"Unexpected Chromium/Electron binary in headless bundle: {rel}")
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
REPORT="$("${PACKAGE_DIR}/bin/electron-cordis-supervisor" \
  --daemon-bin "${PACKAGE_DIR}/bin/electron-cordis-daemon" \
  --load-plan "${PACKAGE_DIR}/config/load-plan.json" \
  --headless)"

python3 - "${REPORT}" <<'PY'
import json, sys
r = json.loads(sys.argv[1])
if (r["headless"] is not True or r["ui_launches"] != 0 or r["clean_shutdown"] is not True
        or r["health_after_ui_restart"]["profile"] != "inkstone-headless"):
    sys.exit(f"Headless smoke test failed: {r}")
print(
    f"[pack-inkstone-headless] SMOKE PASS: profile={r['health_after_ui_restart']['profile']} "
    f"daemon_pid={r['daemon_pid']} ui_launches={r['ui_launches']} clean_shutdown={r['clean_shutdown']}"
)
PY

python3 "${SCRIPT_DIR}/publish-inkstone-headless.py" "${OUT_DIR}" "${WORK_DIR}" --staged "${PACKAGE_DIR}"
