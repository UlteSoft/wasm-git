# uwvm2-ros VM instruction extension

The maintained binary contract, full 19-instruction WasmMTE registry, globals-v2
convention and extension procedure are in [English](../documents/UWVM2_ROS_EXTENSIONS.eu_US.md)
and [简体中文](../documents/UWVM2_ROS_EXTENSIONS.zh_CN.md).

The outer grammar is FF ULEB32(extension_opcode) opcode_specific_operands.
Superseded WasmMTE encodings have no compatibility decoder. Independent witnesses
and exact restoration checks are in tests/memtag_extension.rs and tests/memtag_fuzz.rs.
