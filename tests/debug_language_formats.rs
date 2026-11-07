//! Compiler-independent fixtures check address coordinates and trust boundaries.
use serde_json::json;
use wasm_git::{audit, debug_info, parse, patch};

fn leb(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 127) as u8;
        n >>= 7;
        out.push(b | if n == 0 { 0 } else { 128 });
        if n == 0 {
            return out;
        }
    }
}
fn custom(wasm: &mut Vec<u8>, name: &str, data: &[u8]) {
    let payload = [leb(name.len()), name.as_bytes().to_vec(), data.to_vec()].concat();
    wasm.push(0);
    wasm.extend(leb(payload.len()));
    wasm.extend(payload);
}
fn sample() -> Vec<u8> {
    wat::parse_str("(module (func (export \"entry\") (param i32) (result i32) local.get 0 i32.const 7 i32.add) (func nop))").unwrap()
}
fn body(wasm: &[u8]) -> (usize, usize, usize, Vec<usize>) {
    let mut base = 0;
    for p in wasmparser::Parser::new(0).parse_all(wasm) {
        match p.unwrap() {
            wasmparser::Payload::CodeSectionStart { range, .. } => base = range.start,
            wasmparser::Payload::CodeSectionEntry(body) => {
                let mut ops = body.get_operators_reader().unwrap();
                let mut offsets = vec![];
                while !ops.eof() {
                    offsets.push(ops.original_position());
                    ops.read().unwrap();
                }
                return (base, body.range().start, body.range().end, offsets);
            }
            _ => {}
        }
    }
    panic!("body missing")
}
fn vlq(n: i64) -> String {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut n = (n.unsigned_abs() << 1) | u64::from(n < 0);
    let mut out = String::new();
    loop {
        let d = (n & 31) as usize;
        n >>= 5;
        out.push(table[d | if n == 0 { 0 } else { 32 }] as char);
        if n == 0 {
            return out;
        }
    }
}
fn map(points: &[(usize, Option<(i64, i64)>)]) -> Vec<u8> {
    let (mut previous, mut line, mut col) = (0, 0, 0);
    let mut segments = vec![];
    for &(offset, source) in points {
        let mut s = vlq(offset as i64 - previous);
        previous = offset as i64;
        if let Some((l, c)) = source {
            s.push('A');
            s += &vlq(l - line);
            s += &vlq(c - col);
            line = l;
            col = c;
        }
        segments.push(s);
    }
    serde_json::to_vec(&json!({"version":3,"sourceRoot":"/project","sources":["module.mbt"],"names":[],"mappings":segments.join(",")})).unwrap()
}

#[test]
fn source_maps_use_absolute_offsets_and_unmapped_fences() {
    let wasm = sample();
    let (base, _, end, ops) = body(&wasm);
    let mut d = debug_info::read(&wasm);
    let data = map(&[
        (ops[0], Some((9, 2))),
        (ops[1], None),
        (ops[2], Some((10, 0))),
    ]);
    d.attach_debug_file(&wasm, &data, "module.wasm.map")
        .unwrap();
    let row = &d.lookup(ops[0] + 1).sources[0];
    assert_eq!(row.location.file.as_deref(), Some("/project/module.mbt"));
    assert_eq!(row.location.line, Some(10));
    assert_eq!(row.location.column, Some(3));
    assert_eq!(row.range.code_span.start, ops[0] - base);
    assert!(d.lookup(ops[1]).sources.is_empty());
    assert!(d.lookup(end).sources.is_empty());
    assert_eq!(d.lookup(ops[2]).sources[0].location.line, Some(11));
    assert_eq!(d.inputs[0].binding, "explicit_input_unverified");
    assert_eq!(d.inputs[0].sha256.len(), 64);
}

#[test]
fn source_maps_keep_null_sources_and_literal_root_prefixes() {
    let wasm = sample();
    let (_, _, _, ops) = body(&wasm);
    let mut input: serde_json::Value =
        serde_json::from_slice(&map(&[(ops[0], Some((4, 0)))])).unwrap();
    input["sources"] = json!([null]);
    let mut d = debug_info::read(&wasm);
    d.attach_debug_file(&wasm, &serde_json::to_vec(&input).unwrap(), "anonymous.map")
        .unwrap();
    let location = &d.lookup(ops[0]).sources[0].location;
    assert!(location.file.is_none());
    assert_eq!(location.line, Some(5));
    input["sources"] = json!(["module.mbt"]);
    input["sourceRoot"] = json!("webpack:///");
    let mut d = debug_info::read(&wasm);
    d.attach_debug_file(&wasm, &serde_json::to_vec(&input).unwrap(), "root.map")
        .unwrap();
    assert_eq!(
        d.lookup(ops[0]).sources[0].location.file.as_deref(),
        Some("webpack:///module.mbt")
    );
}

#[test]
fn inline_maps_are_bounded_by_debug_bytes_not_the_wasm_name_limit() {
    use base64::Engine;
    let mut wasm = sample();
    let (_, _, _, ops) = body(&wasm);
    let mut value: serde_json::Value =
        serde_json::from_slice(&map(&[(ops[0], Some((1, 0)))])).unwrap();
    value["vendor_extension"] = json!("x".repeat(1_100_000));
    let uri = format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value).unwrap())
    );
    custom(
        &mut wasm,
        "sourceMappingURL",
        &[leb(uri.len()), uri.into_bytes()].concat(),
    );
    let debug = debug_info::read(&wasm);
    assert_eq!(debug.lookup(ops[0]).sources[0].location.line, Some(2));
    assert!(debug.diagnostics.is_empty(), "{:?}", debug.diagnostics);
}

#[test]
fn indexed_source_maps_are_ordered_and_do_not_bleed_across_sections() {
    let wasm = sample();
    let (_, _, _, ops) = body(&wasm);
    let child: serde_json::Value = serde_json::from_slice(&map(&[(0, Some((1, 0)))])).unwrap();
    let good = json!({"version":3,"sections":[{"offset":{"line":0,"column":ops[0]},"map":child},{"offset":{"line":0,"column":ops[1]},"map":{"version":3,"sources":[],"mappings":""}}]});
    let mut d = debug_info::read(&wasm);
    d.attach_debug_file(&wasm, &serde_json::to_vec(&good).unwrap(), "indexed.map")
        .unwrap();
    assert_eq!(d.lines.len(), 1);
    assert_eq!(d.lines[0].range.file_span.end, ops[1]);
    for bad in [
        json!({"version":3,"sections":[{"offset":{"line":1,"column":0},"map":child}]}),
        json!({"version":3,"sections":[{"offset":{"line":0,"column":0},"url":"https://example.invalid/never-fetch"}]}),
    ] {
        assert!(d
            .attach_debug_file(&wasm, &serde_json::to_vec(&bad).unwrap(), "bad.map")
            .is_err());
    }
}

#[test]
fn embedded_source_maps_and_unknown_reference_conventions_remain_lossless() {
    use base64::Engine;
    let old = sample();
    let (_, _, _, ops) = body(&old);
    let data = map(&[(ops[0], Some((3, 0)))]);
    let url = format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    );
    let mut new = old.clone();
    custom(
        &mut new,
        "sourceMappingURL",
        &[leb(url.len()), url.into_bytes()].concat(),
    );
    let d = debug_info::read(&new);
    assert!(!d.lines.is_empty());
    assert_eq!(d.inputs[0].binding, "embedded_reference");
    custom(
        &mut new,
        "external_debug_info",
        b"not-a-length-prefixed-uri",
    );
    let d = debug_info::read(&new);
    assert!(!d.lines.is_empty());
    assert!(d
        .diagnostics
        .iter()
        .any(|s| s.contains("original bytes retained")));
    let p = patch::create(&old, &new).unwrap();
    assert_eq!(p.apply(&old).unwrap(), new);
}

#[test]
fn duplicate_maps_keep_candidates_and_invalid_instruction_points_are_not_rounded() {
    let wasm = sample();
    let (_, _, _, ops) = body(&wasm);
    let mut d = debug_info::read(&wasm);
    d.attach_debug_file(
        &wasm,
        &map(&[(ops[0], Some((1, 0))), (ops[0], Some((2, 0)))]),
        "duplicates.map",
    )
    .unwrap();
    assert!(d.lookup(ops[0]).multiple_source_candidates);
    assert!(d
        .diagnostics
        .iter()
        .any(|s| s.contains("duplicate generated")));
    let mut d = debug_info::read(&wasm);
    d.attach_debug_file(&wasm, &map(&[(ops[0] + 1, Some((1, 0)))]), "immediate.map")
        .unwrap();
    assert!(d.lines.is_empty());
    assert!(d.diagnostics.iter().any(|s| s.contains("boundary")));
    for mappings in [
        "!",
        "g",
        "AAAAAA",
        "ADAA",
        "AADA",
        "AAAD",
        "AAAAA",
        "gggggggggggggA",
    ] {
        let bad = json!({"version":3,"sources":["x"],"names":[],"mappings":mappings});
        assert!(
            d.attach_debug_file(&wasm, &serde_json::to_vec(&bad).unwrap(), "bad.map")
                .is_err(),
            "{mappings}"
        );
    }
}

fn dwarf(wasm: &mut Vec<u8>, language: u16) {
    let (base, start, end, _) = body(wasm);
    let abbrev = [
        1, 0x11, 1, 0x13, 5, 0x25, 8, 3, 8, 0, 0, 2, 0x2e, 0, 3, 8, 0x11, 1, 0x12, 6, 0, 0, 0,
    ];
    let mut info = vec![4, 0, 0, 0, 0, 0, 4, 1];
    info.extend(language.to_le_bytes());
    info.extend(b"test compiler\0module.source\0\x02calculate\0");
    info.extend(((start - base) as u32).to_le_bytes());
    info.extend(((end - start) as u32).to_le_bytes());
    info.push(0);
    custom(wasm, ".debug_abbrev", &abbrev);
    custom(
        wasm,
        ".debug_info",
        &[(info.len() as u32).to_le_bytes().to_vec(), info].concat(),
    );
}

#[test]
fn dwarf_languages_are_open_and_sidecar_code_binding_is_checked() {
    for language in [
        0x0002, 0x0004, 0x0010, 0x0011, 0x0016, 0x001c, 0x0021, 0x8001,
    ] {
        let wasm = sample();
        let mut external = wasm.clone();
        dwarf(&mut external, language);
        let embedded = debug_info::read(&external);
        assert_eq!(embedded.units[0].language_code, Some(language));
        assert_eq!(embedded.units[0].producer.as_deref(), Some("test compiler"));
        assert_eq!(embedded.units[0].dwarf_version, 4);
        let mut d = debug_info::read(&wasm);
        d.attach_debug_file(&wasm, &external, "module.debug.wasm")
            .unwrap();
        assert_eq!(d.inputs[0].binding, "matching_code_payload");
        assert_eq!(
            d.symbols[0].metadata_source.as_deref(),
            Some("module.debug.wasm")
        );
        let mut bad = external.clone();
        let (_, _, _, ops) = body(&bad);
        bad[ops[1] + 1] = 8;
        assert!(d
            .attach_debug_file(&wasm, &bad, "wrong.debug.wasm")
            .is_err());
        let mut stripped = b"\0asm\x01\0\0\0".to_vec();
        stripped.extend(&external[wasm.len()..]);
        let mut d = debug_info::read(&wasm);
        d.attach_debug_file(&wasm, &stripped, "unbound.debug.wasm")
            .unwrap();
        assert_eq!(d.inputs[0].binding, "explicit_input_unverified");
        assert_eq!(d.symbols[0].ranges, embedded.symbols[0].ranges);
    }
}

#[test]
fn numeric_symbol_maps_and_padded_name_subsection_lengths_are_supported() {
    let mut wasm = sample();
    let symbols = debug_info::read(&wasm).symbols.len();
    let mut d = debug_info::read(&wasm);
    d.attach_debug_file(&wasm, b"0:calculate\n1:second\n", "module.symbols")
        .unwrap();
    assert_eq!(d.symbols.len(), symbols + 2);
    assert_eq!(d.symbols[0].origin, "symbol_map");
    assert!(d
        .attach_debug_file(&wasm, b"0:a\n0:b", "duplicate.symbols")
        .is_err());
    assert!(d
        .attach_debug_file(&wasm, b"99:outside", "outside.symbols")
        .is_err());
    let payload = [
        1, 0x8c, 0x80, 0x80, 0x80, 0, 1, 0, 9, b'c', b'a', b'l', b'c', b'u', b'l', b'a', b't', b'e',
    ];
    custom(&mut wasm, "name", &payload);
    let parsed = parse::parse_module(&wasm).unwrap();
    assert_eq!(
        parsed.function_names.get(&0).map(String::as_str),
        Some("calculate")
    );
    assert!(debug_info::read(&wasm)
        .names
        .iter()
        .any(|n| n.name == "calculate"));
}

#[test]
fn import_only_modules_accept_symbol_maps_and_empty_source_maps() {
    let wasm = wat::parse_str(r#"(module (import "m" "f" (func)))"#).unwrap();
    let mut debug = debug_info::read(&wasm);
    debug
        .attach_debug_file(&wasm, b"0:imported_function\n", "module.symbols")
        .unwrap();
    assert_eq!(debug.symbols[0].function_index, Some(0));
    assert!(debug.symbols[0].ranges.is_empty());
    assert!(debug
        .attach_debug_file(&wasm, b"1:invalid\n", "bad.symbols")
        .is_err());
    let mut debug = debug_info::read(&wasm);
    debug
        .attach_debug_file(&wasm, &map(&[]), "empty.map")
        .unwrap();
    assert_eq!(debug.status, "decoded");
    assert!(debug.lines.is_empty());
    assert_eq!(debug.inputs.len(), 1);
}

fn put(data: &mut [u8], at: usize, value: u64, size: usize) {
    data[at..at + size].copy_from_slice(&value.to_le_bytes()[..size]);
}
fn go_fixture(magic: u32, indexed: bool, overlap: bool) -> Vec<u8> {
    let mut data = vec![0; 320];
    put(&mut data, 0, magic as u64, 4);
    data[6] = 1;
    data[7] = 8;
    put(&mut data, 8, 1, 8);
    let modern = matches!(magic, 0xffff_fff0 | 0xffff_fff1);
    let split = magic != 0xffff_fffb;
    let width = if modern { 4 } else { 8 };
    let names = 96;
    let units = 128;
    let files = 144;
    let pctab = 192;
    let funcs = if split { 224 } else { 0 };
    let ft = if split { 224 } else { 16 };
    let record = if split {
        224 + (2 + 1) * width + 8
    } else {
        224
    };
    if split {
        put(&mut data, 16, 1, 8);
        let first = if modern { 3 } else { 2 };
        for (i, offset) in [names, units, files, pctab, funcs].iter().enumerate() {
            put(&mut data, 8 + 8 * (first + i), *offset as u64, 8);
        }
    } else {
        put(&mut data, 40, files as u64, 4);
        put(&mut data, files, 2, 4);
        put(&mut data, files + 4, 160, 4);
    }
    let pc = if indexed { 0x1000 } else { 0x1000_0000 };
    let end = if indexed { 0x1001 } else { 0x1001_0000 };
    put(&mut data, ft, pc, width);
    put(&mut data, ft + width, (record - funcs) as u64, width);
    put(&mut data, ft + 2 * width, end, width);
    data[names + 3..names + 13].copy_from_slice(b"calculate\0");
    let path = b"/src/main.go\0";
    let file_start = if split { files } else { 160 };
    data[file_start..file_start + path.len()].copy_from_slice(path);
    data[pctab + 1..pctab + 7].copy_from_slice(&[if split { 2 } else { 4 }, 1, 0, 16, 1, 0]);
    let size = if modern { 4 } else { 8 };
    put(&mut data, record, pc, size);
    put(
        &mut data,
        record + size,
        if split { 3 } else { (names + 3) as u64 },
        4,
    );
    put(
        &mut data,
        record + size + 16,
        if split { 1 } else { (pctab + 1) as u64 },
        4,
    );
    put(
        &mut data,
        record + size + 20,
        if split { 4 } else { (pctab + 4) as u64 },
        4,
    );
    // Zero bytes really are omitted, as in the Go linker. The decoder must
    // reconstruct memory holes, not concatenate the data payloads.
    let mut wat = String::from("(module (import \"env\" \"f\" (func)) (memory 1) (func nop)");
    let mut at = 0;
    while at < data.len() {
        if data[at] == 0 {
            at += 1;
            continue;
        }
        let begin = at;
        while at < data.len() && data[at] != 0 {
            at += 1;
        }
        wat += &format!(
            "(data (i32.const {}) \"{}\")",
            4096 + begin,
            data[begin..at]
                .iter()
                .map(|b| format!("\\{b:02x}"))
                .collect::<String>()
        );
    }
    if overlap {
        wat += "(data (i32.const 4096) \"x\")";
    }
    wat.push(')');
    let mut wasm = wat::parse_str(wat).unwrap();
    custom(&mut wasm, "go:buildid", b"fixture");
    wasm
}

#[test]
fn go_runtime_versions_keep_virtual_pcs_separate_from_instruction_offsets() {
    for (magic, indexed) in [
        (0xffff_fffb, false),
        (0xffff_fffa, false),
        (0xffff_fff0, false),
        (0xffff_fff1, true),
    ] {
        let wasm = go_fixture(magic, indexed, false);
        let d = debug_info::read(&wasm);
        assert_eq!(d.go_tables.len(), 1, "{:?}", d.diagnostics);
        assert_eq!(d.go_lines.len(), 1);
        let row = &d.go_lines[0];
        assert_eq!(row.virtual_pc, [0x1000_0000, 0x1000_0001]);
        assert_eq!(row.function_index, 1);
        assert_eq!(row.location.line, Some(7));
        assert_eq!(row.location.file.as_deref(), Some("/src/main.go"));
        assert!(d.lines.is_empty());
        let (_, _, _, ops) = body(&wasm);
        let point = d.lookup(ops[0]);
        assert!(point.sources.is_empty());
        assert_eq!(point.symbols[0].kind, "go_function");
        assert_eq!(point.symbols[0].function_index, Some(1));
        let parsed = parse::parse_module(&wasm).unwrap();
        assert_eq!(
            parsed.function_names.get(&1).map(String::as_str),
            Some("calculate")
        );
    }
}

#[test]
fn overlapping_go_runtime_data_is_not_mistaken_for_a_pclntab() {
    let wasm = go_fixture(0xffff_fff1, true, true);
    let d = debug_info::read(&wasm);
    assert!(d.go_tables.is_empty());
    assert!(d.diagnostics.iter().any(|s| s.contains("overlapping")));
    assert!(audit::inspect(&wasm).is_ok());
}

fn named_module() -> Vec<u8> {
    wat::parse_str(
        r#"(module $module
        (type $record (struct (field $field i32)))
        (memory $memory 1) (table $table 1 funcref)
        (global $global i32 (i32.const 0)) (tag $tag)
        (data $data (i32.const 0) "x") (elem $element (i32.const 0) func $calculate)
        (func $calculate (param $x i32) (result i32)
          (local $temporary i32) (block $label local.get $x local.set $temporary)
          local.get $temporary))"#,
    )
    .unwrap()
}

#[test]
fn name_index_spaces_preserve_function_and_type_scopes() {
    let wasm = named_module();
    let d = debug_info::read(&wasm);
    for space in [
        "module", "function", "local", "label", "type", "table", "memory", "global", "element",
        "data", "field", "tag",
    ] {
        assert!(
            d.names.iter().any(|n| n.index_space == space),
            "missing {space}: {:?}",
            d.names
        );
    }
    assert!(d
        .names
        .iter()
        .any(|n| n.name == "field" && n.scope_index == Some(0)));
    assert!(d
        .names
        .iter()
        .any(|n| n.name == "temporary" && n.scope_index == Some(0) && n.index == 1));
}

#[test]
fn go_language_is_not_a_runtime_format_signature() {
    let mut wasm = sample();
    let mut producer = vec![1];
    producer.extend(
        [
            leb(8),
            b"language".to_vec(),
            vec![1],
            leb(2),
            b"Go".to_vec(),
            vec![0],
        ]
        .concat(),
    );
    custom(&mut wasm, "producers", &producer);
    assert!(debug_info::read(&wasm).diagnostics.is_empty());
}

#[cfg(feature = "client")]
#[test]
fn explicit_sidecars_reach_inspect_diff_and_queries_and_report_bad_conventions() {
    use std::{fs, process::Command};
    let root = std::env::temp_dir().join(format!("wasm-git-language-cli-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let old = sample();
    let (_, _, _, ops) = body(&old);
    let mut new = old.clone();
    new[ops[1] + 1] = 8;
    fs::write(root.join("old.wasm"), &old).unwrap();
    fs::write(root.join("new.wasm"), &new).unwrap();
    fs::write(root.join("input.map"), map(&[(ops[0], Some((9, 2)))])).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let view = run(&["inspect", "old.wasm", "--debug-file", "input.map", "--json"]);
    assert!(!view["debug_info"]["lines"].as_array().unwrap().is_empty());
    let view = run(&[
        "diff",
        "old.wasm",
        "new.wasm",
        "--old-debug-file",
        "input.map",
        "--new-debug-file",
        "input.map",
        "--format",
        "json",
    ]);
    assert_eq!(view["summary"]["functions_changed"], 1);
    let annotations = &view["changes"][0]["function"]["source_annotations"];
    assert!(!annotations.as_array().unwrap().is_empty());
    assert_eq!(annotations[0]["new"]["sources"][0]["location"]["line"], 10);
    fs::write(root.join("input.map"), b"{not a map").unwrap();
    let view = run(&[
        "diff",
        "old.wasm",
        "new.wasm",
        "--new-debug-file",
        "input.map",
        "--format",
        "json",
    ]);
    assert!(view["audit"]["changed"].as_bool().unwrap());
    assert!(view["debug_context"][1]["diagnostics"][0]
        .as_str()
        .unwrap()
        .contains("no mappings inferred"));
    let go = go_fixture(0xffff_fff1, true, false);
    fs::write(root.join("go.wasm"), go).unwrap();
    let view = run(&["debug-map", "go.wasm", "--go-pc", "268435456", "--json"]);
    assert_eq!(view["debug_info"]["go_lines"][0]["location"]["line"], 7);
    let view = run(&[
        "debug-map",
        "go.wasm",
        "--source",
        "/src/main.go",
        "--line",
        "7",
        "--json",
    ]);
    assert_eq!(view["debug_info"]["go_lines"].as_array().unwrap().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

/// Run exactly this ignored test per pinned E-core process. Each family uses a
/// valid Core module; malformed debug conventions never authorize dropping bytes.
#[test]
#[ignore = "opt-in 600-second language debug fuzz; run through tools/fuzz-debug-languages.py"]
fn timed_language_debug_fuzz() {
    use std::{
        fs,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    let worker: u64 = std::env::var("WASM_GIT_DEBUG_WORKER")
        .unwrap()
        .parse()
        .unwrap();
    let seconds: u64 = std::env::var("WASM_GIT_DEBUG_SECONDS")
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=600).contains(&seconds));
    let output = std::path::PathBuf::from(std::env::var_os("WASM_GIT_DEBUG_OUTPUT").unwrap());
    let allowed = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".tmp")
        .canonicalize()
        .unwrap();
    assert!(output.canonicalize().unwrap().starts_with(allowed));
    let case_dir = output.join(format!("inputs-{worker}"));
    fs::create_dir(&case_dir).unwrap();
    let start: u128 = std::env::var("WASM_GIT_DEBUG_START_MS")
        .unwrap()
        .parse()
        .unwrap();
    while SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        < start
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    let beginning = Instant::now();
    let mut state = 0x6c61_6e67_7761_736du64 ^ worker;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut counts = [0u64; 10];
    let mut cases = 0u64;
    let mut cli_rounds = 0u64;
    while beginning.elapsed() < Duration::from_secs(seconds) {
        let random = next();
        let family = (cases % 10) as usize;
        let mut old = sample();
        let (_, _, _, ops) = body(&old);
        old[ops[1] + 1] = (random % 63) as u8;
        let mut new = old.clone();
        new[ops[1] + 1] = ((random + 1) % 63) as u8;
        let line = ((random >> 8) % 100_000) as i64;
        let mut sidecar = None;
        match family {
            0 => {
                let mut value: serde_json::Value = serde_json::from_slice(&map(&[
                    (ops[0], Some((line, 0))),
                    (ops[1], None),
                    (ops[2], Some((line + 1, 1))),
                ]))
                .unwrap();
                if random & 1 != 0 {
                    value["sources"][0] = serde_json::Value::Null;
                }
                if random & 2 != 0 {
                    value["sourceRoot"] = json!("webpack:///");
                }
                sidecar = Some(serde_json::to_vec(&value).unwrap());
            }
            1 => {
                let mut data = map(&[(ops[0], Some((line, 0)))]);
                for _ in 0..1 + random % 4 {
                    let i = next() as usize % data.len();
                    data[i] = next() as u8;
                }
                sidecar = Some(data);
            }
            2 => {
                use base64::Engine;
                let data = map(&[(ops[0], Some((line, 0)))]);
                let url = format!(
                    "data:application/json;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(data)
                );
                custom(
                    &mut new,
                    "sourceMappingURL",
                    &[leb(url.len()), url.into_bytes()].concat(),
                );
            }
            3 => {
                dwarf(&mut new, random as u16);
            }
            4 => {
                let mut data = new.clone();
                dwarf(&mut data, random as u16);
                if random & 1 != 0 {
                    data[ops[1] + 1] ^= 1;
                }
                sidecar = Some(data);
            }
            5 => {
                if random & 1 != 0 {
                    old = wat::parse_str(r#"(module (import "old" "f" (func)))"#).unwrap();
                    new = wat::parse_str(r#"(module (import "new" "f" (func)))"#).unwrap();
                }
                sidecar = Some(format!("{}:symbol_{random}\n", (random >> 12) % 4).into_bytes());
                if random & 4 != 0 {
                    sidecar = Some(map(&[]));
                }
            }
            6 => {
                let formats = [
                    (0xffff_fffb, false),
                    (0xffff_fffa, false),
                    (0xffff_fff0, false),
                    (0xffff_fff1, true),
                ];
                let (magic, indexed) = formats[(random % 4) as usize];
                old = go_fixture(magic, indexed, false);
                new = old.clone();
                let data_ranges: Vec<_> = wasmparser::Parser::new(0)
                    .parse_all(&new)
                    .filter_map(|p| match p.unwrap() {
                        wasmparser::Payload::DataSection(s) => Some(
                            s.into_iter()
                                .map(|d| {
                                    let d = d.unwrap();
                                    d.range.end - d.data.len()..d.range.end
                                })
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                let range = &data_ranges[next() as usize % data_ranges.len()];
                let at = range.start + (next() as usize % range.len());
                new[at] ^= 1 << (next() % 8);
            }
            7 => {
                old = named_module();
                new = old.clone();
                let payload = [
                    1, 0x8c, 0x80, 0x80, 0x80, 0, 1, 0, 9, b'c', b'a', b'l', b'c', b'u', b'l',
                    b'a', b't', b'e',
                ];
                custom(&mut new, "name", &payload);
            }
            8 => {
                let mut shifted = new[..8].to_vec();
                custom(&mut shifted, "vendor.unrecognized", &random.to_le_bytes());
                shifted.extend(&new[8..]);
                new = shifted;
                dwarf(&mut new, 0x1c);
            }
            _ => {
                custom(&mut new, "external_debug_info", &random.to_le_bytes());
                custom(&mut new, "sourceMappingURL", &random.to_le_bytes());
            }
        }
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&new)
            .unwrap();
        let mut debug = debug_info::read(&new);
        if let Some(data) = &sidecar {
            let result = debug.attach_debug_file(&new, data, "explicit.map");
            if family == 0 {
                result.unwrap();
                assert_eq!(
                    debug.lookup(ops[0]).sources[0].location.line,
                    Some(line as u64 + 1)
                );
                assert!(debug.lookup(ops[1]).sources.is_empty());
            }
        }
        for row in &debug.lines {
            assert!(
                row.range.file_span.start < row.range.file_span.end
                    && row.range.file_span.end <= new.len()
            );
            assert_eq!(
                debug.lookup(row.range.file_span.start).function_index,
                Some(row.range.function_index)
            );
        }
        let report = audit::compare(&old, &new).unwrap();
        assert!(report.changed);
        let forward = patch::create(&old, &new).unwrap();
        assert_eq!(forward.apply(&old).unwrap(), new);
        let reverse = patch::create(&new, &old).unwrap();
        assert_eq!(reverse.apply(&new).unwrap(), old);
        parse::parse_module(&new).unwrap();
        counts[family] += 1;
        cases += 1;
        #[cfg(feature = "client")]
        if cases.is_multiple_of(511) {
            fs::write(case_dir.join("old.wasm"), &old).unwrap();
            fs::write(case_dir.join("new.wasm"), &new).unwrap();
            if let Some(data) = &sidecar {
                fs::write(case_dir.join("input.map"), data).unwrap();
            }
            let run = |args: &[&str]| {
                let output = std::process::Command::new(env!("CARGO_BIN_EXE_wasm-git"))
                    .current_dir(&case_dir)
                    .env("LLVM_PROFILE_FILE", case_dir.join("cli-%m.profraw"))
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "worker={worker} case={cases}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
            };
            let query = match cli_rounds % 6 {
                0 => ["--offset", "8"],
                1 if debug.code_payload.is_some() => ["--code-offset", "0"],
                1 => ["--offset", "8"],
                2 => ["--function", "0"],
                3 => ["--source", "/src/main.go"],
                4 => ["--symbol", "calculate"],
                _ => ["--go-pc", "268435456"],
            };
            let mut args = vec!["debug-map", "new.wasm", "--json", query[0], query[1]];
            if sidecar.is_some() {
                args.extend(["--debug-file", "input.map"]);
            }
            run(&args);
            let mut args = vec![
                "diff",
                "old.wasm",
                "new.wasm",
                "--format",
                "json",
                "--compact",
            ];
            if sidecar.is_some() {
                args.extend(["--new-debug-file", "input.map"]);
            }
            assert_eq!(run(&args)["audit"]["changed"], true);
            let mut args = vec!["inspect", "new.wasm", "--json"];
            if sidecar.is_some() {
                args.extend(["--debug-file", "input.map"]);
            }
            assert_eq!(run(&args)["function_analysis"]["available"], true);
            cli_rounds += 1;
        }
        if cases.is_multiple_of(512) {
            let record = json!({"worker":worker,"cases":cases,"families":counts,"cli_rounds":cli_rounds,"elapsed_ms":beginning.elapsed().as_millis()});
            fs::write(
                output.join(format!("progress-{worker}.json")),
                record.to_string(),
            )
            .unwrap();
        }
    }
    assert!(counts.iter().all(|n| *n > 0));
    fs::write(output.join(format!("result-{worker}.json")),json!({"status":"PASS","worker":worker,"cases":cases,"families":counts,"cli_rounds":cli_rounds,"elapsed_ms":beginning.elapsed().as_millis()}).to_string()).unwrap();
}
