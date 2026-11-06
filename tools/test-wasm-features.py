#!/usr/bin/env python3
"""Offline lifecycle updater regressions; temporary files are under the project .tmp/."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("update_features", Path(__file__).with_name("update-wasm-features.py"))
update = importlib.util.module_from_spec(spec)
spec.loader.exec_module(update)


def document():
    return {"$schema": "./proposals.schema.json", "proposals": {
        "mvp": {"name": "MVP", "url": "https://example.test/mvp", "phase": "finished", "champions": ["CG"],
                "meeting_notes": {"date": "2017-12-06", "url": "https://example.test/meeting"},
                "affected_specs": ["core"], "spec_version": "1.0"},
        "threads": {"name": "Threads", "url": "https://example.test/threads", "phase": 4, "champions": ["CG"]}}}


def rows(doc=None):
    return update.parse_upstream(json.dumps(doc or document()).encode())


class LifecycleTests(unittest.TestCase):
    def test_withdrawal_keeps_original_identity(self):
        old = rows()
        changed = document()
        changed["proposals"]["threads"].update(phase="inactive", rationale="Replaced by a new encoding")
        new = update.reconcile(old, rows(changed))
        self.assertEqual(new["threads"][2], "inactive")
        self.assertEqual(set(new), set(old))
        self.assertEqual(new["threads"][5], old["threads"][5])

    def test_disappearance_and_reappearance_never_delete(self):
        old = rows()
        current = dict(old)
        del current["threads"]
        new = update.reconcile(old, current)
        self.assertEqual(new["threads"][2], "missing")
        self.assertIn("review", new["threads"][6])
        self.assertEqual(update.reconcile(new, current), new)
        self.assertEqual(update.reconcile(new, old), old)

    def test_upstream_phase_rollback_is_reported_not_hidden(self):
        old = rows()
        current = copy.deepcopy(old)
        current["threads"][2] = "2"
        self.assertEqual(update.reconcile(old, current)["threads"][2], "2")

    def test_new_finished_feature_is_added_without_removing_history(self):
        old = rows()
        changed = document()
        changed["proposals"]["new-feature"] = dict(changed["proposals"]["mvp"], name="New feature", spec_version="4.0")
        current = update.reconcile(old, rows(changed))
        self.assertTrue(old.keys() < current.keys())
        self.assertEqual(current["new-feature"][2], "finished")

    def test_source_roundtrip_handles_quotes_and_unicode(self):
        current = rows()
        current["threads"][1] = '线程 "quoted" \\ path'
        rendered = update.render(current, "a" * 40, "b" * 64)
        self.assertEqual(update.read_snapshot(rendered), current)

    def test_malformed_schema_fails_closed(self):
        for phase in [True, False, 6, 0, "6", "dropped", None]:
            with self.subTest(phase=phase):
                changed = document()
                changed["proposals"]["threads"]["phase"] = phase
                with self.assertRaises(ValueError):
                    rows(changed)
        changed = document()
        changed["proposals"]["threads"]["new_schema_field"] = True
        with self.assertRaises(ValueError):
            rows(changed)
        with self.assertRaises(ValueError):
            update.parse_upstream(b'{"$schema":"x","proposals":{},"proposals":{}}')
        with self.assertRaises(ValueError):
            update.parse_upstream(b'{"$schema":"x","proposals":{}}')

    def test_source_control_characters_are_rejected(self):
        changed = document()
        changed["proposals"]["threads"]["name"] = "title\ninjected source"
        with self.assertRaises(ValueError):
            rows(changed)

    def test_partial_inventory_cannot_mass_deprecate_readers(self):
        previous = {f"feature-{i}": [f"feature-{i}", "Feature", "4", "", "", "https://example.test/", ""] for i in range(30)}
        with self.assertRaises(ValueError):
            update.reconcile(previous, rows())

    def test_check_is_read_only_and_update_touches_only_requested_source(self):
        scratch = Path(__file__).resolve().parents[1] / ".tmp"
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="uwgit-lifecycle-", dir=scratch) as root:
            root = Path(root)
            source = root / "official-input.json"
            source.write_text(json.dumps(document()))
            snapshot = root / "snapshot.rs"
            sentinel = root / "decoder.rs"
            sentinel.write_text("decoder flags never change")
            command = [sys.executable, str(Path(update.__file__)), "--input", str(source),
                       "--revision", "a" * 40, "--snapshot", str(snapshot)]
            self.assertEqual(subprocess.run(command + ["--check"], capture_output=True).returncode, 1)
            self.assertFalse(snapshot.exists())
            subprocess.run(command + ["--update"], capture_output=True, check=True)
            before = snapshot.read_bytes()
            self.assertEqual(subprocess.run(command + ["--check"], capture_output=True).returncode, 0)
            changed = document()
            del changed["proposals"]["threads"]
            source.write_text(json.dumps(changed))
            self.assertEqual(subprocess.run(command + ["--check"], capture_output=True).returncode, 1)
            self.assertEqual(snapshot.read_bytes(), before)
            subprocess.run(command + ["--update"], capture_output=True, check=True)
            self.assertEqual(update.read_snapshot(snapshot.read_text())["threads"][2], "missing")
            source.write_text("{ broken JSON")
            preserved = snapshot.read_bytes()
            self.assertEqual(subprocess.run(command + ["--update"], capture_output=True).returncode, 2)
            self.assertEqual(snapshot.read_bytes(), preserved)
            self.assertEqual(sentinel.read_text(), "decoder flags never change")


if __name__ == "__main__":
    unittest.main()
