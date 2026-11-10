#!/usr/bin/env python3
"""Long byte-patch chains, divergent histories and independent packed forks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import time

from syntax_ports import HEADER, leb, signed, name, section, probes


def digest(data):
    return hashlib.sha256(data).hexdigest()


def functions(a, b):
    bodies = [b"\0\x41" + signed(value) + b"\x0b" for value in (a, b)]
    return (HEADER + section(1, b"\x01\x60\0\x01\x7f") + section(3, b"\x02\0\0")
            + section(7, b"\x02" + name("a") + b"\0\0" + name("b") + b"\0\x01")
            + section(10, b"\x02" + b"".join(leb(len(body)) + body for body in bodies)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("previous", "current", "output"):
        parser.add_argument("--" + option, type=Path, required=True)
    parser.add_argument("--corpus", action="append", type=Path, required=True)
    parser.add_argument("--steps", type=int, default=2048)
    parser.add_argument("--forks", type=int, default=16)
    parser.add_argument("--replay-queue", type=int, default=128)
    parser.add_argument("--duration", type=float, default=1800)
    parser.add_argument("--heartbeat", type=float, default=600)
    parser.add_argument("--seed", type=int, default=0)
    args = parser.parse_args()
    assert 1 <= args.forks <= args.steps and args.replay_queue >= 2
    assert args.duration >= 0 and args.heartbeat > 0
    scratch = Path(os.environ["WASM_FUZZER_TEST_ROOT"]).resolve()
    output = args.output.resolve()
    assert Path.cwd().resolve().is_relative_to(scratch) and output.is_relative_to(scratch)
    group = Path("/sys/fs/cgroup") / Path("/proc/self/cgroup").read_text().split("0::", 1)[1].strip().lstrip("/")
    assert (group / "memory.max").read_text().strip() == "68719476736"
    assert (group / "memory.swap.max").read_text().strip() == "0"
    os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:4])
    output.mkdir()
    binaries = [args.previous.resolve(), args.current.resolve()]
    started = time.monotonic()
    report = {"schema": 1, "cgroup": str(group), "seed": args.seed, "required_seconds": args.duration,
              "steps_required": args.steps, "forks_required": args.forks, "queue_required": args.replay_queue,
              "tools": [{"path": str(p), "sha256": digest(p.read_bytes())} for p in binaries],
              "commits": 0, "patch_applications": 0, "checkouts": 0, "forks": 0,
              "long_forward_replays": 0, "long_reverse_replays": 0,
              "operations": {}, "failures": []}
    next_status, next_heartbeat = 0, args.heartbeat

    def save():
        report["elapsed_seconds"] = time.monotonic() - started
        pending = output / "report.pending"
        pending.write_text(json.dumps(report, indent=2) + "\n")
        pending.replace(output / "report.json")

    def progress():
        nonlocal next_status, next_heartbeat
        elapsed = time.monotonic() - started
        if elapsed >= next_status:
            save()
            print(f"STATUS history {elapsed:.1f}s commits={report['commits']} patches={report['patch_applications']} checkouts={report['checkouts']} forks={report['forks']} failures=0", flush=True)
            next_status = elapsed + 30
        if elapsed >= next_heartbeat:
            print(f"HEARTBEAT history {next_heartbeat / 60:g}min failures=0", flush=True)
            next_heartbeat += args.heartbeat

    def run(*options, binary=None, code=0):
        command = [str(binary or binaries[1]), "--threads", "1", *map(str, options)]
        result = subprocess.run(command, cwd=output, capture_output=True, timeout=120)
        if result.returncode != code:
            (output / "failure.stdout").write_bytes(result.stdout)
            (output / "failure.stderr").write_bytes(result.stderr)
            raise RuntimeError(f"{command}: exit {result.returncode}, expected {code}: {result.stderr.decode(errors='replace')}")
        key = str(options[0])
        report["operations"][key] = report["operations"].get(key, 0) + 1
        progress()
        return result.stdout.decode().strip()

    def commit(repo, data, message, indexed=False):
        path = repo / "module.wasm" if indexed else output / "commit-input.wasm"
        path.write_bytes(data)
        if indexed:
            run("add", repo, "module.wasm")
            run("commit", repo, "-m", message)
        else:
            run("commit", repo, path, "-m", message)
        report["commits"] += 1
        return run("rev-parse", repo, "HEAD")

    def checkout(repo, revision, expected, indexed=False):
        path = output / "checkout.wasm"
        path.unlink(missing_ok=True)
        if indexed:
            run("show", repo, revision, "--file", "module.wasm", "--output", path)
        else:
            run("checkout", repo, revision, path)
        assert path.read_bytes() == expected, (repo, revision)
        report["checkouts"] += 1

    def apply(source, patch, expected, binary):
        destination = output / "applied.wasm"
        destination.unlink(missing_ok=True)
        run("apply", source, patch, "--output", destination, binary=binary)
        assert destination.read_bytes() == expected, patch
        report["patch_applications"] += 1
        return destination.read_bytes()

    try:
        rng = random.Random(args.seed)
        corpus = []
        for root in args.corpus:
            root = root.resolve()
            assert root.is_relative_to(scratch)
            for line in (root / "manifest.jsonl").read_text().splitlines():
                item = json.loads(line)
                data = (root / item["file"]).read_bytes()
                assert digest(data) == item["sha256"]
                corpus.append(data)
        assert corpus
        future = [after for _, _, _, after in probes(random.Random(args.seed), quick=True)]
        snapshots = output / "snapshots"
        patches = output / "patches"
        snapshots.mkdir()
        patches.mkdir()
        repo = output / "history"
        run("init", repo)
        versions = [corpus[0]]
        (snapshots / "0.wasm").write_bytes(versions[0])
        commits = [commit(repo, versions[0], "root")]
        edges, forks, branch_versions = [], [], []
        with (output / "chain.jsonl").open("w") as records:
            for i in range(1, args.steps + 1):
                data = future[(i // 3) % len(future)] if i % 3 == 0 else rng.choice(corpus)
                data += section(0, name("history.revision") + i.to_bytes(8, "little") + rng.randbytes(16))
                path = snapshots / f"{i}.wasm"
                path.write_bytes(data)
                old = snapshots / f"{i - 1}.wasm"
                forward, reverse = patches / f"{i}.forward.wgp", patches / f"{i}.reverse.wgp"
                writer, reader = binaries[i % 2], binaries[1 - i % 2]
                run("patch", old, path, "--output", forward, binary=writer)
                run("patch", path, old, "--output", reverse, binary=reader)
                assert forward.read_bytes().startswith(b"WGTPAT01") and reverse.read_bytes().startswith(b"WGTPAT01")
                apply(old, forward, data, reader)
                apply(path, reverse, versions[-1], writer)
                versions.append(data)
                edges.append((forward, reverse))
                commits.append(commit(repo, data, f"revision {i}"))
                records.write(json.dumps({"step": i, "parent": commits[-2], "commit": commits[-1],
                                          "sha256": digest(data), "forward_sha256": digest(forward.read_bytes()),
                                          "reverse_sha256": digest(reverse.read_bytes())}) + "\n")
                records.flush()
                if len(forks) < args.forks and i * args.forks // args.steps > len(forks):
                    # A named branch forks from an independently chosen ancestor.
                    ancestor = rng.randrange(i + 1)
                    branch = f"branch-{len(forks)}"
                    run("branch", repo, branch, commits[ancestor])
                    run("switch", repo, branch)
                    branch_data = versions[ancestor] + section(0, name(branch) + rng.randbytes(31))
                    branch_tip = commit(repo, branch_data, branch)
                    branch_versions.append((branch_tip, branch_data))
                    checkout(repo, branch_tip, branch_data)
                    run("switch", repo, "main")
                    assert run("rev-parse", repo, "HEAD") == commits[i]
                    # Actual repository forks share storage but own their refs.
                    fork = output / f"fork-{len(forks)}"
                    run("repack", repo)
                    run("fork", repo, fork)
                    fork_data = data + section(0, name("fork.private") + rng.randbytes(32))
                    fork_tip = commit(fork, fork_data, "independent fork write")
                    run("gc", fork, "--prune=now")
                    run("fsck", fork)
                    checkout(fork, fork_tip, fork_data)
                    assert run("rev-parse", repo, "HEAD") == commits[i]
                    forks.append((fork, i, fork_tip, fork_data))
                    report["forks"] += 1
        report["chain_steps"] = len(edges)
        # Replay the entire chain from only its root, then reverse from only its
        # final result. Neither pass substitutes a stored intermediate input.
        work = output / "replay-input.wasm"
        for binary in binaries:
            current = versions[0]
            for i, (forward, _) in enumerate(edges, 1):
                work.write_bytes(current)
                current = apply(work, forward, versions[i], binary)
                report["long_forward_replays"] += 1
            assert current == versions[-1]
            for i in range(args.steps, 0, -1):
                work.write_bytes(current)
                current = apply(work, edges[i - 1][1], versions[i - 1], binary)
                report["long_reverse_replays"] += 1
            assert current == versions[0]
        run("gc", repo, "--prune=now")
        run("fsck", repo)
        clone, restored = output / "clone", output / "from-bundle"
        run("clone", repo, clone)
        bundle = output / "history.wgb"
        run("bundle", repo, bundle)
        run("init", restored)
        run("unbundle", restored, bundle, "main")
        nested = output / "nested-fork"
        run("fork", forks[-1][0], nested)
        nested_data = versions[-1] + section(0, name("nested.fork") + rng.randbytes(32))
        nested_tip = commit(nested, nested_data, "nested fork write")
        run("gc", nested)
        for recipient in [repo, clone, restored, forks[-1][0], nested]:
            run("fsck", recipient)
            for revision, data in zip(commits, versions):
                checkout(recipient, revision, data)
            for revision, data in branch_versions:
                checkout(recipient, revision, data)
        # Deleting this test-owned source proves that forks are independent,
        # including after pruning and replacement packs in both repositories.
        shutil.rmtree(repo)
        for fork, index, tip, data in forks:
            run("fsck", fork)
            checkout(fork, commits[0], versions[0])
            checkout(fork, commits[index], versions[index])
            checkout(fork, tip, data)
        report["forks_survived_source_removal"] = True
        checkout(nested, nested_tip, nested_data)
        run("fsck", nested)
        report["nested_fork"] = True

        # Long indexed replay queues, plus a two-parent merge with an oracle
        # encoded independently from the merge/rebase implementations.
        queue = output / "queue"
        run("init", queue)
        base = commit(queue, functions(0, 0), "base", True)
        run("branch", queue, "topic")
        a = rng.randrange(100, 1 << 20)
        ours = commit(queue, functions(a, 0), "upstream", True)
        run("switch", queue, "topic")
        incoming = [commit(queue, functions(0, i), f"queue {i}", True) for i in range(1, args.replay_queue + 1)]
        run("branch", queue, "unrebased")
        run("switch", queue, "main")
        picked = json.loads(run("cherry-pick", queue, *incoming))
        assert picked["status"] == "committed", picked
        checkout(queue, "HEAD", functions(a, args.replay_queue), True)
        report["cherry_pick_steps"] = args.replay_queue
        picked_head = run("rev-parse", queue, "HEAD")
        picked_ids = []
        cursor = picked_head
        for _ in incoming:
            picked_ids.append(cursor)
            cursor = json.loads(run("show", queue, cursor))["parents"][0]
        assert cursor == ours
        reverted = json.loads(run("revert", queue, *picked_ids))
        assert reverted["status"] == "committed", reverted
        checkout(queue, "HEAD", functions(a, 0), True)
        report["revert_steps"] = args.replay_queue
        run("switch", queue, "topic")
        rebased = json.loads(run("rebase", queue, ours))
        assert rebased["status"] == "rebased", rebased
        checkout(queue, "HEAD", functions(a, args.replay_queue), True)
        report["rebase_steps"] = args.replay_queue
        run("branch", queue, "merge-target", ours)
        run("switch", queue, "merge-target")
        merged = json.loads(run("merge", queue, "unrebased", "-m", "independent function edits"))
        assert merged["status"] == "merged", merged
        parents = json.loads(run("show", queue, "HEAD"))["parents"]
        assert parents == [ours, incoming[-1]], parents
        checkout(queue, "HEAD", functions(a, args.replay_queue), True)
        report["two_parent_merge"] = True
        assert json.loads(run("merge-base", queue, ours, incoming[-1])) == [base]
        run("gc", queue)
        run("fsck", queue)

        # Continue active randomized reconstruction until the complete timed
        # window ends, after every mandatory long-chain operation has passed.
        iterations = 0
        while time.monotonic() - started < args.duration:
            index = rng.randrange(len(commits))
            checkout(rng.choice([clone, restored, forks[-1][0]]), commits[index], versions[index])
            if index:
                current = apply(snapshots / f"{index - 1}.wasm", edges[index - 1][0], versions[index], rng.choice(binaries))
                work.write_bytes(current)
                apply(work, edges[index - 1][1], versions[index - 1], rng.choice(binaries))
            if iterations % 128 == 0:
                run("fsck", rng.choice([clone, restored, forks[-1][0], queue]))
            iterations += 1
        report["random_history_checks"] = iterations
        assert all(digest(p.read_bytes()) == record["sha256"] for p, record in zip(binaries, report["tools"]))
        report["passed"] = True
        save()
        print(f"PASS history: {args.steps} chain steps, {report['forks']} independent forks, {report['patch_applications']} patch applications, {report['checkouts']} historical checkouts", flush=True)
    except BaseException as error:
        report["failures"].append(repr(error))
        save()
        raise


if __name__ == "__main__":
    main()
