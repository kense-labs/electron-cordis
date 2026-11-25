#!/usr/bin/env python3
"""Unit and fail-closed negative tests for script/gen-cordis-build.py."""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
GEN = ROOT / "script" / "gen-cordis-build.py"
PROFILES = ROOT / "profiles"


class GenCordisBuildTests(unittest.TestCase):
    def run_gen(self, profile: pathlib.Path, out_dir: pathlib.Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(GEN), str(profile), "--out-dir", str(out_dir)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_inkstone_desktop_profile_generates_gn_and_splits_delivery(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            out = pathlib.Path(td)
            res = self.run_gen(PROFILES / "inkstone-desktop.cordis.json", out)
            self.assertEqual(res.returncode, 0, res.stderr)
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertTrue(plan["build_chromium"])
            self.assertTrue(plan["gn_args"]["electron_cordis_mode"])
            self.assertTrue(plan["gn_args"]["enable_pdf_viewer"])
            self.assertTrue(plan["gn_args"]["enable_pdf"])
            self.assertTrue(plan["gn_args"]["enable_electron_extensions"])
            self.assertFalse(plan["gn_args"]["enable_prompt_api"])
            self.assertFalse(plan["gn_args"]["enable_builtin_spellchecker"])
            self.assertIn("@dsh/inkstone-harness", plan["bundled_plugins"])
            self.assertIn("@dsh/plugin-science-pack", plan["on_demand_plugins"])
            self.assertTrue((out / "cordis.args.gn").is_file())
            self.assertTrue((out / "load-plan.json").is_file())

    def test_inkstone_headless_skips_chromium_completely(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            out = pathlib.Path(td)
            res = self.run_gen(PROFILES / "inkstone-headless.cordis.json", out)
            self.assertEqual(res.returncode, 0, res.stderr)
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertFalse(plan["build_chromium"])
            self.assertFalse((out / "cordis.args.gn").exists())
            self.assertEqual(plan["gn_args"]["electron_cordis_plugins"], [])

    def test_dsh_studio_profile_builds_cleanly_without_pdf_or_extensions(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            out = pathlib.Path(td)
            res = self.run_gen(PROFILES / "dsh-studio.cordis.json", out)
            self.assertEqual(res.returncode, 0, res.stderr)
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertTrue(plan["build_chromium"])
            self.assertFalse(plan["gn_args"]["enable_pdf_viewer"])
            self.assertFalse(plan["gn_args"]["enable_pdf"])
            self.assertFalse(plan["gn_args"]["enable_electron_extensions"])

    def test_fail_closed_on_unknown_rust_plugin(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            bad = pathlib.Path(td) / "bad.cordis.json"
            bad.write_text(
                json.dumps(
                    {
                        "schema": "electron-cordis.profile.v1",
                        "name": "bad",
                        "plugins": [{"id": "@dsh/does-not-exist", "runtime": "rust"}],
                    }
                ),
                encoding="utf-8",
            )
            res = self.run_gen(bad, pathlib.Path(td) / "out")
            self.assertNotEqual(res.returncode, 0)
            self.assertIn("FAIL-CLOSED", res.stderr)

    def test_fail_closed_on_electron_plugin_without_web_surface(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            bad = pathlib.Path(td) / "bad-ui.cordis.json"
            bad.write_text(
                json.dumps(
                    {
                        "schema": "electron-cordis.profile.v1",
                        "name": "bad-ui",
                        "plugins": [{"id": "@dsh/electron-pdf-viewer", "runtime": "rust"}],
                    }
                ),
                encoding="utf-8",
            )
            res = self.run_gen(bad, pathlib.Path(td) / "out")
            self.assertNotEqual(res.returncode, 0)
            self.assertIn("@dsh/electron-web-surface", res.stderr)

    def test_fail_closed_on_illegal_delivery_or_isolation(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            for field, bad_val in [("isolation", "root-bypass"), ("delivery", "magic")]:
                bad = pathlib.Path(td) / f"bad-{field}.cordis.json"
                bad.write_text(
                    json.dumps(
                        {
                            "schema": "electron-cordis.profile.v1",
                            "name": f"bad-{field}",
                            "plugins": [
                                {
                                    "id": "@dsh/cap-safe-storage",
                                    "runtime": "rust",
                                    field: bad_val,
                                }
                            ],
                        }
                    ),
                    encoding="utf-8",
                )
                res = self.run_gen(bad, pathlib.Path(td) / f"out-{field}")
                self.assertNotEqual(res.returncode, 0)
                self.assertIn("FAIL-CLOSED", res.stderr)


if __name__ == "__main__":
    unittest.main()
