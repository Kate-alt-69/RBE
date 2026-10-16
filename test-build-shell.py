#!/usr/bin/env python3
"""Check every Unix builder and exercise packaging without compiling Rust.

RBE_TEST_BASH selects the interpreter, including macOS's system Bash 3.2.
The disposable fixture substitutes Cargo and uname; real caches and dist files
are never touched, and no signing material is printed or saved.
"""

import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent
BASH = os.environ.get("RBE_TEST_BASH", "/bin/bash")


class ShellBuilderTests(unittest.TestCase):
	def test_each_script_parses(self):
		# bash -n accepts only one script; additional paths become its arguments.
		for script in sorted(ROOT.glob("build*.sh")):
			with self.subTest(script=script.name):
				result = subprocess.run([BASH, "-n", str(script)], capture_output=True, text=True)
				self.assertEqual(result.returncode, 0, result.stderr)

	def test_deferred_admin_packaging_on_native_targets(self):
		for system, arch, target in (
			("Darwin", "x86_64", "x86_64-apple-darwin"),
			("Darwin", "arm64", "aarch64-apple-darwin"),
			("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
			("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
		):
			for explicit in (False, True):
				with self.subTest(target=target, explicit=explicit), tempfile.TemporaryDirectory() as temporary:
					root = Path(temporary).resolve()
					shutil.copyfile(ROOT / "build-core.sh", root / "build-core.sh")
					for directory in ("engine", "container-runtime", "bin"):
						(root / directory).mkdir()
					# All subprocesses use fixtures; rustup must not install targets.
					fixtures = {
						"uname": 'import os, sys\nprint(os.environ["FAKE_SYSTEM" if sys.argv[1] == "-s" else "FAKE_ARCH"])\n',
						"rustup": 'import os, sys\nassert sys.argv[1:] == ["target", "list", "--installed"]\nprint(os.environ["FAKE_TARGET"])\n',
						"cargo": '''import json, os, pathlib, sys
args = sys.argv[1:]
assert args[0] == "build", args
target = args[args.index("--target") + 1]
assert target == os.environ["FAKE_TARGET"], target
assert os.environ.get("RBE_DENY_ADMIN_SETUP") == "1"
assert not any(name in os.environ for name in (
	"RBE_ADMIN_PASSWORD", "RBE_ADMIN_AUTH_ROUNDS",
	"RBE_ADMIN_AUTH_SALT_HEX", "RBE_ADMIN_AUTH_VERIFIER_HEX",
))
package = args[args.index("-p") + 1]
binary = args[args.index("--bin") + 1] if "--bin" in args else package
path = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / target / "release" / binary
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(binary)
with open(os.environ["FAKE_CALLS"], "a") as calls:
	calls.write(json.dumps(args) + "\\n")
''',
					}
					for name, body in fixtures.items():
						path = root / "bin" / name
						path.write_text(f"#!{sys.executable}\n{body}")
						path.chmod(0o755)
					# Keep interpreter/tool lookup while avoiding any user's real secrets.
					env = {name: value for name, value in os.environ.items() if not name.startswith("RBE_")}
					env.update({
						"PATH": str(root / "bin") + os.pathsep + os.environ["PATH"],
						"CARGO_TARGET_DIR": str(root / "cargo cache"),
						"FAKE_SYSTEM": system, "FAKE_ARCH": arch, "FAKE_TARGET": target,
						"FAKE_CALLS": str(root / "calls.jsonl"),
						"RBE_CONTAINER_SIGNING_PRIVATE_KEY": secrets.token_hex(32),
						"RBE_BUILD_ID": "shell-fixture",
					})
					args = [BASH, str(root / "build-core.sh"), "--deny-admin-setup"]
					if explicit:
						args.append(f"--target={target}")
					result = subprocess.run(args, cwd=root, env=env, capture_output=True, text=True)
					self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
					output = root / "dist" / target
					for artifact in ("backend", "cloud_node", "dep/service", "dep/container"):
						self.assertTrue((output / artifact).is_file(), artifact)
					calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
					self.assertEqual(len(calls), 4)


if __name__ == "__main__":
	unittest.main()
