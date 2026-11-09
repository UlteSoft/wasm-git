# uwvm2-ros VM instruction extension

[简体中文](UWVM2_ROS_EXTENSIONS.zh_CN.md) · [Documentation library](README.eu_US.md)

## Binary contract

The private primary opcode **0xFF** is named **uwvm2-ros VM instruction extension**.
Its framing is `FF ULEB32(extension_opcode) opcode_specific_operands`.
The unsigned 32-bit identifier has **no universal bit partition, implicit version,
domain, memory index, operand count, or payload length**. A future non-WasmMTE
instruction can have entirely different operands. Dispatch compares the complete
identifier against a registered grammar before reading any operand.

ULEB32 follows WebAssembly unsigned LEB: at most five bytes, seven value bits
per byte from least significant to most significant, fifth byte at most 0x0F.
Bounded nonminimal padding is accepted. This is neither a native-endian u32
memory dump nor the big-endian encoding of wasm-git's command protocol. Original
Wasm bytes are never swapped or canonicalized during audit, patching or storage.

An unknown identifier has unknown operand length. Decoding stops at that
instruction and preserves the remaining expression as opaque bytes. Apparent
end/FF/standard-opcode bytes inside that suffix cannot establish new instruction
boundaries. Independently framed later functions and sections remain auditable.
A truncated or overflowing ULEB at a proven field boundary is a known encoding
failure. Unknown IDs never license interpreting a following byte as memoryidx.
Standard FB/FC/FD/FE families keep their standard grammars. This private extension
is not an approved Core feature; core1/core2/core3 do not enable its instructions.

## WasmMTE instruction registry

The following complete IDs register 19 WasmMTE instructions. Their historical
allocation resembles version/domain/operation fields, but **that allocation is
not a global interpretation rule**. Each instruction below independently takes
one ULEB32(memoryidx). Memory64 also uses u32 for this immediate. Every stack
operand and result uses the selected memory's address type: i32 for memory32,
i64 for memory64. Inputs run deepest to topmost. The length n is an unsigned
**byte count**, q supplies a source pointer tag, and t is a tag or tag offset.

- `0x01010020` `memtag.status`: `[] -> [mode]`.
- `0x01010021` `memtag.extract`: `[p] -> [tag]`.
- `0x01010022` `memtag.insert`: `[p, t] -> [p']`.
- `0x01010023` `memtag.tagbits`: `[] -> [tag_bits]`.
- `0x01010024` `memtag.startbit`: `[] -> [start_bit]`.
- `0x01010025` `memtag.copy`: `[p, q] -> [p']`.
- `0x01010026` `memtag.sub`: `[p, q] -> [difference]`.
- `0x01010027` `memtag.load`: `[p] -> [p']`.
- `0x01010028` `memtag.untag`: `[p] -> [untagged_p]`.
- `0x01010029` `memtag.untagstore`: `[p, n] -> [untagged_p]`.
- `0x0101002A` `memtag.untagstorez`: `[p, n] -> [untagged_p]`.
- `0x0101002B` `memtag.store`: `[p, n] -> []`.
- `0x0101002C` `memtag.storez`: `[p, n] -> []`.
- `0x0101002D` `memtag.random`: `[p] -> [p']`.
- `0x0101002E` `memtag.randomstore`: `[p, n] -> [p']`.
- `0x0101002F` `memtag.randomstorez`: `[p, n] -> [p']`.
- `0x01010036` `memtag.hint`: `[p, q, t] -> [p']`.
- `0x01010037` `memtag.hintstore`: `[p, n, q, t] -> [p']`.
- `0x01010038` `memtag.hintstorez`: `[p, n, q, t] -> [p']`.

0x01010033 (memtag.add) is **reserved and rejected**, with no invented arithmetic
or stack semantics. All unregistered IDs, including reserved GC-domain IDs,
remain unknown. This linear-memory family does not implement guest GC tagging.

Independent byte witnesses:

```text
memtag.status memory 0: FF A0 80 84 08 00
memtag.store memory 0: FF AB 80 84 08 00
memtag.hintstorez memory 1: FF B8 80 84 08 01
```

In the referenced execution ABI, pointer tags occupy the high 4/8 bits of
memory32/64 addresses; allocation tags cover 16-byte granules. memtag.load reads
an allocation tag, not data. Tag stores affect every granule intersecting the
range; z variants clear exactly n data bytes. Zero-length random-store may still
consume entropy and change its returned pointer tag. Disabled lowering preserves
the ordinary bounds-checked zeroing of all four z variants. These semantics
explain audit impact; wasm-git does not execute them or certify runtime bounds,
owner mappings, entropy, hardware MTE modes or concurrency safety. Unsafe-path
analysis marks allocation-tag loads and all tag-store variants as memory sinks.

## Globals metadata convention

Only the name uwvm.memtag.linear.globals has an allocation-metadata field view:

```text
ULEB64(2) ULEB64(count)
repeat count times:
    ULEB32(memoryidx) ULEB64(address) ULEB64(byte_length)
```

ULEB64 is bounded to ten bytes with only value bit zero allowed in the tenth
byte. Legal padding is accepted. Addresses and lengths stay u64 on 32-bit hosts.
Empty lists and empty descriptors are allowed. Checked addition prevents an
overflowing address + byte_length from becoming a decoded range. Version, count,
and each memory index/address/byte length have individual fields and original
spans. Runtime canonical owners and allocation overlap are not validated here;
a section name does not authenticate its producer. Interpretation is bounded to
100,000 entries and actual payload capacity before allocation. Larger sections
remain fully stored and byte-diffable.

Custom sections are community conventions. Other versions, truncated/trailing
bytes, overflowing ranges or interpretation-budget excess produce an opaque
view with a diagnostic, **not rejection of the Wasm file for failing a custom
convention**. Former memtag.globals and old instruction formats have no
compatibility decoder or type-validation path. Generic opaque auditing preserves
their original bytes just as for any unrecognized payload.

## Audit and maintenance

Feature memtag_extension provides typed recognition in all-known and
recognized-but-disabled evidence under standard Core profiles. Function IR
retains every memory immediate. Opcode/memory rebinding changes appear at
function/block/instruction levels with original offsets and byte evidence.
Padding-only changes remain byte edits without inventing a function-body change.
Globals field updates appear in custom diffs and in `--section
custom:uwvm.memtag.linear.globals`. Binary patch replay, history, GC, bundle and
clone retain exact bytes, including opaque suffixes. `--format json` and global
`--format binary-v1` are application output views, never repository storage.

To add an extension, allocate unused complete u32 IDs; document each operand's
encoding, stack types, feature gate and failure behavior; add its own decoder
arm and typed IR. Do not change an existing ID's grammar or impose WasmMTE's
memory-index layout on another family. Incompatible revisions require new IDs;
reserved IDs require an explicit design decision. Keep bilingual specifications,
byte witnesses, malformed-input regressions, provenance and release smoke tests
synchronized. Raw custom data and byte evidence may contain private content;
interpretation never fetches embedded URLs or accesses runtime owners.

The WasmMTE mapping and globals layout were checked against the supplied
2026-11-08 design README, SHA-256
48492664d0e9fa2ffabbc600b3efeb33863f83dfd46ce7a74c6e9edab765db24,
with the subsequent user-directed generic u32 namespace clarification. The
unmodified external LLVM mt-2 producer uses a superseded encoding and needs its
separate migration patch before it can produce this format.
