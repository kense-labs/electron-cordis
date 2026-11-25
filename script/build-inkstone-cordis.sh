#!/usr/bin/env bash
# Build an isolated parallel Electron-Cordis Inkstone candidate without touching
# native Electron 43.2.0 Inkstone build outputs or configs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CORDIS_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
INKSTONE_WEB_ROOT="$(cd "${INKSTONE_WEB_ROOT:-${CORDIS_ROOT}/../web}" && pwd)"
export INKSTONE_WEB_ROOT
PROFILE="${1:-inkstone-desktop}"
OUT_PLAN="${CORDIS_ROOT}/cordis/target/generated/${PROFILE}"

echo "[build-inkstone-cordis] 1/3 Checking parity gates..."
python3 "${CORDIS_ROOT}/script/check-inkstone-parity.py"

echo "[build-inkstone-cordis] 2/3 Compiling profile ${PROFILE}..."
python3 "${CORDIS_ROOT}/script/gen-cordis-build.py" \
  "${CORDIS_ROOT}/profiles/${PROFILE}.cordis.json" \
  --out-dir "${OUT_PLAN}"
python3 "${CORDIS_ROOT}/script/gen-module-registry.py" \
  "${OUT_PLAN}/build-plan.json" \
  --out-dir "${OUT_PLAN}"

echo "[build-inkstone-cordis] 3/3 Running isolated electron-builder in ${INKSTONE_WEB_ROOT}..."
cd "${INKSTONE_WEB_ROOT}"
case "$(uname -s)" in
  Darwin)
    pnpm exec electron-builder --mac dir --arm64 \
      --config "${CORDIS_ROOT}/script/inkstone-cordis-mac-config.mjs" \
      --publish never
    ;;
  MINGW*|MSYS*|CYGWIN*)
    pnpm exec electron-builder --win dir --x64 \
      --config "${CORDIS_ROOT}/script/inkstone-cordis-win-config.mjs" \
      --publish never
    ;;
  *)
    echo "[build-inkstone-cordis] Unsupported host OS for desktop pack: $(uname -s)" >&2
    exit 1
    ;;
esac
