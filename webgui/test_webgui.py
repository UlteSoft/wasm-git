"""Native integration checks; run with WASM_GIT_CLIENT/WASM_GIT_SERVER set. No dependencies."""
from __future__ import annotations

import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from app import Client, Problem, Server, client_path, read_state
from jobs import JobManager
import supervision

ROOT = Path(__file__).resolve().parents[1]
CLIENT = Path(os.environ.get("WASM_GIT_CLIENT", ROOT / ".tmp/target/debug/wasm-git")).resolve()
SERVER = Path(os.environ.get("WASM_GIT_SERVER", ROOT / ".tmp/target/debug/wasm-git-server")).resolve()


def wasm(value):
    return bytes.fromhex("0061736d010000000105016000017f030201000709010576616c756500000a0601040041") + bytes([value, 11])


def native(*args):
    return subprocess.check_output([str(CLIENT), *map(str, args)], text=True)


def fingerprint(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*") if p.is_file()}


class WebGUI(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not CLIENT.is_file() or not SERVER.is_file():
            raise RuntimeError("Set WASM_GIT_CLIENT and WASM_GIT_SERVER to built native executables.")
        scratch = ROOT / ".tmp/webgui-tests"
        scratch.mkdir(parents=True, exist_ok=True)
        cls.temp = tempfile.TemporaryDirectory(dir=scratch)
        cls.root = Path(cls.temp.name)
        cls.client = Client(CLIENT, timeout=15)
        cls.server_client = Client(SERVER, timeout=15)
        cls.token = secrets.token_hex(32)
        cls.http = Server(0, cls.client, None, cls.token, cls.root / "runtime", server_client=cls.server_client, mode="server", root=cls.root)
        cls.thread = threading.Thread(target=cls.http.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.http.shutdown()
        cls.http.server_close()
        cls.thread.join()
        for manager in cls.http.managers.values():
            manager.close()
        cls.client.close()
        cls.server_client.close()
        cls.temp.cleanup()

    def setUp(self):
        self.repo = self.root / secrets.token_hex(5)
        native("init", self.repo)
        (self.repo / "module.wasm").write_bytes(wasm(11))
        native("add", self.repo, "module.wasm")
        native("commit", self.repo, "-m", "Initial module")
        self.first = native("rev-parse", self.repo, "HEAD").strip()
        (self.repo / "module.wasm").write_bytes(wasm(12))
        native("add", self.repo, "module.wasm")
        native("commit", self.repo, "-m", "Update return value")

    def request(self, path, data=None, headers=None, expected=200, raw=False):
        connection = http.client.HTTPConnection("127.0.0.1", self.http.server_port, timeout=20)
        body = None if data is None else json.dumps(data).encode()
        values = {"X-Wasm-Git-Token": self.token, "Content-Type": "application/json"}
        values.update(headers or {})
        connection.request("GET" if data is None else "POST", path, body, values)
        response = connection.getresponse()
        content = response.read()
        self.assertEqual(response.status, expected, content[:1000])
        connection.close()
        return content if raw else json.loads(content)

    def query(self, operation, **data):
        return self.request("/api/query", {"operation": operation, "repository": str(self.repo), **data})["result"]

    def action(self, action, **data):
        return self.request("/api/action", {"action": action, "repository": str(self.repo), **data})["result"]

    def wait_job(self, job, success=0):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            view = self.request("/api/jobs/poll", {"id": job["id"]})
            if not view["running"]:
                if success is not None:
                    self.assertEqual(view["exit_code"], success, view)
                return view
            time.sleep(.02)
        self.fail("job did not complete")

    def job(self, args, **options):
        return self.request("/api/jobs/start", {"args": args, "cwd": str(self.repo), **options})

    def test_auth_host_origin_and_static_assets(self):
        for headers, code in [({"X-Wasm-Git-Token": ""}, 401), ({"Host": "evil.example"}, 403),
                               ({"Origin": "https://evil.example"}, 403), ({"Sec-Fetch-Site": "cross-site"}, 403),
                               ({"X-Wasm-Git-Token": "é"}, 401)]:
            self.request("/api/session", headers=headers, expected=code)
        for path in ("/", "/style.css", "/app.js", "/catalog.js", "/server.js", "/locales.js", "/i18n.js"):
            self.assertGreater(len(self.request(path, raw=True)), 100)
        self.request("/../Cargo.toml", expected=404)
        self.request("/api/query", [], expected=400)
        self.request("/api/query", {"operation": "exec", "repository": str(self.repo)}, expected=400)
        self.request("/api/query", {"operation": "tree", "repository": str(self.repo), "revision": "--help"}, expected=400)

    def test_previews_are_read_only_and_byte_accurate(self):
        before = fingerprint(self.repo)
        overview = self.query("overview")
        self.assertEqual(overview["status"]["worktree"], "clean")
        self.assertEqual(len(self.query("history")["commits"]), 2)
        self.assertEqual(self.query("tree")["entries"][0]["path"], "module.wasm")
        result = self.query("diff", old="HEAD^", new="HEAD", mode="revisions")
        audit = result["files"][0]["diff"]["audit"]
        restored, cursor = bytearray(), 0
        for edit in audit["edits"]:
            start, end = edit["old_span"]["start"], edit["old_span"]["end"]
            self.assertEqual(wasm(11)[start:end].hex(), edit["old_hex"])
            restored.extend(wasm(11)[cursor:start]); restored.extend(bytes.fromhex(edit["new_hex"])); cursor = end
        restored.extend(wasm(11)[cursor:])
        self.assertEqual(bytes(restored), wasm(12))
        self.assertIn("I32Const", self.query("diff", old="HEAD^", new="HEAD", mode="revisions", representation="text"))
        self.assertTrue(self.query("diff", old="HEAD^", new="HEAD", section="code")["binary_changed"])
        self.assertTrue(self.query("inspect", file="module.wasm")["function_analysis"]["available"])
        self.assertIn("debug_info", self.query("debug", file="module.wasm"))
        self.assertEqual(fingerprint(self.repo), before)

    def test_staging_commit_branches_stash_gc_and_failure_receipts(self):
        (self.repo / "module.wasm").write_bytes(wasm(13))
        self.assertEqual(self.action("stage", file="module.wasm")["exit_code"], 0)
        self.assertTrue(self.query("overview")["status"]["staged"])
        self.assertEqual(self.action("unstage", file="module.wasm")["exit_code"], 0)
        self.assertFalse(self.query("overview")["status"]["staged"])
        self.assertEqual(self.action("stage-all")["exit_code"], 0)
        self.assertEqual(self.action("commit", message="Exact message\n\nMore context")["exit_code"], 0)
        self.assertEqual(self.query("history")["commits"][0]["message"], "Exact message\n\nMore context")
        self.assertEqual(self.action("branch-create", name="review")["exit_code"], 0)
        self.assertEqual(self.action("switch", name="review")["exit_code"], 0)
        self.assertEqual(self.query("overview")["status"]["branch"], "review")
        self.assertNotEqual(self.action("switch", name="missing")["exit_code"], 0)
        (self.repo / "module.wasm").write_bytes(wasm(14))
        self.assertEqual(self.action("stash-save", message="saved")["exit_code"], 0)
        stash = self.query("stashes")[0]
        self.assertEqual(self.action("stash-apply", id=stash["id"])["exit_code"], 0)
        self.assertEqual((self.repo / "module.wasm").read_bytes(), wasm(14))
        self.request("/api/action", {"action": "restore", "repository": str(self.repo), "file": "module.wasm"}, expected=409)
        self.assertEqual(self.action("restore", file="module.wasm", confirmed=True)["exit_code"], 0)
        self.assertEqual(self.action("gc")["exit_code"], 0)
        self.assertEqual(self.action("fsck")["exit_code"], 0)

    def test_literal_paths_unborn_unstage_and_traversal(self):
        root = self.root / secrets.token_hex(5)
        native("init", root)
        path = "$(echo no); unicode 中文[1].wasm"
        (root / path).write_bytes(wasm(1))
        result = self.request("/api/action", {"action": "stage", "repository": str(root), "file": path})
        self.assertEqual(result["result"]["exit_code"], 0, result)
        result = self.request("/api/action", {"action": "unstage", "repository": str(root), "file": path})
        self.assertEqual(result["result"]["exit_code"], 0, result)
        self.assertEqual((root / path).read_bytes(), wasm(1))
        for path in ("../Cargo.toml", str(ROOT / "Cargo.toml"), ".wasm-git/HEAD"):
            self.request("/api/query", {"operation": "inspect", "repository": str(self.repo), "source": "worktree", "file": path}, expected=400)

    def test_full_cli_exact_arguments_binary_stdin_and_output(self):
        message = "literal $(do-not-run) ; <script> & 中文\nsecond line"
        job = self.job(["commit", str(self.repo), "--allow-empty", "-m", message])
        self.wait_job(job)
        self.assertEqual(self.query("history")["commits"][0]["message"], message)
        job = self.job(["cat-file", str(self.repo), "--batch-command", "--buffer"], keep_stdin=True)
        self.request("/api/jobs/input", {"id": job["id"], "base64": base64.b64encode(b"info HEAD\nflush\n").decode(), "eof": True})
        self.wait_job(job)
        output = self.request(f"/api/jobs/{job['id']}/stdout", raw=True)
        self.assertIn(b"commit", output)
        input_path = self.repo / "stdin.bin"
        input_path.write_bytes(wasm(19))
        job = self.job(["hash-object", str(self.repo), "--stdin"], stdin_file=str(input_path))
        self.wait_job(job)
        output = self.request(f"/api/jobs/{job['id']}/stdout", raw=True)
        self.assertEqual(output.decode().strip(), native("hash-object", self.repo, input_path).strip())
        # Native cat-file --batch emits an actual binary Wasm payload unchanged.
        blob = self.query("tree")["entries"][0]["object_id"]
        job = self.job(["cat-file", str(self.repo), "--batch"], keep_stdin=True)
        self.request("/api/jobs/input", {"id": job["id"], "base64": base64.b64encode((blob + "\n").encode()).decode(), "eof": True})
        self.wait_job(job)
        output = self.request(f"/api/jobs/{job['id']}/stdout", raw=True)
        self.assertIn(wasm(12), output)
        self.request("/api/jobs/remove", {"id": job["id"]})
        self.request("/api/jobs/poll", {"id": job["id"]}, expected=400)

    def test_cancel_exclusive_jobs_and_output_budget(self):
        job = self.job(["cat-file", str(self.repo), "--batch"], keep_stdin=True)
        self.request("/api/jobs/start", {"args": ["version"]}, expected=400)
        self.request("/api/action", {"repository": str(self.repo), "action": "gc"}, expected=409)
        self.request("/api/jobs/cancel", {"id": job["id"]})
        done = self.wait_job(job, success=None)
        self.assertIsNotNone(done["reason"])
        manager = JobManager(Client(CLIENT), self.root / secrets.token_hex(5), limit=16)
        try:
            result = manager.start({"args": ["--help"]}, self.repo)
            item = manager.get(result["id"])
            self.assertTrue(item.done.wait(10))
            self.assertIn("limit", item.reason)
            self.assertLessEqual(item.paths[0].stat().st_size, 16)
        finally:
            manager.close()

    def test_server_service_audit_and_native_policy_compile(self):
        root = self.root / secrets.token_hex(5)
        root.mkdir()
        repo = root / "team" / "project"
        repo.parent.mkdir()
        native("clone", self.repo, repo)
        view = self.request("/api/supervision", {"root": str(root)})["result"]
        self.assertEqual(view["repositories"][0]["path"], str(repo))
        job = self.job(["--root", str(root), "--listen", "127.0.0.1:0", "--read-only"], target="service")
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                log = self.request(f"/api/jobs/{job['id']}/stdout", raw=True)
                if log.strip():
                    break
                time.sleep(.02)
            record = json.loads(log)
            self.assertTrue(record["read_only"])
            probe = self.request("/api/supervision", {"operation": "probe", "address": record["listen"]})["result"]
            self.assertTrue(probe["reachable"])
            self.assertEqual(self.action("fsck")["exit_code"], 0)
            admin = self.job(["--help"], target="server")
            self.wait_job(admin)
            self.assertIn(b"compile-protection", self.request(f"/api/jobs/{admin['id']}/stdout", raw=True))
        finally:
            self.request("/api/jobs/cancel", {"id": job["id"]})
            self.wait_job(job, success=None)

        policy = {"version": 1, "principals": [{"name": "alice", "token_sha256": hashlib.sha256(b"gui-test").hexdigest(),
                  "grants": [{"repository": "team/*", "read": True, "write": ["refs/heads/*"], "force": False, "create": True}]}]}
        protection = {"version": 1, "rules": [{"repository": "team/*", "branches": ["refs/heads/main"],
                                               "required_reviews": 0, "required_checks": []}]}
        for command, value in [("compile-policy", policy), ("compile-protection", protection)]:
            source, destination = root / f"{command}.json", root / f"{command}.bin"
            source.write_text(json.dumps(value))
            self.wait_job(self.job([command, str(source), str(destination)], target="server"))
            self.assertTrue(destination.read_bytes().startswith(b"WGTM"))
        job = self.job(["--root", str(root), "--listen", "127.0.0.1:0", "--allow-write",
                        "--policy", str(root / "compile-policy.bin")], target="service")
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                log = self.request(f"/api/jobs/{job['id']}/stdout", raw=True)
                if log.strip():
                    break
                time.sleep(.02)
            address = json.loads(log)["listen"]
            self.action("remote-add", name="gui-test", url=f"http://{address}/team/project")
            (self.repo / "module.wasm").write_bytes(wasm(15))
            self.action("stage-all")
            self.action("commit", message="A supervised receive")
            push = self.job(["push", str(self.repo), "gui-test", "main:main"], environment={"WASM_GIT_REMOTE_TOKEN": "gui-test"})
            self.wait_job(push)
            audit = self.request("/api/supervision", {"operation": "audit", "file": str(root / "receive-audit.bin")})["result"]
            self.assertTrue(any(row["outcome"] == "accepted" and row["actor"] == "alice" for row in audit), audit)
            self.assertEqual(native("rev-parse", repo, "HEAD"), native("rev-parse", self.repo, "HEAD"))
            self.assertNotIn("environment", self.request("/api/jobs"))
        finally:
            self.request("/api/jobs/cancel", {"id": job["id"]})
            self.wait_job(job, success=None)

    @unittest.skipIf(os.name == "nt", "POSIX process-group regression")
    def test_query_timeout_kills_inherited_output_pipe_children(self):
        program = self.root / "inherited-pipe.py"
        program.write_text("import subprocess, sys\nsubprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n")
        client = Client(sys.executable, timeout=.25)
        started = time.monotonic()
        try:
            with self.assertRaisesRegex(Problem, "exceeded"):
                client.run([str(program)])
            self.assertLess(time.monotonic() - started, 5)
            self.assertFalse(client.children)
        finally:
            client.close()

    def test_no_build_and_auto_build_command(self):
        with self.assertRaises(Problem):
            client_path(str(self.root / "missing"), no_build=True)
        # Check automatic source compilation without downloading or building a
        # second toolchain during a test. The normal build is already verified.
        expected = ROOT / ".tmp/webgui-build/release/wasm-git"
        with patch("app.shutil.which", return_value="cargo"), patch("app.subprocess.run") as run, patch("app.Path.is_file", return_value=True):
            run.return_value.returncode = 0
            self.assertEqual(client_path(build=True), expected)
            args = run.call_args.args[0]
            self.assertEqual(args[-2:], ["--bin", "wasm-git"])
            self.assertEqual(run.call_args.kwargs["env"]["CARGO_TARGET_DIR"], str(expected.parent.parent))

    def test_start_status_stop_lifecycle_and_client_server_isolation(self):
        state_dir = self.root / "lifecycle"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0)); port = sock.getsockname()[1]
        base = [sys.executable, str(ROOT / "webgui/client.py")]
        options = ["--port", str(port), "--state-dir", str(state_dir)]
        try:
            result = subprocess.run(base + ["start", "--client", str(CLIENT), "--repo", str(self.repo), "--no-build", *options], capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stderr)
            state = read_state(state_dir / f"{port}.state")
            self.assertIsNotNone(state)
            result = subprocess.run(base + ["status", *options], capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0)
            result = subprocess.run(base + ["start", "--client", str(CLIENT), *options], capture_output=True, timeout=10)
            self.assertIn(b"Already running", result.stdout)
        finally:
            result = subprocess.run(base + ["stop", *options], capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(read_state(state_dir / f"{port}.state"))
        result = subprocess.run(base + ["status", *options], capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
