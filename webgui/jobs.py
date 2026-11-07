"""Bounded background jobs for the complete client CLI; raw output stays binary."""
from __future__ import annotations

import base64
import os
from pathlib import Path
import secrets
import signal
import subprocess
import threading
import time


def terminate(process):
    """Stop only the process group created for this job, including SSH children."""
    try:
        if os.name == "nt":
            if process.poll() is None:
                taskkill = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32/taskkill.exe"
                subprocess.run([str(taskkill), "/PID", str(process.pid), "/T", "/F"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10, creationflags=subprocess.CREATE_NO_WINDOW, check=False)
        else:
            os.killpg(process.pid, signal.SIGKILL)
    except (OSError, subprocess.SubprocessError):
        if process.poll() is None:
            process.kill()


class Job:
    def __init__(self, manager, args, cwd, environment, stdin_path, keep_stdin):
        self.manager = manager
        self.id = secrets.token_hex(12)
        self.args, self.cwd = args, str(cwd)
        self.directory = manager.directory / self.id
        self.directory.mkdir(mode=0o700)
        self.started = time.monotonic()
        self.finished = None
        self.reason = None
        self.exit_code = None
        self.done = threading.Event()
        self.input_lock = threading.Lock()
        self.stdin_closed = bool(stdin_path) or not keep_stdin
        self.sizes = [0, 0]
        self.paths = [self.directory / "stdout.bin", self.directory / "stderr.bin"]
        streams = [p.open("wb", buffering=0) for p in self.paths]
        source = None
        try:
            if stdin_path:
                target = Path(stdin_path).expanduser()
                target = target if target.is_absolute() else cwd / target
                if not target.is_file():
                    raise ValueError("Standard input must be a regular file.")
                source = target.open("rb")
            options = {"start_new_session": True} if os.name != "nt" else {
                "creationflags": subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW}
            self.process = subprocess.Popen(
                [manager.client.binary, *args], cwd=cwd,
                env={**os.environ, "NO_COLOR": "1", "CLICOLOR": "0", **environment},
                stdin=source if source else (subprocess.PIPE if keep_stdin else subprocess.DEVNULL),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, **options)
        except BaseException:
            for stream in streams:
                stream.close()
            raise
        finally:
            if source:
                source.close()
        self.readers = []
        for i, pipe in enumerate((self.process.stdout, self.process.stderr)):
            reader = threading.Thread(target=self._drain, args=(i, pipe, streams[i]), daemon=True)
            self.readers.append(reader)
            reader.start()
        threading.Thread(target=self._wait, daemon=True).start()

    def _drain(self, index, pipe, output):
        try:
            while chunk := pipe.read1(65536):
                if self.sizes[index] + len(chunk) > self.manager.limit:
                    self.cancel("output limit exceeded; refresh repository state before retrying")
                    break
                remaining = memoryview(chunk)
                while remaining:
                    written = output.write(remaining)
                    if not written:
                        raise OSError("zero-length output write")
                    remaining = remaining[written:]
                self.sizes[index] += len(chunk)
        except OSError as error:
            self.cancel(f"output capture failed: {error}")
        finally:
            output.close()
            pipe.close()

    def _wait(self):
        try:
            self.exit_code = self.process.wait()
            for reader in self.readers:
                reader.join()
            if self.process.stdin:
                self.process.stdin.close()
            self.stdin_closed = True
        finally:
            self.finished = time.monotonic()
            if self.manager.exclusive:
                self.manager.client.writer.release()
            self.done.set()

    def cancel(self, reason="cancelled; refresh repository state before retrying"):
        if not self.done.is_set():
            self.reason = reason
            terminate(self.process)

    def send(self, payload, eof):
        if self.stdin_closed or self.done.is_set():
            raise ValueError("Standard input is closed.")
        # The queue is deliberately one small chunk. Never block an HTTP handler
        # behind a client that is not reading its input pipe.
        if not self.input_lock.acquire(blocking=False):
            raise ValueError("Previous input is still being delivered. Wait before sending more.")
        if eof:
            self.stdin_closed = True

        def write():
            try:
                self.process.stdin.write(payload)
                self.process.stdin.flush()
                if eof:
                    self.process.stdin.close()
            except (OSError, ValueError):
                self.stdin_closed = True
            finally:
                self.input_lock.release()

        threading.Thread(target=write, daemon=True).start()

    def view(self, offsets=(0, 0)):
        streams = []
        for path, offset in zip(self.paths, offsets):
            if type(offset) is not int or offset < 0:
                raise ValueError("Invalid output offset.")
            with path.open("rb") as stream:
                stream.seek(offset)
                content = stream.read(65536)
            streams.append({"offset": offset + len(content), "base64": base64.b64encode(content).decode("ascii")})
        return {"id": self.id, "args": self.args, "cwd": self.cwd,
                "running": not self.done.is_set(), "exit_code": self.exit_code,
                "reason": self.reason, "stdin_closed": self.stdin_closed,
                "stdin_busy": self.input_lock.locked(), "sizes": self.sizes.copy(),
                "seconds": round((self.finished or time.monotonic()) - self.started, 2),
                "stdout": streams[0], "stderr": streams[1]}


class JobManager:
    def __init__(self, client, directory, limit=256 * 1024 * 1024, exclusive=True):
        self.client, self.directory, self.limit = client, Path(directory), limit
        self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.jobs = {}
        self.lock = threading.Lock()
        self.closed = False
        self.exclusive = exclusive

    def start(self, data, default_cwd):
        args = data.get("args")
        if not isinstance(args, list) or not args or len(args) > 1024 or any(
                not isinstance(a, str) or "\0" in a for a in args):
            raise ValueError("Arguments must be a nonempty array of strings without NUL bytes.")
        cwd = Path(data.get("cwd") or str(default_cwd)).expanduser().resolve()
        if not cwd.is_dir():
            raise ValueError("Working directory does not exist.")
        env = data.get("environment", {})
        if not isinstance(env, dict) or any(not isinstance(k, str) or not k or "=" in k or "\0" in k
                                           or not isinstance(v, str) or "\0" in v for k, v in env.items()):
            raise ValueError("Environment must be an object of valid string names and values.")
        stdin = data.get("stdin_file", "")
        if not isinstance(stdin, str) or "\0" in stdin:
            raise ValueError("Invalid standard input file.")
        with self.lock:
            if self.closed:
                raise ValueError("The GUI is shutting down.")
            if len(self.jobs) >= 64:
                raise ValueError("Remove completed jobs before starting another (maximum 64 per session).")
            if not self.exclusive and any(not job.done.is_set() for job in self.jobs.values()):
                raise ValueError("A managed service is already running. Stop it before starting another.")
            if self.exclusive and not self.client.writer.acquire(blocking=False):
                raise ValueError("Another repository action or command is running. Wait or cancel it first.")
            try:
                job = Job(self, args, cwd, env, stdin, data.get("keep_stdin") is True)
                self.jobs[job.id] = job
                return job.view()
            except BaseException:
                if self.exclusive:
                    self.client.writer.release()
                raise

    def get(self, identifier):
        with self.lock:
            if not isinstance(identifier, str) or identifier not in self.jobs:
                raise ValueError("Unknown command job.")
            return self.jobs[identifier]

    def close(self):
        with self.lock:
            self.closed = True
            jobs = list(self.jobs.values())
        for job in jobs:
            job.cancel("GUI stopped; inspect repository state before retrying")
        for job in jobs:
            job.done.wait(timeout=10)
