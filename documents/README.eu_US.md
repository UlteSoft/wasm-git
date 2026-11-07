# wasm-git Documentation Library

[简体中文](README.zh_CN.md) · [Project overview](../README.eu_US.md)

This library describes the current pre-release implementation. It separates
physical restoration guarantees, explanatory evidence, repository workflows,
and deployment responsibilities. Examples use the actual `wasm-git` and
`wasm-git-server` interfaces. English and Simplified Chinese editions share the
project-specific `.eu_US.md` and `.zh_CN.md` suffix convention.

## Reading guide

- [Local Web GUI](WEBGUI.eu_US.md) — bilingual client operations, full command access,
  audit previews and independent server supervision. [中文](WEBGUI.zh_CN.md)

- [WasmHub integration](WASMHUB.eu_US.md) — SSH/OpenPGP signing, durable PRs,
  reviews/CI, branch protection and merge recovery. [中文](WASMHUB.zh_CN.md)

- [Git workflow parity](GIT_PARITY.eu_US.md) — implemented contracts, remaining
  local/server gaps, and verification boundaries. [中文](GIT_PARITY.zh_CN.md)

- [Technical white paper](WHITEPAPER.eu_US.md) — problem statement, evidence model,
  matching and alignment, custom/debug metadata, storage, distribution, performance,
  threat model, and verification boundaries. [中文](WHITEPAPER.zh_CN.md)
- [User manual](USER_MANUAL.eu_US.md) — audit, section selection, source mapping,
  patches, trees, branches, conflict resolution, replay, exchange, shallow/partial
  history, signatures, GC, and troubleshooting. [中文](USER_MANUAL.zh_CN.md)
- [Server administration](SERVER_ADMINISTRATION.eu_US.md) — HTTP/SSH deployment,
  binary authorization policies, hooks, audit, trusted authors, shared forks,
  object pools, limits, backups, and recovery. [中文](SERVER_ADMINISTRATION.zh_CN.md)
- [Binary-format reference](BINARY_FORMAT.eu_US.md) — current object, manifest,
  `WGTPACK4`, patch, control-record, signature, and RPC layouts and validation
  requirements. [中文](BINARY_FORMAT.zh_CN.md)
- [Development and verification](DEVELOPMENT.eu_US.md) — project-local `.tmp/`
  output, cgroups, build combinations, independent restoration checks, compiler
  oracles, fuzz methodology, VTune, and publication hygiene. [中文](DEVELOPMENT.zh_CN.md)

Start with the user manual for local use, the administration manual for hosting,
and the white paper plus format reference for integrations. Existing `docs/`
files provide narrower implementation notes and historical development context;
this library is the maintained bilingual overview. Source-level contracts and
regression tests remain essential when implementing independent tooling.

## Interpretation rules

“Exact” refers to original bytes and evidence with explicit bounds. Function or
source correspondences can remain ambiguous. “Supported” must identify whether
it means retained storage, known framing, typed analysis, validation, or runtime
execution; wasm-git does not execute Wasm as part of an audit. Git-like workflows
use wasm-git-native formats and a documented subset of options. Pre-release formats
may change without migration.

Update both language editions in the same change. Keep command examples and
numeric limits synchronized, and do not turn a finite test result into a universal
correctness claim. Generated run reports and profiles remain in `.tmp/`.

## Release engineering

See the [platform and release guide](RELEASE.eu_US.md) for native and WASIp1 builds, CI/CD gates, QEMU tests and capability limits.
