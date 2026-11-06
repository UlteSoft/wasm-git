# uwgit Binary Format Reference

**Edition:** current pre-release implementation, 2026-11-06
[简体中文](BINARY_FORMAT.zh_CN.md) · [Library](README.eu_US.md)

## 1. Scope and version policy

This reference describes the formats written by this checkout. It is not a
promise that unreleased layouts remain stable. The repository format marker is
the eight-byte value `UWGRPO\0\x02`. Old JSON repositories, textual control files,
and superseded pre-release pack layouts are rejected rather than migrated.
Readers must validate the version, complete record, semantic invariants, and
referenced object graph; recognizing a magic prefix is insufficient.

Human-readable object IDs and hexadecimal edit evidence are presentation values.
Canonical object and hot-index references are raw 32-byte SHA-256 values. Necessary
UTF-8 strings, such as paths and messages, are length-prefixed data inside binary
records. They do not make the repository a text or JSON document store.

There are two encoding families. Explicit object/pack/patch framing uses fixed
big-endian integers. Cold state and RPC metadata use the canonical positional
record codec described in section 6. Do not apply one family's integer rules to
the other.

## 2. Primitive object encoding

In the explicit framing below, `u32be` and `u64be` mean unsigned big-endian
integers. `oid` means 32 raw bytes. `bytes` is a `u32be` length followed by exactly
that many bytes; `string` is the same representation with valid UTF-8. Counts
must fit both their field and the remaining input's minimum possible record size.
Overflow, truncation, trailing fields, invalid ordering, and invalid flags fail.

### 2.1 Object envelope and identity

```text
8 bytes   magic = UWGO\0\0\0\x02
1 byte    kind
u64be     payload length
N bytes   payload
```

The object ID hashes this entire envelope, including kind and length. Kind 1 is
original Wasm, kind 2 is a commit, and kind 3 is a named tree. A Wasm payload is
not rewritten by validation, normalization, compression, or signature display.
An accepted header can precede an opaque unsupported encoding; interpretation
status is separate from storage identity.

### 2.2 Commit payload

Fields appear in this order:

1. Layout byte: 1 for a single module, 2 for a named tree.
2. Target `oid`: the Wasm or tree object required by that layout.
3. Parent count `u32be`, then that many parent `oid` values in declared order.
4. Message `string`.
5. Explicit-path flag: 0 for absent, 1 followed by a path `string`.
6. Signature flag: 0 for absent, 1 followed by a `bytes` field containing the
   canonical `AuthorSignature` state record.

Layout and path combinations undergo additional semantic checks. Parent order
is significant for merge/mainline operations. Duplicate or invalid parents,
wrong target types, invalid paths, and incomplete required graphs are rejected.
The local shallow flag is not serialized into a commit and does not change its
real parents or object ID.

The signature record has version, author, timestamp, a 32-byte public key, and a
64-byte Ed25519 signature. Its signed message is the literal domain
`uwgit.author.commit.v1\0`, followed by the author's explicit `string`, timestamp
`u64be`, public key, and a length-prefixed canonical unsigned commit payload.
The signature field is absent in that unsigned form. Strict signature verification
and trusted-author binding are separate operations.

### 2.3 Named tree payload

A tree is a `u32be` entry count followed by path `string` / target `oid` pairs in
strict ascending UTF-8 path-byte order. The empty tree is a zero count. Validation
rejects duplicate/reversed paths, prohibited path forms, file/directory prefix
conflicts, missing non-promised targets, and targets of the wrong kind. Directory
objects, POSIX modes, arbitrary Git blobs, and symlink entries are not part of
this tree format.

## 3. Bundle framing and selective manifests

```text
8 bytes   magic = UWGB\0\0\0\x02
u64be     manifest byte length
N bytes   manifest
u64be     number of objects
repeated  oid, u64be envelope length, complete object envelope
```

Object entries are in strict ID order. The manifest contains a layout byte,
HEAD identity `string`, sorted branch and tag maps, then sorted shallow-boundary
and promised-object ID sets. Maps use a `u32be` count and name `string` / `oid`
pairs; sets use a `u32be` count and raw IDs. Layout values 1–4 express supported
single-module/tree/tag/detached-head arrangements and are semantically validated
against the selected graph.

Promises declare omitted Wasm content; they do not manufacture object envelopes.
Shallow boundaries retain the actual parents of the boundary commits. Sources
for obtaining missing data are local configuration, not arbitrary URLs installed
from a received manifest. Full publication validates the closure using receiver
objects where permitted. Installation orders Wasm before trees and parents before
children, then publishes the selected references.

## 4. Immutable packs

### 4.1 Outer layout

Current packs use `UWGPACK4` and are named by the SHA-256 of the complete file.
The embedded index and data are published as one immutable directory entry:

```text
8 bytes   magic = UWGPACK4
u32be     object count C
1024      256 cumulative u32be fanout values
32 * C    strictly sorted raw object IDs
72 * C    corresponding fixed-width index records
32        SHA-256 of magic, count, and complete index
remaining compressed payloads
```

The index is limited to 64 MiB; a reconstructed raw object is limited to 4 GiB.
Fanout values must be monotonic, end at C, and exactly describe the ID table.
ID order is independent of physical compressed-data order.

Each 72-byte record consists of, in order: `offset` u64be, `compressed_len` u64be,
`raw_len` u64be, `base` u32be, `payload_len` u64be, `crc32` u32be, and a 32-byte
external base ID. Offsets are relative to the start of compressed data. Entries
must cover that data exactly without overlaps, gaps, or trailing bytes. CRC32
covers the corresponding compressed entry.

### 4.2 Full objects and delta bases

`base = 0xffffffff` denotes a full object; its decoded payload is the complete
object envelope and `payload_len = raw_len`. A normal index value refers to a
base object in the same pack, whose physical position must precede the dependent
object. `base = 0xfffffffe` denotes an external object-ID base. Only external-base
records have a nonzero external-ID field.

Writers use retained shared full Wasm bases for cross-pack deltas. Readers still
validate the combined dependency graph, including missing bases, cycles, and a
maximum total depth of eight across files. A pack that references another pack
is not independently portable unless its base objects are supplied too.

### 4.3 Compression and delta instructions

Each entry is exactly one Zstandard frame. For a delta, decoding yields a bounded
instruction stream; `payload_len` is its length and is at most `raw_len + 5`.
Instructions use the explicit big-endian family:

- Tag 0: insert, followed by `u32be` byte count and the literal bytes.
- Tag 1: copy, followed by `u32be` source offset and `u32be` byte count.

Zero-length operations, unknown tags, truncated instructions, out-of-bounds copy
ranges, frame concatenation, trailing bytes, and mismatched reconstructed lengths
are rejected. Reconstruction must yield the exact envelope identified by the
entry's object ID. The index checksum and CRC do not substitute for this check.

The current writer uses Zstandard level 6, shared native workers, bounded
history/path candidates, full anchors, and bounded delta budgets. Compression
parameters are representation choices and do not enter object identity. Pruning
must retain required bases or write a new self-sufficient representation first.

## 5. Offline byte patches

```text
8 bytes   magic = UWGPAT01
32 bytes  SHA-256 of the complete original file
32 bytes  SHA-256 of the complete target file
u64be     original length
u64be     target length
u32be     edit count
repeated  old start/end u64be, new start/end u64be,
          original span bytes, target span bytes
```

Span lengths determine literal byte lengths; no hex or JSON payload is stored.
These hashes identify raw Wasm files, not repository envelopes. The in-memory
report's `format` is `uwgit-byte-patch-binary-v1`; it is not an additional string
field in the wire format.

Validation checks monotonic ranges, identical-gap accounting, original byte
matches, total lengths, and final target digest. Patch validity makes no claim
about structural decoding, semantic equivalence, or global edit minimality.
Application verifies before atomically publishing a new output file.

## 6. Canonical positional records and control files

Cold records begin with `UWGM\0\0\0\x01` and encode a schema-defined sequence of
fields with bincode 1.3.3 canonical variable integers. Values 0–250 use one byte;
251/252/253/254 introduce little-endian u16/u32/u64/u128 values respectively where
the schema permits that width. Signed values use the codec's signed mapping.
Boolean/Option tags, numeric enum discriminants, collection lengths, and field
order follow the exact Rust schema. This is not a self-describing map format.

Decoding is input-budget-limited and rejects trailing bytes. Re-encoding must
produce exactly the input, rejecting nonminimal integers and noncanonical map
ordering or duplicate-key encodings. Schema evolution must be explicitly reviewed
and versioned; a type change can affect persisted bytes even if a field name is
only an implementation detail.

The index has an outer `UWGITIDX\x06` header and its current positional record.
Configuration, index images, stash, pending/rebase state, reflog, worktrees,
transaction journals, selective-history state, pool configuration, trust records,
and upload metadata are binary. UTF-8 strings inside some cold recovery fields
remain schema values; they do not represent JSON persistence.

HEAD and references use `UWGREF\0\x01` plus a tag. Tag 1 is a raw 32-byte object
ID; tag 0 is a length-prefixed branch name and is valid only for symbolic HEAD.
Writer locks use `UWGLCK\0\x01` plus a big-endian PID. The PID does not authorize
automatic lock deletion. Worktree common-directory paths use positional records.
Private/public author key files are `UWGKEY01` plus a 32-byte seed and `UWGPUB01`
plus a 32-byte public key, respectively.

## 7. RPC and negotiated object transfer

An RPC packet is `UWGRPC02`, metadata length `u32be`, payload length `u64be`, the
positional metadata record, and raw payload. Metadata is limited to 64 KiB and
the complete packet to 64 MiB. Transfers use 1 MiB slices with a 4 GiB bundle
limit. The envelope version and metadata's logical request version are distinct.

The current outer operation discriminants follow declaration order:
`Advertise=0`, `Fetch=1`, `FetchSlice=2`, `UploadSlice=3`, `ApplyUpload=4`, `Fork=5`,
`Push=6`, `UpdateRef=7`, `RefsPage=8`, `CancelUpload=9`, `PublishBatch=10`, and
`ObjectSlice=11`. Uploaded publication uses a separate nonrecursive enum:
`Push=0`, `UpdateRef=1`, `PublishBatch=2`. It cannot recursively embed uploads.
Read the [protocol types](../src/protocol.rs) for the exact positional field
schemas; do not infer omission semantics for required Option fields.

A negotiated compressed stream begins with `UWGNET01`, its total decoded length
`u64be`, then repeated `u32be` frame lengths and Zstandard frames. Each non-final
block decodes to exactly 1 MiB. The decoded stream begins with `UWGTRN01`, the
manifest length/content, and object count. Entries are ID-sorted. Mode 0 carries
a full length-prefixed envelope; mode 1 carries a raw base ID, target envelope
length, delta length, and copy/insert instructions.

A transfer base must already be verified at the receiver or appear as a complete
mode-0 object in that transfer. In-transfer delta chains and cycles are not
accepted. After reconstruction, the receiver verifies original IDs and closure
before publication. Physical transfer compression never changes stored identity.

## 8. Service records and integration rules

Authorization policies and hook update records use the positional codec. Receive
audit records are appended with a `u32be` record length; each record is bounded
at 4 MiB. Rendering a log has a cumulative limit and refuses incomplete tails.
JSON policy input and JSON audit output are external interfaces, not runtime
storage. Similarly, an exported mergetool `manifest.json` is a review artifact.

An independent reader must enforce framing, integer overflow, remaining-input
bounds, schema/version rules, ordering, path constraints, compression limits,
delta dependencies, object hashes, signatures where present, and graph semantics.
Passing an outer checksum alone is insufficient. Prefer the shared library for
writers; copying illustrative layouts without semantic validation is unsafe.

Authoritative implementation locations are [binary primitives](../src/binary.rs),
[object codecs](../src/repository_binary.rs), [repository envelopes](../src/repository.rs),
[pack records](../src/repository_pack.rs), [patches](../src/patch.rs),
[network streams](../src/repository_network.rs), [RPC](../src/protocol.rs), and
[author signing](../src/repository_signing.rs). Regression sources exercise
truncation, corruption, limits, and original-byte restoration.
