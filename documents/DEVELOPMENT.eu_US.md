# uwgit Development and Verification Guide

**Edition:** 2026-11-06 · [简体中文](DEVELOPMENT.zh_CN.md) · [Library](README.eu_US.md)

## 1. Source and artifact policy

Version source code, lockfiles, reviewed dependency patches, reproducible test
programs, small intentional fixtures, and durable documentation. Generated
executables, compiler output, downloaded corpora, profiles, logs, run receipts,
intermediate archives, and random-test cases belong under the project-local
`.tmp/` directory. The directory is ignored by Git. Never force-add it.

The two 41-byte Wasm files under `tests/fixtures/` are intentional regression
inputs. C/C++/Rust/Go source fixtures are inputs as well. They are not bulk
compiler output or benchmark recordings. Historical per-run reports should be
kept outside published history; a methodology document can describe how to
reproduce a check without embedding a machine-specific success transcript.

Configure paths before building or testing from the source root:

```sh
mkdir -p .tmp/target .tmp/temp .tmp/logs .tmp/cargo-home .tmp/pycache
export CARGO_TARGET_DIR="$PWD/.tmp/target"
export CARGO_HOME="$PWD/.tmp/cargo-home"
export TMPDIR="$PWD/.tmp/temp"
export PYTHONPYCACHEPREFIX="$PWD/.tmp/pycache"
```

An existing trusted Cargo cache can be used instead if it does not place generated
files in tracked source. Use a recent Rust toolchain with rustfmt and Clippy and
a native C compiler. The development baseline is Rust 1.93.1; the repository does
not currently declare a lower supported MSRV. Preserve both root and fuzz
lockfiles, and use `--locked` for reproducible dependency selection.

## 2. Resource confinement

Run compilation, tests, and profiling inside a prepared cgroup. The Linux test
baseline is `memory.max=68719476736` (64 GiB) and `memory.swap.max=0`. Confirm the
actual process membership, not only the existence of some limited container.
Check inherited limits and record `memory.events` before and after the run.
Do not count an OOM-killed or externally interrupted test as a successful finish.

For an E-core campaign, identify E-cores from host topology. On an applicable
Intel hybrid host, `/sys/devices/cpu_atom/cpus` exposes the relevant logical CPUs;
do not assume CPU numbering on another machine. Pin one independent worker to
each requested physical E-core and record its observed affinity. A process pinned
to one CPU does not test real 16-worker parallel execution merely because its CLI
uses automatic threading; perform a separate 1-versus-16 worker comparison.

The scripts for real compiler oracles check an effective cgroup memory bound of
at most 64 GiB. They require a new output directory beneath this checkout's
`.tmp/` and reject existing or escaping output paths. They do not install toolchains
or create cgroups. Configure the parent process's temporary directory as above.

## 3. Required build and regression checks

```sh
cargo fmt --all -- --check
python3 tools/test-wasm-features.py
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features
cargo test --locked --no-default-features --features client --test build_modes
cargo check --locked --no-default-features --features server --lib --bins
cargo fmt --manifest-path fuzz/Cargo.toml -- --check
cargo check --locked --manifest-path fuzz/Cargo.toml --bin raw_audit
```

These checks separate shared implementation, default/client behavior, server-only
compilation, and the independent fuzz workspace. A compile check of a fuzz target
is not a fuzz campaign or an AddressSanitizer run. Tests marked `ignored` remain
unexecuted unless their required external tools/corpora are supplied and they are
explicitly selected. Report pass, failure, ignored, and filtered counts separately.

Use focused checks after a localized change. Typical suites cover
`structured_diff_cli`, `call_dependencies_cli`, `custom_audit_cli`,
`section_diff_cli`, `debug_symbols`, `proposal_diff_cli`, `history_gc_cli`,
`pack_fork_cli`, `server_hosting_cli`, and `server_expansion`. Full regression is
appropriate before publishing changes that span these layers. Preserve process
exit codes as well as logs; a final “test result” line from one target is not the
exit status of the entire Cargo invocation.

## 4. Independent restoration properties

An audit verifier should reconstruct the target from the original input and the
reported physical edits. For each edit it must independently check ordered bounds,
the old/new evidence bytes, and equality of all unchanged gaps. Concatenating
unchanged old bytes and declared replacement bytes must produce the exact target.
Check both directions and serialized patch round trips.

A second producer should construct a whole-file replacement patch without using
the main byte-diff algorithm. For sufficiently small inputs, an independent
quadratic insertion/deletion-distance implementation can validate an
`exact_minimal=true` claim. A hash comparison alone does not validate reported
positions or identify an erroneous display explanation.

For repository workflows, record raw files before committing, then restore chosen
versions after GC, repack, fork, source removal, bundle transfer, shallow/deepened
history, partial hydration, signatures, and object-pool replica failure. Verify
all retained versions relevant to the case, not just HEAD. Include damaged packs
whose outer checksums are valid but whose delta operations are invalid.

## 5. Structured intent and real compiler checks

Controlled source edits establish an independent expected change. Test a constant
replacement, a branch-condition change, function insertion/removal/reordering,
import reordering, export rebinding, and layout or custom-metadata changes.
Check exact function indices and symbol evidence where known. Compiler optimization
may attribute an instruction to an inline caller or callee, so preserve the actual
DWARF evidence rather than rewriting it to the expected source line.

```sh
python3 tools/test-cpp-intent.py \
  --output "$PWD/.tmp/cpp-intent-run1" \
  --uwgit "$CARGO_TARGET_DIR/release/wasm-git"
python3 tools/test-debug-symbols.py \
  --output "$PWD/.tmp/debug-symbols-run1" \
  --uwgit "$CARGO_TARGET_DIR/release/wasm-git"
```

Build the release client first and supply the compiler options shown by each
script's `--help`. The C++ oracle needs Clang/LLD, a native C++ compiler, Node,
and WABT tools. The debug oracle additionally needs the Rust Wasm target and
TinyGo; it covers C/C++ DWARF variants and Rust/TinyGo metadata with and without
names. These optional tools are not dependencies of ordinary library tests.
Runtime checks are distinct from static audit checks. A runtime that cannot
execute a proposal encoding cannot certify its execution merely because static
mapping succeeds.

## 6. Random and coverage-guided testing

The existing `fuzz/fuzz_targets/raw_audit.rs` exercises arbitrary paired payloads
under a Core header, including malformed content. It checks exact evidence,
reconstruction, standard-profile physical consistency, and independent patch
production. `fuzz/generate_seeds.rs` builds seed inputs. Use a compatible nightly
compiler and cargo-fuzz only when running the actual libFuzzer/ASan campaign;
store target output, corpus, discoveries, and logs explicitly under `.tmp/`.
Legacy remote wrappers are environment-specific and must be reviewed/configured
before use; the local artifact policy remains authoritative.

A **valid-module campaign** is a separate experiment. Use a pinned generator such
as wasm-smith plus wasm-mutate, or a deterministic grammar-aware generator, and
validate both inputs before admitting a test case. Record discarded candidates
separately. Include independent modules, multi-stage mutations, controlled local
instruction edits, large payload insertion/deletion, function movement, all known
section kinds, and recognized/alternate custom conventions. Compiler-produced
modules complement synthetic generation.

For a requested 30-minute/16-core campaign, require at least 1800 seconds of
monotonic active testing per worker, snapshot supervision at 10/20/30 minutes,
and verify all 16 affinities, progress, failure counters, resource events, and
required feature/workflow counters. Record interruptions explicitly, resume from
persisted completed-case boundaries, and exclude downtime and duplicate cases.
Do not present cumulative segmented execution as uninterrupted execution.

Record generator/toolchain versions, source and binary hashes, seed derivation,
input hashes, per-worker durations, coverage counters, and failing input bytes.
Cover section-only replay, custom/debug ranges, function/block/instruction
explanations, binary storage, history transforms, compression, network exchange,
shallow/partial restoration, author signatures, and object-pool/fork retention.
A category counter is evidence that a category was exercised, not a proof of
complete path or branch coverage. Zero observed failures means all completed
samples passed; it does not establish universal correctness or universal intent
inference.

## 7. Performance analysis with the existing VTune installation

Use the already installed Intel VTune; do not install a second copy. Keep debug
line information and frame pointers in a release build, and place results under
`.tmp/`. The following assumes the official installation path exists:

```sh
RUSTFLAGS='-C debuginfo=1 -C force-frame-pointers=yes' \
  cargo build --locked --release --features server
/opt/intel/oneapi/vtune/latest/bin64/vtune -collect hotspots \
  -knob sampling-mode=hw -result-dir .tmp/vtune-hotspots \
  -- "$CARGO_TARGET_DIR/release/wasm-git" --threads 16 \
  diff .tmp/old.wasm .tmp/new.wasm --compact --format json
/opt/intel/oneapi/vtune/latest/bin64/vtune -collect uarch-exploration \
  -result-dir .tmp/vtune-uarch \
  -- "$CARGO_TARGET_DIR/release/wasm-git" --threads 16 \
  diff .tmp/old.wasm .tmp/new.wasm --compact --format json
```

Use fresh result directories and redirect command/report output into `.tmp/logs/`
where appropriate. Consult the installed VTune help for collection knobs supported
by that version. Hardware sampling requires existing PMU permissions; report
unavailable counters rather than silently calling software samples hardware data
or changing host security settings. Resolve hotspots to the exact profiled source.

Warm up, alternate baseline/candidate order, repeat identical workloads, and
report medians with input size, history structure, thread count, affinity, and
memory limits. Normalize retired instructions by completed work. On hybrid CPUs,
interpret microarchitecture percentages within the same PMU/core type. Verify
byte-identical audit/patch/history results before interpreting a speedup. Measure
full diff, compact rendering, selected-section paths, debug mapping, and GC
separately; one improved subroutine is not an end-to-end latency guarantee.

## 8. Feature lifecycle and documentation maintenance

`src/features.rs` retains implemented readers. `src/feature_upstream.rs` records
reviewed community metadata. The scheduled GitHub workflow checks upstream
changes and produces a reviewable candidate; it does not delete readers, disable
historical encodings, automatically merge updates, or implement new proposals.
Keep regression inputs when an upstream feature becomes deprecated.

```sh
python3 tools/test-wasm-features.py
python3 tools/update-wasm-features.py --check
```

The second command contacts upstream; use its explicit input/revision options for
an offline pinned review. Update the paired `.eu_US.md` and `.zh_CN.md` documents
together when behavior changes. Preserve command examples, numeric limits, field
names, and implementation boundaries across translations. The locale suffix is
an intentional project convention; do not silently rename `.eu_US` to `.en_US`.

## 9. Publication hygiene

Before committing, inspect staged paths and sizes, check links and examples, and
run the relevant checks. Keep `.tmp/`, generated binaries, logs, downloaded
archives, credentials, and profile data out of the index. Review the complete
reachable history with Git object inventory and integrity checks when preparing
a cleaned publication; the current working tree alone cannot expose deleted
historical artifacts.

A history cleanup should identify exact unwanted paths, preserve a local backup
outside tracked source, rewrite in an isolated clone, compare the final source
tree, and validate the rewritten repository. Push only intended refs using an
expected-old-tip lease. Do not use an unconditional mirror push or discard remote
work that appeared during cleanup. After publication, fetch a fresh clone and
verify its tip, tree, object integrity, and artifact exclusions. Hosting-provider
caches and old independent clones are outside a branch rewrite's erasure guarantee.
