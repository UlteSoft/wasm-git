# Exact Core features and maximal byte auditing

wasm-git 0.1.4 recognizes the entire pinned wasmparser 0.263.0 Core grammar plus
its independent uwvm memtag extension. Audit and diff always decode with the
**all-known** grammar. Selecting Core 1, 2 or 3 changes compatibility evidence;
it never hides known instructions or changes their byte boundaries.

`wasm-git validate --list-features` lists 31 exact policy switches. The list
matches wasm-fuzzer 0.2, including floats, gc-types, bulk-memory-opt,
call-indirect-overlong, experimental proposals and memtag. Components are not
accepted by this Core validation command.

```sh
wasm-git validate module.wasm --config corpus/config.json --json
wasm-git validate module.wasm --features floats,gc-types,sign-extension --json
wasm-git inspect module.wasm --json
wasm-git diff before.wasm after.wasm --standard core1 --format json
```

The configuration path reads the exact `features` array written by wasm-fuzzer;
there is no implied MVP or default-proposal baseline. With no policy argument,
validation enables all 31 known Core/extension switches. Original memtag bytes
are checked directly, including globals v2 framing, minimum memory bounds,
overflow and allocation-granule overlap. Unknown policy names are errors.

Successful audit projections expose original byte spans for opcode/subopcode,
each index, flags, memarg alignment/memory-or-array/ordering/offset, lanes, v128 bytes, block types,
branch tables, catch tables and continuation handler entries. Recursive type
members expose prefixes, supertype/descriptor/describes indices, field storage
types, mutability and continuation function indices. Memory/table/global/tag
declarations expose limits, value types, flags and custom page sizes. These
fields partition proven encodings; malformed or unknown grammar still stops at
the existing opaque frontier and preserves every original byte.

Continuation and descriptor operators now have typed IR variants and operands,
including resume/throw/switch handlers and descriptor cast branches. Typed IR
revision 2 unifies memory operands; pre-release IR discriminants are not stable. Typed
decoding does not establish continuation execution semantics, complete control
flow, or equivalence between programs; unsupported control-flow analysis stays
explicit. Future proposal revisions require new readers.

The paired wasm-fuzzer campaign independently checks original-byte validation,
field-span coverage, forward/reverse audit reconstruction, serialized patch
reconstruction and seed replay. It requires at least 30 minutes in the existing
64 GiB zero-swap cgroup. Evidence lives only in wasm-fuzzer/.tmp. Hosted CI builds
the ten release targets; tag publication requires a successful
`qualification/cgroup-64gib` status for that exact commit. This status summarizes
a finite qualification run, not a proof of all possible modules.

The same timed gate runs hypothetical updates in every Core section and all
157 reviewed reserved/extension admission-site families. Every byte-valued
selector is exhaustively varied; wider fields combine individual bit patterns,
boundary cases and seeded random values. The saved baseline and candidate each
create forward/reverse patches, and both apply every patch with exact original
bytes. Reports require every catalog entry and distinct selector count.

Syntax upgrade contracts, unknown frontiers, and the pre-1.0 IR policy are specified in [Forward syntax evolution](SYNTAX_EVOLUTION.eu_US.md).
