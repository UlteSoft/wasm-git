//! Independently replay selected ranges in original files. These views are not
//! whole-file patches; the separate binary patch must still restore everything.
#![cfg(feature = "client")]
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use wasm_git::{
    audit::{
        self,
        section::{self, Selector},
    },
    patch::{self, BytePatch},
    standard::Standard,
};
const CORE: &[u8] = b"\0asm\x01\0\0\0";
const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "uwgit-sections-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().into()
    }
    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.path(name);
        fs::write(&p, bytes).unwrap();
        p
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn json(args: &[&str]) -> Value {
    serde_json::from_str(&run(args)).unwrap()
}
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
fn frame(bytes: &mut Vec<u8>, id: u8, payload: &[u8]) {
    bytes.push(id);
    bytes.extend(leb(payload.len()));
    bytes.extend(payload);
}
fn custom(bytes: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut p = leb(name.len());
    p.extend(name.as_bytes());
    p.extend(data);
    frame(bytes, 0, &p);
}
fn from_hex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn span(v: &Value) -> std::ops::Range<usize> {
    v["start"].as_u64().unwrap() as usize..v["end"].as_u64().unwrap() as usize
}
fn compare(a: &[u8], b: &[u8], selectors: &[&str]) -> Value {
    let ss: Vec<Selector> = selectors.iter().map(|s| s.parse().unwrap()).collect();
    let r = serde_json::to_value(section::compare(a, b, Standard::AllKnown, &ss).unwrap()).unwrap();
    assert_eq!(r["whole_module_patch"], false);
    for c in r["changes"].as_array().unwrap() {
        let range = |s: &Value, len| {
            if s.is_null() {
                len..len
            } else {
                span(&s["span"])
            }
        };
        let ar = range(&c["old"], a.len());
        let br = range(&c["new"], b.len());
        for (site, bytes) in [(&c["old"], a), (&c["new"], b)] {
            if !site.is_null() {
                assert_eq!(
                    site["sha256"],
                    format!("{:x}", Sha256::digest(&bytes[span(&site["span"])]))
                );
            }
        }
        let mut cursor = ar.start;
        let mut out = Vec::new();
        for e in c["edits"].as_array().unwrap() {
            let x = span(&e["old_span"]);
            let y = span(&e["new_span"]);
            assert!(cursor <= x.start && x.end <= ar.end && br.start <= y.start && y.end <= br.end);
            assert_eq!(from_hex(e["old_hex"].as_str().unwrap()), a[x.clone()]);
            assert_eq!(from_hex(e["new_hex"].as_str().unwrap()), b[y.clone()]);
            out.extend_from_slice(&a[cursor..x.start]);
            out.extend_from_slice(&b[y]);
            cursor = x.end;
        }
        out.extend_from_slice(&a[cursor..ar.end]);
        assert_eq!(out, b[br]);
    }
    let patch = patch::create(a, b).unwrap();
    assert_eq!(
        BytePatch::from_bytes(&patch.to_bytes().unwrap())
            .unwrap()
            .apply(a)
            .unwrap(),
        b
    );
    r
}

#[test]
fn selecting_custom_skips_unknown_code_and_keeps_nonconsensus_payloads() {
    let mut a = CORE.to_vec();
    frame(&mut a, 10, &[1, 3, 0, 0xff, 0x0b]);
    custom(&mut a, "name", &[0xff, 1]);
    custom(&mut a, "private", &[1, 2]);
    let mut b = a.clone();
    *b.last_mut().unwrap() = 7;
    let r = compare(&a, &b, &["custom:private"]);
    assert_eq!(r["old"]["sections"].as_array().unwrap().len(), 1);
    assert_eq!(r["old"]["sections"][0]["kind"], "custom:private");
    assert_eq!(r["old"]["standard"]["validation"], "not_validated");
    assert_eq!(r["selected_content_changed"], true);
    assert_eq!(
        r["changes"][0]["edits"][0]["old_span"]["start"],
        a.len() - 1
    );
    let r = compare(&a, &b, &["custom:name"]);
    assert_eq!(r["selected_content_changed"], false);
    assert_eq!(r["module_binary_changed"], true);
    assert_eq!(r["old"]["sections"][0]["status"], "opaque");
}

#[test]
fn typed_section_details_use_real_module_context_and_original_offsets() {
    let a = wat::parse_str("(module (memory 1) (func (export \"f\") (result i32) i32.const 7) (data (i32.const 0) \"abc\"))").unwrap();
    let b = wat::parse_str("(module (memory 2) (func (export \"f\") (result i32) i32.const 9) (data (i32.const 0) \"axc\"))").unwrap();
    let r = compare(&a, &b, &["code", "data"]);
    assert_eq!(r["changes"].as_array().unwrap().len(), 2);
    assert_eq!(r["old"]["standard"]["validation"], "valid");
    assert!(r["old"]["sections"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| [10, 11].contains(&s["id"].as_u64().unwrap())));
    assert!(r["changes"].as_array().unwrap().iter().any(|c| c["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"].as_str().unwrap().contains("operator"))));
    let full = serde_json::to_value(audit::compare(&a, &b).unwrap()).unwrap();
    for side in ["old", "new"] {
        for s in r[side]["sections"].as_array().unwrap() {
            assert!(full[side]["sections"].as_array().unwrap().contains(s));
        }
    }
}

#[test]
fn duplicate_custom_sections_keep_byte_correspondence_and_uncertainty() {
    let mut a = CORE.to_vec();
    custom(&mut a, "x", b"unchanged");
    custom(&mut a, "x", b"old");
    let mut b = CORE.to_vec();
    custom(&mut b, "other", b"prefix");
    custom(&mut b, "x", b"new");
    custom(&mut b, "x", b"unchanged");
    let r = compare(&a, &b, &["custom:x"]);
    assert_eq!(r["changes"].as_array().unwrap().len(), 2);
    assert!(r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["ambiguous"] == true));
    assert!(r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["content_changed"] == false
            && c["position_changed"] == true
            && c["edits"].as_array().unwrap().is_empty()));
    // Multiple unmatched same-name payloads must not invent a pairing.
    custom(&mut a, "x", b"third");
    custom(&mut b, "x", b"fourth");
    let r = compare(&a, &b, &["custom:x"]);
    assert_eq!(
        r["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["pairing"] == "unpaired")
            .count(),
        4
    );
}

#[test]
fn selectors_keep_unknown_ids_broken_names_and_truncated_outer_ranges() {
    let mut a = CORE.to_vec();
    frame(&mut a, 200, &[1, 2, 3]);
    frame(&mut a, 0, &[1, 0xff]);
    a.extend([10, 0x80]);
    let mut b = a.clone();
    b[11] = 8;
    *b.last_mut().unwrap() = 0x81;
    for selector in ["all", "id:200", "ordinal:0", "id:10", "custom", "opaque"] {
        compare(&a, &b, &[selector]);
    }
    let r = compare(&a, &b, &["id:10"]);
    assert_eq!(r["old"]["sections"][0]["status"], "malformed");
    let r = compare(&a, &b, &["custom:missing"]);
    assert_eq!(r["unmatched_selectors"][0], "custom:missing");
    assert_eq!(r["selection_complete"], serde_json::json!([false, false]));
    let report = section::compare(
        &a,
        &b,
        Standard::AllKnown,
        &["custom:missing".parse().unwrap()],
    )
    .unwrap();
    assert!(report.to_text(false).contains("absence is not proved"));
    assert!(!report
        .to_text(false)
        .contains("Selected content: identical"));
    for bad in ["codes", "id:256", "id:-1", "ordinal:-1", ""] {
        assert!(bad.parse::<Selector>().is_err());
    }
    let a = [CORE, &[200, 1, 7]].concat();
    let b = [CORE, &[201, 1, 7]].concat();
    let r = compare(&a, &b, &["all"]);
    assert_eq!(r["changes"].as_array().unwrap().len(), 2);
    assert!(r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["pairing"] == "unpaired"));
}

#[test]
fn components_and_future_encodings_are_auditable_without_fabricating_core_ir() {
    let mut a = COMPONENT.to_vec();
    frame(&mut a, 1, CORE);
    custom(&mut a, "community", b"before");
    let mut b = COMPONENT.to_vec();
    frame(&mut b, 1, &[CORE, b"\xff\x01\x80"].concat());
    custom(&mut b, "community", b"after");
    let r = compare(&a, &b, &["core_module", "custom:community"]);
    assert_eq!(r["changes"].as_array().unwrap().len(), 2);
    assert_eq!(r["old"]["sections"][0]["kind"], "core_module");
    assert_eq!(r["old"]["standard"]["validation"], "not_validated");
    let mut future = b"\0asm\xff\xff\xff\xffgarbage\0\xff".to_vec();
    let before = future.clone();
    future[10] = 0;
    let r = compare(&before, &future, &["all"]);
    assert_eq!(r["old"]["sections"][0]["kind"], "opaque_encoding");
    assert_eq!(r["old"]["sections"].as_array().unwrap().len(), 1);
    let t = Temp::new();
    let x = t.write("a.wasm", &a);
    let y = t.write("b.wasm", &b);
    let full = json(&["diff", &x, &y, "--format", "json"]);
    assert_eq!(full["function_analysis"]["available"], false);
    assert_eq!(full["audit"]["changed"], true);
    let merged = wasm_git::merge::merge(&a, &a, &b).unwrap();
    assert_eq!(merged.bytes.unwrap(), b);
    let mut third = a.clone();
    custom(&mut third, "third", b"x");
    assert_eq!(
        wasm_git::merge::merge(&a, &third, &b).unwrap().status,
        "conflict"
    );
}

#[test]
fn cli_formats_threads_and_compact_preserve_the_selected_edits() {
    let mut a = CORE.to_vec();
    custom(&mut a, "producers", &[0]);
    custom(&mut a, "x\n\u{1b}[31m", b"a");
    let mut b = a.clone();
    *b.last_mut().unwrap() = b'b';
    let t = Temp::new();
    let x = t.write("a.wasm", &a);
    let y = t.write("b.wasm", &b);
    let args = ["diff", &x, &y, "--section", "custom", "--format", "json"];
    let serial = run(&[&["--threads", "1"], args.as_slice()].concat());
    assert_eq!(
        serial,
        run(&[&["--threads", "0"], args.as_slice()].concat())
    );
    let full: Value = serde_json::from_str(&serial).unwrap();
    let compact = json(&[args.as_slice(), &["--compact"]].concat());
    assert_eq!(full["changes"], compact["changes"]);
    assert_eq!(compact["region_trees_omitted"], true);
    let text = run(&["diff", &x, &y, "--section", "custom"]);
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("\\n"));
    assert!(run(&["diff", &x, &y, "--section", "custom", "--short"])
        .contains("Selected content: changed"));
}

#[test]
fn section_profiles_keep_thread_simd_memory64_and_gc_edits_identical() {
    let samples = [
        ("(module (memory 1 1 shared) (func (param i32) (result i32) local.get 0 i32.atomic.load offset=4))", "offset=4", "offset=8"),
        ("(module (func (param v128 v128) (result v128) local.get 0 local.get 1 i8x16.add))", "i8x16.add", "i8x16.sub"),
        ("(module (memory i64 1) (func (param i64) (result i32) local.get 0 i32.load offset=33))", "offset=33", "offset=34"),
        ("(module (type (struct (field i32))) (func (result (ref 0)) i32.const 1 struct.new 0))", "const 1", "const 2"),
    ];
    for (source, before, after) in samples {
        let a = wat::parse_str(source).unwrap();
        let b = wat::parse_str(source.replace(before, after)).unwrap();
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&b)
            .unwrap();
        let mut previous = None;
        for standard in [
            Standard::Core1,
            Standard::Core2,
            Standard::Core3,
            Standard::AllKnown,
        ] {
            let r = serde_json::to_value(
                section::compare(&a, &b, standard, &["code".parse().unwrap()]).unwrap(),
            )
            .unwrap();
            assert_eq!(r["changes"].as_array().unwrap().len(), 1);
            let edits = r["changes"][0]["edits"].clone();
            assert!(!edits.as_array().unwrap().is_empty());
            if let Some(p) = &previous {
                assert_eq!(p, &edits);
            }
            previous = Some(edits);
        }
        compare(&a, &b, &["code"]);
    }
}

#[test]
fn encoding_only_section_edits_and_non_argv_custom_names_remain_selectable() {
    let a = [CORE, &[0, 3, 1, b'x', 1]].concat();
    let b = [CORE, &[0, 0x83, 0, 1, b'x', 1]].concat();
    let r = compare(&a, &b, &["custom:x"]);
    assert_eq!(r["changes"][0]["content_changed"], true);
    assert_eq!(r["changes"][0]["payload_changed"], false);
    let name = format!("a\0{}", "x".repeat(100_001));
    let mut a = CORE.to_vec();
    custom(&mut a, &name, b"old");
    let mut b = a.clone();
    *b.last_mut().unwrap() = b'y';
    let r = compare(&a, &b, &["ordinal:0"]);
    assert_eq!(r["changes"][0]["old"]["kind"], format!("custom:{name}"));
    assert_eq!(r["old"]["validation"], "not_validated");
}

#[test]
fn unknown_encoding_history_survives_gc_fork_and_bundle_transfer() {
    let t = Temp::new();
    let root = t.path("history");
    run(&["init", &root]);
    let repo = wasm_git::repository::Repository::open(std::path::Path::new(&root)).unwrap();
    let a = b"\0asm\x02\x03\x04\x05\0\xfffuture";
    let b = b"\0asm\x02\x03\x04\x05\0\xfefuture";
    let first = repo.commit(a, "opaque first").unwrap();
    let second = repo.commit(b, "opaque second").unwrap();
    run(&["gc", &root]);
    run(&["fsck", &root]);
    let fork = t.path("fork");
    run(&["fork", &root, &fork]);
    let bundle = t.path("history.uwgb");
    run(&["bundle", &root, &bundle]);
    let peer = t.path("peer");
    run(&["init", &peer]);
    run(&["unbundle", &peer, &bundle, "received"]);
    for path in [&root, &fork, &peer] {
        let r = wasm_git::repository::Repository::open(std::path::Path::new(path)).unwrap();
        assert_eq!(r.wasm_bytes(&first).unwrap(), a);
        assert_eq!(r.wasm_bytes(&second).unwrap(), b);
        r.fsck().unwrap();
    }
}

#[test]
fn repository_worktree_plumbing_and_added_deleted_files_honor_selection() {
    let t = Temp::new();
    let repo = t.path("repo");
    run(&["init", &repo]);
    let path = PathBuf::from(&repo).join("component.wasm");
    let mut a = COMPONENT.to_vec();
    custom(&mut a, "x", b"old");
    custom(&mut a, "secret", b"do-not-display");
    fs::write(&path, &a).unwrap();
    run(&["add", &repo, "--all"]);
    run(&["commit", &repo, "-m", "first"]);
    let first = run(&["rev-parse", &repo, "HEAD"]).trim().to_owned();
    let mut b = COMPONENT.to_vec();
    custom(&mut b, "x", b"new");
    custom(&mut b, "secret", b"another-hidden-payload");
    fs::write(&path, &b).unwrap();
    for args in [
        vec!["diff", &repo, "--format", "json"],
        vec!["diff-files", &repo],
        vec!["diff-index", &repo, "HEAD"],
    ] {
        let output = run(&[args.as_slice(), &["--section", "custom:x"]].concat());
        let r: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(r["files"][0]["diff"]["selected_content_changed"], true);
        assert!(!output.contains("custom:secret"));
        assert!(!output.contains("do-not-display"));
    }
    run(&["add", &repo, "--all"]);
    run(&["commit", &repo, "-m", "second"]);
    let second = run(&["rev-parse", &repo, "HEAD"]).trim().to_owned();
    for args in [
        vec!["diff", &first, &second, "--repo", &repo, "--format", "json"],
        vec!["diff-tree", &repo, &first, &second],
    ] {
        let r = json(&[args.as_slice(), &["--section", "custom:x"]].concat());
        assert_eq!(
            r["files"][0]["diff"]["changes"].as_array().unwrap().len(),
            1
        );
    }
    fs::remove_file(&path).unwrap();
    let deleted = json(&["diff-files", &repo, "--section", "custom:x"]);
    assert_eq!(
        deleted["files"][0]["diff"]["changes"][0]["new"],
        Value::Null
    );
    assert_eq!(deleted["files"][0]["content"], Value::Null);
    let added = PathBuf::from(&repo).join("new.wasm");
    fs::write(&added, &b).unwrap();
    run(&["add", &repo, "--all"]);
    let staged = json(&[
        "diff",
        &repo,
        "--cached",
        "--format",
        "json",
        "--section",
        "custom:x",
    ]);
    assert_eq!(staged["files"].as_array().unwrap().len(), 2);
    for f in staged["files"].as_array().unwrap() {
        assert_eq!(f["content"], Value::Null);
        assert!(f["diff"]["changes"].is_array());
    }
    // GC preserves exact Component bytes and every earlier revision.
    run(&["gc", &repo]);
    run(&["fsck", &repo]);
    let repository = wasm_git::repository::Repository::open(std::path::Path::new(&repo)).unwrap();
    let revisions =
        wasm_git::repository::read_file_revisions(std::path::Path::new(&repo), &[&first, &second])
            .unwrap();
    assert_eq!(revisions[0].files["component.wasm"], a);
    assert_eq!(revisions[1].files["component.wasm"], b);
    drop(repository);
}
