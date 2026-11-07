# Human, application and storage interfaces

[简体中文](COMMAND_PROTOCOL.zh_CN.md) · [Documentation library](README.eu_US.md)

## Three separate contracts

A terminal summary, an application response and repository storage serve different
purposes. Human summaries prioritize navigation and review; they are not parsable
APIs. Applications select an explicit versioned transport and examine the process
exit status. Objects, indexes, packs, configuration state and network packets retain
their independent native binary encodings. A presentation format never changes an
object ID or rewrites a stored Wasm module.

The Python Web GUI calls the native client with **`--format binary-v1`**. It
validates the binary frame and response version, then projects only the fields
needed by the current view into its authenticated browser API. Browser JSON is a
transport between the local bridge and the page, not repository storage and not
a UI component. The page renders project cards, file groups, tables and audit
panels. Raw command output is available only through a separate deliberate review
or export action.

## Selecting an output format

Global options precede the command:

```sh
wasm-git --format text status /path/to/project
wasm-git --format json status /path/to/project
wasm-git --format binary-v1 inspect module.wasm --json > inspection.bin
wasm-git --format json --threads 8 diff before.wasm after.wasm
wasm-git-server --format json audit-log /srv/wasm-git/receive-audit.bin
```

- `text`: printable human summary. Nested collections and lengthy values may be
  summarized. It is not an exhaustive audit export.
- `json`: one compact UTF-8 response followed by LF, including on command failure.
- `binary-v1`: one versioned binary response, with no trailing newline.
- `native`: the command's own output, including raw batch bytes and NUL-separated
  projections. This also preserves existing command-line scripts.

Without a global option, the client uses human text when stdout is a terminal and
command-native output when stdout is redirected. Applications must select a format
explicitly; they must not infer a protocol from terminal detection. The standalone
server defaults to native output, including its readiness and stdio protocols.

Command-local options keep their original meanings: `sign-commit --format ssh`,
`archive --format tar`, `for-each-ref --format TEMPLATE` and `diff --format json`
are not global transport selectors. A global machine format automatically requests
structured diff/features/tree/ref data when there is no explicit local projection.
An explicit local text projection is returned with result kind `text`.

Object batch streams and running HTTP/stdio servers have existing stream protocols.
Global JSON/binary response formats are rejected for those operations **before**
reading batch input or starting a service. Use native mode. Administrative server
commands (`compile-policy`, `compile-protection`, `audit-log`, help) support the
new response formats. Parse failures before a format can be selected, OS-level
startup errors and invalid non-UTF-8 arguments can still report to stderr without
a machine frame; a consumer must handle an absent or invalid frame as a failure.

## Response envelope, version 1

Both machine formats carry the same logical record:

```json
{"protocol":"wasm-git.command","version":1,"command":"status","ok":true,"exit_code":0,"result":{"kind":"data","value":{}},"error":null}
```

The `value` above is abbreviated only for this example. `result.kind` is `data`
with an object/array `value`, `text` with an exact `text` string, or `empty`.
`command` is the command name; full argument arrays and environment variables
are not included in the envelope. An error is `{code:"command_failed",message:…}`;
`ok` equals `exit_code == 0`. A nonzero status may still carry a useful conflict,
partial publication or negative-query result. Preserve and inspect that result;
do not treat every nonzero exit as a failed JSON/binary decoder.

The payload retains the existing command report's schema and evidence. Envelope
version 1 is a framing contract, not a claim that every command has identical
fields. Recognize the command and any command-local schema version. Reject an
unknown envelope version or result kind. Verify the envelope exit status against
the child process return code. stderr is reserved for lower-level diagnostics;
normal command errors in machine mode belong in the response envelope.

## Binary-v1 layout and endianness

The first eight bytes are `57 47 54 43 4c 49 00 01` (`WGTCLI`, NUL, version 1).
They are followed by an **unsigned 64-bit big-endian** payload length. Exactly
that many bytes encode one tagged value. No checksum, padding, implicit alignment
or newline follows. Do not cast the frame to a host struct. The protocol uses
explicit byte operations and is independent of host endianness and pointer size.

The tag is one byte. Tags and payloads are:

- `0`: null; `1`: false; `2`: true. No following payload.
- `3`: negative signed 64-bit two's-complement integer, eight big-endian bytes.
- `4`: unsigned 64-bit integer, eight big-endian bytes. Nonnegative integers use
  this tag, including those representable by signed 64-bit values.
- `5`: finite IEEE-754 binary64, eight big-endian bytes.
- `6`: UTF-8 string, unsigned 64-bit big-endian byte length then those bytes.
- `7`: array, unsigned 64-bit big-endian element count then tagged values.
- `8`: map, unsigned 64-bit big-endian pair count, then each key as a length-prefixed
  UTF-8 string **without a tag**, followed by its tagged value. Keys are unique
  and strictly ordered by UTF-8 lexical order.

For example `0x0102030405060708` is encoded as tag `04` followed by
`01 02 03 04 05 06 07 08`. Negative `-0x0102030405060708` uses tag `03` followed by
`fe fd fc fb fa f9 f8 f8`. `-1.125` uses tag `05` followed by
`bf f2 00 00 00 00 00 00`. UTF-8 lengths count bytes, not characters. The two
characters `中文` have six UTF-8 bytes.

These rules describe the **outer command response**. Wasm opcodes, LEB128 values,
original byte ranges, object batch contents and other embedded encodings are not
byte-swapped. Audit hex strings preserve the original byte order; reconstructing
new Wasm uses the native old/new spans and bytes, independently of response framing.

`webgui/protocol.py` is the dependency-free reference decoder. It rejects bad
magic/version, inconsistent frame/field lengths, truncation, trailing data,
unknown tags, malformed UTF-8, nonfinite floats, duplicate/out-of-order map keys
and noncanonical signed nonnegative integers. The GUI limits frames to 32 MiB,
nesting to 128 and decoded nodes to 1,000,000. These are consumer resource limits;
other consumers should set bounds appropriate to their application. Large reports
should use file/section selection or an explicitly bounded native-output job.

## Confidentiality and audit fidelity

Machine responses are meant for a trusted local caller. They may contain user
repository content, source annotations or diagnostic details. Binary encoding is
neither encryption nor a sanitizer. It is incorrect to claim a lossless arbitrary
Wasm report can universally be stripped of secrets without losing evidence.

The GUI adds a separate presentation boundary: default responses omit full argv,
environment, key/signature blobs and process working directories; known credential
fields, URL user information, bearer/basic authentication values and recognizable
secret assignments are removed or masked. History and inspection views select
needed fields. Source views omit embedded source contents and build provenance.
Instruction text, byte spans and hex evidence are not silently rewritten.

Raw reports and job streams require explicit review/export. Export notices explain
that original user content may contain sensitive values. Job output is stored in
restricted binary temporary files and removed when the GUI stops. Project recents
use a private SQLite database; passwords and environment overrides are not saved
there. Command inputs remain local and are passed as arrays, without a shell.

The GUI binds to loopback with a per-session token, Host/Origin/fetch-site checks,
no CORS allowance and restrictive CSP. It is an administrative interface for the
launching user, not a safe public multi-tenant service. Projects organize operations
and prevent accidental cross-project application; they are not separate OS security
principals. Use operating-system isolation for untrusted users or native plugins.

## Verification

Rust golden-byte tests fix integer, float, UTF-8, collection and header encoding.
Independent Python fixtures reject reversed lengths and malformed frames. Native
integration checks compare JSON and binary responses for successful, failing,
help and negative-query commands; verify stdout/stderr separation; reconstruct
Wasm from exact audit edits; and exercise hidden versus deliberate raw output.
The test suite also covers project persistence and isolation, readonly previews,
server policies, receive auditing, raw object streams and lifecycle cleanup.
