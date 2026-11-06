# uwgit Technical White Paper

**Edition:** pre-release implementation, 2026-11-06
**Audience:** reviewers, compiler engineers, repository operators, and integrators
**Language:** English (`eu_US`) · [简体中文](WHITEPAPER.zh_CN.md)

## Abstract

uwgit combines lossless WebAssembly change auditing with a decentralized,
content-addressed repository. Its primary evidence is the original input byte
sequence. Structural interpretation explains that evidence through sections,
functions, control flow, typed operands, dependency bindings, and available
compiler metadata. The interpretation can be incomplete or ambiguous without
making the underlying bytes disappear.

The client and server share a native Rust implementation. Binary objects and
recovery records are distinct from human-readable reports. Immutable compressed
packs, bounded deltas, shared forks, selective transfer, and object-pool replicas
reduce the cost of preserving history while maintaining the identity of the
original objects. This paper specifies the design rationale and assurance
boundaries of the current implementation; it is not a claim of complete Git
compatibility, execution verification, or universal inference of developer intent.

## 1. Problem and requirements

A source change can cause a compiler to reorder Wasm functions, renumber imports,
change LEB widths, reorganize local declarations, or update debug information.
Conversely, a small binary change can redirect an unchanged numeric call index
to another import. Reviewing only text disassembly or matching functions by their
current index obscures these distinctions.

A useful audit therefore needs several independent answers:

1. Which physical bytes changed, including encoding choices and unknown content?
2. Which declarations, functions, blocks, and instructions explain those bytes?
3. Did reference bindings change even when an instruction's numeric bytes did not?
4. Which relationships are supported by evidence, and which remain ambiguous?
5. Can every accepted revision be restored without relying on the interpretation?

uwgit prioritizes accurate evidence and reproducible restoration. Compact display
is a presentation choice, not permission to discard bytes from history. Efficient
storage is achieved through binary schemas, compression, and deltas rather than
by storing a human-readable diff as the authoritative artifact.

## 2. Evidence and assurance model

### 2.1 Physical identity

Let `A` and `B` denote two accepted input byte sequences. The patching contract is
that successful creation and application reconstruct `B` exactly from `A` and a
binary patch. Application checks the expected source digest, ordered edit ranges,
old byte images, output length, and expected target digest. It publishes the
result only after those checks succeed.

This contract includes semantically redundant encodings, NaN bit patterns,
custom payloads, duplicate entries, and unknown regions. It does not require a
WAT round trip, successful function matching, or a runtime capable of executing
the module. A matching digest is an integrity check, not proof of authorship.

### 2.2 Structural interpretation

The scanner records half-open byte ranges `[start, end)` in the original file.
Known sections contain nested evidence for entries, function bodies, local
declarations, instructions, and operands. Explanations refer back to these
ranges rather than inventing offsets in regenerated code.

An edit script and a function correspondence report answer different questions.
The script supplies exact replacement evidence. Correspondences help a reviewer
recognize moves, modifications, and dependency changes, but do not establish
unique historical identity or behavioral equivalence.

### 2.3 Explicit incompleteness

Parsing, validation, feature eligibility, and matching completeness are separate
results. A reader may understand an encoding that a selected standard forbids.
A parser may identify a section while a validator rejects its type relationships.
A bounded matcher may stop before considering every plausible correspondence.
These outcomes must not be collapsed into a single “supported” label.

When an unknown opcode prevents reliable immediate decoding, the remaining
expression or function is retained as opaque. Length-delimited later functions
and sections can still be audited. Malformed framing stops boundary discovery;
the implementation does not search arbitrary bytes for a plausible next section.

## 3. Processing architecture

The implementation separates the following responsibilities:

- **Framing and physical audit:** identify known boundaries, preserve opaque
  spans, compare bytes, and expose replayable edits.
- **Typed Core analysis:** project known declarations and operators into an
  in-memory representation used for function and dependency comparison.
- **Correspondence and structured diff:** rank candidates, preserve ambiguity,
  and align functions, control structures, and instructions.
- **Metadata interpretation:** recognize bounded custom-section conventions and
  map available compiler debug information to original positions.
- **Repository engine:** store immutable objects, validate object graphs, manage
  references and workspaces, and recover interrupted publication.
- **Transfer and service layer:** authenticate requests, select reachable data,
  negotiate binary transfer, validate incoming state, and authorize publication.

These layers share one library. Cargo features select the client executable,
server executable, or both; the default build selects the client. The default
client excludes the listening server module. An audit does not execute Wasm.

## 4. Change-localization strategy

### 4.1 Byte-level alignment

The byte differ attempts a bounded Myers insertion/deletion search. A successful
exact search reports `exact_minimal=true` for that distance model. If the work or
distance budget is exhausted, it switches to bounded content-window alignment.
Candidate anchors are unique in the relevant comparison and checked byte for
byte; hashes narrow the search rather than replace equality checks.

Fallback gaps retain complete replacement bytes and report
`exact_minimal=false`. A non-minimal script can still reconstruct the target
exactly. “Minimal” does not mean minimum source-code edits, minimum compressed
patch size, or the developer's intended conceptual change.

### 4.2 Function correspondence

The matcher combines function structure, type information, normalized instruction
features, dependency evidence, and export roles. Unique matching names from a
recognized name section increase the weight of otherwise plausible candidates;
a name alone cannot override incompatible function-body evidence.

Index shifts are treated as relocation rather than automatic modification.
Import and call identities use structured fields, avoiding ambiguous concatenated
strings. `call`, `return_call`, and `ref.func` comparisons carry the resolved
target, type, and correspondence evidence. Export retargeting is reported even
when the selected implementations themselves have not changed.

Duplicate bodies, recursive dependencies, repeated symbols, and equally ranked
candidates remain explicit ambiguity. A bounded similarity search reports whether
it completed. The current same-type candidate-pair limit is 16,777,216; exhausting
it must not convert unresolved endpoints into confidently added or removed
functions. Physical byte evidence remains available.

### 4.3 Control structures and instructions

Structured comparison distinguishes control blocks and instruction sequences,
including nested and sibling block movement. Branch targets are interpreted in
their control context, so inserting a block can be distinguished from redirecting
a branch where the evidence supports that conclusion. Operands, catch targets,
`br_table` defaults, function references, and typed immediates participate in the
explanation. Unknown or unsupported projections do not acquire fabricated
control-flow meanings.

A reviewer should use the smallest trustworthy scope: an operand replacement
when alignment is complete, a block or function change when finer alignment is
uncertain, and a raw span when structure cannot be decoded. The implementation
reports its evidence and limits; it does not recover arbitrary source statements
from optimized machine-independent code.

### 4.4 Section selection

A selected-section report groups top-level physical sections by kind and, for
custom sections, exact name. Identical payload candidates are verified byte for
byte. Duplicate groups retain ambiguity. Position changes are separate from
content changes, so growth of an earlier section does not falsely modify all
later payloads.

Selection intentionally narrows the question. It omits the module header and does
not recompute cross-section bindings or signature inputs. Corresponding sections
may move non-monotonically, so their edits cannot be concatenated into a valid
whole-file patch. Use the complete patch API for whole-module restoration.

## 5. WebAssembly evolution and custom data

Core 1, 2, and 3 use the Core binary header version 1; the header is not a language
feature-version indicator. The selected standard controls eligibility reporting,
while known immediate lengths continue to be decoded. The pinned parser supports
known proposal encodings; the feature inventory distinguishes typed coverage,
partial Core projections, custom conventions, host/text features, and framing-only
support. Threads support here means binary audit and validation, not execution or
proof that a concurrent program is race-free.

Retained decoder features are append-only. A community status refresh can mark an
encoding deprecated, but must not remove its reader or reinterpret historical
bytes as a different wire revision. The scheduled workflow proposes reviewable
metadata updates from a recorded upstream revision; it does not automatically
implement new grammar. Component Model handling preserves raw bytes and known
outer framing without claiming complete component semantic validation.

The Core specification defines the custom-section envelope; payload conventions
are independently established by tool communities. Recognizing a name is not
sufficient evidence that its payload follows the expected convention. uwgit
provides bounded readers for names, producers, target features, linking metadata,
branch hints, source references, and supported signature containers. Unknown,
alternate, or malformed convention payloads remain exact raw data. A convention
parse failure is not a reason to reject an otherwise acceptable module from
lossless storage.

DWARF and linker metadata provide advisory source locations, inline call sites,
symbols, and supported variable-location descriptions. Offsets remain tied to the
module SHA-256 and original code layout. Missing, conflicting, duplicate, or
unsupported debug information is reported rather than guessed. External debug
URLs are not fetched, and location expressions are not executed.

## 6. Repository and storage design

### 6.1 Logical objects

The repository stores Wasm objects, commits, and flat named trees. A Wasm object's
payload is the original binary. A commit identifies either one module or a tree,
ordered parents, a message, an optional explicit module path, and optional author
signature metadata. Trees map canonical relative paths to Wasm object IDs.
Directories are implicit; this is not Git's mode, symlink, and arbitrary-file model.

Object IDs are SHA-256 over the canonical typed binary envelope. Compression,
delta-base choices, and pack ordering do not change those IDs. Reports display
hexadecimal IDs; hot object indexes use raw 32-byte digests. Mutable control and
recovery records use versioned binary schemas rather than JSON or text refs.

### 6.2 Packed history

Current packs use `UWGPACK4`, a sorted object-ID index, a 256-entry fanout, fixed
records, per-entry CRC32, index SHA-256, and independently compressed Zstandard
frames. Full objects and block-copy/insert deltas reconstruct the same original
envelopes. The writer evaluates bounded history/path candidates and actual
compressed sizes; it does not promise a globally optimal pack.

Candidate groups retain full anchors and bounded delta chains. Cross-pack bases
can refer to retained shared objects, avoiding a private full copy for the first
fork-specific revision. Readers validate missing bases, cycles, combined chain
depth, frame bounds, reconstructed sizes, and object hashes. The maximum total
delta depth is eight, including external-pack dependencies.

GC compresses by default and preserves objects. Explicit pruning computes the
retained closure, including applicable reference and recovery roots, before
repacking. Publication installs and synchronizes a complete new representation
before old storage is removed. Failures may leave redundant immutable objects;
redundancy is preferable to publishing references to incomplete history.

### 6.3 Shared forks and replicas

A same-filesystem local fork hard-links immutable base packs and owns independent
references and control state. GC retains eligible shared packs and encodes private
additions separately. Removing the source directory does not invalidate the
fork's links. This is filesystem-level sharing of data, not a second source-code
implementation or a dependency on a mutable source pathname.

Object pools replicate immutable history to configured repository nodes. Each
owner pins its required commits using persistent owner-specific references.
Eviction is permitted only after every configured node confirms the pins. Reads
try configured sources and verify the original object hash. A fork acquires
independent pins; releasing its source owner's pins does not authorize deleting
the fork's retained history. Pools do not implement multi-master reference
consensus or an independent tenant-security boundary inside one pool repository.

## 7. Distribution and publication

HTTP and SSH exchange uwgit-native binary frames and selected object graphs.
Negotiation omits verified existing closure, uses bounded deltas, and compresses
transfer blocks. Shallow history retains original parent IDs and explicit local
boundaries. Partial clones retain commit/tree structure and explicit promises for
missing Wasm objects; a promise is never a verified empty blob.

Hydration validates the requested object's envelope, type, length, and hash before
installation. Bundle contents cannot introduce arbitrary fetch addresses: sources
are explicitly configured locally. Remote object reads are restricted to the
repository's exposed reachable data. A shallow push may be completed with verified
objects already held by the receiver, but publication requires a complete graph.

Receive processing separates authentication, repository/reference authorization,
object validation, optional author trust policy, quarantine hooks, and reference
publication. Atomic batches use the shared durable reference-transaction machinery.
An identity-bearing binary receive audit records outcomes. Network interruption
can make the client's observation uncertain even when the server has committed;
clients should query current refs before retrying a potentially completed update.

## 8. Performance model

One Rayon pool serves parallel scanning, tree comparisons, function features,
candidate scoring, packing, and decoding. The default uses available logical
CPUs; an explicit global worker limit avoids consuming a whole shared host.
Zstandard does not start a nested worker pool. Deterministic ordering and
thread-count comparisons are part of verification.

Compact reporting removes omitted fields before expensive JSON construction.
Custom-only section views avoid unrelated function parsing. Feature computation
is reused across candidates, delta searches are bounded, and debug lookups use
range indexes. These choices target avoidable allocation and repeated work rather
than changing the evidence contract.

Full snapshots, graph verification, and some transfer construction still load
substantial object sets into memory. A 4 GiB bundle limit is a format limit, not a
promise that an operation fits in 4 GiB of RAM. Compression ratios and latency
must be measured on representative history under recorded CPU and memory limits.
No fixed speedup or equivalence to Git latency is asserted by this document.

## 9. Threat model and operational limits

Uploaded Wasm is data and is not executed by the service. Native decoders still
process untrusted inputs; length checks, allocation budgets, graph checks, and
bounded search reduce exposure but do not establish a formal security proof.
Server hooks are administrator-selected executable programs and require their own
operational isolation. The HTTP listener relies on a reverse proxy for public TLS.

Content digests, transport identities, commit signatures, and custom-section
signature containers have separate meanings. Native Ed25519, SSHSIG and OpenPGP
commit verification check the signed commit; a trust table additionally binds a
key to an author identity. External formats use explicit verification or signed
receive admission; ordinary object reads do not execute external tools. The
[WasmHub guide](WASMHUB.eu_US.md) defines PR review and publication gates.
A custom signature-container view may report changed signed inputs
without performing signature verification. Neither mechanism proves program safety.

Administrators who can replace repository files can delete data or alter local
trust policy. Backups must include required object data, references, promises,
pool ownership, and the separately managed credentials and policy configuration.
The service is a repository backend, not a complete hosting website: user lifecycle,
review UI, abuse controls, distributed reference consensus, and full Git option
parity remain outside the implementation.

## 10. Verification strategy

Tests should verify independent properties rather than merely deserialize output
produced by the same path. Required comparisons include replaying audit edits
against original bytes, validating both patch directions, checking known function
and source-location changes, and restoring every selected historical revision
before and after repacking, forking, and transfer.

Random valid modules complement compiler-produced fixtures. A campaign should
record generator versions, seeds, input hashes, accepted-input validation,
feature/section coverage, affinity, cgroup limits, monotonic worker durations,
failures, and interrupted runs. Known compiler edits supply an independent intent
oracle; unconstrained random edits do not provide an oracle for source intention.
All generated evidence belongs under `.tmp/`, outside versioned source.

A finite campaign with zero observed restoration failures establishes equality
for its completed samples. It does not prove universal correctness, complete
branch coverage, or support for all future Wasm semantics. See the
[verification guide](DEVELOPMENT.eu_US.md) for reproducible checks and profiling.

## 11. Implementation map and references

The primary implementation references are [audit](../src/audit.rs),
[function matching](../src/matcher.rs), [structured diff](../src/structured_diff.rs),
[custom interpretation](../src/audit_custom.rs), [debug mapping](../src/debug_info.rs),
[feature retention](../src/features.rs), [repository objects](../src/repository.rs),
[packing](../src/repository_pack.rs), [partial history](../src/repository_partial.rs),
[signing](../src/repository_signing.rs), and [receive processing](../src/server_receive.rs).
The [binary-format reference](BINARY_FORMAT.eu_US.md) documents current framing.

The [WebAssembly Core binary specification](https://webassembly.github.io/spec/core/binary/modules.html)
defines the module and custom-section envelopes. Compiler debug mappings follow
recognized [WebAssembly DWARF conventions](https://github.com/WebAssembly/tool-conventions/blob/main/Dwarf.md)
where the payload supplies sufficient evidence. Git's
[pack-format documentation](https://git-scm.com/docs/gitformat-pack) and
[partial-clone design](https://git-scm.com/docs/partial-clone) inform the storage
and missing-object workflows; uwgit defines its own binary formats and contracts.
