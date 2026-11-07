#!/usr/bin/env python3
"""Run a command in an owned 64 GiB / zero-swap systemd user cgroup (Linux)."""

import argparse
import os
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    command = parser.parse_args().command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("provide a command after --")
    root = Path(__file__).resolve().parents[1]
    name = f"wasm-git-test-{os.getpid()}-{time.time_ns()}"
    scratch = root / ".tmp" / name
    scratch.mkdir(parents=True)
    temporary = scratch / "temp"
    temporary.mkdir()
    group = None
    keeper = None
    created = False
    result = 1
    try:
        subprocess.run(["systemd-run", "--user", f"--unit={name}",
                        "--property=MemoryMax=68719476736", "--property=MemorySwapMax=0",
                        "--property=Delegate=yes", "--property=KillMode=process",
                        "/usr/bin/sleep", "infinity"], check=True)
        created = True
        keeper = subprocess.check_output(["systemctl", "--user", "show", name,
                                          "--property=MainPID", "--value"], text=True).strip()
        relative = subprocess.check_output(["systemctl", "--user", "show", name,
                                            "--property=ControlGroup", "--value"], text=True).strip()
        group = Path("/sys/fs/cgroup") / relative.lstrip("/")
        if (group / "memory.max").read_text().strip() != "68719476736":
            raise RuntimeError("64 GiB memory limit was not applied")
        if (group / "memory.swap.max").read_text().strip() != "0":
            raise RuntimeError("zero-swap limit was not applied")

        def enter():
            # Only the child joins; the parent remains able to collect evidence
            # if the kernel kills the workload. No thread is running at fork.
            (group / "cgroup.procs").write_text(str(os.getpid()))

        environment = dict(os.environ, TMPDIR=str(temporary), PYTHONDONTWRITEBYTECODE="1")
        environment.setdefault("CARGO_TARGET_DIR", str(root / ".tmp/target"))
        environment.setdefault("XDG_CACHE_HOME", str(root / ".tmp/cache"))
        result = subprocess.run(command, cwd=root, env=environment, preexec_fn=enter).returncode
    finally:
        if group and group.exists():
            stats = "\n".join(f"{key}:\n{(group / key).read_text().strip()}" for key in
                              ("memory.max", "memory.swap.max", "memory.peak", "memory.events"))
            (scratch / "resources.txt").write_text(stats + "\n")
            print(stats, flush=True)
            print(f"resource evidence: {scratch / 'resources.txt'}", flush=True)
        if created:
            # Another task may have joined this delegated group. Do not kill
            # unknown processes (including reparented descendants) on cleanup.
            # A successful workload need not imply that the group is empty.
            # KillMode=process also protects a process joining after this check.
            members = set()
            if group and group.exists():
                for path in group.rglob("cgroup.procs"):
                    members.update(path.read_text().split())
            if members - {keeper}:
                print(f"cgroup still has live processes; left running: {name}; "
                      f"pids={','.join(sorted(members - {keeper}))}", flush=True)
            else:
                subprocess.run(["systemctl", "--user", "stop", name], check=True)
    raise SystemExit(result if result >= 0 else 128 - result)


if __name__ == "__main__":
    main()
