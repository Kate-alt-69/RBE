#!/usr/bin/env python3
"""Select an existing Cargo target without guessing artifact compatibility."""

import json
import os
from pathlib import Path
import subprocess
import sys


def select_target(root: Path) -> tuple[Path, str]:
	"""Resolve explicit settings first, then ask Cargo about its configuration.

	Cargo owns fingerprint validation, features, profiles, compiler versions and
	dependent rebuilds. We only choose a stable target directory; we never copy
	rlibs between arbitrary caches or change source/fingerprint timestamps.
	"""
	for name in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"):
		if os.environ.get(name):
			return (root / os.environ[name]).resolve(), name
	if os.environ.get("RBE_BUILD_CACHE_DIR"):
		return (root / os.environ["RBE_BUILD_CACHE_DIR"] / "cargo-target").resolve(), "RBE_BUILD_CACHE_DIR"
	# Cargo metadata honors workspace/ancestor/CARGO_HOME config, including
	# relative paths and paths containing spaces. No registry access is needed.
	for workspace in (root / "engine", root / "container-runtime"):
		result = subprocess.run(
			["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"],
			cwd=workspace, check=True, capture_output=True, text=True,
		)
		target = Path(json.loads(result.stdout)["target_directory"]).resolve()
		if target != (workspace / "target").resolve():
			return target, f"Cargo configuration ({workspace.name})"
	# Render needs its restored persistent cache rather than a fresh checkout's
	# default target. On local hosts, also recognize earlier direct Cargo builds.
	if os.environ.get("XDG_CACHE_HOME"):
		return (root / os.environ["XDG_CACHE_HOME"] / "rbe-build/cargo-target").resolve(), "XDG_CACHE_HOME"
	shared = root / ".cache/rbe-build/cargo-target"
	for target in (shared, root / "engine/target", root / "container-runtime/target", root / "target"):
		if (target / ".rustc_info.json").is_file():
			return target.resolve(), "existing Cargo target"
	return shared.resolve(), "RBE local fallback"


def configure(root: Path) -> Path:
	target, source = select_target(root)
	target.mkdir(parents=True, exist_ok=True)
	print(f"RBE shared Cargo artifact cache:\n  target-dir: {target}\n  source: {source}", file=sys.stderr)
	if os.environ.get("RENDER") and os.environ.get("XDG_CACHE_HOME"):
		if target.is_relative_to(Path(os.environ["XDG_CACHE_HOME"]).resolve()):
			print("  persistence: Render build cache", file=sys.stderr)
	return target


if __name__ == "__main__":
	try:
		print(configure(Path(sys.argv[1]).resolve()))
	except (OSError, ValueError, KeyError, subprocess.CalledProcessError):
		print("ERROR: Cannot resolve the Cargo build cache. Check Cargo configuration or set CARGO_TARGET_DIR explicitly.", file=sys.stderr)
		sys.exit(1)
