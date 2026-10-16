#!/usr/bin/env python3
"""Offline contract tests for snapshot integrity and binary-only Git publication."""
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("snapshots", Path(__file__).with_name("binary-snapshots.py"))
snapshots = importlib.util.module_from_spec(spec)
spec.loader.exec_module(snapshots)

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
			# A newer manifest already on the server must win over this older run.
			with patch.object(snapshots, "get_json", return_value=dict(self.meta, run=43)):
				snapshots.publish(self.assets, self.meta, "test/repo")
			self.assertEqual(snapshots.git("ls-remote", "origin", f"refs/tags/{snapshots.LATEST}"), latest)
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

if __name__ == "__main__":
	unittest.main()
