#!/usr/bin/env python3
"""Build a validated WASIp1 client, including MVP-compatible std and libc."""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
LIBC_REV = "3f7eb4c7d6ede4dde3c4bffa6ed14e8d656fe93f"
SDK = ("https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-27/"
       "wasi-sdk-27.0-x86_64-linux.tar.gz",
       "b7d4d944c88503e4f21d84af07ac293e3440b1b6210bfd7fe78e0afd92c23bc2",
       "wasi-sdk-27.0-x86_64-linux")
LIBC = (f"https://codeload.github.com/WebAssembly/wasi-libc/tar.gz/{LIBC_REV}",
        "6bb86e09dc5ed43260e81176537d702568689cf4203b8beb1f505609c4139ca5",
        f"wasi-libc-{LIBC_REV}")
TOOLS = ("https://github.com/bytecodealliance/wasm-tools/releases/download/v1.263.0/"
         "wasm-tools-1.263.0-x86_64-linux.tar.gz",
         "21c036c1740a0552124c79c6e2170cb61c5b016005671a6d5bc06cf642165431",
         "wasm-tools-1.263.0-x86_64-linux")
RUNTIME = ("https://github.com/bytecodealliance/wasmtime/releases/download/v36.0.20/"
           "wasmtime-v36.0.20-x86_64-linux.tar.xz",
           "66d843c88cd9ecd5ca31004301ee337b8750bf08a839d06c2e7598e531b06de8",
           "wasmtime-v36.0.20-x86_64-linux")


def run(args, **kwargs):
    print("+", " ".join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), check=True, **kwargs)


def fetch(root, item):
    url, digest, name = item
    archive = root / (name + ".archive")
    if not archive.exists():
        run(["curl", "-fSL", "--retry", "3", url, "-o", archive])
    if hashlib.sha256(archive.read_bytes()).hexdigest() != digest:
        raise RuntimeError(f"SHA-256 mismatch: {archive}")
    destination = root / name
    if not destination.exists():
        with tarfile.open(archive) as source:
            source.extractall(root, filter="data")
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=["mvp", "core2"], required=True)
    parser.add_argument("--sdk", type=Path)
    parser.add_argument("--libc-source", type=Path)
    parser.add_argument("--wasm-tools", type=Path)
    parser.add_argument("--wasmtime", type=Path)
    parser.add_argument("--build-only", action="store_true", help="compile/package in CI; qualification runs in the existing local cgroup")
    args = parser.parse_args()
    root = Path(os.environ.get("WASM_RELEASE_SCRATCH", ROOT / ".tmp")).resolve() / "wasi"
    root.mkdir(parents=True, exist_ok=True)
    sdk = args.sdk.resolve() if args.sdk else fetch(root, SDK)
    libc = args.libc_source.resolve() if args.libc_source else fetch(root, LIBC)
    validator = None if args.build_only else (args.wasm_tools.resolve() if args.wasm_tools else fetch(root, TOOLS) / "wasm-tools")
    runtime = None if args.build_only else (args.wasmtime.resolve() if args.wasmtime else fetch(root, RUNTIME) / "wasmtime")
    sysroot = root / "sysroot-mvp"
    # The upstream Makefile explicitly enables bulk-memory for memcpy/move/set.
    # Clearing this source override keeps musl's existing scalar implementations.
    # Rebuild the entire libc; disabling features only in Rust is insufficient.
    run(["make", "-C", libc, f"-j{min(8, os.cpu_count() or 1)}",
         f"CC={sdk}/bin/clang", f"AR={sdk}/bin/llvm-ar", f"NM={sdk}/bin/llvm-nm",
         "TARGET_TRIPLE=wasm32-wasip1", f"SYSROOT={sysroot}", f"OBJDIR={root}/libc-objects",
         "EXTRA_CFLAGS=-O2 -DNDEBUG -mcpu=mvp", "BULK_MEMORY_SOURCES=", "BUILD_LIBSETJMP=no"])
    features = "+bulk-memory,+mutable-globals,+nontrapping-fptoint,+sign-ext,+reference-types,+multivalue,+simd128"
    rust_flags = ["-Ctarget-cpu=mvp", "-Clink-self-contained=no",
                  f"-Clink-arg=--sysroot={sysroot}", "-Clink-arg=-mcpu=mvp"]
    c_flags = f"-mcpu=mvp --sysroot={sysroot}"
    if args.profile == "core2":
        rust_flags.append(f"-Ctarget-feature={features}")
        c_flags += " -mbulk-memory -mmutable-globals -mnontrapping-fptoint -msign-ext -mreference-types -mmultivalue -msimd128"
    target = root / f"target-{args.profile}"
    environment = dict(os.environ, RUSTC_BOOTSTRAP="1", RUSTFLAGS=" ".join(rust_flags),
                       CARGO_TARGET_DIR=str(target), CC_wasm32_wasip1=f"{sdk}/bin/clang",
                       AR_wasm32_wasip1=f"{sdk}/bin/llvm-ar", CFLAGS_wasm32_wasip1=c_flags,
                       CARGO_TARGET_WASM32_WASIP1_LINKER=f"{sdk}/bin/clang")
    # Scoped build-std on pinned Rust 1.93.1; never set RUSTC_BOOTSTRAP globally.
    run(["cargo", "build", "--locked", "--release", "--no-default-features", "--features", "client",
         "--target", "wasm32-wasip1", "-Zbuild-std=std,panic_abort"], cwd=ROOT, env=environment)
    binary = target / "wasm32-wasip1" / "release" / "wasm-git.wasm"
    if not args.build_only:
        run([validator, "validate", f"--features={'mvp' if args.profile == 'mvp' else 'wasm2'}", binary])
        # All imports must remain Preview 1; no Preview 2 component adapter or host
        # networking extensions may silently become a release requirement.
        imports = subprocess.check_output([validator, "print", "--skeleton", binary], text=True)
        import re
        modules = re.findall(r'\(import "([^"]+)"', imports)
        if not modules or set(modules) != {"wasi_snapshot_preview1"}:
            raise RuntimeError(f"unexpected import modules: {set(modules)}")
        run([os.sys.executable, ROOT / "tools/release.py", "smoke", "--label", f"wasip1-{args.profile}",
             "--client", binary, f"--runner-arg={runtime}", "--runner-arg=run",
             "--runner-arg=-Ccache=y", "--runner-arg=--dir=.::/"])
    run([os.sys.executable, ROOT / "tools/release.py", "package", "--label",
         f"wasm32-wasip1-{args.profile}", "--client", binary])


if __name__ == "__main__":
    main()
