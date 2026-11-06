use wasm_git::{audit, debug_info, parse, patch};

fn uleb(mut value: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let b = (value & 127) as u8;
        value >>= 7;
        bytes.push(b | if value != 0 { 128 } else { 0 });
        if value == 0 {
            return bytes;
        }
    }
}
fn custom(bytes: &mut Vec<u8>, name: &str, data: &[u8]) {
    let payload = [uleb(name.len()), name.as_bytes().to_vec(), data.to_vec()].concat();
    bytes.push(0);
    bytes.extend(uleb(payload.len()));
    bytes.extend(payload);
}
fn body(module: &[u8]) -> (usize, usize, usize, Vec<usize>) {
    let mut base = 0;
    for p in wasmparser::Parser::new(0).parse_all(module) {
        match p.unwrap() {
            wasmparser::Payload::CodeSectionStart { range, .. } => base = range.start,
            wasmparser::Payload::CodeSectionEntry(body) => {
                let mut r = body.get_operators_reader().unwrap();
                let mut offsets = Vec::new();
                while !r.eof() {
                    offsets.push(r.original_position());
                    r.read().unwrap();
                }
                return (base, body.range().start, body.range().end, offsets);
            }
            _ => {}
        }
    }
    panic!("no body")
}
fn dwarf(mut wasm: Vec<u8>, invalid_start: bool, unterminated: bool, cycle: bool) -> Vec<u8> {
    let (base, start, end, ops) = body(&wasm);
    // Independently encoded DWARF4, with an abstract origin and two source rows.
    let abbrev = vec![
        1, 0x11, 1, 0x10, 0x17, 0, 0, // CU, stmt_list/sec_offset
        2, 0x2e, 1, 3, 8, 0x11, 1, 0x12, 6, 0x3a, 0x0b, 0x3b, 0x0b, 0, 0, 3, 0x1d, 0, 0x31, 0x13,
        0x11, 1, 0x12, 6, 0x58, 0x0b, 0x59, 0x0b, 0, 0, 4, 0x05, 0, 3, 8, 2, 0x18, 0, 0, 5, 0x34,
        0, 3, 8, 2, 0x17, 0, 0, 0,
    ];
    let mut info = vec![4, 0, 0, 0, 0, 0, 4, 1, 0, 0, 0, 0];
    let symbol_offset = (info.len() + 4) as u32;
    info.extend([2]);
    info.extend(b"calculate\0");
    let start_pc = if invalid_start { ops[0] + 1 } else { start } - base;
    info.extend((start_pc as u32).to_le_bytes());
    info.extend(((end - base - start_pc) as u32).to_le_bytes());
    info.extend([1, 10]);
    let inline_offset = (info.len() + 4) as u32;
    info.push(3);
    info.extend(if cycle { inline_offset } else { symbol_offset }.to_le_bytes());
    info.extend(((ops[1] - base) as u32).to_le_bytes());
    info.extend(((ops[2] - ops[1]) as u32).to_le_bytes());
    info.extend([1, 12, 4]);
    info.extend(b"x\0\x04\xed\0\0\x9f");
    info.push(5);
    info.extend(b"ranged\0\0\0\0\0");
    info.extend([0, 0]);
    let info = [(info.len() as u32).to_le_bytes().to_vec(), info].concat();
    let mut header = vec![1, 1, 1, 0xfb, 14, 13, 0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];
    header.extend(b"/source\0\0sample.c\0\x01\0\0\0");
    let mut line = vec![4, 0];
    line.extend((header.len() as u32).to_le_bytes());
    line.extend(header);
    line.extend([0, 5, 2]);
    line.extend(((start - base) as u32).to_le_bytes());
    line.extend([3, 9, 1, 2]);
    line.extend(uleb(ops[1] - start));
    line.extend([3, 1, 1, 2]);
    line.extend(uleb(end - ops[1]));
    if !unterminated {
        line.extend([0, 1, 1]);
    }
    let line = [(line.len() as u32).to_le_bytes().to_vec(), line].concat();
    custom(&mut wasm, ".debug_abbrev", &abbrev);
    custom(&mut wasm, ".debug_info", &info);
    custom(&mut wasm, ".debug_line", &line);
    let mut locations = ((ops[1] - base) as u32).to_le_bytes().to_vec();
    locations.extend(((ops[2] - base) as u32).to_le_bytes());
    locations.extend([3, 0, 0xed, 2, 1]);
    locations.extend([0; 8]);
    custom(&mut wasm, ".debug_loc", &locations);
    wasm
}
fn sample() -> Vec<u8> {
    wat::parse_str("(module (func (param i32) (result i32) local.get 0 i32.const 42 i32.add))")
        .unwrap()
}

#[test]
fn source_rows_inline_origin_and_original_offsets_agree() {
    let wasm = dwarf(sample(), false, false, false);
    let (base, start, end, ops) = body(&wasm);
    let d = debug_info::read(&wasm);
    assert_eq!(d.status, "decoded", "{:?}", d.diagnostics);
    assert_eq!(d.lines.len(), 2);
    assert_eq!(d.lookup(start).sources[0].location.line, Some(10));
    let p = d.lookup(ops[1] + 1); // immediate byte; retain containing instruction.
    assert_eq!(p.code_offset, Some(ops[1] + 1 - base));
    assert_eq!(p.instruction_span.unwrap().start, ops[1]);
    assert_eq!(p.sources[0].location.file.as_ref(), "/source/sample.c");
    assert_eq!(p.sources[0].location.line, Some(11));
    assert_eq!(p.symbols.len(), 4);
    let inline = p
        .symbols
        .iter()
        .find(|s| s.kind == "inlined_subroutine")
        .unwrap();
    assert_eq!(inline.name.as_deref(), Some("calculate"));
    assert_eq!(inline.call_site.as_ref().unwrap().line, Some(12));
    assert_eq!(inline.parent_offset, Some(p.symbols[0].metadata_offset));
    assert!(d.lookup(end).sources.is_empty());
    assert!(d.lookup(base).sources.is_empty());
    assert_eq!(d.module_sha256.as_ref().unwrap().len(), 64);
}

#[test]
fn inserting_custom_bytes_before_code_does_not_confuse_pc_and_file_offset() {
    let wasm = sample();
    let mut shifted = wasm[..8].to_vec();
    custom(&mut shifted, "arbitrary", &[7; 61]);
    shifted.extend(&wasm[8..]);
    let a = debug_info::read(&dwarf(wasm, false, false, false));
    let b = debug_info::read(&dwarf(shifted, false, false, false));
    assert_eq!(a.lines[0].range.code_span, b.lines[0].range.code_span);
    assert_ne!(a.lines[0].range.file_span, b.lines[0].range.file_span);
}

#[test]
fn malformed_debug_is_opaque_and_byte_patch_still_roundtrips() {
    let old = sample();
    let mut new = old.clone();
    custom(&mut new, ".debug_info", &[255, 255, 255, 255, 9]);
    let d = debug_info::read(&new);
    assert_eq!(d.status, "opaque");
    assert!(!d.diagnostics.is_empty());
    assert!(parse::parse_module(&new).is_ok());
    assert!(audit::inspect(&new).is_ok());
    let patch = patch::create(&old, &new).unwrap();
    assert_eq!(patch.apply(&old).unwrap(), new);
}

#[test]
fn repeated_debug_sections_are_not_silently_chosen() {
    let mut wasm = dwarf(sample(), false, false, false);
    custom(&mut wasm, ".debug_info", &[0; 12]);
    let d = debug_info::read(&wasm);
    assert!(d.lines.is_empty());
    assert!(d.symbols.is_empty());
    assert!(d.diagnostics.iter().any(|s| s.contains("repeated DWARF")));
}

#[test]
fn range_in_an_immediate_is_not_rounded_to_an_instruction() {
    let d = debug_info::read(&dwarf(sample(), true, false, false));
    assert_eq!(d.status, "partial");
    assert!(d
        .symbols
        .iter()
        .find(|s| s.kind == "subprogram")
        .unwrap()
        .ranges
        .is_empty());
    assert!(d
        .diagnostics
        .iter()
        .any(|s| s.contains("not a decoded instruction")));
}

#[test]
fn unterminated_lines_and_cyclic_origins_never_create_false_locations() {
    let d = debug_info::read(&dwarf(sample(), false, true, false));
    assert!(d.lines.is_empty());
    assert!(d.diagnostics.iter().any(|s| s.contains("unterminated")));
    let d = debug_info::read(&dwarf(sample(), false, false, true));
    let inline = d
        .symbols
        .iter()
        .find(|s| s.kind == "inlined_subroutine")
        .unwrap();
    assert!(inline.name.is_none());
    assert!(d.diagnostics.iter().any(|s| s.contains("cyclic")));
}

#[test]
fn relocation_target_is_read_from_payload_not_guessed_from_name() {
    let mut wasm = dwarf(sample(), false, false, false);
    let section_index = wasmparser::Parser::new(0)
        .parse_all(&wasm)
        .filter_map(|p| match p.unwrap() {
            wasmparser::Payload::CustomSection(s) => Some(s.name().to_owned()),
            _ => None,
        })
        .position(|n| n == ".debug_info")
        .unwrap();
    // sample has type/function/code before the three debug sections.
    let target = section_index + 3;
    custom(
        &mut wasm,
        "reloc.surprising-name",
        &[uleb(target), vec![0]].concat(),
    );
    let d = debug_info::read(&wasm);
    assert!(d.lines.is_empty());
    assert!(d.diagnostics.iter().any(|s| s.contains("unapplied DWARF")));
}

#[test]
fn stripped_module_has_no_guessed_source_labels() {
    let d = debug_info::read(&sample());
    assert_eq!(d.status, "absent");
    assert!(d.lines.is_empty());
    assert!(d.symbols.is_empty());
}

#[test]
fn undefined_linking_symbol_cannot_claim_a_defined_function_body() {
    let mut wasm = sample();
    let table = [1, 0, 0x50, 0, 1, b'x']; // explicit-name, undefined function symbol.
    let data = [vec![2, 8], uleb(table.len()), table.to_vec()].concat();
    custom(&mut wasm, "linking", &data);
    let debug = debug_info::read(&wasm);
    assert_eq!(debug.symbols[0].name.as_deref(), Some("x"));
    assert!(debug.symbols[0].ranges.is_empty());
    assert!(parse::parse_module(&wasm)
        .unwrap()
        .function_names
        .is_empty());
}

#[test]
fn wasm_variable_slots_and_location_lists_are_static_and_scoped() {
    let wasm = dwarf(sample(), false, false, false);
    let (_, start, _, ops) = body(&wasm);
    let d = debug_info::read(&wasm);
    assert_eq!(d.status, "decoded", "{:?}", d.diagnostics);
    let x = d
        .symbols
        .iter()
        .find(|s| s.name.as_deref() == Some("x"))
        .unwrap();
    assert_eq!(x.kind, "parameter");
    assert_eq!(x.locations[0].expression_hex, "ed00009f");
    assert_eq!(x.locations[0].wasm_slots[0].index_space, "local");
    assert_eq!(x.locations[0].wasm_slots[0].index, 0);
    assert_eq!(x.locations[0].evaluation, "not_performed");
    let ranged = d
        .symbols
        .iter()
        .find(|s| s.name.as_deref() == Some("ranged"))
        .unwrap();
    assert_eq!(
        ranged.locations[0].wasm_slots[0].index_space,
        "operand_stack"
    );
    assert_eq!(ranged.ranges[0].file_span.start, ops[1]);
    assert!(!d
        .lookup(start)
        .symbols
        .iter()
        .any(|s| s.name.as_deref() == Some("ranged")));
    assert!(d
        .lookup(ops[1])
        .symbols
        .iter()
        .any(|s| s.name.as_deref() == Some("ranged")));
}

fn custom_span(wasm: &[u8], name: &str) -> std::ops::Range<usize> {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .find_map(|p| match p.unwrap() {
            wasmparser::Payload::CustomSection(s) if s.name() == name => {
                Some(s.data_offset()..s.data_offset() + s.data().len())
            }
            _ => None,
        })
        .unwrap()
}

#[test]
fn location_list_is_intersected_with_scope_without_losing_original_pc_range() {
    let mut wasm = dwarf(sample(), false, false, false);
    let (base, start, end, ops) = body(&wasm);
    let span = custom_span(&wasm, ".debug_info");
    let name = b"calculate\0";
    let low = wasm[span.clone()]
        .windows(name.len())
        .position(|s| s == name)
        .unwrap()
        + span.start
        + name.len();
    wasm[low..low + 4].copy_from_slice(&((ops[1] - base) as u32).to_le_bytes());
    wasm[low + 4..low + 8].copy_from_slice(&((end - ops[1]) as u32).to_le_bytes());
    let loc = custom_span(&wasm, ".debug_loc");
    wasm[loc.start..loc.start + 4].copy_from_slice(&((start - base) as u32).to_le_bytes());
    let d = debug_info::read(&wasm);
    let ranged = d
        .symbols
        .iter()
        .find(|s| s.name.as_deref() == Some("ranged"))
        .unwrap();
    assert_eq!(
        ranged.locations[0].declared_code_range,
        Some([(start - base) as u64, (ops[2] - base) as u64])
    );
    assert_eq!(
        ranged.locations[0].range_interpretation,
        "intersection_with_lexical_scope"
    );
    assert_eq!(ranged.ranges[0].file_span.start, ops[1]);
    assert!(!d
        .lookup(start)
        .symbols
        .iter()
        .any(|s| s.name.as_deref() == Some("ranged")));
}

#[test]
fn unknown_instruction_suffix_is_not_given_guessed_debug_boundaries() {
    let mut wasm = dwarf(sample(), false, false, false);
    let (_, _, _, ops) = body(&wasm);
    wasm[ops[2]] = 0xff;
    let d = debug_info::read(&wasm);
    assert!(d.lookup(ops[2]).sources.is_empty());
    assert!(d
        .symbols
        .iter()
        .filter(|s| s.kind == "subprogram")
        .all(|s| s.ranges.is_empty()));
    assert!(!d.diagnostics.is_empty());
    assert!(audit::inspect(&wasm).is_ok());
}

#[test]
fn line_zero_does_not_inherit_the_previous_source_line() {
    let mut wasm = dwarf(sample(), false, false, false);
    let (_, _, _, ops) = body(&wasm);
    let span = custom_span(&wasm, ".debug_line");
    let offset = wasm[span.clone()]
        .windows(4)
        .position(|s| s == [3, 1, 1, 2])
        .unwrap()
        + span.start;
    wasm[offset + 1] = 0x76; // advance_line -10: line 10 becomes zero.
    let d = debug_info::read(&wasm);
    assert_eq!(d.lookup(ops[0]).sources[0].location.line, Some(10));
    assert!(d.lookup(ops[1]).sources.is_empty());
}

#[test]
fn corrupted_debug_payloads_never_reject_or_mutate_the_original_module() {
    let original = dwarf(sample(), false, false, false);
    for name in [".debug_info", ".debug_abbrev", ".debug_line", ".debug_loc"] {
        let span = custom_span(&original, name);
        for i in (span.start..span.end).step_by(3) {
            let mut wasm = original.clone();
            wasm[i] ^= 0xff;
            let _ = debug_info::read(&wasm);
            assert!(parse::parse_module(&wasm).is_ok());
            let patch = patch::create(&original, &wasm).unwrap();
            assert_eq!(patch.apply(&original).unwrap(), wasm);
        }
    }
}

#[cfg(feature = "client")]
#[test]
fn debug_queries_survive_native_history_gc_and_exact_restore() {
    use std::{fs, process::Command};
    let root = std::env::temp_dir().join(format!("uwgit-debug-gc-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let repo = root.join("repo");
    let file = root.join("module.wasm");
    let original = dwarf(sample(), false, false, false);
    fs::write(&file, &original).unwrap();
    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o.stdout
    };
    run(&["init", repo.to_str().unwrap()]);
    run(&[
        "commit",
        repo.to_str().unwrap(),
        file.to_str().unwrap(),
        "-m",
        "debug metadata",
    ]);
    let query = [
        "debug-map",
        "HEAD",
        "--repo",
        repo.to_str().unwrap(),
        "--symbol",
        "calculate",
        "--json",
    ];
    let before = run(&query);
    run(&["gc", repo.to_str().unwrap()]);
    assert_eq!(before, run(&query));
    let output = root.join("restored.wasm");
    run(&[
        "show",
        repo.to_str().unwrap(),
        "HEAD",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert_eq!(fs::read(output).unwrap(), original);
    run(&["fsck", repo.to_str().unwrap()]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dwarf_supplies_advisory_matching_name_when_name_section_absent() {
    let wasm = dwarf(sample(), false, false, false);
    let p = parse::parse_module(&wasm).unwrap();
    assert_eq!(p.function_names[&0], "calculate");
    assert_eq!(p.function_symbol_sources[&0], "debug_symbol");
}

#[cfg(feature = "client")]
#[test]
fn changed_instruction_has_old_and_new_source_annotations() {
    use std::{fs, process::Command};
    let a = dwarf(sample(), false, false, false);
    let b = dwarf(
        wat::parse_str("(module (func (param i32) (result i32) local.get 0 i32.const 43 i32.add))")
            .unwrap(),
        false,
        false,
        false,
    );
    let root = std::env::temp_dir().join(format!("uwgit-debug-diff-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a"), &a).unwrap();
    fs::write(root.join("b"), &b).unwrap();
    let diff = || {
        let o = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .arg("diff")
            .arg(root.join("a"))
            .arg(root.join("b"))
            .args(["--format", "json", "--compact"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()
    };
    let r = diff();
    assert_eq!(r["summary"]["functions_changed"], 1);
    let annotations = r["changes"][0]["function"]["source_annotations"]
        .as_array()
        .unwrap();
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0]["old"]["sources"][0]["location"]["line"], 11);
    assert_eq!(annotations[0]["new"]["sources"][0]["location"]["line"], 11);
    assert_ne!(
        r["debug_context"][0]["module_sha256"],
        r["debug_context"][1]["module_sha256"]
    );
    fs::write(root.join("a"), sample()).unwrap();
    fs::write(root.join("b"), &a).unwrap();
    assert_eq!(diff()["summary"]["functions_changed"], 0);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "client")]
#[test]
fn cli_supports_offset_symbol_source_queries_without_opening_source_files() {
    use std::{fs, process::Command};
    let root = std::env::temp_dir().join(format!("uwgit-debug-cli-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let wasm = dwarf(sample(), false, false, false);
    let (_, _, _, ops) = body(&wasm);
    let file = root.join("a.wasm");
    fs::write(&file, &wasm).unwrap();
    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .arg("debug-map")
            .arg(&file)
            .args(args)
            .arg("--json")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()
    };
    let d = run(&["--offset", &ops[1].to_string()]);
    assert_eq!(d["annotation"]["function_index"], 0);
    let d = run(&["--symbol", "calculate"]);
    assert_eq!(d["debug_info"]["symbols"].as_array().unwrap().len(), 2);
    let d = run(&["--source", "/source/sample.c", "--line", "11"]);
    assert_eq!(d["debug_info"]["lines"].as_array().unwrap().len(), 1);
    let d = run(&["--source", "missing"]);
    assert!(d["debug_info"]["lines"].as_array().unwrap().is_empty());
    fs::remove_dir_all(root).unwrap();
}
