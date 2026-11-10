#!/usr/bin/env python3
"""Cross-version patch qualification across all Core syntax locations."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import time
from collections import Counter
from collections import deque
from concurrent.futures import ThreadPoolExecutor

from syntax_ports import inventory as port_inventory, probes as port_probes

HEADER = b"\0asm\x01\0\0\0"


def leb(n):
    result = bytearray()
    while n >= 128:
        result.append((n & 127) | 128)
        n >>= 7
    return bytes(result + bytes([n]))


def section(kind, payload):
    return bytes([kind]) + leb(len(payload)) + payload


def name(s):
    raw = s.encode()
    return leb(len(raw)) + raw


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cases():
    # Each case has an independently framed custom section after the new syntax.
    # Payloads deliberately include plausible known opcodes after unknown fields.
    suffix = section(0, name("future.tail") + b"\x00\xff\x41\x00\x0b")
    base = HEADER + suffix
    yield "module-version", base, b"\0asm\x02\0\0\0" + suffix
    payloads = {
        "custom-payload": (0, name("future.vendor") + b"\xff\x41\x00\x0b"),
        "type-constructor": (1, b"\x01\x7a\x60\x00\x00"),
        "import-kind": (2, b"\x01\x01m\x01x\x7d\x00\x00"),
        "function-type-index": (3, b"\x01\x80\x80\x80\x80\x0f"),
        "table-reference": (4, b"\x01\xff\x00\x00"),
        "memory-flags": (5, b"\x01\x80\x01\x00\x00"),
        "global-value": (6, b"\x01\xff\x00\x00\x41\x00\x0b"),
        "export-kind": (7, b"\x01\x01x\x7d\x00"),
        "start-index": (8, b"\xff\xff\xff\xff\x0f"),
        "element-flags": (9, b"\x01\x08\x41\x00\x0b\x00"),
        "data-flags": (11, b"\x01\x03\x41\x00\x0b\x00"),
        "data-count": (12, b"\xff\xff\xff\xff\x0f"),
        "tag-kind": (13, b"\x01\x7d\x00"),
        "unknown-section": (250, b"\x00\x41\x00\x0b"),
        "name-subsection": (0, name("name") + section(127, b"\xff\x00")),
        "linking-subsection": (0, name("linking") + b"\x02" + section(127, b"\xff\x00")),
        "custom-name": (0, name("future.custom.convention") + b"\x02\x80\x00"),
    }
    for label, (kind, payload) in payloads.items():
        yield label, base, HEADER + section(kind, payload) + suffix
        # Every payload position can change, including nested field lengths and
        # unknown bytes. Patches operate on original offsets, never decoded IR.
        for index in range(len(payload)):
            changed = bytearray(payload)
            changed[index] ^= 0x80
            yield f"{label}-byte-{index}", HEADER + section(kind, payload) + suffix, HEADER + section(kind, changed) + suffix
    prelude = HEADER + section(1, b"\x01\x60\x00\x00") + section(3, b"\x02\x00\x00")
    for label, instructions in {
        "future-opcode": b"\xfe\x05\x41\x00\x0b",
        "future-prefix": b"\xff\xff\xff\xff\xff\x0f\x41\x00\x0b",
        "future-memarg": b"\x28\x80\x01\x41\x00\x0b",
        "future-ordering": b"\xfe\x10\x12\x7f\x00\x0b",
        "future-local-type": b"\x01\x01\x7a\x41\x00\x0b",
        "future-block-type": b"\x02\x7a\x41\x00\x0b",
        "future-select-type": b"\x1c\x01\x7a\x41\x00\x0b",
        "acquire-release": b"\xfe\x04\xfe\x03\x01\x0b",
        "array-target": b"\x28\x22\x00\x00\x0b",
    }.items():
        body = instructions if label == "future-local-type" else b"\x00" + instructions
        code = b"\x02" + leb(len(body)) + body + b"\x03\x00\x01\x0b"
        yield label, base, prelude + section(10, code) + suffix


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("previous", "current", "output"):
        parser.add_argument("--" + flag, type=Path, required=True)
    parser.add_argument("--corpus", type=Path, action="append", default=[])
    parser.add_argument("--random-cases", type=int, default=0)
    parser.add_argument("--duration", type=float, default=0)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--heartbeat", type=float, default=600)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--quick-ports", action="store_true", help="Two selectors per admission site; smoke tests only")
    args = parser.parse_args()
    assert args.random_cases >= 0 and args.duration >= 0 and args.heartbeat > 0
    assert 1 <= args.workers <= 16
    assert not args.quick_ports or args.duration < 1800
    scratch = Path(os.environ["WASM_FUZZER_TEST_ROOT"]).resolve()
    assert Path.cwd().resolve().is_relative_to(scratch)
    output = args.output.resolve()
    assert output.is_relative_to(scratch)
    group = Path("/sys/fs/cgroup") / Path("/proc/self/cgroup").read_text().split("0::", 1)[1].strip().lstrip("/")
    assert (group / "memory.max").read_text().strip() == "68719476736"
    assert (group / "memory.swap.max").read_text().strip() == "0"
    output.mkdir()
    os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:4])
    binaries = {"previous": args.previous.resolve(), "current": args.current.resolve()}
    started = time.monotonic()
    report = {"schema": 3, "cgroup": str(group), "tools": {}, "cases": [], "applications": 0,
              "seed": args.seed, "required_seconds": args.duration, "random_cases": 0,
              "mutation_counts": {}, "location_counts": {}, "sources": [], "failures": [],
              "port_cases": 0, "quick_ports": args.quick_ports,
              "port_inventory_sha256": sha(Path(__file__).with_name("syntax_ports.py"))}
    counts, locations, ports = Counter(), Counter(), Counter()
    next_status, next_heartbeat = 0, args.heartbeat

    def save():
        report["elapsed_seconds"] = time.monotonic() - started
        report["mutation_counts"] = dict(counts)
        report["location_counts"] = dict(locations)
        report["port_counts"] = dict(ports)
        pending = output / "report.pending"
        pending.write_text(json.dumps(report, indent=2) + "\n")
        pending.replace(output / "report.json")

    def progress():
        nonlocal next_status, next_heartbeat
        elapsed = time.monotonic() - started
        if elapsed >= next_status:
            save()
            print(f"STATUS syntax {elapsed:.1f}s ports={report['port_cases']} random={report['random_cases']} applications={report['applications']} failures=0", flush=True)
            next_status = elapsed + 30
        if elapsed >= next_heartbeat:
            print(f"HEARTBEAT syntax {next_heartbeat / 60:g}min ports={report['port_cases']} random={report['random_cases']} failures=0", flush=True)
            next_heartbeat += args.heartbeat

    def run(binary, *options, success=True):
        result = subprocess.run([str(binary), *map(str, options)], cwd=output,
                                capture_output=True, timeout=60)
        if (result.returncode == 0) != success:
            (output / "failure.stdout").write_bytes(result.stdout)
            (output / "failure.stderr").write_bytes(result.stderr)
            raise RuntimeError(f"{binary.name} {options}: {result.returncode}: {result.stderr.decode(errors='replace')}")
        return result.stdout

    for label, binary in binaries.items():
        report["tools"][label] = {"sha256": sha(binary), "version": run(binary, "version").decode().strip()}
    def check(label, before, after, retain=True):
        folder = output / label
        folder.mkdir()
        paths = [folder / "before.wasm", folder / "after.wasm"]
        for path, data in zip(paths, [before, after]):
            path.write_bytes(data)
        for old, new in [paths, paths[::-1]]:
            for writer, binary in binaries.items():
                patch = folder / f"{writer}-{old.stem}.wgp"
                run(binary, "patch", old, new, "--output", patch)
                assert patch.read_bytes().startswith(b"WGTPAT01")
                for reader, consumer in binaries.items():
                    destination = folder / f"{writer}-{reader}-{old.stem}.wasm"
                    run(consumer, "apply", old, patch, "--output", destination)
                    assert destination.read_bytes() == new.read_bytes(), (label, writer, reader)
                # The newer reader must retain the old format's integrity gate.
                wrong = folder / "wrong.wasm"
                wrong.write_bytes(old.read_bytes() + b"\x00")
                run(binaries["current"], "apply", wrong, patch, success=False)
        inspected = json.loads(run(binaries["current"], "inspect", paths[1], "--json"))
        assert inspected["audit"]["standard"]["grammar"]["schema"] == "wasm-git-grammar-v1"
        audited = inspected["audit"]
        assert audited["byte_len"] == len(after)

        def bounded(span):
            assert 0 <= span["start"] <= span["end"] <= len(after), span

        def regions_bounded(regions):
            for region in regions:
                bounded(region["span"])
                regions_bounded(region["children"])

        for entry in audited["sections"]:
            for key in ["span", "header_span", "payload_span"]:
                bounded(entry[key])
            regions_bounded(entry["regions"])
        for entry in audited["standard"]["evidence"]:
            bounded(entry["span"])
            assert entry["raw_hex"] == after[entry["span"]["start"]:entry["span"]["end"]].hex(), entry
        if label in {"future-opcode", "future-prefix", "future-memarg", "future-ordering", "future-local-type", "future-block-type", "future-select-type"}:
            audit = inspected["audit"]
            unknown = [e for e in audit["standard"]["evidence"] if e["status"] == "unknown"]
            assert unknown, label
            # The independently framed second function remains interpretable.
            assert "Nop" in json.dumps(audit), label
        if retain:
            report["cases"].append(label)
        else:
            shutil.rmtree(folder)
        return 8

    for label, before, after in cases():
        report["applications"] += check(label, before, after)
    save()

    # All byte-valued selectors are exhaustive. Wider fields include every
    # single-bit and complemented-bit value, LEB boundaries, and seeded random
    # values. Keep only a bounded set of subprocesses and inputs in flight.
    port_rng = random.Random(args.seed ^ 0x5349544553)
    catalog = port_inventory()
    report["required_ports"] = {
        port.name: {"encoding": port.encoding, "selectors": len(port.values(random.Random(0), quick=args.quick_ports))}
        for port in catalog
    }
    seen = {port.name: set() for port in catalog}
    with (output / "port-cases.jsonl").open("w") as records, ThreadPoolExecutor(max_workers=args.workers) as executor:
        pending = deque()

        def finish():
            future, record = pending.popleft()
            try:
                report["applications"] += future.result()
            except Exception as error:
                report["failures"].append({**record, "error": str(error)})
                save()
                raise
            ports[record["port"]] += 1
            seen[record["port"]].add(record["value"])
            report["port_cases"] += 1
            progress()

        for index, (port, value, before, after) in enumerate(port_probes(port_rng, quick=args.quick_ports)):
            label = f"port-{index:08d}"
            record = {"index": index, "port": port.name, "value": value,
                      "before_hex": before.hex(), "after_hex": after.hex(),
                      "source_sha256": hashlib.sha256(before).hexdigest(),
                      "target_sha256": hashlib.sha256(after).hexdigest()}
            records.write(json.dumps(record) + "\n")
            records.flush()
            pending.append((executor.submit(check, label, before, after, False), record))
            if len(pending) >= args.workers * 2:
                finish()
        while pending:
            finish()
    report["port_unique_selectors"] = {key: len(values) for key, values in seen.items()}
    report["all_ports_covered"] = all(len(seen[key]) == spec["selectors"] for key, spec in report["required_ports"].items())
    assert report["all_ports_covered"]
    save()

    # Sources include every section family even when an upstream generator did
    # not happen to emit that section. Real fuzzer corpora add nested field spans.
    sources = [(label, after) for label, _, after in cases() if "-byte-" not in label]
    for corpus in args.corpus:
        corpus = corpus.resolve()
        assert corpus.is_relative_to(scratch)
        for line in (corpus / "manifest.jsonl").read_text().splitlines():
            item = json.loads(line)
            source = corpus / item["file"]
            data = source.read_bytes()
            assert hashlib.sha256(data).hexdigest() == item["sha256"]
            sources.append((str(source), data))
    pools = {}
    for index, (name, data) in enumerate(sources):
        report["sources"].append({"name": name, "sha256": hashlib.sha256(data).hexdigest()})
        path = output / "source.wasm"
        path.write_bytes(data)
        audit = json.loads(run(binaries["current"], "inspect", path, "--json"))["audit"]
        pools.setdefault("module-version", []).append((index, 4, 8, "module/version", None))
        for section_record in audit["sections"]:
            family = f"section-{section_record['id']}"
            frame = (section_record["span"]["start"], section_record["payload_span"]["start"],
                     section_record["payload_span"]["end"], section_record["id"])
            targets = pools.setdefault(family, [])
            for key in ["header_span", "payload_span"]:
                span = section_record[key]
                targets.append((index, span["start"], span["end"], section_record["path"] + "/" + key, frame))

            def visit(regions):
                for region in regions:
                    span = region["span"]
                    if 8 <= span["start"] <= span["end"] <= len(data):
                        targets.append((index, span["start"], span["end"], region["path"], frame))
                    visit(region["children"])
            visit(section_record["regions"])
    assert {f"section-{i}" for i in range(14)} <= set(pools), set(pools)
    report["available_locations"] = {k: len(v) for k, v in pools.items()}
    rng = random.Random(args.seed)
    families = sorted(pools)
    with (output / "random-cases.jsonl").open("w") as records:
        while report["random_cases"] < args.random_cases or time.monotonic() - started < args.duration:
            index = report["random_cases"]
            family = families[index % len(families)]
            source_index, lo, hi, field, frame = rng.choice(pools[family])
            source_name, before = sources[source_index]
            kind = rng.choice(["flip", "replace", "insert", "delete", "duplicate", "reverse", "future-selector"])
            if family == "module-version":
                lo, hi, replacement, kind = 4, 8, rng.getrandbits(32).to_bytes(4, "little"), "version"
            else:
                lo = rng.randrange(lo, max(lo + 1, hi))
                hi = rng.randrange(lo, min(hi, lo + 32) + 1)
                old = before[lo:hi]
                if kind == "flip" and old:
                    replacement = bytes([old[0] ^ (1 << rng.randrange(8))]) + old[1:]
                elif kind == "delete":
                    replacement = b""
                elif kind == "duplicate":
                    replacement = old * 2 + rng.randbytes(1)
                elif kind == "reverse":
                    replacement = old[::-1]
                elif kind == "future-selector":
                    replacement = b"\xff" + leb(rng.getrandbits(32))
                else:
                    if kind == "insert":
                        hi = lo
                    replacement = rng.randbytes(rng.randrange(1, 33))
            after = before[:lo] + replacement + before[hi:]
            reframe = frame is not None and lo >= frame[1] and hi <= frame[2]
            if reframe:
                start, payload, end, section_id = frame
                changed = before[payload:lo] + replacement + before[hi:end]
                after = before[:start] + section(section_id, changed) + before[end:]
            if after == before:
                continue
            label = f"random-{index:08d}"
            record = {"index": index, "location": family, "field": field, "mutation": kind,
                      "source": source_name, "source_sha256": hashlib.sha256(before).hexdigest(),
                      "span": [lo, hi], "replacement_hex": replacement.hex(), "reframe_section": reframe,
                      "target_sha256": hashlib.sha256(after).hexdigest()}
            records.write(json.dumps(record) + "\n")
            records.flush()
            try:
                report["applications"] += check(label, before, after, retain=False)
            except Exception as error:
                report["failures"].append({**record, "error": str(error)})
                save()
                raise
            report["random_cases"] += 1
            counts[kind] += 1
            locations[family] += 1
            progress()
    report["passed"] = all(sha(binaries[k]) == v["sha256"] for k, v in report["tools"].items())
    assert report["passed"]
    assert report["port_inventory_sha256"] == sha(Path(__file__).with_name("syntax_ports.py"))
    save()
    print(f"PASS syntax upgrade: {len(report['cases'])} fixed + {report['port_cases']} port + {report['random_cases']} random cases, {report['applications']} patch applications", flush=True)


if __name__ == "__main__":
    main()
