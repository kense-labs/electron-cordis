#!/usr/bin/env bash
# Internalize dshbox/cordis-rs core crates (MIT) into electron-cordis/cordis/crates/
# so Electron-Cordis owns the microkernel source directly with zero external crates.io dependency.
set -euo pipefail

REPO_URL="${CORDIS_RS_REPO:-https://github.com/dshbox/cordis-rs.git}"
REF="${CORDIS_RS_REF:-main}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEST_CRATES="${ROOT_DIR}/cordis/crates"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

echo "[internalize-cordis-rs] Cloning ${REPO_URL} (${REF})..."
git clone --depth 1 --branch "${REF}" "${REPO_URL}" "${TMP_DIR}/cordis-rs"
REV="$(git -C "${TMP_DIR}/cordis-rs" rev-parse HEAD)"

for crate in cordis-core cordis-loader cordis-timer; do
  if [[ ! -d "${TMP_DIR}/cordis-rs/crates/${crate}" ]]; then
    echo "[internalize-cordis-rs] ERROR: missing crates/${crate} in upstream repo" >&2
    exit 1
  fi
  rm -rf "${DEST_CRATES:?}/${crate}"
  cp -R "${TMP_DIR}/cordis-rs/crates/${crate}" "${DEST_CRATES}/${crate}"
  # Ensure internalized crate Cargo.toml resolves sibling cordis-* via path instead of registry.
  python3 - "${DEST_CRATES}/${crate}/Cargo.toml" <<'PY'
import pathlib, re, sys
p = pathlib.Path(sys.argv[1])
text = p.read_text(encoding="utf-8")
# Replace workspace-inherited package fields if upstream Cargo.toml uses `.workspace = true`
# while preserving our workspace's edition/rust-version/license inheritance.
# Rewrite sibling cordis-core / cordis-timer / cordis-loader deps to workspace = true.
for dep in ("cordis-core", "cordis-loader", "cordis-timer"):
    def _rewrite_table(m):
        body = m.group(1)
        feat = re.search(r'features\s*=\s*\[[^\]]*\]', body)
        if feat:
            return f'{dep} = {{ workspace = true, {feat.group(0)} }}'
        return f'{dep}.workspace = true'
    text = re.sub(
        rf'(?m)^{dep}\s*=\s*\{{([^}}]*)\}}',
        _rewrite_table,
        text,
    )
    text = re.sub(
        rf'(?m)^{dep}\s*=\s*"[^"]*"',
        f'{dep}.workspace = true',
        text,
    )
p.write_text(text, encoding="utf-8")
PY
  echo "[internalize-cordis-rs] Internalized crates/${crate}"
done

cp "${TMP_DIR}/cordis-rs/LICENSE" "${ROOT_DIR}/cordis/LICENSE-CORDIS-RS"

cat > "${ROOT_DIR}/cordis/UPSTREAM-CORDIS.json" <<EOF
{
  "upstream_repo": "${REPO_URL}",
  "ref": "${REF}",
  "commit": "${REV}",
  "license": "MIT",
  "internalized_crates": [
    "crates/cordis-core",
    "crates/cordis-loader",
    "crates/cordis-timer"
  ],
  "mode": "internalized_source_owned_in_repo"
}
EOF

echo "[internalize-cordis-rs] Done at commit ${REV}."
echo "[internalize-cordis-rs] Next: cd ${ROOT_DIR}/cordis && cargo check --workspace"
