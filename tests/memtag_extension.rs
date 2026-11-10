//! Literal uwvm FF packed-v1 witnesses, independent of a Wasm text assembler.
use wasm_git::{
    audit,
    ir::{Immediate, UnsafeSinkKindIr},
    parse,
    patch::BytePatch,
    standard::Standard,
};
use wasmparser::{BinaryReader, OperatorsReader, Validator, WasmFeatures};

const OPS: &[(u8, &str, u8, u8)] = &[
    (0x20, "MemtagStatus", 0, 1),
    (0x21, "MemtagExtract", 1, 1),
    (0x22, "MemtagInsert", 2, 1),
    (0x23, "MemtagTagbits", 0, 1),
    (0x24, "MemtagStartbit", 0, 1),
    (0x25, "MemtagCopy", 2, 1),
    (0x26, "MemtagSub", 2, 1),
    (0x27, "MemtagLoad", 1, 1),
    (0x28, "MemtagUntag", 1, 1),
    (0x29, "MemtagUntagstore", 2, 1),
    (0x2a, "MemtagUntagstorez", 2, 1),
    (0x2b, "MemtagStore", 2, 0),
    (0x2c, "MemtagStorez", 2, 0),
    (0x2d, "MemtagRandom", 1, 1),
    (0x2e, "MemtagRandomstore", 2, 1),
    (0x2f, "MemtagRandomstorez", 2, 1),
    (0x36, "MemtagHint", 3, 1),
    (0x37, "MemtagHintstore", 4, 1),
    (0x38, "MemtagHintstorez", 4, 1),
];
fn leb(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        bytes.push(byte | if value == 0 { 0 } else { 128 });
        if value == 0 {
            return bytes;
        }
    }
}
fn instruction(op: u8, memory: u32) -> Vec<u8> {
    let mut bytes = vec![0xff];
    bytes.extend(leb(0x0101_0000 + u64::from(op)));
    bytes.extend(leb(u64::from(memory)));
    bytes
}
fn section(module: &mut Vec<u8>, id: u8, bytes: &[u8]) {
    module.push(id);
    module.extend(leb(bytes.len() as u64));
    module.extend_from_slice(bytes);
}

fn custom(module: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut bytes = leb(name.len() as u64);
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(data);
    section(module, 0, &bytes);
}

#[test]
fn literal_wire_witnesses_and_standard_prefixes_remain_independent() {
    assert_eq!(instruction(0x20, 0), [0xff, 0xa0, 0x80, 0x84, 0x08, 0]);
    assert_eq!(instruction(0x2b, 0), [0xff, 0xab, 0x80, 0x84, 0x08, 0]);
    let standard = wat::parse_str("(module (memory 1 1 shared) (func i32.const 0 i32.const 0 i32.const 16 memory.fill i32.const 0 i32.atomic.load drop v128.const i32x4 0 0 0 0 drop))").unwrap();
    assert!(valid(&standard, WasmFeatures::all()));
    assert_eq!(
        audit::inspect(&standard).unwrap().standard.validation,
        "valid"
    );
}

#[test]
fn padded_memory_index_selects_the_declared_memory_type() {
    for wide in [false, true] {
        let mut bytes = b"\0asm\x01\0\0\0".to_vec();
        section(
            &mut bytes,
            1,
            &[1, 0x60, 0, 1, if wide { 0x7e } else { 0x7f }],
        );
        section(&mut bytes, 3, &[1, 0]);
        let memories = [2, 0, 1, if wide { 4 } else { 0 }, 1];
        section(&mut bytes, 5, &memories);
        let mut instruction = instruction(0x20, 1);
        *instruction.last_mut().unwrap() = 0x81;
        instruction.push(0);
        let body = [vec![0], instruction, vec![0x0b]].concat();
        section(
            &mut bytes,
            10,
            &[vec![1], leb(body.len() as u64), body].concat(),
        );
        assert!(valid(&bytes, WasmFeatures::all()));
        let parsed = parse::parse_module(&bytes).unwrap();
        assert!(matches!(
            parsed.functions[0].operators[0].immediate,
            Immediate::MemoryIndex(1)
        ));
        assert_eq!(audit::inspect(&bytes).unwrap().standard.validation, "valid");
    }
    for memory in [129, 0x0123_4567, u32::MAX] {
        let bytes = instruction(0x20, memory);
        let mut reader = OperatorsReader::new(BinaryReader::new(&bytes, 0));
        assert!(
            matches!(reader.read().unwrap(), wasmparser::Operator::MemtagStatus { mem } if mem == memory)
        );
        assert_eq!(reader.original_position(), bytes.len() as u64);
    }
}

#[test]
fn unregistered_extension_ids_never_imply_a_memory_operand_or_later_boundary() {
    for word in [
        0,
        0x20,
        0x0101_0033,
        0x0101_0039,
        0x0102_0020,
        0x0201_0020,
        0xdead_beef,
        u32::MAX,
    ] {
        let mut bytes = vec![0xff];
        bytes.extend(leb(u64::from(word)));
        // These bytes could be a future operand, not an i32.const or memoryidx.
        bytes.extend_from_slice(&[0x41, 0x7f, 0x1a]);
        let module = fixture(&bytes, 0, 0, false, false);
        assert!(!valid(&module, WasmFeatures::all()));
        let audit = audit::inspect(&module).unwrap();
        let code = audit.sections.iter().find(|s| s.id == 10).unwrap();
        let regions = &code.regions[0].children;
        assert!(regions.iter().all(|r| !r.description.contains("I32Const")));
        assert!(audit
            .standard
            .evidence
            .iter()
            .any(|e| e.feature == "unknown"));
    }
    // No legacy FC opcode, multi-field FF header or old metadata parser exists.
    for &(opcode, _, inputs, outputs) in OPS {
        assert!(!valid(
            &fixture(&[0xfc, opcode, 0], inputs, outputs, false, false),
            WasmFeatures::all()
        ));
    }
    let mut module = fixture(&instruction(0x20, 0), 0, 1, false, false);
    custom(&mut module, "memtag.globals", &[1, 1, 16, 16]);
    let audit = audit::inspect(&module).unwrap();
    assert_eq!(audit.sections.last().unwrap().status, "opaque");
    assert!(audit
        .sections
        .last()
        .unwrap()
        .regions
        .iter()
        .all(|r| r.children.is_empty()));
}

#[test]
fn extension_opcode_uleb_is_bounded_and_failures_never_resynchronize() {
    for word in [
        &[][..],
        &[0x80][..],
        &[0x80, 0x80, 0x80, 0x80, 0x10][..],
        &[0x80, 0x80, 0x80, 0x80, 0x80, 0][..],
    ] {
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(word);
        let mut reader = OperatorsReader::new(BinaryReader::new(&bytes, 37));
        assert!(reader.read().is_err());
        // The primary FF byte was read, the extension word was not committed.
        assert_eq!(reader.original_position(), 38);
        let module = fixture(&bytes, 0, 0, false, false);
        assert_ne!(
            audit::inspect(&module).unwrap().standard.validation,
            "valid"
        );
    }
}

#[test]
fn globals_v2_has_individual_u32_u64_fields_and_exact_custom_diff() {
    let mut payload = vec![2, 2, 1];
    payload.extend(leb(0x0123_4567_89ab_cdef));
    payload.extend(leb(0x1020_3040));
    payload.push(0);
    payload.extend(leb(u64::MAX));
    payload.push(0); // Empty descriptor makes no allocation claim.
    let mut old = fixture(&instruction(0x20, 0), 0, 1, true, false);
    custom(&mut old, "uwvm.memtag.linear.globals", &payload);
    let mut new = fixture(&instruction(0x20, 0), 0, 1, true, false);
    payload[3] ^= 1;
    custom(&mut new, "uwvm.memtag.linear.globals", &payload);
    let inspected = audit::inspect(&old).unwrap();
    assert_eq!(inspected.standard.validation, "valid");
    let section = inspected.sections.last().unwrap();
    assert_eq!(section.status, "decoded");
    assert_eq!(
        section.regions[3].children[1].description,
        "81985529216486895"
    );
    assert_eq!(
        section.regions[4].children[1].description,
        u64::MAX.to_string()
    );
    let report = audit::compare(&old, &new).unwrap();
    assert!(report.custom_changes[0]
        .decoded_changes
        .iter()
        .any(|c| c.path.ends_with("/global[0]/address")));
    assert_eq!(
        BytePatch::from_bytes(
            &wasm_git::patch::create(&old, &new)
                .unwrap()
                .to_bytes()
                .unwrap()
        )
        .unwrap()
        .apply(&old)
        .unwrap(),
        new
    );
    let selected = audit::section::compare(
        &old,
        &new,
        Standard::AllKnown,
        &["custom:uwvm.memtag.linear.globals".parse().unwrap()],
    )
    .unwrap();
    let json = serde_json::to_string(&selected).unwrap();
    assert!(json.contains("global[0]/address"));
}

#[test]
fn alternate_or_malformed_globals_stay_opaque_without_rejecting_custom_bytes() {
    let mut overflow = vec![2, 1, 0];
    overflow.extend(leb(u64::MAX));
    overflow.push(1);
    for payload in [
        vec![],
        vec![3, 0],
        vec![2, 1],
        vec![2, 0, 0],
        vec![2, 0xff, 0xff, 0xff, 0xff, 0x7f],
        overflow,
    ] {
        let mut bytes = b"\0asm\x01\0\0\0".to_vec();
        custom(&mut bytes, "uwvm.memtag.linear.globals", &payload);
        let inspected = audit::inspect(&bytes).unwrap();
        assert_eq!(inspected.standard.validation, "valid");
        assert_eq!(inspected.sections[0].status, "opaque");
    }
    let mut bytes = b"\0asm\x01\0\0\0".to_vec();
    custom(
        &mut bytes,
        "uwvm.memtag.linear.globals",
        &[0x82, 0, 0x81, 0, 0x80, 0, 0x90, 0, 0x80, 0],
    );
    let inspected = audit::inspect(&bytes).unwrap();
    assert_eq!(inspected.sections[0].status, "decoded");
}

#[test]
#[cfg(feature = "client")]
fn nested_memtag_operation_and_memory_rebinding_are_function_level_edits() {
    let mut a = vec![0x02, 0x40, 0x41, 0, 0x41, 16];
    a.extend(instruction(0x2b, 0));
    a.push(0x0b);
    let mut b = vec![0x02, 0x40, 0x41, 0, 0x41, 16];
    b.extend(instruction(0x2c, 1));
    b.push(0x0b);
    let old = fixture(&a, 0, 0, false, false);
    let new = fixture(&b, 0, 0, false, false);
    assert!(valid(&old, WasmFeatures::all()) && valid(&new, WasmFeatures::all()));
    let root = std::env::temp_dir().join(format!("wasm-git-memtag-cli-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("old.wasm"), &old).unwrap();
    std::fs::write(root.join("new.wasm"), &new).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args([
            "diff",
            root.join("old.wasm").to_str().unwrap(),
            root.join("new.wasm").to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["functions_changed"], 1);
    assert_eq!(report["summary"]["functions_added"], 0);
    assert_eq!(report["summary"]["functions_removed"], 0);
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("MemtagStorez") && json.contains("MemtagStore"));
    let report = audit::compare(&old, &new).unwrap();
    assert!(report
        .semantic_changes
        .iter()
        .any(|c| c.path.contains("operator[3]")));
}
fn fixture(code: &[u8], operands: u8, results: u8, memory64: bool, wrong_inputs: bool) -> Vec<u8> {
    let mut module = b"\0asm\x01\0\0\0".to_vec();
    section(&mut module, 1, &[1, 0x60, 0, 0]);
    section(&mut module, 3, &[1, 0]);
    let flags = if memory64 { 4 } else { 0 };
    section(&mut module, 5, &[2, flags, 1, flags, 1]);
    section(&mut module, 7, &[1, 1, b'f', 0, 0]);
    let mut body = vec![0];
    let value64 = memory64 != wrong_inputs;
    for _ in 0..operands {
        body.extend_from_slice(&[if value64 { 0x42 } else { 0x41 }, 0]);
    }
    body.extend_from_slice(code);
    body.extend(std::iter::repeat_n(0x1a, results as usize));
    body.push(0x0b);
    let mut payload = vec![1, body.len() as u8];
    payload.extend_from_slice(&body);
    section(&mut module, 10, &payload);
    module
}
fn valid(bytes: &[u8], features: WasmFeatures) -> bool {
    Validator::new_with_features(features)
        .validate_all(bytes)
        .is_ok()
}
#[test]
fn all_nineteen_instructions_keep_types_and_memory_indices_in_both_widths() {
    for &(opcode, name, inputs, outputs) in OPS {
        for wide in [false, true] {
            for memory in [0_u32, 1] {
                let bytes = fixture(&instruction(opcode, memory), inputs, outputs, wide, false);
                assert!(
                    valid(&bytes, WasmFeatures::all()),
                    "{name}, memory64={wide}"
                );
                assert!(!valid(
                    &bytes,
                    WasmFeatures::all() - WasmFeatures::MEMTAG_EXTENSION
                ));
                let parsed = parse::parse_module(&bytes).unwrap();
                let operation = parsed.functions[0]
                    .operators
                    .iter()
                    .find(|op| op.opcode.as_str() == name)
                    .unwrap();
                assert!(
                    matches!(operation.immediate, Immediate::MemoryIndex(index) if index == memory)
                );
                let all = audit::inspect(&bytes).unwrap();
                assert_eq!(all.standard.validation, "valid");
                assert!(all
                    .standard
                    .evidence
                    .iter()
                    .any(|e| e.feature == "memtag_extension"));
                for profile in [Standard::Core1, Standard::Core2, Standard::Core3] {
                    let core = audit::inspect_with_standard(&bytes, profile).unwrap();
                    assert_ne!(core.standard.validation, "valid");
                    assert!(core
                        .standard
                        .evidence
                        .iter()
                        .any(|e| e.feature == "memtag_extension" && e.status != "supported"));
                }
                if inputs != 0 {
                    assert!(!valid(
                        &fixture(&instruction(opcode, memory), inputs, outputs, wide, true),
                        WasmFeatures::all()
                    ));
                }
                assert!(!valid(
                    &fixture(&instruction(opcode, 2), inputs, outputs, wide, false),
                    WasmFeatures::all()
                ));
            }
        }
    }
}
#[test]
fn memory_immediate_is_u32_and_reserved_holes_stay_unknown() {
    for &(opcode, _, _, _) in OPS {
        // Nonminimal-but-valid unsigned LEB is retained by the lossless audit.
        // Five-byte padded packed word and padded memory index.
        let bytes = vec![0xff, opcode | 0x80, 0x80, 0x84, 0x88, 0, 0x81, 0];
        let mut reader = OperatorsReader::new(BinaryReader::new(&bytes, 0));
        assert!(reader.read().is_ok());
        assert_eq!(reader.original_position(), bytes.len() as u64);
        for immediate in [
            &[][..],
            &[0x80][..],
            &[0x80, 0x80, 0x80, 0x80, 0x10][..],
            &[0x80, 0x80, 0x80, 0x80, 0x80, 0][..],
        ] {
            let mut malformed = instruction(opcode, 0);
            malformed.pop();
            malformed.extend_from_slice(immediate);
            let mut reader = OperatorsReader::new(BinaryReader::new(&malformed, 0));
            assert!(reader.read().is_err());
        }
    }
    for hole in [0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x39] {
        let bytes = instruction(hole, 0);
        let mut reader = OperatorsReader::new(BinaryReader::new(&bytes, 0));
        assert!(reader.read().is_err());
    }
}
#[test]
fn tagged_modules_are_losslessly_diffed_without_claiming_core_standardization() {
    let old = fixture(&instruction(0x22, 0), 2, 1, false, false);
    let mut new = fixture(&instruction(0x22, 1), 2, 1, false, false);
    let mut custom = vec![25];
    custom.extend_from_slice(b"uwvm.memtag.linear.globals");
    custom.extend_from_slice(&[2, 1, 0, 16, 16]);
    section(&mut new, 0, &custom);
    let encoded = wasm_git::patch::create(&old, &new)
        .unwrap()
        .to_bytes()
        .unwrap();
    assert_eq!(
        BytePatch::from_bytes(&encoded)
            .unwrap()
            .apply(&old)
            .unwrap(),
        new
    );
    let audit = audit::inspect(&new).unwrap();
    assert_eq!(audit.standard.validation, "valid");
    let feature = wasm_git::features::catalog(Standard::AllKnown)
        .into_iter()
        .find(|feature| feature.id == "memtag_extension")
        .unwrap();
    assert!(feature.default_recognition && !feature.standardization_ready);
    assert_eq!(feature.selected_standard_allows, Some(true));
    let core = wasm_git::features::catalog(Standard::Core3)
        .into_iter()
        .find(|feature| feature.id == "memtag_extension")
        .unwrap();
    assert_eq!(core.selected_standard_allows, Some(false));
}

#[test]
fn result_type_comes_from_selected_memory_not_native_pointer_width() {
    for &(opcode, name, inputs, outputs) in OPS {
        if outputs == 0 {
            continue;
        }
        for wide in [false, true] {
            for correct_result in [false, true] {
                let mut bytes = b"\0asm\x01\0\0\0".to_vec();
                let result64 = wide == correct_result;
                section(
                    &mut bytes,
                    1,
                    &[1, 0x60, 0, 1, if result64 { 0x7e } else { 0x7f }],
                );
                section(&mut bytes, 3, &[1, 0]);
                // The selected second memory differs from memory zero.
                section(
                    &mut bytes,
                    5,
                    &[2, if wide { 0 } else { 4 }, 1, if wide { 4 } else { 0 }, 1],
                );
                let mut body = vec![0];
                for _ in 0..inputs {
                    body.extend_from_slice(&[if wide { 0x42 } else { 0x41 }, 0]);
                }
                body.extend_from_slice(&instruction(opcode, 1));
                body.push(0x0b);
                let mut code = vec![1, body.len() as u8];
                code.extend_from_slice(&body);
                section(&mut bytes, 10, &code);
                assert_eq!(
                    valid(&bytes, WasmFeatures::all()),
                    correct_result,
                    "{name}, memory64={wide}"
                );
            }
        }
    }
}

#[test]
fn allocation_tag_accesses_remain_visible_in_unsafe_path_analysis() {
    for &(opcode, name, inputs, outputs) in OPS {
        let expected = match opcode {
            0x27 => Some(UnsafeSinkKindIr::MemoryAccess),
            0x29 | 0x2a | 0x2b | 0x2c | 0x2e | 0x2f | 0x37 | 0x38 => {
                Some(UnsafeSinkKindIr::MemoryBulk)
            }
            _ => None,
        };
        for wide in [false, true] {
            let bytes = fixture(&instruction(opcode, 1), inputs, outputs, wide, false);
            let analysis = parse::parse_module(&bytes).unwrap().resolve().analyze();
            match expected {
                Some(kind) => {
                    assert_eq!(analysis.unsafe_paths.len(), 1, "{name}");
                    assert_eq!(analysis.unsafe_paths[0].sink.opcode, name);
                    assert_eq!(analysis.unsafe_paths[0].sink.kind, kind);
                }
                None => assert!(analysis.unsafe_paths.is_empty(), "{name}"),
            }
        }
    }
}
