# wasm-git 0.1.4 prerelease

Update maximal Core recognition to wasmparser 0.263.0, covering all 30 upstream
Core switches and the independent uwvm memtag extension. Compatibility views
mark precise feature requirements without hiding known grammar.

- validate --config accepts wasm-fuzzer 0.2.1's exact feature configurations.
- Acquire-release atomic memargs, fences and pause have typed operands and exact
  field spans. Multibyte array accesses preserve array type indices, offsets,
  alignments and vector lanes. One canonical memory operand representation covers
  these fields across scalar, SIMD, atomic and array accesses.
- Retain detailed continuation/descriptor/legacy-exception operands and physical
  spans for instructions, declarations, recursive types and metadata.
- Adapt upstream's 64-bit offsets with checked input-slice conversions. Upstream
  supplies the former local bounds fix. The memtag flag moves to bit 63 without
  changing any WasmMTE wire bytes.
- Audit and serialized patches reconstruct every original byte in both directions.
  Typed recognition does not imply continuation execution or semantic equivalence.
- Ten archives retain established names: four static-musl Linux, two static-CRT
  Windows, two macOS, and WASIp1 MVP/Core2. Native archives contain client/server;
  WASIp1 contains the client. SHA256SUMS covers all archives.
- CI compiles/packages; publication requires a successful 30-minute local paired
  qualification in the existing 64 GiB, zero-swap cgroup. The combined gate runs
  exact-feature validation, randomized syntax updates, long history replay and
  every regular and opt-in regression.
- The syntax campaign covers every Core section and all 157 reviewed reserved
  or extension admission-site families. Long replay requires 2,048 continuous
  patch edges, 16 independent forks, a nested fork, source-removal recovery and
  128-step cherry-pick/revert/rebase queues. Every revision is checked after GC,
  repacking, cloning and bundle import; both timed opt-in fuzzers run 600 seconds.

See FEATURE_COMBINATIONS.eu_US.md for flags and scope. Runtime evidence stays in
wasm-fuzzer/.tmp. Generated guests are validated and replayed, not executed.
Unsupported consumer profiles and typed memtag projections are explicit.

The grammar contract is now included in audit/feature reports. A checked syntax inventory gates future parser upgrades. Typed IR revision 2 unifies memory operands and preserves full supertype lists; pre-release IR/fingerprint compatibility is deliberately not retained. See [Forward syntax evolution](SYNTAX_EVOLUTION.eu_US.md).

Patch compatibility starts at 0.1.4: future versions must preserve the current WGTPAT01 format and exact-byte behavior. A fixed baseline fixture and an upgrade acceptance matrix cover all Core section locations, unknown nested syntax, framed recovery and source-hash rejection. Earlier pre-baseline envelopes are intentionally outside this contract.

Acceptance completed in 2,084.63 seconds with no failures or OOM events: 42,348
valid-generator inputs, 28,800 admission-site probes, 2,048 random section updates,
248,096 syntax patch applications, 12,288 long-history patch applications and
10,410 historical checkouts. All 1,554 paired regular tests and 13 opt-in tests
passed. Both extended campaigns completed 600 seconds: memtag checked 202,370
legal modules and 404,740 serialized patch replays; language/debug checked
1,984,929 cases. The official Core corpus covered 7,200 extracted modules and
5,948 fixed mutations; runtime assertions were not executed.

The patch viewer regenerates its audit with the current parser from verified
base/target bytes. Historical patches were checked unchanged across decoder
upgrades: opaque pause/orderings became typed instructions and operands, and
GC-array memory operands gained the correct layout. The matching base remains
required for complete type and index context; patch bytes are never rewritten.
