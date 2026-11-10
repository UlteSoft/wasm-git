"""Reviewed Core/extension wire admission sites for hypothetical syntax upgrades.

Numeric values are never interpreted as future semantics. The requirement is
bounded inspection and exact preservation through the established patch format.
See documents/SYNTAX_EVOLUTION.eu_US.md for the normative source mapping.
"""
from dataclasses import dataclass

HEADER = b"\0asm\x01\0\0\0"


def leb(n):
    out = bytearray()
    while n >= 128:
        out.append((n & 127) | 128)
        n >>= 7
    return bytes(out + bytes([n]))


def signed(n):
    out = bytearray()
    while True:
        byte = n & 127
        n >>= 7
        done = (n == 0 and byte & 64 == 0) or (n == -1 and byte & 64 != 0)
        out.append(byte if done else byte | 128)
        if done:
            return bytes(out)


def section(i, data):
    return bytes([i]) + leb(len(data)) + data


def name(text):
    data = text.encode()
    return leb(len(data)) + data


TAIL = section(0, name("syntax.after") + b"\x41\0\x0b")
TYPE = section(1, b"\x01\x60\0\0")


def module(i, payload):
    return HEADER + (TYPE if i != 1 else b"") + section(i, payload) + TAIL


def instruction(data):
    body = b"\0\0" + data + b"\x0b"
    code = b"\x02" + leb(len(body)) + body + b"\x03\0\x01\x0b"
    return HEADER + TYPE + section(3, b"\x02\0\0") + section(5, b"\x01\0\x01") + section(10, code) + TAIL


@dataclass(frozen=True)
class Port:
    name: str
    encoding: str
    build: object
    baseline: int = 0
    complete_byte_space: bool = False

    def values(self, rng, quick=False):
        if self.encoding == "u8":
            # Byte selectors are cheap enough to enumerate completely, even
            # when most values are currently reserved.
            values = set(range(256))
            count, bits = 256, 8
        else:
            bits = 64 if self.encoding == "u64" else 33 if self.encoding == "s33" else 32
            maximum = (1 << bits) - 1
            values = {0, 1, maximum, 127, 128, 255, 256, 16383, 16384}
            values.update(1 << bit for bit in range(bits))
            values.update(maximum ^ (1 << bit) for bit in range(bits))
            count = 192 if bits == 64 else 128
        while len(values) < count:
            values.add(rng.getrandbits(bits))
        values = sorted(values)
        rng.shuffle(values)
        if self.encoding == "s33":
            values = [v - (1 << 33) if v >= 1 << 32 else v for v in values]
        return values[:2] if quick else values

    def encode(self, value):
        if self.encoding == "u8":
            return bytes([value])
        if self.encoding == "version32":
            return value.to_bytes(4, "little")
        return signed(value) if self.encoding == "s33" else leb(value)


def inventory():
    ports = []

    def add(label, encoding, make, baseline=0, complete=False):
        ports.append(Port(label, encoding, make, baseline, complete))

    def decl(label, i, before, after, encoding="u8", baseline=0, complete=False):
        add(label, encoding, lambda v, tail, i=i, before=before, after=after: module(i, before + v + after + tail), baseline, complete)

    def op(label, before, after=b"", encoding="u8", baseline=0, complete=False):
        add(label, encoding, lambda v, tail, before=before, after=after: instruction(before + v + after + tail), baseline, complete)

    # Module envelope, all section IDs, every section's size and entry/scalar
    # count. These are independent from the selected decoder's known IDs.
    add("module.version", "version32", lambda v, tail: b"\0asm" + v + TAIL, 1)
    add("module.section_id", "u8", lambda v, tail: HEADER + section(v[0], tail) + TAIL, complete=True)
    for i in range(14):
        add(f"section.{i}.size", "u32", lambda v, tail, i=i: HEADER + bytes([i]) + v + tail + TAIL)
        if i != 0:
            add(f"section.{i}.count_or_index", "u32", lambda v, tail, i=i: module(i, v + tail))
    decl("custom.name_length", 0, b"", b"name", "u32")
    decl("custom.name_bytes", 0, b"\x01", b"", complete=True)
    add("custom.namespace", "u32", lambda v, tail: module(0, name("future." + v.hex()) + tail))
    for convention in ["name", "linking", "dylink.0", "metadata.code.branch_hint", "producers", "target_features", "sourceMappingURL", ".debug_info"]:
        prefix = name(convention) + (b"\x02" if convention == "linking" else b"")
        add(f"custom.{convention}.selector", "u8", lambda v, tail, prefix=prefix: module(0, prefix + v + leb(len(tail)) + tail))
        add(f"custom.{convention}.payload", "u32", lambda v, tail, prefix=prefix: module(0, prefix + v + tail))

    # Every type constructor position, recursive/subtype extension prefixes,
    # packed storage and mutability, nullable/shared/exact heap constructors.
    decl("type.group_constructor", 1, b"\x01", b"\0\0", baseline=0x60, complete=True)
    decl("type.recursive_member_constructor", 1, b"\x01\x4e\x01", b"\0\0", baseline=0x60)
    decl("type.subtype_constructor", 1, b"\x01\x50\0", b"\0\0", baseline=0x60)
    decl("type.shared_composite_constructor", 1, b"\x01\x65", b"\0\0", baseline=0x60)
    decl("type.supertype_count", 1, b"\x01\x50", b"\0\x60\0\0", "u32")
    decl("type.parameter_constructor", 1, b"\x01\x60\x01", b"\0", baseline=0x7f, complete=True)
    decl("type.result_constructor", 1, b"\x01\x60\0\x01", b"", baseline=0x7f)
    decl("type.reference_heap", 1, b"\x01\x60\x01\x63", b"\0", "s33", -16)
    decl("type.shared_heap", 1, b"\x01\x60\x01\x63\x65", b"\0", baseline=0x70)
    decl("type.nonnullable_heap", 1, b"\x01\x60\x01\x64", b"\0", "s33", -16)
    decl("type.exact_heap_index", 1, b"\x01\x60\x01\x63\x62", b"\0", "u32")
    decl("type.describes_constructor", 1, b"\x01\x4c\0", b"\0\0", baseline=0x60)
    decl("type.descriptor_constructor", 1, b"\x01\x4d\0", b"\0\0", baseline=0x60)
    decl("type.continuation_index", 1, b"\x01\x5d", b"", "u32")
    decl("type.struct_storage", 1, b"\x01\x5f\x01", b"\0", baseline=0x7f)
    decl("type.array_storage", 1, b"\x01\x5e", b"\0", baseline=0x7f)
    decl("type.struct_mutability", 1, b"\x01\x5f\x01\x7f", b"")
    decl("type.array_mutability", 1, b"\x01\x5e\x7f", b"")
    for label, before in [("ordinary", b"\x01\x01m\x01x"), ("compact_discriminator", b"\x01\x01m\0"),
                          ("compact1_kind", b"\x01\x01m\0\x7f\x01\x01x"), ("compact2_kind", b"\x01\x01m\0\x7e")]:
        decl(f"import.{label}", 2, before, b"\0\0\0\0")
    decl("export.kind", 7, b"\x01\x01x", b"\0", complete=True)
    decl("table.initializer_reserved", 4, b"\x01\x40", b"\x70\0\x01\xd0\x70\x0b")
    decl("table.reference_constructor", 4, b"\x01", b"\0\x01", baseline=0x70)
    decl("table.limits_flags", 4, b"\x01\x70", b"\x01\x02\0")
    decl("memory.limits_flags", 5, b"\x01", b"\x01\x02\0")
    decl("global.value_constructor", 6, b"\x01", b"\0\x41\0\x0b", baseline=0x7f)
    decl("global.mutability_shared_flags", 6, b"\x01\x7f", b"\x41\0\x0b")
    decl("tag.attribute", 13, b"\x01", b"\0")
    for label, kind, before, after in [
        ("table_flags", 1, b"\x70", b"\x01\x02\0"),
        ("memory_flags", 2, b"", b"\x01\x02\0"),
        ("global_flags", 3, b"\x7f", b""),
        ("tag_attribute", 4, b"", b"\0"),
    ]:
        decl(f"import.{label}", 2, b"\x01\x01m\x01x" + bytes([kind]) + before, after)
    decl("element.segment_flags", 9, b"\x01", b"\0\x41\0\x0b\0\0", "u32")
    decl("element.external_kind", 9, b"\x01\x01", b"\0")
    decl("element.reference_constructor", 9, b"\x01\x05", b"\0", baseline=0x70)
    decl("data.segment_flags", 11, b"\x01", b"\0\x41\0\x0b\0", "u32")

    # Root and prefixed opcode namespaces, including the registered private
    # packed opcode's extension/vendor/operation byte ranges.
    op("instruction.primary_opcode", b"", complete=True, baseline=1)
    for prefix in [0xfb, 0xfc, 0xfd, 0xfe, 0xff]:
        op(f"instruction.prefix_{prefix:02x}_subopcode", bytes([prefix]), b"\0\0\0\0", "u32")
    for shift, label in [(24, "vendor"), (16, "extension"), (8, "group"), (0, "operation")]:
        add(f"memtag.{label}", "u8", lambda v, tail, shift=shift: instruction(b"\xff" + leb((0x01010020 & ~(255 << shift)) | (v[0] << shift)) + b"\0" + tail))
    op("instruction.block_type", b"\x02", b"\x0b", "s33", -64)
    op("instruction.reference_heap", b"\xd0", b"\x1a", "s33", -16)
    op("instruction.select_type", b"\x1c\x01", baseline=0x7f)
    for subopcode in [0x18, 0x19, 0x25, 0x26]:
        op(f"instruction.cast_{subopcode:02x}_flags", b"\xfb" + leb(subopcode), b"\0\x6e\x6e")
    op("instruction.catch_kind", b"\x1f\x40\x01", b"\0\0\x0b")
    for opcode in [0xe3, 0xe4, 0xe5]:
        op(f"instruction.resume_{opcode:02x}_handler", bytes([opcode]) + (b"\0\0\x01" if opcode == 0xe4 else b"\0\x01"), b"\0\0")
    for label, code in [("scalar", b"\x28"), ("simd", b"\xfd\0"), ("simd_lane", b"\xfd\x54"),
                        ("atomic", b"\xfe\x10"), ("atomic_rmw", b"\xfe\x1e"), ("atomic_wait", b"\xfe\x01")]:
        op(f"memarg.{label}.flags", code, b"\0\0\0", "u32")
        op(f"memarg.{label}.memory_index", code + b"\x40", b"\0\0", "u32")
        op(f"memarg.{label}.array_index", code + b"\x20", b"\0\0", "u32")
        op(f"memarg.{label}.offset", code + b"\0", b"\0", "u64")
    for label, code, suffix in [("fence", b"\xfe\x03", b""), ("load", b"\xfe\x10\x10", b"\0"),
                                ("rmw", b"\xfe\x1e\x10", b"\0"), ("global", b"\xfe\x4f", b"\0"),
                                ("table", b"\xfe\x58", b"\0"), ("struct", b"\xfe\x5c", b"\0\0"),
                                ("array", b"\xfe\x67", b"\0")]:
        op(f"ordering.{label}", code, suffix)
    for label, code in [("memory_size", b"\x3f"), ("memory_grow", b"\x40"),
                        ("call_indirect", b"\x11\0"), ("return_call_indirect", b"\x13\0"),
                        ("memory_init", b"\xfc\x08\0"), ("memory_copy", b"\xfc\x0a"),
                        ("memory_fill", b"\xfc\x0b"), ("memory_discard", b"\xfc\x12")]:
        op(f"formerly_reserved_index.{label}", code, b"\0", "u32")
    op("formerly_reserved_index.memory_copy_source", b"\xfc\x0a\0", encoding="u32")
    op("instruction.select_type_count", b"\x1c", b"\x7f", "u32", 1)
    op("instruction.branch_table_count", b"\x0e", b"\0", "u32")
    op("instruction.catch_count", b"\x1f\x40", b"\x0b", "u32")
    op("instruction.handler_count", b"\xe3\0", encoding="u32")
    op("simd.lane", b"\xfd\x15")
    op("simd.shuffle_lane", b"\xfd\x0d", b"\0" * 15)
    add("code.body_size", "u32", lambda v, tail: HEADER + TYPE + section(3, b"\x01\0") + section(10, b"\x01" + v + b"\0\x0b" + tail) + TAIL)
    add("code.local_type", "u8", lambda v, tail: HEADER + TYPE + section(3, b"\x01\0") + section(10, b"\x01" + leb(4 + len(tail)) + b"\x01\x01" + v + tail + b"\x0b") + TAIL, 0x7f)
    add("code.local_group_count", "u32", lambda v, tail: HEADER + TYPE + section(3, b"\x01\0") + section(10, b"\x01" + leb(len(v) + 3 + len(tail)) + v + b"\x01\x7f" + tail + b"\x0b") + TAIL, 1)
    add("code.local_repeat_count", "u32", lambda v, tail: HEADER + TYPE + section(3, b"\x01\0") + section(10, b"\x01" + leb(len(v) + 3 + len(tail)) + b"\x01" + v + b"\x7f" + tail + b"\x0b") + TAIL, 1)
    assert len({p.name for p in ports}) == len(ports)
    return ports


def probes(rng, quick=False):
    ports = inventory()
    pending = [(port, port.values(rng, quick=quick)) for port in ports]
    # Round-robin admission sites: every port is exercised before any receives
    # its second selector, and every required selector is eventually visited.
    for ordinal in range(max(len(values) for _, values in pending)):
        for port, values in pending:
            if ordinal >= len(values):
                continue
            value = values[ordinal]
            tail = rng.randbytes(rng.randrange(0, 17))
            before = port.build(port.encode(port.baseline), tail)
            after = port.build(port.encode(value), tail)
            yield port, value, before, after
