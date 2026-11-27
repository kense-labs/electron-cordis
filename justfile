set positional-arguments

# Start a Cordis-backed Electron app in development mode.
[env("CORDIS_DEV_CWD", invocation_directory())]
[script("node")]
@dev *args:
    Promise.all([import('node:path'), import('node:url')]).then(([{ resolve }, { pathToFileURL }]) =>
      import(pathToFileURL(resolve('script/dev-cordis.mjs'))),
    );

# Build and run the Cordis daemon with the headless profile.
dev-headless:
    #!/usr/bin/env bash
    set -euo pipefail

    runtime_dir=$(mktemp -d /tmp/electron-cordis-dev.XXXXXXXX)
    trap 'rm -rf -- "$runtime_dir"' EXIT
    python3 script/gen-cordis-build.py profiles/inkstone-headless.cordis.json \
        --out-dir "$runtime_dir" >&2

    socket="$runtime_dir/cordis.sock"
    printf 'Cordis dev socket: %s\n' "$socket" >&2
    bin_dir=$(python3 script/build-cordis-runtime.py "$runtime_dir/build-plan.json")
    "$bin_dir/electron-cordis-daemon" \
        --load-plan "$runtime_dir/load-plan.json" --socket "$socket"

# Run an already-built Electron from the Chromium checkout.
dev-desktop *args:
    #!/usr/bin/env bash
    set -euo pipefail

    checkout=${ELECTRON_CORDIS_CHECKOUT:-../..}
    case "${ELECTRON_CORDIS_CHECKOUT:-}" in
        ""|/*|[A-Za-z]:/*|[A-Za-z]:\\*) ;;
        *) checkout={{ quote(invocation_directory()) }}/"$checkout" ;;
    esac
    if [[ ! -f "$checkout/src/chrome/VERSION" ]]; then
        printf 'Set ELECTRON_CORDIS_CHECKOUT to the gclient checkout root.\n' >&2
        exit 1
    fi

    start_script="$checkout/src/electron/script/start.js"
    if [[ ! -f "$start_script" ]]; then
        printf 'Electron startup script is missing: %s\n' "$start_script" >&2
        exit 1
    fi
    exec node "$start_script" "$@"

# Compile the release daemon and supervisor for a repository profile.
build-cordis profile="inkstone-desktop":
    #!/usr/bin/env bash
    set -euo pipefail
    plan_dir=$(mktemp -d /tmp/electron-cordis-build.XXXXXXXX)
    trap 'rm -rf -- "$plan_dir"' EXIT
    profile={{quote(profile)}}
    python3 script/gen-cordis-build.py "profiles/$profile.cordis.json" --out-dir "$plan_dir"
    python3 script/build-cordis-runtime.py "$plan_dir/build-plan.json" --release

# Generate or build the Chromium target with a repository's Electron Lite profile.
build-electron-lite profile="inkstone-desktop" *args:
    bash script/build-cordis-electron.sh "$@"

# Generate or build the default desktop Electron in the Chromium checkout.
build-electron *args:
    bash script/build-cordis-electron.sh --desktop inkstone-desktop "$@"

# Install a macOS Electron ZIP to a new directory with filesystem compression.
install-desktop archive destination:
    @CORDIS_INSTALL_CWD={{ quote(invocation_directory()) }} python3 script/install-cordis-desktop.py "$@"

# Compare two Electron binaries or native-platform distribution ZIPs with the desktop compatibility probe.
[env("CORDIS_PARITY_CWD", invocation_directory())]
[script("node")]
verify-desktop upstream candidate *args:
    Promise.all([import('node:path'), import('node:url')]).then(([{ resolve }, { pathToFileURL }]) =>
      import(pathToFileURL(resolve('script/verify-desktop-parity.mjs'))),
    );

# Benchmark clean-profile startup and process-tree memory from two macOS distributions.
benchmark-desktop upstream candidate *args:
    @CORDIS_PARITY_CWD={{ quote(invocation_directory()) }} python3 script/benchmark-desktop.py "$@"

# Inspect the mapped resources omitted by the Inkstone Slim packaging preview.
inspect-inkstone-resources *args:
    @CORDIS_PARITY_CWD={{ quote(invocation_directory()) }} node script/inspect-inkstone-resources.mjs "$@"

# Package an on-demand native plugin and sign its archive manifest.
pack-plugin-archive *args:
    @CORDIS_PLUGIN_PACK_CWD={{ quote(invocation_directory()) }} python3 script/pack-cordis-plugin-archive.py "$@"

# Reuse one set of native fixtures and compare the outcomes of upstream Electron specs.
verify-desktop-specs upstream candidate reports *files:
    node script/spec-runner.js --prepareOnly
    @CORDIS_PARITY_CWD={{ quote(invocation_directory()) }} node script/verify-desktop-specs.mjs "$@"
