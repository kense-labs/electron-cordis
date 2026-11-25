#!/usr/bin/env python3
"""M1 build pipeline & side-by-side canary isolation verification tests.

Verifies:
1. `gen-cordis-build.py` + `gen-module-registry.py` prune ALL Class-C modules
   (BrowserView, TouchBar, inAppPurchase, pushNotifications, contentTracing,
   netLog, ShareMenu, ServiceWorkerMain, sharedTexture) and route Class-A OS
   capabilities to `@dsh/electron-compat` with `LEGACY_* 0`.
2. `inkstone-headless.cordis.json` sets `build_chromium = false` and skips
   Electron GN/module registry generation completely.
3. `inkstone-cordis-mac-config.mjs` enforces side-by-side canary isolation
   (`appId = cn.internai.inkstone.cordis`, `output = dist/cordis-macos`,
   `channel = cordis-canary`, zero Go `runtime`, slim pack stripping
   on-demand packs, and FailClosed refusal under `release` profile).
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
GEN_BUILD = ROOT / "script" / "gen-cordis-build.py"
GEN_REG = ROOT / "script" / "gen-module-registry.py"
MAC_CFG = ROOT / "script" / "inkstone-cordis-mac-config.mjs"
PROFILES = ROOT / "profiles"


class TestM1BuildPipeline(unittest.TestCase):
    def test_desktop_profile_prunes_all_class_c_and_routes_class_a_to_compat(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            subprocess.run(
                [sys.executable, str(GEN_BUILD), str(PROFILES / "inkstone-desktop.cordis.json"), "--out-dir", str(out)],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                [sys.executable, str(GEN_REG), str(out / "build-plan.json"), "--out-dir", str(out / "reg")],
                check=True,
                capture_output=True,
            )

            header = (out / "reg" / "cordis_linked_bindings.h").read_text(encoding="utf-8")
            ts_list = (out / "reg" / "cordis_module_list.ts").read_text(encoding="utf-8")

            # All 9 Class-C macros MUST be disabled (0).
            for macro in [
                "CLASS_C_BROWSER_VIEW",
                "CLASS_C_TOUCH_BAR",
                "CLASS_C_IN_APP_PURCHASE",
                "CLASS_C_PUSH_NOTIFICATIONS",
                "CLASS_C_CONTENT_TRACING",
                "CLASS_C_NET_LOG",
                "CLASS_C_SHARE_MENU",
                "CLASS_C_SERVICE_WORKER_MAIN",
                "CLASS_C_SHARED_TEXTURE",
            ]:
                self.assertIn(f"#define ELECTRON_CORDIS_ENABLE_{macro} 0", header)

            # Class-A capabilities offloaded to Rust MUST disable legacy C++ bindings & route to @dsh/electron-compat.
            for legacy_macro in ["LEGACY_SAFESTORAGE", "LEGACY_CLIPBOARD", "LEGACY_DIALOG", "LEGACY_AUTOUPDATER"]:
                self.assertIn(f"#define ELECTRON_CORDIS_ENABLE_{legacy_macro} 0", header)

            self.assertIn("require('@dsh/electron-compat').safeStorage", ts_list)
            self.assertIn("require('@dsh/electron-compat').clipboard", ts_list)
            self.assertIn("require('@dsh/electron-compat').dialog", ts_list)
            self.assertIn("require('@dsh/electron-compat').autoUpdater", ts_list)
            # Class-B core web-surface remains enabled.
            self.assertIn("name: 'BrowserWindow'", ts_list)
            self.assertIn("name: 'ipcMain'", ts_list)

    def test_headless_profile_skips_chromium_and_module_registry(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp)
            subprocess.run(
                [sys.executable, str(GEN_BUILD), str(PROFILES / "inkstone-headless.cordis.json"), "--out-dir", str(out)],
                check=True,
                capture_output=True,
            )
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertFalse(plan["build_chromium"])
            self.assertFalse((out / "cordis.args.gn").exists())

            res = subprocess.run(
                [sys.executable, str(GEN_REG), str(out / "build-plan.json"), "--out-dir", str(out / "reg")],
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("skips Chromium", res.stdout)
            self.assertFalse((out / "reg" / "cordis_module_list.ts").exists())

    @unittest.skipUnless(sys.platform == "darwin", "macOS canary config test requires darwin")
    def test_macos_side_by_side_canary_config_isolation_and_slim_pack(self) -> None:
        base_env = {**os.environ, "ELECTRON_CORDIS_ALLOW_STOCK_DRYRUN": "1"}
        base_env.pop("INKSTONE_CORE_RUNTIME_BUILD_PROFILE", None)

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
        extra_targets = [e.get("to") for e in (cfg.get("extraResources") or [])]
        self.assertNotIn("runtime", extra_targets, "legacy Go runtime must never be bundled")

        # Slim pack strips heavy on-demand packs (science-pack / science-mcp / feishu-cli)
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
        for heavy in ("science-pack", "science-mcp", "feishu-cli"):
            self.assertNotIn(heavy, slim_targets)

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
