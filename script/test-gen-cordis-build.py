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
    def run_gen(self, profile: pathlib.Path, out_dir: pathlib.Path, *flags: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(GEN), str(profile), "--out-dir", str(out_dir), *flags],
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
            self.assertFalse(plan["gn_args"]["electron_cordis_mode"])
            self.assertEqual(set(plan["gn_args"]), {
                "electron_cordis_mode", "electron_cordis_profile", "electron_cordis_plugins",
            })
            self.assertIn("@dsh/inkstone-harness", plan["bundled_plugins"])
            self.assertIn("@dsh/plugin-science-pack", plan["on_demand_plugins"])
            self.assertIn("@dsh/cap-desktop-capture", plan["on_demand_plugins"])
            self.assertNotIn("cap-desktop-capture", plan["cargo_features"])
            self.assertIn("cap-clipboard", plan["cargo_features"])
            self.assertNotIn("bridge-mcp", plan["cargo_features"])
            self.assertNotIn("import(", (out / "cordis.args.gn").read_text(encoding="utf-8"))
            self.assertTrue((out / "load-plan.json").is_file())

    def test_inkstone_electron_lite_retains_pdf_viewer_dependencies(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            out = pathlib.Path(td)
            res = self.run_gen(PROFILES / "inkstone-desktop.cordis.json", out, "--electron-lite")
            self.assertEqual(res.returncode, 0, res.stderr)
            args = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))["gn_args"]
            self.assertTrue(args["electron_cordis_mode"])
            self.assertTrue(args["enable_pdf_viewer"])
            self.assertTrue(args["enable_electron_extensions"])
            self.assertFalse(args["enable_prompt_api"])
            self.assertFalse(args["enable_builtin_spellchecker"])

    def test_inkstone_headless_skips_chromium_completely(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            out = pathlib.Path(td)
            res = self.run_gen(PROFILES / "inkstone-headless.cordis.json", out)
            self.assertEqual(res.returncode, 0, res.stderr)
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertFalse(plan["build_chromium"])
            self.assertEqual(plan["cargo_features"], ["cap-os-shell", "cap-safe-storage"])
            self.assertFalse((out / "cordis.args.gn").exists())
            self.assertEqual(plan["gn_args"]["electron_cordis_plugins"], [])

    def test_dsh_studio_electron_lite_disables_pdf_and_extensions(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            out = pathlib.Path(td)
            res = self.run_gen(PROFILES / "dsh-studio.cordis.json", out, "--electron-lite")
            self.assertEqual(res.returncode, 0, res.stderr)
            plan = json.loads((out / "build-plan.json").read_text(encoding="utf-8"))
            self.assertTrue(plan["build_chromium"])
            self.assertFalse(plan["gn_args"]["enable_pdf_viewer"])
            self.assertNotIn("enable_pdf", plan["gn_args"])
            self.assertNotIn("enable_printing", plan["gn_args"])
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
