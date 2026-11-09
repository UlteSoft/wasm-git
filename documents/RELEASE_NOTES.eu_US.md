# wasm-git 0.1.2 prerelease

This release adds typed WasmMTE auditing inside the extensible **uwvm2-ros VM
instruction extension**, `0xFF + ULEB32(opcode) + opcode-specific operands`.
The outer identifier is a complete u32; no universal version/domain bit layout
or mandatory memory operand is imposed on future instruction families.

- All 19 registered WasmMTE instructions retain typed memory indices, including
  memory32/64, imported/shared memories, and multi-byte or padded immediates.
  Reserved add, unregistered IDs and superseded encodings have no compatibility
  decoder. Unknown instruction suffixes retain exact original byte evidence.
- Opcode/memory changes are visible in function, block and instruction audits.
  `uwvm.memtag.linear.globals` version 2 exposes individual u32/u64 fields and
  section-filtered updates. Alternate custom formats remain opaque and lossless.
- Binary patches, history and compression/GC preserve original bytes, verified
  by independent forward/reverse reconstruction and per-platform release smoke.
- English/Simplified Chinese format specifications ship inside every archive.
- Includes the bilingual local Web GUI, project creation/open/clone/switching,
  separate server supervision, and explicit text/JSON/binary command responses
  with strict byte-order validation and deliberate raw-output reveal/export.

Local pre-publication validation completed a 600-second campaign on 16 E-core workers, in a 64 GiB zero-swap cgroup: 310,633 legal-module update rounds, 621,266 validated modules, 310,633 hostile mutations and 1,242,532 forward/reverse serialized patch replays, with no failure. These are finite test results, not proof of every possible input.

Publication requires the full regression/build matrix: Linux QEMU, native
Windows/macOS and WASIp1 MVP/Core2 execution. Each native archive contains both
client and server. WASIp1 archives contain the client and retain their existing
single-worker, external-process/network-server capability limits.

Verify archives against SHA256SUMS. Repository and network formats are native to
wasm-git; this is a prerelease, not a Git-protocol replacement. Auditing does not
execute Wasm or certify VM memory safety. Raw bytes and explicit exports can
contain private application content; binary encoding is not encryption.
