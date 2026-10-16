#!/usr/bin/env python3
"""Binary-only Git snapshots. Source main is never rewritten and no Releases are created."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parent
PART_SIZE = 80 * 1024 * 1024
TARGETS = (
	"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
	"x86_64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin",
)
LATEST = "rbe-binaries-latest"

def git(*args, env=None, data=None):
	return subprocess.check_output(["git", "-C", str(ROOT), *args], env=env, input=data, text=True).strip()

def encode(value):
	return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

def sha(path):
	with Path(path).open("rb") as handle:
		digest = hashlib.sha256()
		while data := handle.read(65536):
			digest.update(data)
		return digest.hexdigest()

def names(target):
	ext = ".exe" if "windows" in target else ""
	return [name + ext for name in ("backend", "cloud_node", "dep/container", "dep/service")]

def plan(run, attempt):
	base = (ROOT / "VERSION").read_text().strip()
	if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", base):
		raise ValueError("VERSION must contain a base major.minor.patch version")
	if run < 1 or attempt < 1:
		raise ValueError("build numbers must be positive")
	version = f"{base}-dev.{run}.{attempt}"
	return dict(format=1, version=version, tag=f"rbe-v{version}", source=git("rev-parse", "HEAD"),
		run=run, attempt=attempt, update_protocol=1, credential_format=1)

def metadata():
	return validate_metadata(json.loads(os.environ["RBE_BINARY_METADATA"]))

def validate_metadata(value):
	if set(value) != {"format", "version", "tag", "source", "run", "attempt", "update_protocol", "credential_format"}:
		raise ValueError("invalid build metadata fields")
	if not re.fullmatch(r"rbe-v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)-dev\.[1-9][0-9]*\.[1-9][0-9]*", value["tag"]) or value["tag"] != f'rbe-v{value["version"]}':
		raise ValueError("invalid snapshot tag")
	if not re.fullmatch(r"[0-9a-f]{40}", value["source"]):
		raise ValueError("invalid source commit")
	if (value["format"], value["update_protocol"], value["credential_format"]) != (1, 1, 1):
		raise ValueError("unsupported metadata format")
	if any(type(value[key]) is not int or value[key] < 1 for key in ("run", "attempt")):
		raise ValueError("invalid build sequence")
	if not value["version"].endswith(f'-dev.{value["run"]}.{value["attempt"]}'):
		raise ValueError("build version and sequence disagree")
	return value

def check_header(path, target):
	"""Catch mismatched cross-platform staging before publishing a complete snapshot."""
	with path.open("rb") as handle:
		header = handle.read(64)
	if "linux" in target:
		machine = 183 if target.startswith("aarch64") else 62
		valid = header[:6] == b"\x7fELF\x02\x01" and int.from_bytes(header[18:20], "little") == machine
	elif "windows" in target:
		valid = header[:2] == b"MZ"
		if valid:
			with path.open("rb") as handle:
				handle.seek(int.from_bytes(header[60:64], "little"))
				valid = handle.read(6) == b"PE\0\0\x64\x86"
	else:
		cpu = 0x0100000c if target.startswith("aarch64") else 0x01000007
		valid = header[:4] == b"\xcf\xfa\xed\xfe" and int.from_bytes(header[4:8], "little") == cpu
	if not valid:
		raise ValueError(f"runtime artifact does not match {target}: {path.name}")

def snapshot(dist, target, output, meta, part_size=PART_SIZE, inspect=True):
	if target not in TARGETS or not 0 < part_size <= PART_SIZE:
		raise ValueError("unsupported target or binary part size")
	files = {}
	for name in names(target):
		path = dist / name
		if path.is_symlink() or not path.is_file() or not 0 < path.stat().st_size <= 1024 ** 3:
			raise ValueError(f"missing, linked, or oversized binary: {name}")
		if inspect:
			check_header(path, target)
	if inspect:
		reported = json.loads(subprocess.check_output([str((dist / names(target)[0]).resolve()), "--version-json"], timeout=30))
		if any(reported[key] != meta[key] for key in ("source", "version")) or reported["target"] != target or reported["deferred_admin_setup"] is not True:
			raise ValueError("public binary identity/deferred admin policy does not match the build plan")
	for name in names(target):
		path = dist / name
		parts = []
		with path.open("rb") as handle:
			for index in range((path.stat().st_size + part_size - 1) // part_size):
				data = handle.read(part_size)
				suffix = "" if path.stat().st_size <= part_size else f".part{index:04}"
				relative = f"binaries/{target}/{name}{suffix}"
				destination = output / relative
				destination.parent.mkdir(parents=True, exist_ok=True)
				destination.write_bytes(data)
				parts.append(dict(path=relative, size=len(data), sha256=hashlib.sha256(data).hexdigest()))
		files[name] = dict(size=path.stat().st_size, sha256=sha(path), parts=parts)
	manifest = dict(meta, targets={target: files})
	path = output / "manifests" / f"{target}.json"
	path.parent.mkdir(parents=True, exist_ok=True)
	path.write_bytes(encode(manifest))
	return manifest

def validate(manifest, target):
	validate_metadata({key: value for key, value in manifest.items() if key != "targets"})
	if target not in TARGETS or manifest.get("format") != 1 or manifest.get("update_protocol") != 1 or manifest.get("credential_format") != 1:
		raise ValueError("unsupported binary manifest")
	if manifest.get("tag") != f'rbe-v{manifest.get("version")}' or not re.fullmatch(r"rbe-v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)-dev\.[1-9][0-9]*\.[1-9][0-9]*", manifest["tag"]):
		raise ValueError("invalid binary tag")
	if not re.fullmatch(r"[0-9a-f]{40}", manifest.get("source", "")):
		raise ValueError("invalid binary source commit")
	files = manifest["targets"][target]
	if set(files) != set(names(target)):
		raise ValueError("snapshot is not a complete runtime bundle")
	for name, binary in files.items():
		if not 0 < binary["size"] <= 1024 ** 3 or not re.fullmatch(r"[0-9a-f]{64}", binary["sha256"]):
			raise ValueError("invalid binary size/checksum")
		parts = binary["parts"]
		if not 1 <= len(parts) <= 128 or sum(p["size"] for p in parts) != binary["size"]:
			raise ValueError("invalid binary part bounds")
		for index, part in enumerate(parts):
			expected = f"binaries/{target}/{name}"
			if len(parts) > 1:
				expected += f".part{index:04}"
			if part["path"] != expected or not 0 < part["size"] <= PART_SIZE or not re.fullmatch(r"[0-9a-f]{64}", part["sha256"]):
				raise ValueError("invalid binary part path, size, or checksum")
	return files

def assemble(assets, meta):
	manifest = dict(meta, targets={})
	for target in TARGETS:
		bundle = json.loads((assets / "manifests" / f"{target}.json").read_text())
		if {k: v for k, v in bundle.items() if k != "targets"} != meta or set(bundle["targets"]) != {target}:
			raise ValueError("mixed source/version/platform snapshots cannot be published")
		files = validate(bundle, target)
		for binary in files.values():
			digest = hashlib.sha256()
			for part in binary["parts"]:
				path = assets / part["path"]
				if path.is_symlink() or path.stat().st_size != part["size"] or sha(path) != part["sha256"]:
					raise ValueError("binary part changed before publication")
				with path.open("rb") as handle:
					while data := handle.read(65536):
						digest.update(data)
			if digest.hexdigest() != binary["sha256"]:
				raise ValueError("binary checksum does not match its parts")
		manifest["targets"][target] = files
	(assets / "manifest.json").write_bytes(encode(manifest))
	return manifest

def tree_commit(assets, manifest):
	"""An empty temporary index makes an orphan binary tree, never a source deletion."""
	with tempfile.TemporaryDirectory() as temporary:
		env = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / "index"))
		git("read-tree", "--empty", env=env)
		paths = ["manifest.json"] + [p["path"] for files in manifest["targets"].values() for binary in files.values() for p in binary["parts"]]
		for path in sorted(paths):
			blob = git("hash-object", "-w", "--", str((assets / path).resolve()))
			git("update-index", "--add", "--cacheinfo", "100644", blob, path, env=env)
		tree = git("write-tree", env=env)
		date = git("show", "-s", "--format=%cI", manifest["source"])
		env.update(GIT_AUTHOR_NAME="RBE binary builder", GIT_AUTHOR_EMAIL="rbe-build@users.noreply.github.com",
			GIT_COMMITTER_NAME="RBE binary builder", GIT_COMMITTER_EMAIL="rbe-build@users.noreply.github.com",
			GIT_AUTHOR_DATE=date, GIT_COMMITTER_DATE=date)
		return git("commit-tree", tree, env=env, data=f'RBE binary snapshot {manifest["version"]}\n\nSource: {manifest["source"]}\n')

def repository(value):
	if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*", value) or ".." in value:
		raise ValueError("repository must be owner/name")
	return value

def get_json(repo, reference):
	if not re.fullmatch(r"[A-Za-z0-9.-]+", reference) or ".." in reference:
		raise ValueError("unsafe Git reference")
	with urllib.request.urlopen(f"https://raw.githubusercontent.com/{repository(repo)}/{reference}/manifest.json", timeout=30) as response:
		data = response.read(2 * 1024 * 1024 + 1)
	if len(data) > 2 * 1024 * 1024:
		raise ValueError("binary manifest exceeds its size limit")
	return json.loads(data)

def publish(assets, meta, repo):
	if os.environ.get("GITHUB_REF") != "refs/heads/main" or os.environ.get("GITHUB_EVENT_NAME") not in ("push", "workflow_dispatch"):
		raise ValueError("binary tag publication is restricted to trusted main workflows")
	if git("rev-parse", "HEAD") != meta["source"]:
		raise ValueError("publisher checkout does not match the source build")
	manifest = assemble(assets, meta)
	commit = tree_commit(assets, manifest)
	tag = meta["tag"]
	refs = dict(line.split()[::-1] for line in git("ls-remote", "--tags", "origin", f"refs/tags/{tag}", f"refs/tags/{LATEST}").splitlines())
	if refs.get(f"refs/tags/{tag}", commit) != commit:
		raise ValueError("refusing to overwrite an immutable binary version tag")
	old = refs.get(f"refs/tags/{LATEST}", "")
	update_latest = True
	if old:
		previous = get_json(repo, old)
		update_latest = (meta["run"], meta["attempt"]) >= (previous["run"], previous["attempt"])
	args = ["push", "--atomic"]
	if update_latest:
		args.append(f"--force-with-lease=refs/tags/{LATEST}:{old}")
	args.extend(["origin", f"{commit}:refs/tags/{tag}"])
	if update_latest:
		args.append(f"{commit}:refs/tags/{LATEST}")
	git(*args)
	print(f"Published binary-only tag {tag} at {commit}; source main is unchanged.")

def download(repo, reference, target, destination):
	first = get_json(repo, reference)
	pinned = get_json(repo, first["tag"])
	if first != pinned:
		raise ValueError("moving alias changed during resolution; retry")
	files = validate(pinned, target)
	if destination.exists():
		raise ValueError("download destination must be new; existing installations are never overwritten")
	destination.parent.mkdir(parents=True, exist_ok=True)
	with tempfile.TemporaryDirectory(dir=destination.parent) as temporary:
		root = Path(temporary) / "bundle"
		for name, binary in files.items():
			path = root / name
			path.parent.mkdir(parents=True, exist_ok=True)
			with path.open("xb") as out:
				for part in binary["parts"]:
					url = f'https://raw.githubusercontent.com/{repository(repo)}/{pinned["tag"]}/{part["path"]}'
					digest = hashlib.sha256()
					with urllib.request.urlopen(url, timeout=180) as response:
						remaining = part["size"]
						while remaining:
							data = response.read(min(65536, remaining))
							if not data:
								raise ValueError("truncated binary download")
							digest.update(data)
							out.write(data)
							remaining -= len(data)
						if response.read(1) or digest.hexdigest() != part["sha256"]:
							raise ValueError("binary part size/checksum mismatch")
			if sha(path) != binary["sha256"]:
				raise ValueError("binary checksum mismatch")
			path.chmod(0o700)
		(root / "build-info.json").write_bytes(encode(pinned))
		root.rename(destination)
	return pinned

def main():
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument("-help", action="help")
	sub = parser.add_subparsers(dest="command", required=True)
	p = sub.add_parser("plan"); p.add_argument("--run", type=int, required=True); p.add_argument("--attempt", type=int, default=1); p.add_argument("--github-output", type=Path)
	p = sub.add_parser("snapshot"); p.add_argument("--dist", type=Path, required=True); p.add_argument("--target", choices=TARGETS, required=True); p.add_argument("--output", type=Path, required=True)
	p = sub.add_parser("publish"); p.add_argument("--assets", type=Path, required=True); p.add_argument("--repository", required=True)
	p = sub.add_parser("download"); p.add_argument("--repository", default="Kate-alt-69/RBE"); p.add_argument("--tag", default=LATEST); p.add_argument("--target", choices=TARGETS, required=True); p.add_argument("--destination", type=Path, required=True)
	for command in sub.choices.values():
		command.add_argument("-help", action="help")
	args = parser.parse_args()
	if args.command == "plan":
		meta = plan(args.run, args.attempt)
		if args.github_output:
			with args.github_output.open("a") as handle:
				handle.write(f'metadata={json.dumps(meta, separators=(",", ":"))}\nversion={meta["version"]}\n')
		print(encode(meta).decode(), end="")
	elif args.command == "snapshot":
		snapshot(args.dist, args.target, args.output, metadata())
	elif args.command == "publish":
		publish(args.assets, metadata(), repository(args.repository))
	else:
		result = download(args.repository, args.tag, args.target, args.destination)
		print(f'Verified {result["version"]} from {result["source"]} in {args.destination}')

if __name__ == "__main__":
	try:
		main()
	except (ValueError, OSError, subprocess.SubprocessError, KeyError, TypeError) as error:
		raise SystemExit(f"RBE binary snapshot failed: {error}") from None
