#!/usr/bin/env python3
"""Build the daemon and supervisor for a generated build plan and print their directory."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("build_plan", type=Path)
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()

    cordis = Path(__file__).resolve().parent.parent / "cordis"
    plan = json.loads(args.build_plan.read_text(encoding="utf-8"))
    if plan.get("schema") != "electron-cordis.build-plan.v1":
        parser.error("expected an Electron Cordis build plan")
    features = ",".join(f"electron-cordis-daemon/{name}" for name in sorted(set(plan["cargo_features"])))
    feature_key = hashlib.sha256(features.encode()).hexdigest()[:16]
    target = Path(os.environ.get("CARGO_TARGET_DIR") or cordis / "target")
    target = (cordis / target).resolve()

    binaries = {"electron-cordis-daemon", "electron-cordis-supervisor"}
    command = [
        "cargo", "build", "--locked", "--no-default-features",
        "--target-dir", str(target / "profiles" / feature_key),
        "--message-format=json-render-diagnostics",
    ]
    for binary in sorted(binaries):
        command.extend(["--package", binary, "--bin", binary])
    if features:
        command.extend(["--features", features])
    env = os.environ.copy()
    if args.release:
        command.append("--release")
        target_triple = env.get("CARGO_BUILD_TARGET", sys.platform)
        if "linux" in target_triple.split("-"):
            env.setdefault("CARGO_PROFILE_RELEASE_STRIP", "symbols")

    artifacts = {}
    with subprocess.Popen(command, cwd=cordis, env=env, stdout=subprocess.PIPE, text=True) as build:
        assert build.stdout is not None
        for line in build.stdout:
            message = json.loads(line)
            if message["reason"] == "compiler-message" and message["message"]["rendered"]:
                sys.stderr.write(message["message"]["rendered"])
            if message["reason"] == "compiler-artifact" and message["executable"]:
                name = message["target"]["name"]
                if name in binaries:
                    artifacts[name] = Path(message["executable"]).parent
        if code := build.wait():
            return code

    if artifacts.keys() != binaries or len(set(artifacts.values())) != 1:
        raise RuntimeError("Cargo did not produce the daemon and supervisor in the same directory")
    print(artifacts["electron-cordis-daemon"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
