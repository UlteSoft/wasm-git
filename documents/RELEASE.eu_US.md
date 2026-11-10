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
These are the declared release targets, not a claim that every CPU or OS version
is supported. The Rust toolchain is pinned to 1.93.1. Linux cross compilation
uses cargo-zigbuild 0.24.1 and Zig 0.15.2. Native Windows and macOS binaries are
built on their respective GitHub-hosted operating systems. Since 0.1.3, runtime
qualification is performed locally in the existing 64 GiB zero-swap cgroup;
hosted CI compiles and packages without running tests. Tag publication requires
the exact commit's successful `qualification/cgroup-64gib` status. See
[Feature qualification](FEATURE_COMBINATIONS.eu_US.md).

Windows packages do not require a separate VC redistributable. Packaging reads
the PE import tables of both executables and rejects dynamic CRT dependencies.

The client is still the default Cargo build. Use `--no-default-features
--features server` for the standalone server or `--features server` for both.
The object, pack and network formats are wasm-git formats, not native Git formats.

## 2. WASI Preview 1 profiles

Both Wasm artifacts are command modules importing only
`wasi_snapshot_preview1`. MVP and Core 2.0 describe the instruction/validation
profile of **wasm-git itself**. They do not restrict the features in modules that
wasm-git can audit. The same lossless parser and custom-section support are present
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

For the paired release qualification, every runtime test runs in the existing 64 GiB,
zero-swap cgroup through `wasm-fuzzer/tools/run-in-cgroup.sh`. All products,
including wasm-git test executables, temporary repositories, validator builds,
logs and downloaded archives, stay under `wasm-fuzzer/.tmp`. No new resource
group or limit changes are required. Regression executables are built with
`cargo test --no-run` and executed from that scratch directory.

`tools/release.py smoke` checks precise nested-block differences, opaque custom
sections, original-byte patches, overwrite protection, history/GC, bundles and
clones. Native clients also exercise the standalone HTTP server. Set
`WASM_RELEASE_SCRATCH` to an absolute directory below wasm-fuzzer/.tmp when
invoking this script from the paired qualification runner.

After downloading a release, `wasm-fuzzer/tools/verify-releases.py` checks all
10 archive hashes and binary architectures. It executes the four Linux targets
through QEMU user-mode and the two WASIp1 targets through Wasmtime. It validates
the complete WASI binary against the declared MVP/Core2 feature profile.
Windows/macOS archives receive header and packaging checks on the Linux host;
no native Windows/macOS execution is claimed by these local checks.

## 4. CI and publication gates

Hosted CI checks formatting, warnings, source inventory and compilation of the
regression harnesses (`--no-run`). It builds the ten release targets and packages
only allowlisted files. It does not run runtime tests outside the existing local
qualification cgroup.

A `vVERSION` tag requires a successful `qualification/cgroup-64gib` commit status
for the exact commit, published only after at least 30 minutes of paired
wasm-fuzzer/wasm-git cross qualification. Every build job must succeed and the
tag must match Cargo.toml. The publish job downloads only that run's artifacts,
requires ten archives, creates SHA256SUMS and publishes a GitHub prerelease.
Only the publish job has release write permission. Before tagging, run
`tools/release.py inventory` to reject tracked intermediates and oversized
historical blobs. Qualification reports remain in ignored local scratch space.

Starting with 0.1.4, use `wasm-fuzzer/tools/qualify.py` for this gate. It runs
the exact-feature multi-validator corpus, hypothetical syntax updates in every
Core section and reserved/extension admission-site family, long history replay,
and every regular and opt-in regression test. The first three campaigns must
each run for at least 30 minutes. Full selector coverage, byte-exact replay,
2,048 continuous patch edges, 16 independent forks, nested forks, 128-step replay
queues and the saved patch baseline are mandatory. Both timed opt-in fuzzers
must complete their full 600 seconds; smoke settings cannot qualify a release.
See [Forward syntax evolution](SYNTAX_EVOLUTION.eu_US.md).

## 5. References

- [Rust WASIp1 target support](https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip1.html)
- [Rust Wasm feature selection and rebuilding std](https://doc.rust-lang.org/rustc/platform-support/wasm32-unknown-unknown.html)
- [WASI SDK](https://github.com/WebAssembly/wasi-sdk)
- [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
- [GitHub-hosted runner platforms](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
