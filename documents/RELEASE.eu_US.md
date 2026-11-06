# Release Engineering and Platform Support

**Edition:** 2026-11-06 · [简体中文](RELEASE.zh_CN.md) · [Library](README.eu_US.md)

## 1. Release contents and compatibility

GitHub releases contain ten platform archives and `SHA256SUMS`. Each native
archive contains the `wasm-git` client, `wasm-git-server`, Apache license, project
overview, and this bilingual guide. Each WASI archive contains only the client
and those documents. Archives are assembled from an explicit file allowlist;
build trees, test repositories, generated modules, profiles, downloaded tools,
tokens, and temporary signing keys are never package inputs.

Native release targets are Linux x86_64, AArch64, ARMv7 hard-float and RISC-V
RV64GC (static musl); Windows x86_64 and ARM64 (MSVC with a statically linked CRT); and macOS x86_64 and ARM64.
WASI targets are `wasm32-wasip1-mvp` and `wasm32-wasip1-core2`.
These are the tested release targets, not a claim that every CPU or OS version
is supported. The Rust toolchain is pinned to 1.93.1. Linux cross compilation
uses cargo-zigbuild 0.24.1 and Zig 0.15.2. Native Windows and macOS binaries are
built and executed on their respective GitHub-hosted operating systems.

Windows packages do not require a separate VC redistributable. Packaging reads
the PE import tables of both executables and rejects dynamic CRT dependencies.

The client is still the default Cargo build. Use `--no-default-features
--features server` for the standalone server or `--features server` for both.
The object, pack and network formats are uwgit formats, not native Git formats.

## 2. WASI Preview 1 profiles

Both Wasm artifacts are command modules importing only
`wasi_snapshot_preview1`. MVP and Core 2.0 describe the instruction/validation
profile of **uwgit itself**. They do not restrict the features in modules that
uwgit can audit. The same lossless parser and custom-section support are present
in both builds.

`tools/build-wasi.py` rebuilds Rust `std` and `panic_abort` using a scoped
`RUSTC_BOOTSTRAP=1`/`-Zbuild-std` invocation. It also rebuilds wasi-libc from the
exact revision used by WASI SDK 27, with `-mcpu=mvp` and the upstream Makefile's
forced bulk-memory compilation disabled. This retains the existing scalar musl
memory routines. It does not postprocess the application to hide validation
failures. Zstd C sources are compiled for the selected profile. The bundled Rust
libc is explicitly excluded from linking. The complete executable must pass
independent wasm-tools validation with `mvp` or `wasm2`, and its import modules
are checked before packaging. Downloaded SDK, libc, validator and runtime
archives have pinned SHA-256 digests.

Build and test on an x86_64 Linux build host with Python 3.11+, make, curl, tar,
and Rust 1.93.1 with `rust-src` and `wasm32-wasip1` installed:

```sh
python3 tools/build-wasi.py --profile mvp
python3 tools/build-wasi.py --profile core2
```

The resulting archives are under `.tmp/dist/`. To run the extracted executable
with Wasmtime, preopen the working directory as the guest root:

```sh
wasmtime run --dir .::/ wasm-git.wasm diff old.wasm new.wasm --compact
wasmtime run --dir .::/ wasm-git.wasm init repository
```

WASIp1 execution uses one worker. `--threads 0` and `--threads 1` work;
larger explicit requests return an error instead of attempting to spawn threads.
Native builds continue to use all available logical CPUs by default.
WASIp1 supplies neither process spawning nor portable listener/client sockets
nor advisory file locking through this Rust target. External SSH/GPG/curl/hooks,
HTTP serving, selective fetch service locks and replicated pool operations
therefore require a native build. These capabilities are not emulated by
silently dropping locks. The release smoke covers offline diff, section selection,
patch/apply, commit/history, branch, GC, integrity checking, bundle import and
local clone with exact original bytes.

WASI path resolution uses wasi-libc `realpath` within preopened directories;
it resolves symlinks rather than treating lexical path normalization as a
security boundary. Rust's unavailable process ID is represented as zero only
in diagnostic/staging-name fields. Exclusive file creation still arbitrates
publication and locks. Windows and WASI do not have portable directory `fsync`
through the APIs used here. File contents are synced before publication; crash
recovery remains available, but directory-entry power-loss durability depends
on the host filesystem. Native Unix directory sync errors still propagate.
`version --build-options` reports these host capability distinctions.

## 3. Tests and local resource limits

Every release build runs `tools/release.py smoke`. The fixture has 64 named
functions, one nested-block constant edit, and a separate opaque custom-section
edit exceeding 64 KiB. Checks require a single changed function, identical
serial/default audit evidence, functioning section filters, byte-for-byte
patch restoration, refusal to overwrite an existing export, stable commit IDs
after GC, exact old/new history restoration, bundle import, and local clone.
Native archives additionally exercise the standalone HTTP server and verify a
clone of packed history through the client. Regression suites run separately;
this smoke does not claim exhaustive instruction or operating-system coverage.

Linux release binaries are executed under QEMU user-mode emulation for all four
listed architectures. This checks their CPU ABI and Linux syscall behavior; it
does **not** boot Windows or macOS guests. Full-system QEMU testing of those
systems requires supplied, bootable guest images and guest login/automation
configuration. Native Windows/macOS CI execution is a separate verification
result and must not be reported as local QEMU guest testing.

On a systemd user-session Linux host, use the following wrapper to enforce
64 GiB resident-memory accounting and zero cgroup swap allowance:

```sh
python3 tools/cgroup-run.py -- cargo test --locked --all-features
python3 tools/cgroup-run.py -- python3 tools/release.py smoke \
  --label aarch64-qemu --runner-arg=qemu-aarch64 \
  --client .tmp/target/aarch64-unknown-linux-musl/release/wasm-git \
  --server .tmp/target/aarch64-unknown-linux-musl/release/wasm-git-server
```

The wrapper checks the kernel's effective limits, records peak memory and OOM
events, and stops only its own transient unit. Compilation, test scratch data,
logs, caches, and downloaded toolchains belong under `.tmp/`; required regression
source and intentional small fixtures remain tracked. The 64 GiB limit is a
local test limit, not a claim about GitHub-hosted runner memory.

## 4. CI and publication gates

The CI workflow performs formatting, feature-catalog validation, warning-free
Clippy, the complete Linux regression suite, client-only/server-only build
checks, and the cross-platform release matrix. Native Windows/macOS jobs run
library and CLI/server regressions before their release smoke. The WASI jobs
independently validate and execute both final modules using Wasmtime.

A pushed `vVERSION` tag invokes the same gates. Publication requires every job
to succeed and the tag version to equal `Cargo.toml`. The publisher downloads
only the ten release artifacts from that run, creates SHA-256 checksums, and
publishes a GitHub **prerelease** with the checked-in release notes. PR builds
have read-only permissions; only the final tag publication job can write releases.
Before tagging, `tools/release.py inventory` checks tracked output paths and all
reachable Git history for blobs at least 1 MiB. A large intentional source file
requires an explicit review/update of this policy; generated products must be
removed from history before publication.

## 5. References

- [Rust WASIp1 target support](https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip1.html)
- [Rust Wasm feature selection and rebuilding std](https://doc.rust-lang.org/rustc/platform-support/wasm32-unknown-unknown.html)
- [WASI SDK](https://github.com/WebAssembly/wasi-sdk)
- [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
- [GitHub-hosted runner platforms](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
