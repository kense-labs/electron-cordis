#!/usr/bin/env python3
"""M1 build pipeline & side-by-side canary isolation verification tests.

Verifies:
1. Electron Lite includes only enabled optional Electron modules in the GN plan.
2. Headless generation removes a previously generated desktop args file.
3. `inkstone-cordis-mac-config.mjs` enforces side-by-side canary isolation
   (`appId = cn.internai.inkstone.cordis`, `output = dist/cordis-macos`,
   `channel = cordis-canary`, zero Go `runtime`, slim pack stripping
   on-demand packs, and FailClosed refusal under `release` profile).
"""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import plistlib
from runpy import run_path
import select
import shlex
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from zipfile import ZipFile

ROOT = pathlib.Path(__file__).resolve().parent.parent
GEN_BUILD = ROOT / "script" / "gen-cordis-build.py"
MAC_CFG = ROOT / "script" / "inkstone-cordis-mac-config.mjs"
WIN_CFG = ROOT / "script" / "inkstone-cordis-win-config.mjs"
PROFILES = ROOT / "profiles"


def pe_executable(machine):
    dos = bytearray(64)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, len(dos))
    return dos + struct.pack("<4sH", b"PE\0\0", machine)


class TestM1BuildPipeline(unittest.TestCase):
    def test_lite_build_selects_bundled_optional_modules(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            profile = out / "test.cordis.json"
            profile.write_text(json.dumps({
                "schema": "electron-cordis.profile.v1",
                "name": "test",
                "plugins": [
                    {"id": "@dsh/electron-web-surface"},
                    {"id": "@dsh/electron-content-tracing"},
                    {"id": "@dsh/electron-push-notifications", "delivery": "on-demand"},
                ],
            }), encoding="utf-8")
            subprocess.run(
                [sys.executable, str(GEN_BUILD), str(profile), "--out-dir", str(out), "--electron-lite"],
                check=True,
                capture_output=True,
            )
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertTrue(plan["gn_args"]["electron_cordis_mode"])
            self.assertEqual(plan["gn_args"]["electron_cordis_plugins"], [
                "@dsh/electron-content-tracing",
                "@dsh/electron-web-surface",
            ])

    def test_headless_profile_removes_stale_desktop_args(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            for profile in ("inkstone-desktop", "inkstone-headless"):
                subprocess.run(
                    [sys.executable, str(GEN_BUILD), str(PROFILES / f"{profile}.cordis.json"), "--out-dir", str(out)],
                    check=True,
                    capture_output=True,
                )
                self.assertEqual((out / "cordis.args.gn").exists(), profile == "inkstone-desktop")

    @unittest.skipUnless(os.name == "posix", "Archive staging fixture uses POSIX file modes")
    def test_signed_plugin_archive_is_reproducible_and_keeps_the_published_pair(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            payload, output, key = root / "payload", root / "published", root / "signing-key.pem"
            payload.mkdir()
            native = payload / "native"
            native.write_bytes(b"\x00\xfftest binary")
            native.chmod(0o755)
            subprocess.run([
                "node", "-e", "const { generateKeyPairSync } = require('node:crypto');"
                "const { writeFileSync } = require('node:fs');"
                "const { privateKey } = generateKeyPairSync('ed25519');"
                "writeFileSync(process.argv[1], privateKey.export({ type: 'pkcs8', format: 'pem' }), { mode: 0o600 });",
                str(key),
            ], check=True)
            command = [
                sys.executable, str(ROOT / "script/pack-cordis-plugin-archive.py"), str(payload), str(output),
                "--plugin-id", "@dsh/archive-test", "--version", "1.0.0", "--target-os", "macos",
                "--target-arch", "aarch64", "--signing-key", str(key),
            ]
            first = subprocess.run(command, check=True, capture_output=True)
            self.assertEqual(first.stdout.decode().strip(), str(output.resolve()))
            manifest = output / "manifest.json"
            archive = output / "archive.tar.gz"
            verified = manifest.read_bytes(), archive.read_bytes()
            self.assertEqual(hashlib.sha256(verified[1]).hexdigest(), json.loads(verified[0])["archive_sha256"])
            with tarfile.open(archive) as package:
                entry = package.getmember("native")
                self.assertEqual(entry.mode, 0o755)
                self.assertEqual(entry.mtime, 0)
                self.assertEqual(package.extractfile(entry).read(), native.read_bytes())

            os.utime(native, (10, 10))
            repeated = root / "repeated"
            repeated.mkdir()
            second = [*command]
            second[3] = str(repeated)
            subprocess.run(second, check=True, capture_output=True)
            self.assertEqual((repeated / manifest.name).read_bytes(), verified[0])
            self.assertEqual((repeated / archive.name).read_bytes(), verified[1])
            subprocess.run(command, check=True, capture_output=True)

            native.write_bytes(b"changed payload")
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Output already contains files that differ", result.stderr)
            self.assertEqual((manifest.read_bytes(), archive.read_bytes()), verified)

            alias = payload / "key-alias.pem"
            os.link(key, alias)
            rejected = root / "rejected"
            second[3] = str(rejected)
            result = subprocess.run(second, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Signing key is inside the plugin payload", result.stderr)
            self.assertFalse(rejected.exists())

    @unittest.skipUnless(sys.platform == "darwin", "Build guard test requires macOS")
    def test_build_guard_reaps_children_after_the_launcher_exits(self) -> None:
        guard = run_path(str(ROOT / "script/guard-cordis-build.py"))
        child = """
import signal
import time
signal.signal(signal.SIGINT, signal.SIG_IGN)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
print('ready', flush=True)
time.sleep(60)
"""
        launcher = """
import subprocess
import sys
subprocess.run([sys.executable, '-c', sys.argv[1]], check=True)
"""
        job = subprocess.Popen([sys.executable, "-c", launcher, child], start_new_session=True,
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            self.assertTrue(select.select([job.stdout], [], [], 5)[0], "child did not start")
            self.assertEqual(job.stdout.readline(), b"ready\n")
            guard["stop_build"](job, interrupt_timeout=0.2, terminate_timeout=0.2)
            self.assertFalse(guard["group_exists"](job))
        finally:
            if guard["group_exists"](job):
                os.killpg(job.pid, signal.SIGKILL)
            job.wait(timeout=5)
            job.stdout.close()

    @unittest.skipUnless(os.name == "posix", "Headless publication requires POSIX locking")
    def test_headless_publisher_keeps_previous_package_on_validation_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            output = root / "headless"
            work = root / "staging"
            candidate = work / "package"
            for directory in (output, candidate):
                directory.mkdir(parents=True)
                data = b"verified runtime"
                (directory / "runtime").write_bytes(data)
                manifest = {
                    "schema": "electron-cordis.headless-dist.v1",
                    "profile": "inkstone-headless",
                    "files": {"runtime": {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}},
                }
                (directory / "MANIFEST.json").write_text(json.dumps(manifest), encoding="utf-8")
            (candidate / "runtime").write_bytes(b"invalid runtime!")
            script = ROOT / "script" / "publish-inkstone-headless.py"
            result = subprocess.run(
                [sys.executable, script, output, work, "--staged", candidate],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Distribution file differs from its manifest", result.stderr)
            self.assertEqual((output / "runtime").read_bytes(), b"verified runtime")

    def test_snapshot_package_contains_portable_gn_metadata(self) -> None:
        package = run_path(str(ROOT / "script/package-mksnapshot.py"))
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            assembly = out / "gen/v8/embedded.S"
            assembly.parent.mkdir(parents=True)
            assembly.write_bytes(b"V8 assembly")
            archive_path = out / "mksnapshot.zip"
            with ZipFile(archive_path, "w") as archive:
                archive.writestr("mksnapshot", b"binary")
                archive.writestr("mksnapshot_args", "stale")
            description = (
                "GN warning\n./clang_arm64_snapshot/mksnapshot\n"
                "--turbo-profiling-input\n../../profiles/arm64.profile\n--reorder-builtins\n"
                "--builtins-effects-src\ngen/v8/builtins-effects.cc\n"
                "--embedded_src\ngen/v8/embedded.S\n--startup_blob\nsnapshot_blob.bin\n"
            )
            with patch.object(subprocess, "check_output", return_value=description):
                metadata = package["snapshot_metadata"]("gn", out, {"mksnapshot"})
            package["update_archive"](archive_path, metadata)
            updated = archive_path.stat().st_mtime_ns
            package["update_archive"](archive_path, metadata)
            self.assertEqual(archive_path.stat().st_mtime_ns, updated)
            with ZipFile(archive_path) as archive:
                self.assertEqual(archive.namelist().count("mksnapshot_args"), 1)
                self.assertEqual(archive.read("mksnapshot"), b"binary")
                self.assertEqual(archive.read("gen/v8/embedded.S"), b"V8 assembly")
                self.assertEqual(
                    archive.read("mksnapshot_args"),
                    b"./mksnapshot\n--embedded_src\ngen/v8/embedded.S\n--startup_blob\nsnapshot_blob.bin\n",
                )

    @unittest.skipUnless(sys.platform == "darwin", "macOS archive test requires darwin")
    def test_macos_distribution_replaces_previous_archive(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            app = out / "Electron.app"
            app.mkdir()
            runtime = app / "runtime"
            runtime.write_bytes(b"old")
            (app / "runtime-link").symlink_to("runtime")
            removed = app / "optional-component"
            removed.write_bytes(b"x" * 4096)
            deps = out / "runtime_deps"
            deps.write_text("Electron.app\n", encoding="utf-8")
            archive = out / "dist.zip"
            command = [sys.executable, str(ROOT / "build/zip.py"), archive.name, deps.name,
                       "arm64", "mac", "false", "false"]
            subprocess.run(command, cwd=out, capture_output=True, check=True)
            old_size = archive.stat().st_size

            previous = runtime.stat()
            runtime.write_bytes(b"new")
            os.utime(runtime, ns=(previous.st_atime_ns, previous.st_mtime_ns))
            removed.unlink()
            subprocess.run(command, cwd=out, capture_output=True, check=True)

            self.assertLess(archive.stat().st_size, old_size)
            with ZipFile(archive) as package:
                self.assertNotIn("Electron.app/optional-component", package.namelist())
                self.assertEqual(package.read("Electron.app/runtime"), b"new")
                link = package.getinfo("Electron.app/runtime-link")
                self.assertTrue(stat.S_ISLNK(link.external_attr >> 16))
                self.assertEqual(package.read(link), b"runtime")

            current = archive.read_bytes()
            deps.write_text("missing\n", encoding="utf-8")
            self.assertNotEqual(subprocess.run(command, cwd=out, capture_output=True).returncode, 0)
            self.assertEqual(archive.read_bytes(), current)

    @unittest.skipUnless(sys.platform == "darwin", "macOS canary config test requires darwin")
    def test_macos_canary_validates_the_cached_archive_before_packaging(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            web = pathlib.Path(tmp).resolve()
            (web / "package.json").write_text('{"devDependencies":{"electron":"43.2.0"}}', encoding="utf-8")
            (web / "electron-builder.yml").write_text("extraResources: []\n", encoding="utf-8")
            dist = web / "候选包"
            dist.mkdir()
            archive = dist / "electron-v43.2.0-darwin-arm64.zip"
            contents = "Electron.app/Contents"
            framework = f"{contents}/Frameworks/Electron Framework.framework/Versions/A"
            renderer = f"{contents}/Frameworks/Electron Helper (Renderer).app/Contents"
            arm64 = struct.pack("<II", 0xFEEDFACF, 0x0100000C)
            entries = {
                "version": b"43.2.0",
                f"{contents}/Info.plist": plistlib.dumps({"CFBundleVersion": "43.2.0"}),
                f"{contents}/MacOS/Electron": arm64,
                f"{framework}/Electron Framework": arm64,
                f"{renderer}/MacOS/Electron Helper (Renderer)": arm64,
                f"{framework}/Resources/icudtl.dat": b"ICU",
                f"{framework}/Resources/resources.pak": b"PAK",
            }
            env = {**os.environ, "INKSTONE_WEB_ROOT": str(web), "ELECTRON_CORDIS_DIST": dist.name,
                   "ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN": "1", "ELECTRON_CORDIS_SLIM_PACK": "0"}
            env.pop("INKSTONE_CORE_RUNTIME_BUILD_PROFILE", None)
            command = ["node", "--input-type=module", "-e",
                       f"import cfg from {json.dumps(MAC_CFG.as_uri())}; console.log((await cfg()).electronDist);"]

            def run_config():
                with ZipFile(archive, "w") as package:
                    for name, payload in entries.items():
                        package.writestr(name, payload)
                return subprocess.run(command, env=env, capture_output=True, text=True)

            valid = run_config()
            self.assertEqual(valid.returncode, 0, valid.stderr)
            self.assertEqual(valid.stdout.strip(), str(archive))

            app = f"{contents}/MacOS/Electron"
            entries[app] = struct.pack("<II", 0xFEEDFACF, 0x01000007)
            wrong_arch = run_config()
            self.assertNotEqual(wrong_arch.returncode, 0)
            self.assertIn(f"{app} does not contain a darwin-arm64 executable", wrong_arch.stderr)

            entries[app] = arm64
            entries["version"] = b"47.0.0"
            wrong_version = run_config()
            self.assertNotEqual(wrong_version.returncode, 0)
            self.assertIn("reports Electron '47.0.0', expected 43.2.0", wrong_version.stderr)

            entries["version"] = b"43.2.0"
            del entries[f"{framework}/Resources/resources.pak"]
            incomplete = run_config()
            self.assertNotEqual(incomplete.returncode, 0)
            self.assertIn("/Resources/resources.pak", incomplete.stderr)

    @unittest.skipUnless(sys.platform == "darwin", "Filesystem compression uses macOS ditto")
    def test_macos_desktop_install_publishes_a_complete_bundle_once(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp).resolve()
            source = out / "source"
            contents = pathlib.Path("Electron.app/Contents")
            framework = contents / "Frameworks/Electron Framework.framework/Versions"
            binary = contents / "MacOS/Electron"
            files = {
                pathlib.Path("version"): b"43.2.0",
                contents / "Info.plist": b"metadata",
                binary: b"#!/bin/sh\nexit 0\n",
                framework / "A/Electron Framework": b"framework",
                framework / "A/Resources/icudtl.dat": b"ICU",
                framework / "A/Resources/resources.pak": b"PAK" * 65536,
            }
            for name, payload in files.items():
                path = source / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(payload)
            (source / binary).chmod(0o755)
            (source / framework / "Current").symlink_to("A")
            archive = out / "electron cordis.zip"
            subprocess.run(["/usr/bin/zip", "-q", "-r", "-y", str(archive), "Electron.app", "version"],
                           cwd=source, check=True)

            command = [sys.executable, str(ROOT / "script/install-cordis-desktop.py"), archive.name]
            env = {**os.environ, "CORDIS_INSTALL_CWD": str(out)}
            destination = out / "installed app"
            install = subprocess.run([*command, destination.name], env=env, text=True, capture_output=True)
            self.assertEqual(install.returncode, 0, install.stderr)
            self.assertEqual(install.stdout.strip(), str(destination / "Electron.app"))
            self.assertEqual(stat.S_IMODE((destination / binary).stat().st_mode), 0o755)
            self.assertEqual(os.readlink(destination / framework / "Current"), "A")
            for name, payload in files.items():
                self.assertEqual((destination / name).read_bytes(), payload)

            repeated = subprocess.run([*command, destination.name], env=env, text=True, capture_output=True)
            self.assertNotEqual(repeated.returncode, 0)
            self.assertIn("already exists", repeated.stderr)
            self.assertEqual((destination / binary).read_bytes(), files[binary])

            incomplete = out / "incomplete.zip"
            with ZipFile(incomplete, "w") as package:
                package.writestr("version", files[pathlib.Path("version")])
            rejected = subprocess.run([*command[:2], incomplete.name, "partial"], env=env, text=True,
                                      capture_output=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("Incomplete macOS Electron ZIP", rejected.stderr)
            self.assertFalse((out / "partial").exists())
            self.assertFalse(list(out.glob(".cordis-install-*")))

    def test_windows_distribution_checks_pe_machine(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dist = pathlib.Path(tmp).resolve()
            for name, content in {"version": b"43.2.0", "icudtl.dat": b"ICU", "resources.pak": b"PAK",
                                  "electron.exe": pe_executable(0x8664), "ffmpeg.dll": pe_executable(0x8664)}.items():
                (dist / name).write_bytes(content)
            command = [sys.executable, str(ROOT / "script/validate-inkstone-electron-dist.py"), str(dist),
                       "43.2.0", "win32", "x64"]
            valid = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(valid.returncode, 0, valid.stderr)
            self.assertEqual(valid.stdout.strip(), str(dist))

            (dist / "ffmpeg.dll").write_bytes(pe_executable(0xAA64))
            wrong_arch = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(wrong_arch.returncode, 0)
            self.assertIn("ffmpeg.dll does not contain a win32-x64 executable", wrong_arch.stderr)

    def test_windows_canary_requires_identity_from_the_independent_config(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            web = pathlib.Path(tmp).resolve()
            (web / "package.json").write_text('{"devDependencies":{"electron":"43.2.0"}}', encoding="utf-8")
            (web / "electron-builder.yml").write_text("extraResources: []\n", encoding="utf-8")
            scripts = web / "scripts"
            scripts.mkdir()
            handoff = scripts / "build-windows-update-handoff.mjs"
            handoff.write_text(
                "export function windowsHandoffBuildIdentity() { throw new Error('fixed application identity'); }\n",
                encoding="utf-8",
            )
            archive = web / "candidate.zip"
            with ZipFile(archive, "w") as package:
                for name, data in {"version": b"43.2.0", "icudtl.dat": b"ICU", "resources.pak": b"PAK",
                                   "electron.exe": pe_executable(0x8664), "ffmpeg.dll": pe_executable(0x8664)}.items():
                    package.writestr(name, data)
            command = ["node", "--input-type=module", "-e",
                       "Object.defineProperties(process,{platform:{value:'win32'},arch:{value:'x64'}});"
                       f"const {{default:cfg}}=await import({json.dumps(WIN_CFG.as_uri())});"
                       "console.log(JSON.stringify(await cfg()));"]
            env = {**os.environ, "INKSTONE_WEB_ROOT": str(web), "ELECTRON_CORDIS_DIST": archive.name,
                   "ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN": "0", "ELECTRON_CORDIS_SLIM_PACK": "0",
                   "ELECTRON_CORDIS_NSIS": "1"}
            env.pop("INKSTONE_CORE_RUNTIME_BUILD_PROFILE", None)
            if os.name != "nt":
                python = web / "python"
                python.write_text(f'#!/bin/sh\nexec {shlex.quote(sys.executable)} "$@"\n', encoding="utf-8")
                python.chmod(0o755)
                command[0] = shutil.which("node")
                env["PATH"] = str(web)
            denied = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(denied.returncode, 0)
            self.assertIn("Windows update handoff does not accept cn.internai.inkstone.cordis", denied.stderr)

            handoff.write_text("export function windowsHandoffBuildIdentity() {}\n", encoding="utf-8")
            config = scripts / "direct-windows-candidate-config.mjs"
            config.write_text(
                "export default async () => ({appId: 'cn.internai.inkstone.cordis',"
                "extraMetadata:{name:'inkstone-desktop-cordis',inkstoneRuntime:'direct-windows-verification'},"
                "win:{executableName:'inkstone-cordis',extraResources:[{to:'windows-dsh-launcher'}]}});\n",
                encoding="utf-8",
            )
            allowed = subprocess.run(command, env=env, capture_output=True, text=True, check=True)
            candidate = json.loads(allowed.stdout)
            self.assertEqual(candidate["electronDist"], str(archive))
            self.assertEqual(candidate["extraMetadata"]["inkstoneRuntime"], "direct-windows-verification")
            self.assertEqual(candidate["win"]["target"], [{"target": "nsis", "arch": ["x64"]}])
            self.assertIn({"to": "windows-dsh-launcher"}, candidate["win"]["extraResources"])

            config.write_text(config.read_text().replace("cn.internai.inkstone.cordis", "cn.internai.inkstone.beta"),
                              encoding="utf-8")
            conflict = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(conflict.returncode, 0)
            self.assertIn("independent Windows config must select cn.internai.inkstone.cordis", conflict.stderr)

    def test_slim_resource_preview_reports_literal_filters_and_missing_sources(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            web = pathlib.Path(tmp).resolve()
            (web / "package.json").write_text('{"devDependencies":{"electron":"43.2.0"}}', encoding="utf-8")
            (web / "electron-builder.yml").write_text(
                "extraResources:\n"
                "  - {from: absent, to: feishu-cli}\n"
                "  - {from: unused, to: runtime}\n"
                "win:\n"
                "  extraResources:\n"
                "    - {from: ssh, to: ssh-native, filter: [launcher.exe, manifest.json]}\n",
                encoding="utf-8",
            )
            ssh = web / "ssh"
            ssh.mkdir()
            (ssh / "launcher.exe").write_bytes(b"12345")
            (ssh / "unused.dll").write_bytes(b"1234567890")

            inspector = ROOT / "script" / "inspect-inkstone-resources.mjs"
            result = subprocess.run(
                ["node", inspector, "--web-root", web, "--platform", "win32"],
                capture_output=True, text=True, check=True,
            )
            preview = json.loads(result.stdout)
            rows = {entry["to"]: entry for entry in preview["excluded"]}
            self.assertEqual(set(rows), {"feishu-cli", "ssh-native"})
            self.assertEqual(rows["ssh-native"]["missing"], ["manifest.json"])
            self.assertEqual(rows["feishu-cli"]["missing"], ["."])
            self.assertEqual(preview["measuredRegularFileBytes"], 5)
            self.assertFalse(preview["complete"])

    @unittest.skipUnless(sys.platform == "darwin", "macOS canary config test requires darwin")
    def test_macos_canary_preserves_a_signed_dmg(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            web = pathlib.Path(tmp).resolve()
            hook = web / "scripts/compress-mac-dmg-ulmo.mjs"
            hook.parent.mkdir()
            hook.write_text("export default async result => result.artifactPaths;\n", encoding="utf-8")
            (web / "electron-builder.yml").write_text(
                "afterAllArtifactBuild: scripts/compress-mac-dmg-ulmo.mjs\n", encoding="utf-8"
            )
            env = {**os.environ, "INKSTONE_WEB_ROOT": str(web), "ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN": "1"}
            env.pop("INKSTONE_CORE_RUNTIME_BUILD_PROFILE", None)
            env.pop("ELECTRON_CORDIS_DIST", None)
            eval_script = (
                f"import cfg from {json.dumps(MAC_CFG.as_uri())};"
                "const {afterAllArtifactBuild} = await cfg();"
                "const build = {configuration:{dmg:{sign:true}},artifactPaths:['fixture.dmg']};"
                "const signed = await afterAllArtifactBuild(build);"
                "build.configuration.dmg.sign = false;"
                "console.log(JSON.stringify({signed,unsigned:await afterAllArtifactBuild(build)}));"
            )
            result = subprocess.run(["node", "--input-type=module", "-e", eval_script],
                                    env=env, check=True, text=True, capture_output=True)
            self.assertEqual(json.loads(result.stdout), {"signed": [], "unsigned": ["fixture.dmg"]})

    @unittest.skipUnless(sys.platform == "darwin", "macOS canary config test requires darwin")
    def test_macos_side_by_side_canary_config_isolation_and_slim_pack(self) -> None:
        default_web_root = ROOT.parent / "web"
        if (ROOT.parent / "chrome" / "VERSION").is_file():
            default_web_root = ROOT.parents[2] / "inkstone" / "web"
        if not os.environ.get("INKSTONE_WEB_ROOT") and not (default_web_root / "electron-builder.yml").is_file():
            self.skipTest("set INKSTONE_WEB_ROOT to test the optional Inkstone canary")
        base_env = {**os.environ, "ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN": "1"}
        base_env.pop("INKSTONE_CORE_RUNTIME_BUILD_PROFILE", None)
        base_env.pop("ELECTRON_CORDIS_DIST", None)
        base_env.pop("ELECTRON_CORDIS_DMG", None)

        eval_script = (
            f"import cfg from {json.dumps(MAC_CFG.as_uri())};"
            "const out = await cfg();"
            "console.log(JSON.stringify(out));"
        )

        # Standard canary config
        res = subprocess.run(
            ["node", "--input-type=module", "-e", eval_script],
            env={**base_env, "ELECTRON_CORDIS_SLIM_PACK": "0"},
            check=True,
            capture_output=True,
            text=True,
        )
        cfg = json.loads(res.stdout)
        self.assertEqual(cfg["appId"], "cn.internai.inkstone.cordis")
        self.assertEqual(cfg["directories"]["output"], "dist/cordis-macos")
        self.assertEqual(cfg["publish"]["channel"], "cordis-canary")
        self.assertTrue(cfg["extraMetadata"]["electronCordisBase"])
        self.assertEqual(cfg["extraMetadata"]["inkstoneRuntime"], "direct-mac-candidate")
        self.assertEqual(cfg["mac"]["target"], [{"target": "dir", "arch": ["arm64"]}])
        extra_targets = [e.get("to") for e in (cfg.get("extraResources") or [])]
        self.assertNotIn("runtime", extra_targets, "legacy Go runtime must never be bundled")

        installer = subprocess.run(
            ["node", "--input-type=module", "-e", eval_script],
            env={**base_env, "ELECTRON_CORDIS_SLIM_PACK": "0", "ELECTRON_CORDIS_DMG": "1"},
            check=True,
            capture_output=True,
            text=True,
        )
        installer_cfg = json.loads(installer.stdout)
        self.assertEqual(installer_cfg["mac"]["target"], [
            {"target": "dmg", "arch": ["arm64"]}, {"target": "zip", "arch": ["arm64"]},
        ])
        self.assertFalse(installer_cfg["dmg"]["writeUpdateInfo"])

        # Tool discovery still verifies the bundled Science MCP source tree.
        slim_res = subprocess.run(
            ["node", "--input-type=module", "-e", eval_script],
            env={**base_env, "ELECTRON_CORDIS_SLIM_PACK": "1"},
            check=True,
            capture_output=True,
            text=True,
        )
        slim_cfg = json.loads(slim_res.stdout)
        self.assertEqual(slim_cfg["extraMetadata"]["inkstoneRuntime"], "electron-cordis-mac-slim")
        slim_targets = [
            e.get("to")
            for e in ((slim_cfg.get("extraResources") or []) + (slim_cfg.get("mac", {}).get("extraResources") or []))
        ]
        self.assertIn("science-mcp", slim_targets)
        for heavy in ("science-pack", "feishu-cli"):
            self.assertNotIn(heavy, slim_targets)

        with tempfile.TemporaryDirectory() as dist:
            candidate_slim = subprocess.run(
                ["node", "--input-type=module", "-e", eval_script],
                env={**base_env, "ELECTRON_CORDIS_SLIM_PACK": "1", "ELECTRON_CORDIS_DIST": dist},
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(candidate_slim.returncode, 0)
            self.assertIn("requires runtime support for loading Science Pack and Lark CLI", candidate_slim.stderr)

        # FailClosed: refuses to run under official `release` profile!
        rel_res = subprocess.run(
            ["node", "--input-type=module", "-e", eval_script],
            env={**base_env, "INKSTONE_CORE_RUNTIME_BUILD_PROFILE": "release"},
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(rel_res.returncode, 0)
        self.assertIn("must never run under release profile", rel_res.stderr)


if __name__ == "__main__":
    unittest.main()
