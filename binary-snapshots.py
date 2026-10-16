#!/usr/bin/env python3
"""Verified binary tags and OS-specific GitHub Release downloads."""
import argparse
import hashlib
import gzip
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tarfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent
PART_SIZE = 80 * 1024 * 1024
PLATFORMS = {
	"x86_64-unknown-linux-gnu": ("linux-x64", "Linux x64", "tar.gz"),
	"aarch64-unknown-linux-gnu": ("linux-arm64", "Linux ARM64", "tar.gz"),
	"x86_64-pc-windows-msvc": ("windows-x64", "Windows x64", "zip"),
	"x86_64-apple-darwin": ("macos-intel", "macOS Intel", "tar.gz"),
	"aarch64-apple-darwin": ("macos-arm64", "macOS Apple Silicon", "tar.gz"),
}
TARGETS = tuple(PLATFORMS)
LATEST = "rbe-bin-latest"
VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"

def version_number(value):
	"""Canonical numeric versions have the same bounds as the runtime updater."""
	if not isinstance(value, str) or not re.fullmatch(VERSION_PATTERN, value):
		raise ValueError("VERSION must contain major.minor.patch without leading zeroes")
	parts = tuple(map(int, value.split(".")))
	if any(part > 2 ** 64 - 1 for part in parts):
		raise ValueError("VERSION component exceeds the supported range")
	return parts

def previous_version(source):
	"""Compare the whole push, including merge commits and multi-commit pushes."""
	if source is None:
		parents = git("rev-list", "--parents", "-n", "1", "HEAD").split()
		source = parents[1] if len(parents) > 1 else "0" * 40
	if not re.fullmatch(r"[0-9a-f]{40}", source):
		raise ValueError("previous source must be a full commit SHA")
	if source == "0" * 40:
		return None
	# GitHub's before/base SHA is authoritative. Only that commit is needed;
	# requiring its ancestry would force CI to fetch all intervening history.
	git("cat-file", "-e", f"{source}^{{commit}}")
	if not git("ls-tree", source, "--", "VERSION"):
		return None
	return git("show", f"{source}:VERSION").strip()

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

def plan(run, attempt, previous_source=None):
	base = (ROOT / "VERSION").read_text().strip()
	number = version_number(base)
	if run < 1 or attempt < 1:
		raise ValueError("build numbers must be positive")
	previous = previous_version(previous_source)
	if previous is not None and number < version_number(previous):
		raise ValueError("VERSION must not decrease; publish a new higher version")
	return validate_metadata(dict(format=1, version=base, tag=f"rbe-bin-v{base}-build.{run}.{attempt}", source=git("rev-parse", "HEAD"),
		run=run, attempt=attempt, update_protocol=1, credential_format=1))

def metadata():
	return validate_metadata(json.loads(os.environ["RBE_BINARY_METADATA"]))

def validate_metadata(value):
	if set(value) != {"format", "version", "tag", "source", "run", "attempt", "update_protocol", "credential_format"}:
		raise ValueError("invalid build metadata fields")
	version = value["version"]
	if not isinstance(version, str):
		raise ValueError("invalid binary version")
	base, separator, sequence = version.partition("-dev.")
	version_number(base)
	expected = f'rbe-bin-v{version}-build.{value["run"]}.{value["attempt"]}'
	# Previously published development tags remain downloadable and pinnable.
	legacy = separator and value["tag"] in (f"rbe-v{version}", f"rbe-bin-v{version}")
	if len(value["tag"]) >= 100 or not (value["tag"] == expected and not separator or legacy):
		raise ValueError("invalid snapshot tag")
	if not re.fullmatch(r"[0-9a-f]{40}", value["source"]):
		raise ValueError("invalid source commit")
	if (value["format"], value["update_protocol"], value["credential_format"]) != (1, 1, 1):
		raise ValueError("unsupported metadata format")
	if any(type(value[key]) is not int or not 1 <= value[key] <= 2 ** 64 - 1 for key in ("run", "attempt")):
		raise ValueError("invalid build sequence")
	if separator and sequence != f'{value["run"]}.{value["attempt"]}':
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

def publish(assets, meta, repo, github_release=False):
	if os.environ.get("GITHUB_REF") != "refs/heads/main" or os.environ.get("GITHUB_EVENT_NAME") not in ("push", "workflow_dispatch"):
		raise ValueError("binary tag publication is restricted to trusted main workflows")
	if git("rev-parse", "HEAD") != meta["source"]:
		raise ValueError("publisher checkout does not match the source build")
	if github_release and not os.environ.get("GITHUB_TOKEN"):
		raise ValueError("GITHUB_TOKEN is required to publish GitHub Release downloads")
	manifest = assemble(assets, meta)
	with tempfile.TemporaryDirectory() as temporary:
		# Local packaging failures must leave both mutable aliases untouched.
		packages = release_packages(assets, manifest, Path(temporary)) if github_release else []
		commit, updates = _publish_refs(assets, manifest, repo)
		if github_release and f'rbe-bin-v{version_base(meta)}' in updates:
			publish_release(repo, manifest, packages, latest=LATEST in updates)
	print(f'Published verified binary build {meta["tag"]} at {commit}; source main is unchanged.')

def _publish_refs(assets, meta, repo):
	"""Move the version/latest aliases and publish an immutable build as one Git update."""
	commit = tree_commit(assets, meta)
	tag = meta["tag"]
	version_tag = f'rbe-bin-v{version_base(meta)}'
	refs = dict(line.split()[::-1] for line in git("ls-remote", "--tags", "origin", *[f"refs/tags/{ref}" for ref in (tag, version_tag, LATEST)]).splitlines())
	if refs.get(f"refs/tags/{tag}", commit) != commit:
		raise ValueError("refusing to overwrite an immutable binary build tag")
	args = ["push", "--atomic"]
	updates = []
	for alias in (version_tag, LATEST):
		old = refs.get(f"refs/tags/{alias}", "")
		if old:
			previous = get_json(repo, old)
			validate_metadata({key: value for key, value in previous.items() if key != "targets"})
			if alias == version_tag and version_base(previous) != version_base(meta):
				raise ValueError("version alias points at a different version")
			if (meta["run"], meta["attempt"]) < (previous["run"], previous["attempt"]):
				continue
		args.append(f"--force-with-lease=refs/tags/{alias}:{old}")
		updates.append(alias)
	args.extend(["origin", f"{commit}:refs/tags/{tag}"])
	args.extend(f"{commit}:refs/tags/{alias}" for alias in updates)
	git(*args)
	return commit, updates

def version_base(meta):
	return meta["version"].split("-dev.")[0]

def copy_verified_part(handle, out, part):
	"""Bound both network and local part reads and verify before packaging/installing."""
	digest = hashlib.sha256()
	remaining = part["size"]
	while remaining:
		data = handle.read(min(65536, remaining))
		if not data:
			raise ValueError("truncated binary part")
		digest.update(data)
		out.write(data)
		remaining -= len(data)
	if handle.read(1) or digest.hexdigest() != part["sha256"]:
		raise ValueError("binary part size/checksum mismatch")

def release_packages(assets, manifest, output):
	"""Build deterministic per-OS archives from verified parts, one platform at a time."""
	output.mkdir(parents=True, exist_ok=True)
	packages = []
	for target in TARGETS:
		files = validate(manifest, target)
		label, _, extension = PLATFORMS[target]
		path = output / f'rbe-{version_base(manifest)}-{label}-build.{manifest["run"]}.{manifest["attempt"]}.{extension}'
		with tempfile.TemporaryDirectory() as temporary:
			root = Path(temporary)
			for name, binary in files.items():
				destination = root / name
				destination.parent.mkdir(parents=True, exist_ok=True)
				with destination.open("xb") as out:
					for part in binary["parts"]:
						source = assets / part["path"]
						if source.is_symlink():
							raise ValueError("linked binary part cannot be packaged")
						with source.open("rb") as handle:
							copy_verified_part(handle, out, part)
				if destination.stat().st_size != binary["size"] or sha(destination) != binary["sha256"]:
					raise ValueError("reconstructed binary checksum mismatch")
			info = dict(manifest, targets={target: files})
			(root / "build-info.json").write_bytes(encode(info))
			entries = sorted([*files, "build-info.json"])
			if extension == "zip":
				with zipfile.ZipFile(path, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
					for name in entries:
						entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
						entry.create_system = 3
						entry.compress_type = zipfile.ZIP_DEFLATED
						entry.external_attr = (0o100644 if name == "build-info.json" else 0o100755) << 16
						with (root / name).open("rb") as source, archive.open(entry, "w") as destination:
							shutil.copyfileobj(source, destination, 65536)
			else:
				with path.open("xb") as out, gzip.GzipFile(filename="", mode="wb", fileobj=out, mtime=0) as compressed:
					with tarfile.open(fileobj=compressed, mode="w") as archive:
						for name in entries:
							entry = tarfile.TarInfo(name)
							entry.size = (root / name).stat().st_size
							entry.mode = 0o644 if name == "build-info.json" else 0o755
							with (root / name).open("rb") as source:
								archive.addfile(entry, source)
		if path.stat().st_size > 2 * 1024 ** 3:
			raise ValueError("OS bundle exceeds GitHub's 2 GiB asset limit")
		packages.append(path)
	checksums = output / f'SHA256SUMS-build.{manifest["run"]}.{manifest["attempt"]}.txt'
	checksums.write_bytes("".join(f"{sha(path)}  {path.name}\n" for path in packages).encode("utf-8"))
	return [*packages, checksums]

def github_api(repo, method, endpoint, payload=None, upload=None, missing_ok=False):
	"""Credentials are sent only to GitHub API/upload hosts, never a response URL."""
	token = os.environ.get("GITHUB_TOKEN")
	if not token:
		raise ValueError("GITHUB_TOKEN is required for GitHub Releases")
	headers = {"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json",
		"User-Agent": "RBE-binary-publisher/1", "X-GitHub-Api-Version": "2022-11-28"}
	host = "uploads.github.com" if upload else "api.github.com"
	url = f"https://{host}/repos/{repository(repo)}/{endpoint}"
	class NoRedirect(urllib.request.HTTPRedirectHandler):
		def redirect_request(self, req, fp, code, msg, headers, newurl):
			return None
	opener = urllib.request.build_opener(NoRedirect())
	def request(data):
		try:
			with opener.open(urllib.request.Request(url, data=data, headers=headers, method=method), timeout=600) as response:
				raw = response.read(2 * 1024 * 1024 + 1)
		except urllib.error.HTTPError as error:
			if missing_ok and error.code == 404:
				return None
			raise ValueError(f"GitHub Release {method} failed ({error.code}); retry the publish job") from None
		if len(raw) > 2 * 1024 * 1024:
			raise ValueError("GitHub Release response exceeds its size limit")
		return json.loads(raw) if raw else None
	if upload:
		headers["Content-Type"] = "application/octet-stream"
		headers["Content-Length"] = str(upload.stat().st_size)
		with upload.open("rb") as handle:
			return request(iter(lambda: handle.read(65536), b""))
	headers["Content-Type"] = "application/json"
	return request(encode(payload) if payload is not None else None)

def remote_id(value):
	if type(value) is not int or value < 1:
		raise ValueError("invalid GitHub Release resource ID")
	return value

def release_assets(repo, release_id):
	assets = []
	for page in range(1, 101):
		batch = github_api(repo, "GET", f"releases/{release_id}/assets?per_page=100&page={page}")
		if not isinstance(batch, list) or any(not isinstance(asset, dict) or not isinstance(asset.get("name"), str) for asset in batch):
			raise ValueError("invalid GitHub Release asset listing")
		assets.extend(batch)
		if len(batch) < 100:
			return assets
	raise ValueError("GitHub Release has too many assets to refresh safely")

def publish_release(repo, manifest, packages, latest):
	"""Upload a complete revision before changing download links or removing old assets.

	The workflow serializes publishers. Immutable filenames make failed uploads
	retryable while the existing public Release remains usable throughout refresh.
	"""
	tag = f"rbe-bin-v{version_base(manifest)}"
	release = github_api(repo, "GET", f"releases/tags/{tag}", missing_ok=True)
	if release is None:
		release = github_api(repo, "POST", "releases", {"tag_name": tag, "name": f'RBE {version_base(manifest)}', "draft": True})
	if not isinstance(release, dict):
		raise ValueError("invalid GitHub Release response")
	release_id = remote_id(release["id"])
	previous = release_assets(repo, release_id)
	by_name = {asset["name"]: asset for asset in previous}
	for path in packages:
		asset = by_name.get(path.name)
		if asset and asset.get("state") == "starter" and asset.get("size") == 0:
			# GitHub can leave an empty starter asset after a failed upload.
			github_api(repo, "DELETE", f'releases/assets/{remote_id(asset["id"])}')
			asset = None
		if asset is None:
			query = urllib.parse.urlencode({"name": path.name})
			asset = github_api(repo, "POST", f"releases/{release_id}/assets?{query}", upload=path)
		if not isinstance(asset, dict) or asset.get("size") != path.stat().st_size or asset.get("digest") != f"sha256:{sha(path)}":
			raise ValueError("GitHub Release asset checksum mismatch; existing download links were preserved")
	base_url = f"https://github.com/{repository(repo)}/releases/download/{tag}/"
	rows = ["| Platform | Download |", "| --- | --- |"]
	for target, path in zip(TARGETS, packages):
		rows.append(f"| {PLATFORMS[target][1]} | [{path.name}]({base_url}{path.name}) |")
	body = f'Built from [{manifest["source"]}](https://github.com/{repo}/commit/{manifest["source"]}). Build {manifest["run"]}.{manifest["attempt"]}.\n\n'
	body += "Choose your OS and architecture. Each download contains its complete matching runtime bundle.\n\n" + "\n".join(rows)
	body += f"\n\n[SHA-256 checksums]({base_url}{packages[-1].name})\n\nVerified build tag: `{manifest['tag']}`. This version's downloads refresh when a newer source build succeeds.\n"
	github_api(repo, "PATCH", f"releases/{release_id}", {"name": f'RBE {version_base(manifest)}', "body": body, "draft": False, "prerelease": False, "make_latest": "true" if latest else "false"})
	# Preserve manually uploaded files; remove only this publisher's obsolete bundles.
	patterns = [rf'rbe-{re.escape(version_base(manifest))}-{label}-build\.[1-9][0-9]*\.[1-9][0-9]*\.{re.escape(extension)}' for label, _, extension in PLATFORMS.values()]
	patterns.append(r"SHA256SUMS-build\.[1-9][0-9]*\.[1-9][0-9]*\.txt")
	current = {path.name for path in packages}
	for asset in previous:
		if asset["name"] not in current and any(re.fullmatch(pattern, asset["name"]) for pattern in patterns):
			github_api(repo, "DELETE", f'releases/assets/{remote_id(asset["id"])}')

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
					with urllib.request.urlopen(url, timeout=180) as response:
							copy_verified_part(response, out, part)
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
	p.add_argument("--previous-source", help="Full source SHA before the push; defaults to HEAD's first parent")
	p = sub.add_parser("snapshot"); p.add_argument("--dist", type=Path, required=True); p.add_argument("--target", choices=TARGETS, required=True); p.add_argument("--output", type=Path, required=True)
	p = sub.add_parser("publish"); p.add_argument("--assets", type=Path, required=True); p.add_argument("--repository", required=True)
	p.add_argument("--github-release", action="store_true", help="Refresh the version's GitHub Release with OS-specific bundles")
	p = sub.add_parser("download"); p.add_argument("--repository", default="Kate-alt-69/RBE"); p.add_argument("--tag", default=LATEST); p.add_argument("--target", choices=TARGETS, required=True); p.add_argument("--destination", type=Path, required=True)
	for command in sub.choices.values():
		command.add_argument("-help", action="help")
	args = parser.parse_args()
	if args.command == "plan":
		meta = plan(args.run, args.attempt, args.previous_source)
		if args.github_output:
			with args.github_output.open("a") as handle:
				handle.write(f'metadata={json.dumps(meta, separators=(",", ":"))}\nversion={meta["version"]}\n')
		print(encode(meta).decode(), end="")
	elif args.command == "snapshot":
		snapshot(args.dist, args.target, args.output, metadata())
	elif args.command == "publish":
		publish(args.assets, metadata(), repository(args.repository), args.github_release)
	else:
		result = download(args.repository, args.tag, args.target, args.destination)
		print(f'Verified {result["version"]} from {result["source"]} in {args.destination}')

if __name__ == "__main__":
	try:
		main()
	except (ValueError, OSError, subprocess.SubprocessError, KeyError, TypeError) as error:
		raise SystemExit(f"RBE binary snapshot failed: {error}") from None
