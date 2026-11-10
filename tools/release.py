#!/usr/bin/env python3
"""Build, exercise and package release binaries. Scratch data stays in .tmp/."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import struct
import subprocess
import tarfile
import tempfile
import threading
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SCRATCH = Path(os.environ.get("WASM_RELEASE_SCRATCH", ROOT / ".tmp")).resolve()


def run(args, **kwargs):
    print("+", " ".join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def leb(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def section(kind, payload):
    return bytes([kind]) + leb(len(payload)) + payload


def string(value):
    data = value.encode()
    return leb(len(data)) + data


def module(edited):
    # 64 named functions exercise the parallel scan threshold. A nested block
    # contains the only code edit. Opaque custom bytes exceed the join threshold.
    count = 64
    exports = leb(count) + b"".join(string(f"f{i}") + b"\0" + leb(i) for i in range(count))
    bodies = []
    for i in range(count):
        value = i % 50 + (7 if edited and i == 31 else 0)
        body = b"\0\x02\x7f\x41" + leb(value) + b"\x0b\x0b"
        bodies.append(leb(len(body)) + body)
    names = leb(count) + b"".join(leb(i) + string(f"f{i}") for i in range(count))
    custom = (bytes(range(256)) * 256) + bytes([int(edited)])
    return (b"\0asm\x01\0\0\0" + section(1, b"\x01\x60\0\x01\x7f")
            + section(3, leb(count) + b"\0" * count) + section(7, exports)
            + section(10, leb(count) + b"".join(bodies))
            + section(0, string("name") + section(1, names))
            + section(0, string("vendor.audit") + custom))


def memtag_module(edited, wide):
    """Independent producer: full uwvm2-ros u32 IDs, opcode-specific memidx."""
    shapes = [(0x20, 0, 1), (0x21, 1, 1), (0x22, 2, 1), (0x23, 0, 1),
              (0x24, 0, 1), (0x25, 2, 1), (0x26, 2, 1), (0x27, 1, 1),
              (0x28, 1, 1), (0x29, 2, 1), (0x2a, 2, 1), (0x2b, 2, 0),
              (0x2c, 2, 0), (0x2d, 1, 1), (0x2e, 2, 1), (0x2f, 2, 1),
              (0x36, 3, 1), (0x37, 4, 1), (0x38, 4, 1)]
    body = bytearray(b"\0\x02\x40")
    for opcode, inputs, outputs in shapes:
        body.extend(bytes([0x42 if wide else 0x41, 0]) * inputs)
        opcode = 0x2c if edited and opcode == 0x2b else opcode
        body.extend(b"\xff" + leb(0x01010000 + opcode) + b"\x01")
        body.extend(b"\x1a" * outputs)
    body.extend(b"\x0b\x0b")
    return (b"\0asm\x01\0\0\0" + section(1, b"\x01\x60\0\0")
            + section(3, b"\x01\0") + section(5, bytes([2, 4 if wide else 0, 1,
                                                       4 if wide else 0, 1]))
            + section(7, b"\x01\x01f\0\0")
            + section(10, b"\x01" + leb(len(body)) + body)
            + section(0, string("uwvm.memtag.linear.globals")
                      + bytes([2, 1, 1, 32 if edited else 16, 16])))


def smoke(args):
    parent = SCRATCH / "release-smoke"
    parent.mkdir(parents=True, exist_ok=True)
    client = Path(args.client).resolve()
    prefix = args.runner_arg or []
    with tempfile.TemporaryDirectory(dir=parent, prefix=args.label + "-") as scratch:
        root = Path(scratch)
        old, new = module(False), module(True)
        (root / "old.wasm").write_bytes(old)
        (root / "new.wasm").write_bytes(new)
        calls = 0

        def command(*options, success=True):
            nonlocal calls
            calls += 1
            result = subprocess.run([*prefix, str(client), *options], cwd=root,
                                    capture_output=True, timeout=180)
            if (result.returncode == 0) != success:
                raise RuntimeError(f"{options}: exit {result.returncode}\n"
                                   + result.stderr.decode(errors="replace"))
            return result.stdout

        options = json.loads(command("version", "--build-options"))
        for wide in (False, True):
            before, after = memtag_module(False, wide), memtag_module(True, wide)
            (root / "memtag-old.wasm").write_bytes(before)
            (root / "memtag-new.wasm").write_bytes(after)
            inspected = json.loads(command("inspect", "memtag-new.wasm", "--json"))
            standard = inspected["audit"]["standard"]
            assert standard["validation"] == "valid", standard
            assert sum(e["feature"] == "memtag_extension" for e in standard["evidence"]) == 19
            changed = json.loads(command("diff", "memtag-old.wasm", "memtag-new.wasm",
                                         "--compact", "--format", "json"))
            assert changed["summary"]["functions_changed"] == 1, changed["summary"]
            selected = json.loads(command("diff", "memtag-old.wasm", "memtag-new.wasm",
                                          "--section", "custom:uwvm.memtag.linear.globals",
                                          "--format", "json"))
            assert "global[0]/address" in json.dumps(selected), selected
            name = "memtag64" if wide else "memtag32"
            command("patch", "memtag-old.wasm", "memtag-new.wasm", "--output", name + ".wgp")
            command("apply", "memtag-old.wasm", name + ".wgp", "--output", name + ".wasm")
            assert (root / (name + ".wasm")).read_bytes() == after
            command("init", name)
            command("commit", name, "memtag-old.wasm", "-m", "before")
            first_memtag = command("rev-parse", name, "HEAD").decode().strip()
            command("commit", name, "memtag-new.wasm", "-m", "storez and globals update")
            last_memtag = command("rev-parse", name, "HEAD").decode().strip()
            command("gc", name)
            command("fsck", name)
            for revision, expected in ((first_memtag, before), (last_memtag, after)):
                destination = name + "-" + revision + ".wasm"
                command("checkout", name, revision, destination)
                assert (root / destination).read_bytes() == expected
            command("clone", name, name + "-clone")
            command("checkout", name + "-clone", "HEAD", name + "-clone.wasm")
            assert (root / (name + "-clone.wasm")).read_bytes() == after
            command("bundle", name, name + ".wgb")
            command("init", name + "-bundle")
            command("unbundle", name + "-bundle", name + ".wgb", "main")
            command("checkout", name + "-bundle", "HEAD", name + "-bundle.wasm")
            assert (root / (name + "-bundle.wasm")).read_bytes() == after
        report = json.loads(command("diff", "old.wasm", "new.wasm", "--compact", "--format", "json"))
        assert report["summary"]["functions_changed"] == 1, report["summary"]
        serial = json.loads(command("--threads", "1", "diff", "old.wasm", "new.wasm",
                                    "--compact", "--format", "json"))
        assert report == serial, "thread selection changed audit evidence"
        if not options["host_threads"]:
            command("--threads", "2", "diff", "old.wasm", "new.wasm", success=False)
        for selector in ("code", "custom:vendor.audit"):
            selection = json.loads(command("diff", "old.wasm", "new.wasm", "--section",
                                           selector, "--format", "json"))
            assert selection["whole_module_patch"] is False
        command("patch", "old.wasm", "new.wasm", "--output", "update.wgp")
        command("apply", "old.wasm", "update.wgp", "--output", "restored.wasm")
        assert (root / "restored.wasm").read_bytes() == new
        command("apply", "old.wasm", "update.wgp", "--output", "restored.wasm", success=False)
        assert (root / "restored.wasm").read_bytes() == new
        command("init", "repo")
        command("patch", "old.wasm", "new.wasm", "--output", "repo/.wasm-git/export.wasm", success=False)
        assert not (root / "repo/.wasm-git/export.wasm").exists()
        if os.name != "nt":
            (root / "metadata-alias").symlink_to("repo/.wasm-git", target_is_directory=True)
            command("patch", "old.wasm", "new.wasm", "--output", "metadata-alias/export.wasm", success=False)
            assert not (root / "repo/.wasm-git/export.wasm").exists()
        command("commit", "repo", "old.wasm", "-m", "original")
        first = command("rev-parse", "repo", "HEAD").decode().strip()
        command("branch", "repo", "original")
        command("commit", "repo", "new.wasm", "-m", "nested block and opaque custom update")
        last = command("rev-parse", "repo", "HEAD").decode().strip()
        assert first != last
        command("gc", "repo")
        command("fsck", "repo")
        assert command("rev-parse", "repo", "HEAD").decode().strip() == last
        for revision, expected in ((first, old), (last, new)):
            name = revision + ".wasm"
            command("checkout", "repo", revision, name)
            assert (root / name).read_bytes() == expected
        command("bundle", "repo", "history.wgb")
        command("init", "from-bundle")
        command("unbundle", "from-bundle", "history.wgb", "main")
        command("fsck", "from-bundle")
        assert command("rev-parse", "from-bundle", "HEAD").decode().strip() == last
        command("clone", "repo", "local-clone")
        command("fsck", "local-clone")
        assert command("rev-parse", "local-clone", "HEAD").decode().strip() == last
        command("checkout", "local-clone", "HEAD", "cloned.wasm")
        assert (root / "cloned.wasm").read_bytes() == new

        if args.server:
            server = Path(args.server).resolve()
            with (root / "server.log").open("wb") as log:
                process = subprocess.Popen([*prefix, str(server), "--root", str(root),
                                            "--listen", "127.0.0.1:0", "--read-only"],
                                           cwd=root, stdout=subprocess.PIPE, stderr=log)
                try:
                    ready = queue.Queue()
                    threading.Thread(target=lambda: ready.put(process.stdout.readline()), daemon=True).start()
                    address = json.loads(ready.get(timeout=60))["listen"]
                    command("clone", f"http://{address}/repo", "remote-clone")
                    command("fsck", "remote-clone")
                    assert command("rev-parse", "remote-clone", "HEAD").decode().strip() == last
                    command("checkout", "remote-clone", "HEAD", "remote.wasm")
                    assert (root / "remote.wasm").read_bytes() == new
                    for wide in (False, True):
                        name = "memtag64" if wide else "memtag32"
                        command("clone", f"http://{address}/{name}", "remote-" + name)
                        command("checkout", "remote-" + name, "HEAD", "remote-" + name + ".wasm")
                        assert (root / ("remote-" + name + ".wasm")).read_bytes() == memtag_module(True, wide)
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
        print(f"PASS {args.label}: {calls} CLI checks; precise function diff; exact patch/history/GC/clone restoration", flush=True)


def windows_imports(path):
    """Read the actual PE import table; compiler flags alone are not proof."""
    data = path.read_bytes()
    if data[:2] != b"MZ":
        raise RuntimeError(f"not a PE executable: {path}")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise RuntimeError(f"invalid PE header: {path}")
    count = struct.unpack_from("<H", data, pe + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    if struct.unpack_from("<H", data, optional)[0] != 0x20B:
        raise RuntimeError("release Windows executables must be PE32+")
    sections = optional + optional_size

    def offset(rva):
        for i in range(count):
            size, virtual, raw_size, raw = struct.unpack_from("<4I", data, sections + i * 40 + 8)
            if virtual <= rva < virtual + max(size, raw_size):
                delta = rva - virtual
                if delta >= raw_size or raw + delta >= len(data):
                    break
                return raw + delta
        raise RuntimeError(f"unbacked PE import RVA: {rva}")

    # Import directory is index 1 in the PE32+ optional header.
    rva, size = struct.unpack_from("<2I", data, optional + 112 + 8)
    if not rva or size < 20:
        raise RuntimeError("missing Windows import directory")
    start = offset(rva)
    imports = []
    for pos in range(start, min(start + size, len(data)) - 19, 20):
        descriptor = struct.unpack_from("<5I", data, pos)
        if not any(descriptor):
            return imports
        name = offset(descriptor[3])
        end = data.find(b"\0", name, min(name + 512, len(data)))
        if end < 0:
            raise RuntimeError("unterminated PE import name")
        imports.append(data[name:end].decode("ascii").lower())
    raise RuntimeError("unterminated PE import directory")


def verify_windows_runtime(path):
    imports = windows_imports(path)
    forbidden = [name for name in imports if name.startswith(("vcruntime", "msvcp", "msvcr", "concrt", "api-ms-win-crt-"))
                 or name == "ucrtbase.dll"]
    if forbidden:
        raise RuntimeError(f"Windows release requires an external C/C++ runtime: {path}: {forbidden}")
    print(f"PASS static CRT imports: {path.name}: {', '.join(imports)}", flush=True)


def package(args):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    name = f"wasm-git-{version}-{args.label}"
    destination = SCRATCH / "dist"
    destination.mkdir(parents=True, exist_ok=True)
    # Explicit allowlist: no test data, build trees, credentials or profiles.
    files = [Path(args.client).resolve(), ROOT / "LICENSE", ROOT / "README.md",
             ROOT / "documents" / "RELEASE.eu_US.md", ROOT / "documents" / "RELEASE.zh_CN.md",
             ROOT / "documents" / "FEATURE_COMBINATIONS.eu_US.md",
             ROOT / "documents" / "UWVM2_ROS_EXTENSIONS.eu_US.md",
             ROOT / "documents" / "UWVM2_ROS_EXTENSIONS.zh_CN.md"]
    if args.server:
        files.append(Path(args.server).resolve())
    if args.label.endswith("windows-msvc"):
        verify_windows_runtime(Path(args.client).resolve())
        if args.server:
            verify_windows_runtime(Path(args.server).resolve())
    for path in files:
        if not path.is_file() or path.is_symlink():
            raise RuntimeError(f"missing or symlinked release input: {path}")
    if args.label.endswith("windows-msvc") or args.label.endswith("windows-gnu"):
        archive = destination / (name + ".zip")
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
            for path in files:
                output.write(path, f"{name}/{path.name}")
    else:
        archive = destination / (name + ".tar.gz")
        with tarfile.open(archive, "w:gz") as output:
            for path in files:
                output.add(path, arcname=f"{name}/{path.name}", recursive=False)
    print(archive)


def inventory(_args):
    paths = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).split(b"\0")
    forbidden = (".tmp/", "target/", "build/", "artifacts/", "evidence/", "reports/", "corpus/")
    for encoded in filter(None, paths):
        path = os.fsdecode(encoded)
        if path.startswith(forbidden) or "/__pycache__/" in path or path.endswith((".log", ".pyc")):
            raise RuntimeError(f"generated product tracked by Git: {path}")
    objects = subprocess.check_output(["git", "rev-list", "--objects", "--all"], cwd=ROOT)
    metadata = subprocess.check_output(["git", "cat-file", "--batch-check=%(objecttype) %(objectsize) %(rest)"],
                                       cwd=ROOT, input=objects).decode()
    blobs = [line.split(" ", 2) for line in metadata.splitlines() if line.startswith("blob ")]
    oversized = [line for line in blobs if int(line[1]) >= 1024 * 1024]
    if oversized:
        raise RuntimeError(f"review large historical blobs before release: {oversized}")
    print(f"PASS tracked outputs and all-ref history: {len(blobs)} blobs; none >= 1 MiB")


def checksums(_args):
    root = SCRATCH / "dist"
    archives = sorted([*root.glob("*.tar.gz"), *root.glob("*.zip")])
    if len(archives) != 10:
        raise RuntimeError(f"expected exactly 10 release archives, found {len(archives)}")
    lines = [f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in archives]
    (root / "SHA256SUMS").write_text("".join(lines))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("smoke", "package"):
        child = commands.add_parser(name)
        child.add_argument("--client", required=True)
        child.add_argument("--server")
        child.add_argument("--label", required=True)
        if name == "smoke":
            child.add_argument("--runner-arg", action="append", default=[])
    commands.add_parser("inventory")
    commands.add_parser("checksums")
    args = parser.parse_args()
    {"smoke": smoke, "package": package, "inventory": inventory, "checksums": checksums}[args.command](args)


if __name__ == "__main__":
    main()
