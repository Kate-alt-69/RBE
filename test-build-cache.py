#!/usr/bin/env python3
"""Validate cache discovery and Cargo's real per-crate incremental behavior."""

import importlib.util
import json
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("build_cache", Path(__file__).with_name("build-cache.py"))
build_cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build_cache)


class BuildCacheTests(unittest.TestCase):
	def setUp(self):
		self.temporary = tempfile.TemporaryDirectory()
		self.addCleanup(self.temporary.cleanup)
		self.root = Path(self.temporary.name)
		for workspace in ("engine", "container-runtime"):
			path = self.root / workspace
			path.mkdir()
			(path / "Cargo.toml").write_text('[workspace]\nmembers = ["alpha", "beta", "app"]\nresolver = "2"\n')
			for name in ("alpha", "beta", "app"):
				crate = path / name
				(crate / "src").mkdir(parents=True)
				manifest = f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
				if name == "app":
					manifest += '[dependencies]\nalpha = { path = "../alpha" }\nbeta = { path = "../beta" }\n'
					(crate / "src/main.rs").write_text('fn main() { println!("{}", alpha::value() + beta::value()); }\n')
				else:
					(crate / "src/lib.rs").write_text('pub fn value() -> u8 { 1 }\n')
				(crate / "Cargo.toml").write_text(manifest)
		env = {name: value for name, value in os.environ.items() if name not in (
			"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "RBE_BUILD_CACHE_DIR", "XDG_CACHE_HOME",
		)}
		# An empty Cargo home proves path discovery and fixture builds need no
		# registry downloads or third-party crates, even on a cold machine.
		env["CARGO_HOME"] = str(self.root / "cargo-home")
		self.environment = patch.dict(os.environ, env, clear=True)
		self.environment.start()
		self.addCleanup(self.environment.stop)

	def test_explicit_target_and_rbe_cache_precedence(self):
		with patch.dict(os.environ, {"CARGO_TARGET_DIR": "explicit", "CARGO_BUILD_TARGET_DIR": "other", "RBE_BUILD_CACHE_DIR": "rbe"}):
			self.assertEqual(build_cache.select_target(self.root)[0], self.root / "explicit")
		with patch.dict(os.environ, {"CARGO_BUILD_TARGET_DIR": "cargo-env", "RBE_BUILD_CACHE_DIR": "rbe"}):
			self.assertEqual(build_cache.select_target(self.root)[0], self.root / "cargo-env")
		with patch.dict(os.environ, {"RBE_BUILD_CACHE_DIR": "rbe"}):
			self.assertEqual(build_cache.select_target(self.root)[0], self.root / "rbe/cargo-target")

	def test_cargo_config_and_paths_with_spaces_are_respected(self):
		config = self.root / "engine/.cargo"
		config.mkdir()
		target = self.root / "configured target with spaces"
		(config / "config.toml").write_text(f'[build]\ntarget-dir = {json.dumps(str(target))}\n')
		with patch.dict(os.environ, {"XDG_CACHE_HOME": str(self.root / "xdg")}):
			self.assertEqual(build_cache.select_target(self.root)[0], target)

	def test_cargo_home_config_is_respected(self):
		cargo_home = Path(os.environ["CARGO_HOME"])
		cargo_home.mkdir()
		target = self.root / "global target"
		(cargo_home / "config.toml").write_text(f'[build]\ntarget-dir = {json.dumps(str(target))}\n')
		self.assertEqual(build_cache.select_target(self.root)[0], target)

	def test_existing_direct_cargo_cache_and_render_location(self):
		target = self.root / "engine/target"
		target.mkdir()
		(target / ".rustc_info.json").write_text("{}")
		self.assertEqual(build_cache.select_target(self.root)[0], target)
		with patch.dict(os.environ, {"XDG_CACHE_HOME": str(self.root / "xdg")}):
			self.assertEqual(build_cache.select_target(self.root)[0], self.root / "xdg/rbe-build/cargo-target")

	def test_unchanged_crates_are_fresh_and_only_affected_dependents_rebuild(self):
		target = build_cache.configure(self.root)
		env = {**os.environ, "CARGO_TARGET_DIR": str(target)}

		def build():
			result = subprocess.run(
				["cargo", "build", "--offline", "--release", "--workspace", "--message-format=json"],
				cwd=self.root / "engine", env=env, check=True, capture_output=True, text=True,
			)
			artifacts = [json.loads(line) for line in result.stdout.splitlines()]
			return {entry["target"]["name"]: entry["fresh"] for entry in artifacts if entry.get("reason") == "compiler-artifact"}

		self.assertEqual(build(), {"alpha": False, "beta": False, "app": False})
		self.assertEqual(build(), {"alpha": True, "beta": True, "app": True})
		(self.root / "engine/alpha/src/lib.rs").write_text('pub fn value() -> u8 { 2 }\n')
		self.assertEqual(build(), {"alpha": False, "beta": True, "app": False})
		binary = "app.exe" if os.name == "nt" else "app"
		result = subprocess.check_output([str(target / "release" / binary)], text=True)
		self.assertEqual(result.strip(), "3")

	@unittest.skipUnless(shutil.which("pwsh") or shutil.which("powershell"), "PowerShell is not installed")
	def test_powershell_resolves_and_exports_the_same_target(self):
		helper = Path(__file__).with_name("build-cache.ps1").resolve()
		# Exercise the real adapter with a spaced path and a fresh Cargo home.
		# Its environment export must be available to subsequent Cargo builds.
		script = self.root / "check-cache.ps1"
		quote = lambda value: str(value).replace("'", "''")
		script.write_text(
			f"$ErrorActionPreference = 'Stop'\n. '{quote(helper)}'\n"
			f"Initialize-RbeBuildCache '{quote(Path(__file__).parent.resolve())}'\n"
			"[Console]::WriteLine($env:CARGO_TARGET_DIR)\n",
		)
		target = self.root / "shared cache with spaces"
		result = subprocess.run(
			[shutil.which("pwsh") or shutil.which("powershell"), "-NoProfile", "-File", str(script)],
			env={**os.environ, "RBE_BUILD_CACHE_DIR": str(target)},
			check=True, capture_output=True, text=True,
		)
		self.assertEqual(Path(result.stdout.strip()), target / "cargo-target")
		self.assertTrue((target / "cargo-target").is_dir())


if __name__ == "__main__":
	unittest.main()
