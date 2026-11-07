# wasm-git

[简体中文](README.zh_CN.md) · [Documentation](documents/README.eu_US.md)

wasm-git is a native WebAssembly audit and version-history system. It preserves the
original binary and explains changes without converting it through WAT or
replacing it with a normalized representation. The command-line client is
`wasm-git`; the independently selectable server is `wasm-git-server`.

## What the system provides

- Exact byte evidence, with original offsets, contexts, and reversible binary
  patches that verify the source and target SHA-256 digests.
- Structured comparison of sections, functions, control blocks, instructions,
  operands, call bindings, and export dependencies, with explicit ambiguity and
  incomplete-analysis indicators.
- Custom-section comparison, bounded readers for recognized community
  conventions, and advisory DWARF, name, and linking-symbol mappings.
- Native content-addressed commits and named module trees, branches, tags,
  staging, merge and replay workflows, worktrees, and offline bundles.
- Parallel Zstandard packing, bounded binary deltas, history GC, hard-linked
  forks, cross-pack bases, shallow and partial clones, and on-demand hydration.
- HTTP and SSH transport, repository/reference authorization, atomic reference
  batches, receive hooks, author signatures, and replicated immutable object pools.
- SSH/OpenPGP commit signing, durable cross-fork PRs, review and CI gates,
  protected branches, and recoverable fast-forward/merge/squash publication.
  See the [WasmHub guide](documents/WASMHUB.eu_US.md).

The [white paper](documents/WHITEPAPER.eu_US.md) explains the evidence model and
implementation boundaries. The [user manual](documents/USER_MANUAL.eu_US.md)
covers practical workflows; the [server manual](documents/SERVER_ADMINISTRATION.eu_US.md)
covers deployment and recovery.

## Local Web GUI

The independent Python interface supports English / 简体中文, repository review
and operations, complete native command access, and separate server supervision.
Open, create and clone projects, switch among opened workspaces, and review
staged/working changes beside an inline Wasm audit. Missing native executables
are built from this checkout using Cargo.

Applications can select `--format json` or `--format binary-v1` before a command.
The GUI uses the versioned binary protocol and renders selected fields; original
reports and command streams require deliberate review/export. See the
[interface and byte-order contract](documents/COMMAND_PROTOCOL.eu_US.md).

```sh
python3 webgui/client.py start --open
python3 webgui/server.py start --root /path/to/hosting-root --open
python3 webgui/client.py stop
python3 webgui/server.py stop
```

See the [Web GUI manual](documents/WEBGUI.eu_US.md) for audit previews,
signing/PR command forms, managed services, limits and lifecycle controls.

## Build the client, server, or both

Use a recent Rust toolchain with Cargo and a native C toolchain for the bundled
Zstandard dependency. Rust 1.93.1 is the development baseline described in the
verification guide; no lower minimum version is promised. Dependencies are
locked, and the reviewed `wasmparser` source is vendored.

```sh
# Client only: the default.
cargo build --locked --release

# Server only.
cargo build --locked --release --no-default-features --features server

# Both executables from the shared library.
cargo build --locked --release --features server
```

The default client does not include the listening server module. With both
features enabled, `wasm-git server` is also available. SSH invokes the separately
installed `wasm-git-server` by default. HTTP(S) clients require `curl`; SSH clients
require an OpenSSH-compatible `ssh` executable.

## Review a change

```sh
wasm-git inspect module.wasm --json
wasm-git diff old.wasm new.wasm
wasm-git diff old.wasm new.wasm --compact --format json
wasm-git diff old.wasm new.wasm --section code --section data
wasm-git diff old.wasm new.wasm --section custom:producers
wasm-git debug-map module.wasm --symbol calculate --json
```

`--threads 0` uses the logical CPUs available to the process; an explicit value
from 1 to 256 controls the shared native worker pool. Place this global option
before the command, for example `wasm-git --threads 4 diff old.wasm new.wasm`.
`--compact` retains all byte edits but omits bulky explanatory trees and ordinary
supported-feature evidence. `--short` is a summary, not a restoration artifact.

Section selection can be repeated and is a union of top-level physical sections.
A section report is not a whole-module patch and does not recompute cross-section
call, export, or signature dependencies. Use a complete diff for those questions.

```sh
wasm-git patch old.wasm new.wasm --output change.wgp
wasm-git apply old.wasm change.wgp --check
wasm-git apply old.wasm change.wgp --output restored.wasm
```

Patch and export destinations must be new files. Application verifies the source
bytes, edit ranges, old byte evidence, and final digest before publishing output.

## Record and exchange history

```sh
wasm-git init ./history
wasm-git commit ./history old.wasm -m 'Initial module'
wasm-git commit ./history new.wasm -m 'Update calculation'
wasm-git log ./history HEAD
wasm-git diff HEAD~1 HEAD --repo ./history --format json
wasm-git checkout ./history HEAD restored.wasm
wasm-git fork ./history ./independent-fork
wasm-git gc ./history
wasm-git fsck ./history
```

For a module tree, place Wasm files under the repository directory, stage their
relative paths with `add`, and commit without a positional module argument.
A local fork shares immutable pack files on the same filesystem and owns its own
references. Deleting the source pathname does not remove the fork's hard links.
`gc` compresses and retains objects by default; deletion requires explicit
`--prune=now` and remains subject to retention roots.

## Deploy a repository service

```sh
wasm-git-server --root /srv/wasm-git --listen 127.0.0.1:9419
```

Create repositories under the service root first. HTTP is read-only by default.
A write service requires an explicit token or compiled authorization policy and
`--allow-write`. Terminate public HTTPS at a reverse proxy. SSH uses system
account authentication and can be restricted through a forced command. Consult
the administration manual before enabling writes, signed-author enforcement,
shared pools, or reference rewriting.

## Compatibility and assurance

Storage, compression, transport, and patching preserve accepted Wasm bytes,
including unknown extensions and custom payloads. Structured Core analysis uses
the pinned grammar and the `core1`, `core2`, `core3`, or `all-known` profile.
`wasm-git features --json` distinguishes retained grammar, partial projections,
community conventions, and upstream lifecycle metadata. Deprecated encodings
remain recognized; a proposal's status alone does not prove implementation.

Component Model support covers raw bytes and recognized outer section framing;
it does not provide complete component semantics. Custom payloads that do not
match a recognized convention remain opaque rather than being rejected for that
reason. Symbols and similarity scores are evidence for review, not proof of
source-code intent, authorship, or behavioral equivalence.

Repository state uses native binary formats. JSON is generated for users or
imported as explicit external configuration. wasm-git does not read Git object
stores, speak Git wire protocols, or migrate superseded pre-release formats.
See the manuals for unsupported Git options and resource limits.

## Development

Follow the [development and verification guide](documents/DEVELOPMENT.eu_US.md).
Use `.tmp/` for all generated artifacts and test output, and keep that directory
out of Git. Finite tests establish observed results for their recorded inputs;
they do not prove correctness for every possible or future Wasm module.

## Release engineering

See the [platform and release guide](documents/RELEASE.eu_US.md) for native and WASIp1 builds, CI/CD gates, QEMU tests and capability limits.
