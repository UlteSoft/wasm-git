# Local reader provenance

This is wasmparser 0.263.0 from wasm-tools revision
`7a2fa422cdf9aeeaf488a2acac8bc94a078bfa32`, extended with the uwvm packed-wire
memtag grammar, independent feature gate and typed operator validation. It is
not an unmodified upstream release. The canonical crate SHA-256 is
`4f5fa0a054ae153606df820eb0a84213e28cda41bbe04ba2628e71b6ba100fcb`.

The memtag patch originated in wasm-git at
`42e2c5e48ec657853b8d435342e8fdb617101538` and was ported to this release.
The private flag uses bit 63 to avoid upstream component-feature bits. The
previous local bounds-overflow fix is now supplied by upstream's checked
reader implementation; no validation rule is relaxed. All upstream license
files and VCS metadata remain intact.

Memtag globals v2 metadata validation belongs to the enclosing project;
this reader alone, like upstream wasmparser, treats custom payloads as opaque.

Canonical and modified file hashes are in `../wasmparser-upstream.json`.
