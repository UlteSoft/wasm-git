#!/usr/bin/env python3
"""Local web UI for an already built wasm-git client (Python 3.10+)."""
from __future__ import annotations

import argparse
import base64
import hmac
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import URLError
from urllib.request import ProxyHandler, Request, build_opener
import webbrowser

if __package__:
    from .jobs import JobManager, terminate
    from . import supervision
else:
    from jobs import JobManager, terminate
    import supervision

HERE = Path(__file__).resolve().parent
DEFAULT_STATE = HERE.parent / ".tmp" / "webgui"
STATE = struct.Struct("!8sHQ32s")
MAGIC = b"WGTUI\x00\x00\x01"
MAX_BODY = 32 * 1024
MAX_OUTPUT = 32 * 1024 * 1024
ASSETS = {"/": ("index.html", "text/html"),
          "/app.js": ("app.js", "text/javascript"),
          "/locales.js": ("locales.js", "text/javascript"),
          "/i18n.js": ("i18n.js", "text/javascript"),
          "/catalog.js": ("catalog.js", "text/javascript"),
          "/server.js": ("server.js", "text/javascript"),
          "/style.css": ("style.css", "text/css")}


class Problem(Exception):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def field(data, key, default="", limit=4096):
    value = data.get(key, default)
    if not isinstance(value, str) or len(value) > limit or any(ord(c) < 32 for c in value):
        raise Problem(f"Invalid {key}.")
    return value


def revision(data, key="revision", default="HEAD"):
    value = field(data, key, default, 1024)
    if not value or value.startswith("-"):
        raise Problem(f"{key} must be a revision, branch, tag or commit ID.")
    return value


def repository(value):
    path = Path(value).expanduser().resolve()
    if not path.is_dir() or not (path / ".wasm-git").is_dir():
        raise Problem("Choose a wasm-git repository containing .wasm-git (not a Git .git repository).")
    return path


def client_path(value=None, kind="client", build=False, no_build=False):
    requested = value or os.environ.get("WASM_GIT_CLIENT" if kind == "client" else "WASM_GIT_SERVER")
    executable = "wasm-git" if kind == "client" else "wasm-git-server"
    name = executable + (".exe" if os.name == "nt" else "")
    candidates = [requested] if requested else [shutil.which(name)] + [
        str(HERE.parent / directory / profile / name)
        for directory in ("target", ".tmp/target", ".tmp/webgui-build") for profile in ("release", "debug")]
    for candidate in filter(None, candidates):
        resolved = shutil.which(candidate) or candidate
        path = Path(resolved).expanduser().resolve()
        if not build and path.is_file() and os.access(path, os.X_OK):
            return path
    if (requested and not build) or no_build:
        raise Problem(f"Built {kind} not found. Pass --{kind} /path/to/{name}, or allow automatic compilation.")
    target = HERE.parent / ".tmp/webgui-build"
    target.mkdir(parents=True, exist_ok=True)
    (target / "temp").mkdir(exist_ok=True)
    cargo = shutil.which("cargo")
    if not cargo:
        raise Problem(f"No {kind} binary or Cargo toolchain found. Install Rust or pass --{kind} with an existing executable.")
    command = [cargo, "build", "--locked", "--release", "--bin", executable]
    if kind == "server":
        command += ["--no-default-features", "--features", "server"]
    print(f"Building {name} from this checkout…", flush=True)
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), TMPDIR=str(target / "temp"))
    result = subprocess.run(command, cwd=HERE.parent, env=env, stdin=subprocess.DEVNULL, stdout=sys.stderr)
    if result.returncode:
        raise Problem(f"{kind} build failed (exit {result.returncode}); see Cargo diagnostics above.")
    binary = target / "release" / name
    if not binary.is_file():
        raise Problem(f"Expected build output missing: {binary}. Check Cargo target configuration.")
    return binary


class Client:
    """Only this class starts native processes; no shell or arbitrary command API."""
    def __init__(self, binary, timeout=120, output_limit=MAX_OUTPUT):
        self.binary = str(binary)
        self.timeout = timeout
        self.output_limit = output_limit
        self.slots = threading.BoundedSemaphore(2)
        self.lock = threading.Lock()
        self.children = set()
        self.closed = False
        self.writer = threading.Lock()

    def close(self):
        with self.lock:
            self.closed = True
            children = list(self.children)
        for child in children:
            terminate(child)

    def run(self, args, cwd=None, parse=True, receipt=False):
        if not self.slots.acquire(blocking=False):
            raise Problem("Two client queries are already running. Try again shortly.", 503)
        child = None
        readers = []
        try:
            env = dict(os.environ, NO_COLOR="1", CLICOLOR="0")
            with self.lock:
                if self.closed:
                    raise Problem("The GUI is shutting down.", 503)
                options = {"start_new_session": True} if os.name != "nt" else {
                    "creationflags": subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW}
                child = subprocess.Popen([self.binary, *args], cwd=cwd, env=env,
                                         stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                         stderr=subprocess.PIPE, **options)
                self.children.add(child)
            buffers = [bytearray(), bytearray()]
            overflow = threading.Event()

            def drain(pipe, buffer, limit):
                try:
                    while chunk := pipe.read(65536):
                        if len(buffer) + len(chunk) > limit:
                            overflow.set()
                            terminate(child)
                            return
                        buffer.extend(chunk)
                except OSError:
                    pass
                finally:
                    pipe.close()

            for pipe, buffer, limit in [(child.stdout, buffers[0], self.output_limit),
                                        (child.stderr, buffers[1], 256 * 1024)]:
                thread = threading.Thread(target=drain, args=(pipe, buffer, limit), daemon=True)
                readers.append(thread)
                thread.start()
            deadline = time.monotonic() + self.timeout
            try:
                child.wait(timeout=self.timeout)
                for thread in readers:
                    thread.join(max(0, deadline - time.monotonic()))
                if any(thread.is_alive() for thread in readers):
                    raise subprocess.TimeoutExpired(self.binary, self.timeout)
            except subprocess.TimeoutExpired:
                terminate(child)
                child.wait()
                message = "The operation may have partially completed; refresh and inspect repository/remote state before retrying." if receipt else "Narrow the file/section selection or increase --timeout."
                raise Problem(f"Client exceeded {self.timeout:g} seconds. {message}", 504)
            if overflow.is_set():
                raise Problem("Client output exceeds the GUI limit (32 MiB). Select a file or section; no truncated audit is displayed.", 413)
            if receipt:
                output = buffers[0].decode("utf-8", "replace")
                try:
                    output = json.loads(output)
                except ValueError:
                    pass
                return {"exit_code": child.returncode, "output": output,
                        "stderr": buffers[1].decode("utf-8", "replace")}
            if child.returncode:
                message = buffers[1].decode("utf-8", "replace").strip()
                raise Problem(message or f"Client exited with status {child.returncode}.", 422)
            text = buffers[0].decode("utf-8")
            return json.loads(text) if parse else text
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise Problem(f"Unable to read client response: {error}", 502) from error
        finally:
            if child is not None:
                if child.poll() is None or any(thread.is_alive() for thread in readers):
                    terminate(child)
                child.wait()
                for thread in readers:
                    thread.join(timeout=2)
                with self.lock:
                    self.children.discard(child)
            self.slots.release()

    def query(self, data, default_repo):
        if not isinstance(data, dict):
            raise Problem("Expected a JSON object.")
        root = repository(field(data, "repository", str(default_repo or "")))
        repo = str(root)
        operation = field(data, "operation")
        path = field(data, "file")
        standard = field(data, "standard", "all-known")
        if standard not in ("all-known", "core1", "core2", "core3"):
            raise Problem("Unknown Wasm standard.")
        if operation == "overview":
            return {"repository": repo, "status": self.run(["status", repo]),
                    "refs": self.run(["for-each-ref", repo, "--json"])}
        if operation == "history":
            offset = data.get("offset", 0)
            if type(offset) is not int or offset < 0:
                raise Problem("Invalid history offset.")
            commits = self.run(["log", repo, revision(data)])
            return {"commits": commits[offset:offset + 100], "offset": offset, "total": len(commits)}
        if operation == "tree":
            return self.run(["ls-tree", repo, revision(data), "--json"])
        if operation in ("remotes", "stashes", "reflog"):
            commands = {"remotes": ["remote", repo, "-v"], "stashes": ["stash", repo, "list"], "reflog": ["reflog", repo]}
            return self.run(commands[operation])
        if operation == "help":
            return self.run(["--help"], receipt=True)
        if operation == "diff":
            mode = field(data, "mode", "revisions")
            if mode == "revisions":
                args = ["diff", revision(data, "old", "HEAD^"), revision(data, "new"), "--repo", repo]
            elif mode in ("worktree", "staged"):
                args = ["diff", repo] + (["--cached"] if mode == "staged" else [])
            else:
                raise Problem("Unknown comparison mode.")
            representation = field(data, "representation", "json")
            if representation not in ("json", "text"):
                raise Problem("Unknown representation.")
            args += ["--format", representation, "--compact", f"--standard={standard}"]
            section = field(data, "section")
            if section:
                args += [f"--section={section}"]
            if path:
                args += [f"--file={path}"]
            return self.run(args, parse=representation == "json")
        if operation in ("inspect", "debug"):
            source = field(data, "source", "revision")
            args = ["inspect" if operation == "inspect" else "debug-map"]
            if source == "worktree":
                if not path:
                    raise Problem("Choose a working-tree Wasm file.")
                target = (root / path).resolve()
                if not target.is_relative_to(root) or target.is_relative_to(root / ".wasm-git") or not target.is_file():
                    raise Problem("The selected file must be inside the repository worktree.")
                args += [str(target)]
            elif source == "revision":
                args += [revision(data), "--repo", repo]
                if path:
                    args += [f"--file={path}"]
            else:
                raise Problem("Unknown module source.")
            args += ["--json"]
            if operation == "inspect":
                args += [f"--standard={standard}"]
            else:
                kind = field(data, "query_kind", "all")
                value = field(data, "query")
                if kind not in ("all", "symbol", "function", "offset", "code-offset", "go-pc", "source"):
                    raise Problem("Unknown debug query.")
                if kind != "all":
                    if not value:
                        raise Problem("Enter a symbol, source path or numeric position.")
                    args += [f"--{kind}={value}"]
                sidecar = field(data, "sidecar")
                if sidecar:
                    target = (root / sidecar).resolve()
                    if not target.is_relative_to(root) or target.is_relative_to(root / ".wasm-git") or not target.is_file():
                        raise Problem("The debug sidecar must be a file inside the worktree.")
                    args += [f"--debug-file={target}"]
            return self.run(args)
        raise Problem("Unknown read-only operation.")

    def action(self, data, default_repo):
        if not isinstance(data, dict):
            raise Problem("Expected a JSON object.")
        action = field(data, "action")
        name = lambda key, default="": revision(data, key, default)
        destination = None
        if action in ("init", "clone"):
            value = field(data, "destination")
            if not value:
                raise Problem("Enter a destination directory.")
            destination = str(Path(value).expanduser().resolve())
            if action == "clone":
                args = ["clone", name("url"), destination]
            else:
                args = ["init", destination]
        else:
            root = repository(field(data, "repository", str(default_repo or "")))
            repo = str(root)
            message = data.get("message", "")
            if not isinstance(message, str) or len(message) > 8192 or "\0" in message:
                raise Problem("Invalid commit message.")
            path = field(data, "file")
            if action in ("stage", "unstage", "restore", "remove", "ours", "theirs"):
                if not path or path.startswith(("/", "\\")) or "\\" in path or any(p in ("..", ".wasm-git", "") for p in path.split("/")):
                    raise Problem("Select an exact repository-relative file.")
                selected = ["--", ":(literal)" + path]
                if action == "stage":
                    args = ["add", repo, *selected]
                elif action == "unstage":
                    head = self.run(["status", repo]).get("head")
                    args = ["restore", repo, "--staged", *selected] if head else ["rm", repo, "--cached", "--", path]
                elif action == "remove":
                    args = ["rm", repo, "--", path]
                else:
                    args = ["restore", repo] + ([f"--{action}"] if action in ("ours", "theirs") else []) + selected
            elif action == "stage-all":
                args = ["add", repo, "-A"]
            elif action == "commit":
                if not message.strip():
                    raise Problem("Enter a commit message.")
                args = ["commit", repo, "-m", message]
                if data.get("amend") is True:
                    args += ["--amend"]
            elif action == "branch-create":
                args = ["branch", repo, name("name"), name("revision", "HEAD")]
            elif action == "branch-delete":
                args = ["branch", repo, "-d", name("name")]
            elif action == "switch":
                args = ["switch", repo, name("name")]
            elif action == "tag-create":
                args = ["tag", repo, name("name"), name("revision", "HEAD")]
            elif action == "tag-delete":
                args = ["tag", repo, "-d", name("name")]
            elif action == "remote-add":
                args = ["remote", repo, "add", name("name"), name("url")]
            elif action == "remote-remove":
                args = ["remote", repo, "remove", name("name")]
            elif action == "fetch":
                args = ["fetch", repo, name("remote", "origin")]
            elif action == "pull":
                args = ["pull", repo, name("remote", "origin"), name("branch", "main"), "--ff-only"]
            elif action == "push":
                branch = name("branch", "main")
                if branch.startswith("+") or ":" in branch or "*" in branch or branch.startswith("^"):
                    raise Problem("Push expects a single branch name, not a force/delete/wildcard refspec.")
                args = ["push", repo, name("remote", "origin"), f"{branch}:{branch}", "--set-upstream"]
            elif action == "stash-save":
                args = ["stash", repo, "push"] + (["-m", message] if message else [])
            elif action in ("stash-apply", "stash-pop", "stash-drop"):
                if type(data.get("id")) is int and data["id"] >= 0:
                    data = {**data, "id": str(data["id"])}
                args = ["stash", repo, action.removeprefix("stash-"), name("id")]
            elif action == "reset":
                mode = field(data, "mode", "mixed")
                if mode not in ("soft", "mixed", "hard"):
                    raise Problem("Unknown reset mode.")
                args = ["reset", repo, f"--{mode}", name("revision", "HEAD")]
            elif action in ("merge", "rebase", "cherry-pick", "revert"):
                args = [action, repo, name("revision"), "--record-conflict"]
                if action == "merge":
                    args += ["-m", message or f"Merge {name('revision')}"]
            elif action in ("continue", "abort", "skip"):
                operation = field(data, "pending_operation")
                if operation not in ("merge", "rebase", "cherry-pick", "revert"):
                    raise Problem("Unknown pending operation.")
                args = [operation, repo, f"--{action}"]
                if action == "continue":
                    args += ["--resolved-index"]
            elif action in ("gc", "fsck"):
                args = [action, repo]
            else:
                raise Problem("Unsupported repository action.")
        risky = action in ("restore", "remove", "ours", "theirs", "reset", "stash-pop", "stash-drop", "tag-delete", "abort", "continue", "skip") or (action == "commit" and data.get("amend") is True)
        if risky and data.get("confirmed") is not True:
            raise Problem("Review and confirm this action in the interface before applying it.", 409)
        if not self.writer.acquire(blocking=False):
            raise Problem("Another repository action is running. Wait for its result.", 409)
        try:
            result = self.run(args, receipt=True)
            result["command"] = [Path(self.binary).name, *args]
            if destination and result["exit_code"] == 0:
                result["repository"] = destination
            return result
        finally:
            self.writer.release()


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = os.name != "nt"

    def __init__(self, port, client, repo, token, runtime=None, job_limit=256 * 1024 * 1024,
                 server_client=None, mode="client", root=None):
        self.client, self.repo, self.token = client, repo, token
        runtime = Path(runtime or DEFAULT_STATE / "jobs")
        self.mode, self.root, self.server_client = mode, root, server_client
        self.jobs = JobManager(client, runtime / "client", job_limit)
        self.managers = {"client": self.jobs}
        if server_client:
            self.managers["server"] = JobManager(server_client, runtime / "server", job_limit)
            self.managers["service"] = JobManager(server_client, runtime / "service", job_limit, exclusive=False)
        super().__init__(("127.0.0.1", port), Handler)
        self.origin = f"http://127.0.0.1:{self.server_port}"
        self.origins = {self.origin, f"http://localhost:{self.server_port}"}

    def manager(self, data):
        name = data.get("target", "client")
        if not isinstance(name, str) or name not in self.managers:
            raise ValueError("This executable is not available in this GUI mode.")
        return self.managers[name]

    def find_job(self, identifier):
        for manager in self.managers.values():
            try:
                return manager.get(identifier)
            except ValueError:
                pass
        raise ValueError("Unknown command job.")


class Handler(BaseHTTPRequestHandler):
    server_version = "wasm-git-webgui"

    def setup(self):
        super().setup()
        self.connection.settimeout(10)

    def log_message(self, *_args):
        pass  # Do not persist repository paths, symbol names or credentials.

    def reply(self, status, content, kind="application/json"):
        payload = json.dumps(content, ensure_ascii=True).encode() if kind == "application/json" else content
        self.send_response(status)
        self.send_header("Content-Type", kind + "; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def guard(self, authenticated=False):
        if f"http://{self.headers.get('Host', '')}" not in self.server.origins:
            raise Problem("Invalid local Host header.", 403)
        origin = self.headers.get("Origin")
        if (origin and origin not in self.server.origins) or self.headers.get("Sec-Fetch-Site") == "cross-site":
            raise Problem("This GUI accepts same-origin local requests only.", 403)
        if authenticated and not hmac.compare_digest(self.headers.get("X-Wasm-Git-Token", "").encode(), self.server.token.encode()):
            raise Problem("Open the full URL printed by the start command to authorize this browser tab.", 401)

    def do_GET(self):
        try:
            self.guard(self.path.startswith("/api/"))
            if self.path == "/api/session":
                return self.reply(200, {"repository": str(self.server.repo or ""), "client": self.server.client.binary,
                                        "pid": os.getpid(), "name": "wasm-git webgui", "cwd": str(HERE.parent),
                                        "mode": self.server.mode, "root": str(self.server.root or HERE.parent),
                                        "server": self.server.server_client.binary if self.server.server_client else None,
                                        "job_limit": self.server.jobs.limit})
            if self.path == "/api/help":
                return self.reply(200, self.server.client.run(["--help"], receipt=True))
            if self.path == "/api/server-help" and self.server.server_client:
                return self.reply(200, self.server.server_client.run(["--help"], receipt=True))
            if self.path == "/api/jobs":
                result = []
                for target, manager in self.server.managers.items():
                    with manager.lock:
                        result.extend({**job.view(tuple(job.sizes)), "target": target} for job in manager.jobs.values())
                return self.reply(200, {"jobs": result})
            if self.path.startswith("/api/jobs/"):
                parts = self.path.split("/")
                if len(parts) != 5 or parts[4] not in ("stdout", "stderr"):
                    raise Problem("Unknown output stream.", 404)
                job = self.server.find_job(parts[3])
                target = job.paths[0 if parts[4] == "stdout" else 1]
                with target.open("rb") as stream:
                    size = os.fstat(stream.fileno()).st_size
                    self.send_response(200)
                    self.send_header("Content-Type", "application/octet-stream")
                    self.send_header("Content-Disposition", f'attachment; filename="{parts[4]}.bin"')
                    self.send_header("Content-Length", str(size))
                    self.send_header("Cache-Control", "no-store")
                    self.send_header("X-Content-Type-Options", "nosniff")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    while size:
                        chunk = stream.read(min(size, 65536))
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        size -= len(chunk)
                return
            asset = ASSETS.get(self.path)
            if asset is None:
                raise Problem("Not found.", 404)
            self.reply(200, (HERE / "static" / asset[0]).read_bytes(), asset[1])
        except Problem as error:
            self.reply(error.status, {"error": str(error)})
        except (OSError, ValueError) as error:
            self.reply(400, {"error": str(error)})

    def do_POST(self):
        try:
            self.guard(True)
            if self.headers.get("Transfer-Encoding") or self.headers.get_content_type() != "application/json":
                raise Problem("Send a JSON request with Content-Length.", 415)
            try:
                size = int(self.headers.get("Content-Length", "-1"))
            except ValueError:
                raise Problem("Invalid Content-Length.") from None
            if not 0 <= size <= MAX_BODY:
                raise Problem("Request body exceeds 32 KiB.", 413)
            try:
                data = json.loads(self.rfile.read(size))
            except (ValueError, UnicodeError):
                raise Problem("Invalid JSON request.") from None
            if self.path == "/api/shutdown":
                self.reply(200, {"stopped": True})
                threading.Thread(target=self.server.shutdown, daemon=True).start()
            elif self.path == "/api/query":
                result = self.server.client.query(data, self.server.repo)
                self.reply(200, {"result": result})
            elif self.path == "/api/action":
                result = self.server.client.action(data, self.server.repo)
                self.reply(200, {"result": result})
            elif self.path == "/api/jobs/start":
                if not isinstance(data, dict):
                    raise Problem("Expected a JSON object.")
                self.reply(200, self.server.manager(data).start(data, self.server.repo or HERE.parent))
            elif self.path in ("/api/jobs/poll", "/api/jobs/input", "/api/jobs/cancel", "/api/jobs/remove"):
                if not isinstance(data, dict):
                    raise Problem("Expected a JSON object.")
                job = self.server.find_job(data.get("id"))
                if self.path.endswith("input"):
                    content = data.get("base64", "")
                    if not isinstance(content, str):
                        raise Problem("Expected base64 input.")
                    job.send(base64.b64decode(content, validate=True), data.get("eof") is True)
                    self.reply(200, {"accepted": True})
                elif self.path.endswith("cancel"):
                    job.cancel()
                    self.reply(200, {"cancelled": True})
                elif self.path.endswith("remove"):
                    if not job.done.is_set():
                        raise Problem("Cancel the job before removing its output.", 409)
                    with job.manager.lock:
                        job.manager.jobs.pop(job.id, None)
                        shutil.rmtree(job.directory)
                    self.reply(200, {"removed": True})
                else:
                    self.reply(200, job.view((data.get("stdout", 0), data.get("stderr", 0))))
            elif self.path == "/api/supervision" and self.server.server_client:
                if not isinstance(data, dict):
                    raise Problem("Expected a JSON object.")
                operation = field(data, "operation", "overview")
                if operation == "overview":
                    manager = self.server.managers["service"]
                    with manager.lock:
                        services = [job.view(tuple(job.sizes)) for job in manager.jobs.values()]
                    result = supervision.overview(field(data, "root", str(self.server.root or HERE.parent)), services)
                elif operation == "probe":
                    result = supervision.probe(field(data, "address"))
                elif operation == "audit":
                    result = self.server.server_client.run(["audit-log", field(data, "file")])
                else:
                    raise Problem("Unknown server inspection.")
                self.reply(200, {"result": result})
            else:
                raise Problem("Not found.", 404)
        except Problem as error:
            self.reply(error.status, {"error": str(error)})
        except (OSError, ValueError) as error:
            self.reply(400, {"error": str(error)})


def state_path(args):
    return Path(args.state_dir).expanduser().resolve() / f"{args.port}.state"


def read_state(path):
    try:
        magic, port, pid, token = STATE.unpack(path.read_bytes())
        if magic == MAGIC:
            return port, pid, token.hex()
    except (OSError, ValueError, struct.error):
        pass
    return None


def control(state, stop=False):
    port, pid, token = state
    request = Request(f"http://127.0.0.1:{port}/api/" + ("shutdown" if stop else "session"),
                      data=b"{}" if stop else None,
                      headers={"X-Wasm-Git-Token": token, "Content-Type": "application/json"})
    try:
        with build_opener(ProxyHandler({})).open(request, timeout=2) as response:
            data = json.load(response)
        return data if stop or (data.get("pid") == pid and data.get("name") == "wasm-git webgui") else None
    except (OSError, ValueError, URLError):
        return None


def url(state):
    return f"http://127.0.0.1:{state[0]}/#token={state[2]}"


def serve(args, binary, repo):
    client = Client(binary, args.timeout)
    capabilities = client.run(["version", "--build-options"])
    if capabilities.get("client") is not True:
        raise Problem("The selected executable is not a wasm-git client build.")
    token = secrets.token_bytes(32)
    state_path(args).parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    runtime = tempfile.TemporaryDirectory(prefix="session-", dir=state_path(args).parent)
    server_client = Client(args.server, args.timeout) if args.mode == "server" else None
    server = Server(args.port, client, repo, token.hex(), runtime.name, args.job_output_mib * 1024 * 1024,
                    server_client, args.mode, args.root)
    path = state_path(args)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    record = STATE.pack(MAGIC, server.server_port, os.getpid(), token)
    try:
        # The listening socket has already established exclusive port ownership.
        path.unlink(missing_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as file:
            file.write(record)
        print(f"wasm-git Web GUI: {server.origin}/#token={token.hex()}", flush=True)
        print(f"Repository: {repo or '(choose in browser)'}\nClient: {binary}", flush=True)
        if args.open:
            webbrowser.open(f"{server.origin}/#token={token.hex()}")

        def stop_signal(_number, _frame):
            threading.Thread(target=server.shutdown, daemon=True).start()

        signal.signal(signal.SIGINT, stop_signal)
        signal.signal(signal.SIGTERM, stop_signal)
        server.serve_forever(poll_interval=0.1)
    finally:
        for manager in server.managers.values():
            manager.close()
        if server_client:
            server_client.close()
        client.close()
        server.server_close()
        runtime.cleanup()
        if read_state(path) == (server.server_port, os.getpid(), token.hex()):
            path.unlink(missing_ok=True)


def main(argv=None, mode="client"):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="?", choices=("start", "serve", "stop", "status"), default="start")
    parser.add_argument("--repo", help="wasm-git repository to open; otherwise choose in the browser")
    parser.add_argument("--client", help="existing wasm-git executable; also WASM_GIT_CLIENT")
    parser.add_argument("--port", type=int, default=8738 if mode == "server" else 8737)
    parser.add_argument("--server", help="existing wasm-git-server executable (server GUI)")
    parser.add_argument("--root", help="server repository root (server GUI)")
    parser.add_argument("--build", action="store_true", help="rebuild native executables from this checkout")
    parser.add_argument("--no-build", action="store_true", help="fail rather than compile missing executables")
    parser.add_argument("--state-dir", default=str(DEFAULT_STATE), help="local binary process state directory")
    parser.add_argument("--timeout", type=float, default=120, help="native query deadline in seconds")
    parser.add_argument("--job-output-mib", type=int, default=256, help="per-stream command output limit; binary files in the state directory")
    parser.add_argument("--open", action="store_true", help="open the authenticated URL in your browser")
    args = parser.parse_args(argv)
    args.mode = mode
    if args.build and args.no_build:
        parser.error("--build and --no-build are mutually exclusive")
    if not 1 <= args.port <= 65535 or not 0 < args.timeout <= 3600:
        parser.error("port must be 1..65535; timeout must be greater than 0 and at most 3600 seconds")
    if args.job_output_mib < 1:
        parser.error("--job-output-mib must be positive")
    path = state_path(args)
    state = read_state(path)
    live = state and control(state)
    if args.command in ("stop", "status"):
        if not live:
            print("wasm-git Web GUI is not running.")
            return 0 if args.command == "stop" else 1
        if args.command == "status":
            print(f"Running: {url(state)}\nRepository: {live['repository']}")
            return 0
        if not control(state, True):
            raise Problem("The running GUI did not accept the shutdown request.")
        for _ in range(50):
            if not control(state):
                print("wasm-git Web GUI stopped.")
                return 0
            time.sleep(0.1)
        raise Problem("Shutdown is taking longer than expected; check the foreground process.")
    if live:
        print(f"Already running: {url(state)}\nRepository: {live['repository']}\nUse another --port to open a second instance.")
        if args.open:
            webbrowser.open(url(state))
        return 0
    binary = client_path(args.client, build=args.build, no_build=args.no_build)
    if mode == "server":
        args.server = str(client_path(args.server, kind="server", build=args.build, no_build=args.no_build))
    repo = repository(args.repo) if args.repo else None
    if args.command == "serve":
        serve(args, binary, repo)
        return 0
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    log_path = path.with_suffix(".log")
    command = [sys.executable, str(HERE / f"{mode}.py"), "serve",
               "--client", str(binary), "--port", str(args.port), "--state-dir", str(path.parent),
               "--timeout", str(args.timeout), "--job-output-mib", str(args.job_output_mib)]
    if repo:
        command += ["--repo", str(repo)]
    if mode == "server":
        command += ["--server", args.server]
        if args.root:
            command += ["--root", args.root]
    options = {"start_new_session": True} if os.name != "nt" else {
        "creationflags": subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP}
    fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as log:
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=log, **options)
    for _ in range(100):
        state = read_state(path)
        if state and state[1] == child.pid and control(state):
            print(f"wasm-git Web GUI: {url(state)}\nStop: python3 webgui/{mode}.py stop --port {args.port} --state-dir {path.parent}")
            if args.open:
                webbrowser.open(url(state))
            return 0
        if child.poll() is not None:
            raise Problem(f"GUI failed to start. See {log_path}")
        time.sleep(0.1)
    child.terminate()
    child.wait(timeout=10)
    raise Problem(f"GUI startup timed out. See {log_path}")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Problem, OSError) as error:
        print(f"webgui: {error}", file=sys.stderr)
        raise SystemExit(1)
