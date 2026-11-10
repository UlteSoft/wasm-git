# wasm-git 0.1.3 prerelease

This release adds exact Core feature validation and detailed physical operand
spans while retaining maximal all-known decoding for every compatibility view.

- `validate --config` consumes wasm-fuzzer configurations; `--features` selects
  an exact permission set and `--list-features` lists all 29 supported switches.
- Continuation, descriptor and legacy-try instructions have typed IR operands.
  Shared-everything atomic operations retain typed ordering/index fields.
- Opcode prefixes, indices, memargs, vector lanes, branch/catch/resume tables,
  recursive types, descriptors, continuation types and declarations expose
  precise original byte spans. Malformed/unknown suffixes remain lossless.
- Byte patches retain append-only enum discriminants and exact forward/reverse
  replay. Recognition does not imply complete continuation control-flow semantics.
- Ten release platforms retain the existing names: four static-musl Linux,
  two static-CRT Windows, two macOS and two WASIp1 (MVP/Core2) archives.
  Native archives contain client and server; WASIp1 contains the client.
- Hosted CI compiles and packages. Publication requires a successful local
  30-minute paired qualification status on the exact commit, using the existing
  64 GiB, zero-swap cgroup and wasm-fuzzer/.tmp for all test products.

See FEATURE_COMBINATIONS.eu_US.md for exact flags and scope. Verify downloads
against SHA256SUMS. This prerelease does not execute or certify guest programs.

Qualification on 2026-11-11 completed **1,800.54 seconds**, **40,795** cross
checks over **39,272** distinct modules and **19,620** feature configurations,
with no failure. Every check passed exact wasm-tools/wasm-git policy validation,
seed replay and four complete byte reconstructions (audit/patch, both directions).
The official interpreter accepted 27,660 originals and 378 typed projections;
12,757 experimental profiles were explicitly unsupported. uwvm accepted 26,646
CLI originals and 1,813 imported-module originals through its production API;
12,336 profiles were explicitly unsupported. Unmodified wasm-tools accepted
33,172 originals and 7,623 typed memtag projections. No OOM event occurred.
Separately, all 92,160 fine-grained Core configurations generated two valid
modules each (184,320 modules). Native and WASIp1 MVP/Core2 release smoke passed.
These finite results do not establish every possible module or proposal revision.

The wasm-git regression suite passed 1,523 non-ignored tests. The local Web GUI
passed 14 tests; its bilingual catalog covers all 84 client root commands.
