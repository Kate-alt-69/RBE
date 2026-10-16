#!/usr/bin/env python3
"""Offline contracts for verified Git builds and per-platform Release refreshes."""
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import urllib.parse
import zipfile
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("snapshots", Path(__file__).with_name("binary-snapshots.py"))
snapshots = importlib.util.module_from_spec(spec)
spec.loader.exec_module(snapshots)

class ReleaseAPI:
	"""Offline model of draft creation, resumable uploads and managed-asset cleanup."""
	def __init__(self):
		self.release = None
		self.assets = []
		self.calls = []
		self.next_id = 1
		self.fail_upload = False

	def __call__(self, repo, method, endpoint, payload=None, upload=None, missing_ok=False):
		self.calls.append((method, endpoint))
		if method == "GET" and endpoint.startswith("releases/tags/"):
			return copy.deepcopy(self.release)
		if method == "POST" and endpoint == "releases":
			self.release = dict(payload, id=1)
			return copy.deepcopy(self.release)
		if method == "GET" and endpoint.startswith("releases/1/assets?"):
			return copy.deepcopy(self.assets)
		if method == "POST" and upload:
			if self.fail_upload:
				raise OSError("simulated upload failure")
			name = urllib.parse.parse_qs(urllib.parse.urlparse(endpoint).query)["name"][0]
			asset = dict(id=self.next_id, name=name, size=upload.stat().st_size, digest=f"sha256:{snapshots.sha(upload)}", state="uploaded")
			self.next_id += 1
			self.assets.append(asset)
			return copy.deepcopy(asset)
		if method == "PATCH" and endpoint == "releases/1":
			self.release.update(payload)
			return copy.deepcopy(self.release)
		if method == "DELETE" and endpoint.startswith("releases/assets/"):
			asset_id = int(endpoint.rsplit("/", 1)[1])
			self.assets = [asset for asset in self.assets if asset["id"] != asset_id]
			return None
		raise AssertionError((method, endpoint))

class SnapshotTests(unittest.TestCase):
	def setUp(self):
		self.temporary = tempfile.TemporaryDirectory()
		self.addCleanup(self.temporary.cleanup)
		self.root = Path(self.temporary.name)
		self.repo = self.root / "source"
		self.repo.mkdir()
		self.root_patch = patch.object(snapshots, "ROOT", self.repo)
		self.root_patch.start()
		self.addCleanup(self.root_patch.stop)
		snapshots.git("init", "-b", "main")
		snapshots.git("config", "user.name", "test")
		snapshots.git("config", "user.email", "test@example.invalid")
		(self.repo / "VERSION").write_text("0.1.0\n")
		(self.repo / "source.rs").write_text("source must stay on main\n")
		snapshots.git("add", ".")
		snapshots.git("commit", "-m", "source")
		self.meta = snapshots.plan(42, 1)
		self.assets = self.root / "assets"
		for target in snapshots.TARGETS:
			dist = self.root / target
			for name in snapshots.names(target):
				path = dist / name
				path.parent.mkdir(parents=True, exist_ok=True)
				path.write_bytes((target + ":" + name).encode() * 4)
			snapshots.snapshot(dist, target, self.assets, self.meta, part_size=80, inspect=False)

	def test_binary_tree_has_no_source_or_parent_and_leaves_checkout_untouched(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		before = snapshots.git("status", "--porcelain")
		commit = snapshots.tree_commit(self.assets, manifest)
		self.assertEqual(snapshots.git("rev-list", "--parents", "-n", "1", commit), commit)
		paths = snapshots.git("ls-tree", "-r", "--name-only", commit).splitlines()
		self.assertIn("manifest.json", paths)
		self.assertTrue(all(path == "manifest.json" or path.startswith("binaries/") for path in paths))
		self.assertEqual(snapshots.git("rev-parse", "HEAD"), self.meta["source"])
		self.assertEqual(snapshots.git("status", "--porcelain"), before)
		self.assertEqual(snapshots.tree_commit(self.assets, manifest), commit)

	def test_all_five_platforms_and_identical_source_are_required(self):
		path = self.assets / "manifests" / f"{snapshots.TARGETS[-1]}.json"
		bundle = json.loads(path.read_text())
		bundle["source"] = "b" * 40
		path.write_bytes(snapshots.encode(bundle))
		with self.assertRaises(ValueError): snapshots.assemble(self.assets, self.meta)
		path.unlink()
		with self.assertRaises(OSError): snapshots.assemble(self.assets, self.meta)

	def test_changed_part_blocks_publication(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		part = next(iter(manifest["targets"][snapshots.TARGETS[0]].values()))["parts"][0]
		(self.assets / part["path"]).write_bytes(b"corrupt")
		with self.assertRaises(ValueError): snapshots.assemble(self.assets, self.meta)

	def test_manifest_rejects_traversal_and_missing_runtime(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		target = snapshots.TARGETS[0]
		for bad in ["../../backend", "/backend", "binaries/foreign/backend"]:
			m = copy.deepcopy(manifest)
			m["targets"][target]["backend"]["parts"][0]["path"] = bad
			with self.assertRaises(ValueError): snapshots.validate(m, target)
		del manifest["targets"][target]["cloud_node"]
		with self.assertRaises(ValueError): snapshots.validate(manifest, target)

	def test_download_reassembles_verified_bundle_and_never_overwrites(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		def response(url, **kwargs):
			path = url.split(f'/{self.meta["tag"]}/', 1)[1]
			return io.BytesIO((self.assets / path).read_bytes())
		destination = self.root / "installed"
		with patch.object(snapshots, "get_json", return_value=manifest), patch.object(snapshots.urllib.request, "urlopen", side_effect=response):
			snapshots.download("test/repo", snapshots.LATEST, snapshots.TARGETS[0], destination)
			for name in snapshots.names(snapshots.TARGETS[0]):
				self.assertEqual((destination / name).read_bytes(), (self.root / snapshots.TARGETS[0] / name).read_bytes())
			with self.assertRaises(ValueError): snapshots.download("test/repo", snapshots.LATEST, snapshots.TARGETS[0], destination)

	def test_damaged_or_alias_racing_download_does_not_publish_destination(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		destination = self.root / "installed"
		with patch.object(snapshots, "get_json", return_value=manifest), patch.object(snapshots.urllib.request, "urlopen", return_value=io.BytesIO(b"truncated")):
			with self.assertRaises(ValueError): snapshots.download("test/repo", snapshots.LATEST, snapshots.TARGETS[0], destination)
		self.assertFalse(destination.exists())
		changed = dict(manifest, source="b" * 40)
		with patch.object(snapshots, "get_json", side_effect=[manifest, changed]):
			with self.assertRaises(ValueError): snapshots.download("test/repo", snapshots.LATEST, snapshots.TARGETS[0], destination)
		self.assertFalse(destination.exists())

	def test_atomic_publication_is_main_only_and_older_runs_cannot_rewind_latest(self):
		remote = self.root / "remote.git"
		subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
		snapshots.git("remote", "add", "origin", str(remote))
		with patch.dict(os.environ, {"GITHUB_REF": "refs/pull/1/merge", "GITHUB_EVENT_NAME": "pull_request"}):
			with self.assertRaises(ValueError): snapshots.publish(self.assets, self.meta, "test/repo")
		with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push"}):
			snapshots.publish(self.assets, self.meta, "test/repo")
			latest = snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}")
			version_ref = snapshots.git("ls-remote", "origin", "refs/tags/rbe-bin-v0.1.0")
			# A newer manifest already on the server must win over this older run.
			with patch.object(snapshots, "get_json", return_value=snapshots.plan(43, 1)):
				snapshots.publish(self.assets, self.meta, "test/repo")
			self.assertEqual(snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}"), latest)
			self.assertEqual(snapshots.git("ls-remote", "origin", "refs/tags/rbe-bin-v0.1.0"), version_ref)
		self.assertEqual(snapshots.git("rev-parse", "HEAD"), self.meta["source"])

	def test_latest_moves_atomically_and_immutable_tags_cannot_be_replaced(self):
		remote = self.root / "remote.git"
		subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
		snapshots.git("remote", "add", "origin", str(remote))
		with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push"}):
			snapshots.publish(self.assets, self.meta, "test/repo")
			old = snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}").split()[0]
			meta = snapshots.plan(42, 2)
			for target in snapshots.TARGETS:
				snapshots.snapshot(self.root / target, target, self.assets, meta, part_size=80, inspect=False)
			with patch.object(snapshots, "get_json", return_value=self.meta):
				snapshots.publish(self.assets, meta, "test/repo")
			new = snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}").split()[0]
			self.assertNotEqual(old, new)
			self.assertEqual(snapshots.git("ls-remote", "origin", f'refs/tags/{self.meta["tag"]}').split()[0], old)
			# Simulate a pre-existing immutable tag pointing at a different tree.
			snapshots.git("push", "--force", "origin", f'{self.meta["source"]}:refs/tags/{meta["tag"]}')
			with self.assertRaises(ValueError): snapshots.publish(self.assets, meta, "test/repo")

	def test_metadata_sequence_and_native_headers_are_validated(self):
		with self.assertRaises(ValueError): snapshots.validate_metadata(dict(self.meta, attempt=2))
		path = self.root / "bad-binary"
		path.write_bytes(b"wrong platform" * 8)
		for target in snapshots.TARGETS:
			with self.assertRaises(ValueError): snapshots.check_header(path, target)

	def test_version_file_changes_and_source_only_fetches_work_for_multi_commit_pushes(self):
		before = snapshots.git("rev-parse", "HEAD")
		(self.repo / "VERSION").write_text("0.2.0\n")
		snapshots.git("add", "VERSION")
		snapshots.git("commit", "-m", "Select binary version")
		(self.repo / "source.rs").write_text("bug fix after version change\n")
		snapshots.git("add", "source.rs")
		snapshots.git("commit", "-m", "Bug fix")
		meta = snapshots.plan(43, 1, previous_source=before)
		self.assertEqual(meta["version"], "0.2.0")
		self.assertEqual(meta["tag"], "rbe-bin-v0.2.0-build.43.1")
		snapshots.validate_metadata(meta)
		# Keep the selected version while assigning every build an immutable ref.
		self.assertEqual(snapshots.plan(44, 1)["tag"], "rbe-bin-v0.2.0-build.44.1")
		# CI fetches only source commits, not every historical binary-only tag.
		shallow = self.root / "shallow"
		subprocess.run(["git", "clone", "--no-tags", "--depth=2", self.repo.as_uri(), str(shallow)], check=True, capture_output=True)
		with patch.object(snapshots, "ROOT", shallow):
			snapshots.git("fetch", "--no-tags", "--depth=1", "origin", before)
			self.assertEqual(snapshots.plan(43, 1, previous_source=before)["tag"], "rbe-bin-v0.2.0-build.43.1")

	def test_unchanged_version_keeps_its_number_and_tracks_the_new_source(self):
		before = snapshots.git("rev-parse", "HEAD")
		(self.repo / "source.rs").write_text("ordinary bug fix\n")
		snapshots.git("add", "source.rs")
		snapshots.git("commit", "-m", "Bug fix")
		self.assertEqual(snapshots.LATEST, "rbe-bin-latest")
		meta = snapshots.plan(43, 2, previous_source=before)
		self.assertEqual(meta["version"], "0.1.0")
		self.assertNotEqual(meta["source"], self.meta["source"])
		self.assertEqual(meta["tag"], "rbe-bin-v0.1.0-build.43.2")
		self.assertEqual(snapshots.plan(45, 1, previous_source="0" * 40)["tag"], "rbe-bin-v0.1.0-build.45.1")

	def test_version_files_and_legacy_metadata_are_validated(self):
		for bad in ("1.00.0", "1.0", "v1.0.0", "1.0.0\nother", "18446744073709551616.0.0"):
			with self.subTest(version=bad), self.assertRaises(ValueError):
				snapshots.version_number(bad)
		(self.repo / "VERSION").write_text("0.0.9\n")
		with self.assertRaisesRegex(ValueError, "must not decrease"):
			snapshots.plan(43, 1, previous_source=self.meta["source"])
		with self.assertRaises(ValueError):
			snapshots.plan(43, 1, previous_source="main")
		legacy = dict(self.meta, version="0.1.0-dev.42.1", tag="rbe-v0.1.0-dev.42.1")
		snapshots.validate_metadata(legacy)
		with self.assertRaises(ValueError):
			snapshots.validate_metadata(dict(legacy, tag="rbe-bin-v0.1.0-dev.42.1-build.42.1"))
		stable = dict(self.meta)
		snapshots.validate_metadata(stable)
		for bad in (dict(stable, tag="rbe-v0.1.0"), dict(stable, tag="rbe-bin-v0.1.0"), dict(stable, tag=snapshots.LATEST), dict(stable, run=0)):
			with self.assertRaises(ValueError): snapshots.validate_metadata(bad)

	def test_new_source_refreshes_same_version_while_previous_build_stays_pinned(self):
		remote = self.root / "remote.git"
		subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
		snapshots.git("remote", "add", "origin", str(remote))
		meta = snapshots.plan(43, 1)
		for target in snapshots.TARGETS:
			snapshots.snapshot(self.root / target, target, self.assets, meta, part_size=80, inspect=False)
		with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push"}):
			snapshots.publish(self.assets, meta, "test/repo")
			original = snapshots.git("ls-remote", "origin", "refs/tags/rbe-bin-v0.1.0").split()[0]
			self.assertEqual(snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}").split()[0], original)
			(self.repo / "source.rs").write_text("new bug fix, same version\n")
			snapshots.git("add", "source.rs")
			snapshots.git("commit", "-m", "Fix without changing VERSION")
			retry = snapshots.plan(44, 1)
			for target in snapshots.TARGETS:
				snapshots.snapshot(self.root / target, target, self.assets, retry, part_size=80, inspect=False)
			with patch.object(snapshots, "get_json", return_value=meta):
				snapshots.publish(self.assets, retry, "test/repo")
			current = snapshots.git("ls-remote", "origin", "refs/tags/rbe-bin-v0.1.0").split()[0]
			self.assertNotEqual(current, original)
			self.assertEqual(snapshots.git("ls-remote", "origin", f'refs/tags/{meta["tag"]}').split()[0], original)
			self.assertEqual(snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}").split()[0], current)
			self.assertEqual(json.loads(snapshots.git("show", f"{current}:manifest.json"))["source"], retry["source"])
			(self.repo / "VERSION").write_text("0.2.0\n")
			snapshots.git("add", "VERSION")
			snapshots.git("commit", "-m", "Select next binary version")
			next_version = snapshots.plan(45, 1)
			for target in snapshots.TARGETS:
				snapshots.snapshot(self.root / target, target, self.assets, next_version, part_size=80, inspect=False)
			with patch.object(snapshots, "get_json", return_value=retry):
				snapshots.publish(self.assets, next_version, "test/repo")
			self.assertEqual(snapshots.git("ls-remote", "origin", "refs/tags/rbe-bin-v0.1.0").split()[0], current)
			self.assertNotEqual(snapshots.git("ls-remote", "origin", "refs/tags/rbe-bin-v0.2.0").split()[0], current)

	def test_each_os_archive_contains_only_its_verified_runtime_and_is_reproducible(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		packages = snapshots.release_packages(self.assets, manifest, self.root / "downloads")
		repeated = snapshots.release_packages(self.assets, manifest, self.root / "repeated")
		self.assertEqual([snapshots.sha(p) for p in packages], [snapshots.sha(p) for p in repeated])
		self.assertEqual(len(packages), 6)
		for target, package in zip(snapshots.TARGETS, packages):
			with self.subTest(target=target):
				if package.suffix == ".zip":
					with zipfile.ZipFile(package) as archive:
						contents = {name: archive.read(name) for name in archive.namelist()}
				else:
					with tarfile.open(package, "r:gz") as archive:
						contents = {entry.name: archive.extractfile(entry).read() for entry in archive.getmembers()}
				self.assertEqual(set(contents), set(snapshots.names(target)) | {"build-info.json"})
				for name in snapshots.names(target):
					self.assertEqual(contents[name], (self.root / target / name).read_bytes())
				self.assertEqual(json.loads(contents["build-info.json"])["tag"], self.meta["tag"])
		self.assertEqual(len(packages[-1].read_text().splitlines()), 5)
		part = next(iter(manifest["targets"][snapshots.TARGETS[0]].values()))["parts"][0]
		(self.assets / part["path"]).write_bytes(b"corrupt")
		with self.assertRaises(ValueError): snapshots.release_packages(self.assets, manifest, self.root / "corrupt")

	def test_release_refresh_uploads_before_switching_links_and_preserves_custom_assets(self):
		manifest = snapshots.assemble(self.assets, self.meta)
		packages = snapshots.release_packages(self.assets, manifest, self.root / "downloads")
		api = ReleaseAPI()
		with patch.object(snapshots, "github_api", side_effect=api):
			snapshots.publish_release("test/repo", manifest, packages, latest=True)
			self.assertFalse(api.release["draft"])
			self.assertIn("Windows x64", api.release["body"])
			self.assertEqual(len(api.assets), 6)
			api.calls.clear()
			snapshots.publish_release("test/repo", manifest, packages, latest=True)
			self.assertFalse(any(method == "POST" for method, _ in api.calls))
			api.assets.append(dict(id=999, name="operator-notes.txt"))
			new = dict(manifest, run=43, tag="rbe-bin-v0.1.0-build.43.1")
			# Source metadata in the package must refer to its own concrete build.
			new_packages = snapshots.release_packages(self.assets, new, self.root / "new-downloads")
			api.assets.append(dict(id=1000, name=new_packages[0].name, size=0, state="starter"))
			api.fail_upload = True
			body = api.release["body"]
			with self.assertRaises(OSError): snapshots.publish_release("test/repo", new, new_packages, latest=True)
			self.assertEqual(api.release["body"], body)
			self.assertFalse(any(asset["id"] == 1000 for asset in api.assets))
			api.fail_upload = False
			api.calls.clear()
			snapshots.publish_release("test/repo", new, new_packages, latest=True)
			patch_at = next(i for i, call in enumerate(api.calls) if call[0] == "PATCH")
			self.assertTrue(all(i > patch_at for i, call in enumerate(api.calls) if call[0] == "DELETE"))
			self.assertEqual({asset["name"] for asset in api.assets}, {p.name for p in new_packages} | {"operator-notes.txt"})
			next(asset for asset in api.assets if asset["name"] == new_packages[0].name)["digest"] = "sha256:" + "0" * 64
			api.calls.clear()
			with self.assertRaises(ValueError): snapshots.publish_release("test/repo", new, new_packages, latest=True)
			self.assertFalse(any(method in ("PATCH", "DELETE") for method, _ in api.calls))

	def test_packaging_failure_does_not_advance_version_or_latest(self):
		with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push", "GITHUB_TOKEN": "unit-test-token"}):
			with patch.object(snapshots, "release_packages", side_effect=ValueError("damaged package")), patch.object(snapshots, "_publish_refs") as refs:
				with self.assertRaisesRegex(ValueError, "damaged package"):
					snapshots.publish(self.assets, self.meta, "test/repo", github_release=True)
				refs.assert_not_called()

	def test_complete_publication_refreshes_release_and_tags_without_reuploading_a_retry(self):
		remote = self.root / "remote.git"
		subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
		snapshots.git("remote", "add", "origin", str(remote))
		api = ReleaseAPI()
		with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push", "GITHUB_TOKEN": "unit-test-token"}), patch.object(snapshots, "github_api", side_effect=api):
			snapshots.publish(self.assets, self.meta, "test/repo", github_release=True)
			self.assertEqual(api.release["tag_name"], "rbe-bin-v0.1.0")
			self.assertEqual(len(api.assets), 6)
			api.calls.clear()
			with patch.object(snapshots, "get_json", return_value=self.meta):
				snapshots.publish(self.assets, self.meta, "test/repo", github_release=True)
			self.assertFalse(any(method == "POST" for method, _ in api.calls))
			(self.repo / "source.rs").write_text("newer source, same release\n")
			snapshots.git("add", "source.rs")
			snapshots.git("commit", "-m", "Bug fix")
			new = snapshots.plan(43, 1)
			for target in snapshots.TARGETS:
				snapshots.snapshot(self.root / target, target, self.assets, new, part_size=80, inspect=False)
			with patch.object(snapshots, "get_json", return_value=self.meta):
				snapshots.publish(self.assets, new, "test/repo", github_release=True)
			self.assertIn(new["source"], api.release["body"])
			self.assertEqual(api.release["tag_name"], "rbe-bin-v0.1.0")
			self.assertTrue(all("43.1" in asset["name"] for asset in api.assets))

	def test_release_upload_streams_to_github_without_following_redirects(self):
		seen = {}
		def build_opener(handler):
			seen["handler"] = handler
			class Opener:
				def open(self, request, timeout):
					seen["request"] = request
					seen["bytes"] = b"".join(request.data)
					return io.BytesIO(b'{"id":1}')
			return Opener()
		path = self.root / "upload.zip"
		path.write_bytes(b"bundle" * 50000)
		with patch.dict(os.environ, {"GITHUB_TOKEN": "unit-test-token"}), patch.object(snapshots.urllib.request, "build_opener", side_effect=build_opener):
			snapshots.github_api("test/repo", "POST", "releases/1/assets?name=upload.zip", upload=path)
		self.assertEqual(seen["bytes"], path.read_bytes())
		self.assertEqual(seen["request"].full_url, "https://uploads.github.com/repos/test/repo/releases/1/assets?name=upload.zip")
		self.assertEqual(seen["request"].get_header("Content-length"), str(path.stat().st_size))
		self.assertIsNone(seen["handler"].redirect_request(None, None, 302, "", {}, "https://untrusted.invalid"))

if __name__ == "__main__":
	unittest.main()
