#!/usr/bin/env bash
# Generate and build the desktop or Electron Lite target in the Chromium checkout.
set -euo pipefail

electron="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lite=true
if [[ "${1:-}" == --desktop ]]; then
  lite=false
  shift
fi

profile="${1:-inkstone-desktop}"
if [[ $# -gt 0 ]]; then
  shift
fi

guarded=false
if [[ "${1:-}" == --guarded ]]; then
  guarded=true
  shift
fi

build_config=testing
default_target=electron
dist_target=electron:testing_build
default_jobs=2
if [[ "${1:-}" == --release ]]; then
  build_config=release
  default_target=electron:release_build
  dist_target="$default_target"
  default_jobs=1
  shift
fi

mode="${1:-}"
usage() {
  printf 'Usage: %s [--desktop] [profile] [--guarded] [--release] [--gen-only|--bindings-only|--dist|--ffmpeg-only]\n' "$0" >&2
  exit 2
}
if [[ ! "$profile" =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ || $# -gt 1 ]]; then
  usage
fi
case "$mode" in
  ""|--gen-only) target="$default_target" ;;
  --bindings-only) target=third_party/blink/renderer/bindings:generate_bindings_all ;;
  --dist) target="$dist_target" ;;
  --ffmpeg-only) target=electron:electron_ffmpeg_zip; default_jobs=1 ;;
  *) usage ;;
esac
if "$guarded"; then
  default_jobs=1
fi
jobs="${ELECTRON_CORDIS_BUILD_JOBS:-$default_jobs}"
if [[ ! "$jobs" =~ ^[1-9][0-9]*$ ]]; then
  printf 'ELECTRON_CORDIS_BUILD_JOBS must be a positive integer.\n' >&2
  exit 2
fi

cd "$electron/.."
if [[ -x "$PWD/third_party/cpython3/host/bin/python3" ]]; then
  export PATH="$PWD/third_party/cpython3/host/bin:$PATH"
fi

if [[ "$build_config" == release ]]; then
  out="out/Release"
  lock=.cordis-release-build.lock
else
  out="out/Testing"
  lock=.cordis-testing-build.lock
fi
if "$lite"; then
  if [[ "$build_config" == release ]]; then
    out="out/CordisLiteRelease-$profile"
    lock=".cordis-lite-release-$profile.lock"
  else
    out="out/CordisLite-$profile"
    lock=".cordis-lite-$profile.lock"
  fi
fi

if "$guarded"; then
  log_dir="$out"
  if [[ "$mode" == --ffmpeg-only ]]; then
    log_dir=out/ffmpeg
    lock=.cordis-release-build.lock
  fi
  command=(bash "$electron/script/build-cordis-electron.sh")
  if ! "$lite"; then
    command+=(--desktop)
  fi
  command+=("$profile")
  if [[ "$build_config" == release ]]; then
    command+=(--release)
  fi
  if [[ -n "$mode" ]]; then
    command+=("$mode")
  fi
  export ELECTRON_CORDIS_BUILD_JOBS="$jobs"
  reserve="${ELECTRON_CORDIS_MEMORY_RESERVE:-70}"
  start_at="${ELECTRON_CORDIS_MEMORY_START_AT:-75}"
  exec python3 "$electron/script/guard-cordis-build.py" \
    --reserve "$reserve" --start-at "$start_at" --log "$log_dir/guarded-build.log" \
    --output-lock "out/$lock" -- "${command[@]}"
fi

mac_sdk=
host_os="$(uname -s)"
case "$host_os" in
  Darwin)
    gn="$PWD/buildtools/mac/gn"
    if [[ -n "${ELECTRON_CORDIS_MAC_SDK_PATH:-}" ]]; then
      mac_sdk="$(cd -- "$ELECTRON_CORDIS_MAC_SDK_PATH" && pwd -P)"
      if [[ ! -f "$mac_sdk/usr/lib/libSystem.tbd" ]]; then
        printf 'Invalid macOS SDK path: %s\n' "$mac_sdk" >&2
        exit 2
      fi
      mac_sdk="${mac_sdk//\\/\\\\}"
      mac_sdk="${mac_sdk//\"/\\\"}"
      mac_sdk="${mac_sdk//\$/\\\$}"
    fi
    ;;
  Linux) gn="$PWD/buildtools/linux64/gn" ;;
  *) gn=gn ;;
esac

# Node's config.gypi action also invokes GN by name.
if [[ "$gn" != gn ]]; then
  export PATH="${gn%/gn}:$PATH"
fi
siso="${SISO_PATH:-third_party/siso/cipd/siso}"
if [[ ! -x "$siso" ]]; then
  export PATH="$PATH:$PWD/../depot_tools"
  export SISO_LIMITS="${SISO_LIMITS:+$SISO_LIMITS,}local=$jobs"
fi

generate_manifest() {
  local output="$1" gn_args="$2"
  # Keep the existing manifest on a retry with identical arguments. Its depfile
  # lets Ninja regenerate it when a GN input changes.
  if [[ "$mode" == --gen-only || ! -s "$output/build.ninja" ||
        ! -s "$output/build.ninja.d" || ! -f "$output/build.ninja.stamp" || ! -f "$output/args.gn" ]] ||
     ! grep -Fq -- "--fail-on-unused-args --threads=$jobs --regeneration gen ." "$output/build.ninja" ||
     [[ "$(printf '%s\n' "$gn_args" | "$gn" format --stdin)" != "$(<"$output/args.gn")" ]]; then
    "$gn" gen "$output" --threads="$jobs" --fail-on-unused-args --args="$gn_args"
  fi
}

run_build() {
  local output="$1"
  local -x GOMEMLIMIT="${GOMEMLIMIT:-900MiB}"
  shift
  printf 'Building %s in %s with %s local jobs.\n' "$*" "$output" "$jobs"
  if [[ -x "$siso" ]]; then
    "$siso" ninja -C "$output" --local_jobs="$jobs" "$@"
  else
    autoninja -C "$output" -j "$jobs" "$@"
  fi
}

build_ffmpeg() {
  local ffmpeg_args
  ffmpeg_args="$(printf 'import("//electron/build/args/ffmpeg.gn")\naction_pool_depth = %s\n' "$jobs")"
  if [[ -n "$mac_sdk" ]]; then
    ffmpeg_args+="$(printf '\nmac_sdk_path = "%s"' "$mac_sdk")"
  fi
  generate_manifest out/ffmpeg "$ffmpeg_args"
  run_build out/ffmpeg electron:electron_ffmpeg_zip
}

if [[ "$mode" == --ffmpeg-only ]]; then
  build_ffmpeg
  exit
fi

if "$lite"; then
  python3 "$electron/script/gen-cordis-build.py" "$electron/profiles/$profile.cordis.json" \
    --out-dir "$out" --electron-lite
else
  python3 "$electron/script/gen-cordis-build.py" "$electron/profiles/$profile.cordis.json" \
    --out-dir "$out"
fi
if [[ ! -f "$out/cordis.args.gn" ]]; then
  printf 'Profile %s has no Electron web surface. Use just build-cordis for headless profiles.\n' "$profile" >&2
  exit 2
fi

# Apply the profile after the upstream configuration so it can override defaults.
args="$(
  printf 'import("//electron/build/args/%s.gn")\n' "$build_config"
  cat "$out/cordis.args.gn"
  printf '\nblink_bindings_single_process = true\naction_pool_depth = %s\n' "$jobs"
  if [[ -n "$mac_sdk" ]]; then
    printf 'mac_sdk_path = "%s"\n' "$mac_sdk"
  fi
  if [[ "$build_config" == testing ]]; then
    printf 'concurrent_links = 1\n'
  else
    printf 'thin_lto_backend_jobs = %s\n' "$jobs"
    if [[ "$host_os" == Darwin ]]; then
      printf 'dsymutil_threads = %s\n' "$jobs"
    fi
  fi
)"
generate_manifest "$out" "$args"
if [[ "$mode" == --gen-only ]]; then
  exit 0
fi

targets=("$target")
desktop_release=false
if [[ "$target" == electron:release_build ]] && ! "$lite"; then
  targets+=(electron:electron_chromedriver_zip)
  desktop_release=true
fi
run_build "$out" "${targets[@]}"
if [[ "$target" == "$dist_target" ]]; then
  python3 "$electron/script/package-mksnapshot.py" "$gn" "$out"
fi
if "$desktop_release"; then
  build_ffmpeg
fi
