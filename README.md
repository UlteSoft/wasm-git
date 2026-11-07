# wasm-git

**Lossless WebAssembly auditing and decentralized binary version history.**

wasm-git explains changes to the original Wasm bytes at section, function, block,
instruction, custom-section, and available source-symbol levels. Its native Rust
client and server share one engine. Repository objects, recovery state, packs,
patches, and transport records use binary formats; JSON is an external reporting
and configuration-import interface.

Read the project overview in [English](README.eu_US.md) or
[简体中文](README.zh_CN.md). The locale suffixes `.eu_US` and `.zh_CN` are the
project's documentation naming convention.

## Local Web GUI

The independent Python interface supports English / 简体中文, repository review
and operations, complete native command access, and separate server supervision.
Missing native executables are built from this checkout using Cargo.

```sh
python3 webgui/client.py start --open
python3 webgui/server.py start --root /path/to/hosting-root --open
python3 webgui/client.py stop
python3 webgui/server.py stop
```

See the [Web GUI manual](documents/WEBGUI.eu_US.md) for audit previews,
signing/PR command forms, managed services, limits and lifecycle controls.

## Build

```sh
cargo build --locked --release
cargo build --locked --release --no-default-features --features server
cargo build --locked --release --features server
```

These commands build the client, server, and both executables, respectively.
The executable names are `wasm-git` and `wasm-git-server`.

## Start an audit

```sh
wasm-git diff old.wasm new.wasm --compact
wasm-git diff old.wasm new.wasm --section code --format json
wasm-git patch old.wasm new.wasm --output change.wgp
wasm-git apply old.wasm change.wgp --output restored.wasm
wasm-git debug-map app.wasm --debug-file app.wasm.map --json
```

## Documentation

The [documentation library](documents/README.eu_US.md)
([中文版](documents/README.zh_CN.md)) contains the technical white paper, user
manual, server administration manual, binary-format reference, and development
and verification guide. The [WasmHub guide](documents/WASMHUB.eu_US.md) covers
SSH/OpenPGP signatures, PR review/check gates, branch protection and merge recovery.
Each document has matching English and Chinese editions.

The [debug mapping guide](documents/USER_MANUAL.eu_US.md#5-map-debug-information-to-the-original-binary)
describes language-independent DWARF, all name index spaces, source maps,
explicit sidecars and Go runtime symbols, including their distinct address bases.

The [release guide](documents/RELEASE.eu_US.md) ([中文版](documents/RELEASE.zh_CN.md))
documents the Linux, Windows, macOS and WASIp1 MVP/Core 2.0 packages, CI gates,
QEMU tests, resource limits and platform capability boundaries.

wasm-git uses its own repository and network formats. Git-style workflows do not
imply Git interoperability or support for every Git option. Known Wasm grammar
and community conventions are interpreted within explicit limits; unknown bytes
remain available for lossless audit and restoration. This is pre-release software:
old JSON repositories and superseded pre-release binary formats are not migrated.

Builds, test output, profiles, downloaded corpora, and publication checks belong
under the ignored project-local `.tmp/` directory. Regression source code and
small, intentional fixtures remain versioned.
