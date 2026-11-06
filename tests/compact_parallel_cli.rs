#![cfg(feature = "client")]
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Pair {
    root: PathBuf,
    old: PathBuf,
    new: PathBuf,
}
impl Pair {
    fn new(a: &str, b: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "uwgit-compact-parallel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let old = root.join("old.wasm");
        let new = root.join("new.wasm");
        fs::write(&old, wat::parse_str(a).unwrap()).unwrap();
        fs::write(&new, wat::parse_str(b).unwrap()).unwrap();
        Self { root, old, new }
    }
    fn diff(&self, threads: &str, compact: bool) -> Value {
        self.diff_profile(threads, compact, "all-known")
    }
    fn diff_profile(&self, threads: &str, compact: bool, standard: &str) -> Value {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
        c.args([
            "--threads",
            threads,
            "diff",
            p(&self.old),
            p(&self.new),
            "--format",
            "json",
            "--standard",
            standard,
        ]);
        if compact {
            c.arg("--compact");
        }
        let r = c.output().unwrap();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        serde_json::from_slice(&r.stdout).unwrap()
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn p(p: &Path) -> &str {
    p.to_str().unwrap()
}
#[test]
fn export_rebinding_does_not_count_unchanged_implementations_as_edits() {
    let pair=Pair::new("(module (func (result i32) i32.const 1) (func (result i32) i32.const 2) (export \"f\" (func 0)))","(module (func (result i32) i32.const 1) (func (result i32) i32.const 2) (export \"f\" (func 1)))");
    let r = pair.diff("1", false);
    assert_eq!(r["summary"]["functions_changed"], 0);
    assert_eq!(r["summary"]["operator_replacements"], 0);
    assert_eq!(r["export_binding_notes"][0]["relation"], "different_target");
    assert_eq!(r, pair.diff("4", false));
}
#[test]
fn a_unique_export_role_can_show_signature_evolution_without_proving_identity() {
    let pair = Pair::new(
        "(module (func (export \"f\") (result i32) i32.const 1))",
        "(module (func (export \"f\") (result i64) i64.const 1))",
    );
    let r = pair.diff("1", false);
    assert_eq!(r["summary"]["functions_changed"], 1);
    assert_eq!(r["summary"]["functions_added"], 0);
    assert_eq!(r["summary"]["functions_removed"], 0);
    assert_eq!(r["function_matches"][0]["reason"], "same_export_role");
    assert_eq!(r["function_matches"][0]["ambiguous"], true);
    assert_eq!(r["function_matches"][0]["reference_context_complete"], true);
    assert!(r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["function"]["type_changed"] == true));
}
#[test]
fn compact_reports_keep_exact_edits_and_thread_counts_do_not_change_results() {
    let ops = (0..300)
        .map(|i| format!("i32.const {} drop ", i % 60))
        .collect::<String>();
    let pair = Pair::new(
        &format!("(module (func (export \"f\") {ops}))"),
        &format!("(module (func (export \"f\") nop {ops}))"),
    );
    let full = pair.diff("1", false);
    let compact = pair.diff("4", true);
    assert_eq!(compact["summary"]["operator_insertions"], 1);
    assert_eq!(full["audit"]["edits"], compact["audit"]["edits"]);
    assert!(compact["audit"]["old"].get("sections").is_none());
    assert!(compact["audit"].get("semantic_changes").is_none());
    assert_eq!(compact["audit"]["view"]["region_trees_omitted"], true);
    assert_eq!(compact, pair.diff("1", true));
    assert!(
        serde_json::to_vec(&compact).unwrap().len() * 5 < serde_json::to_vec(&full).unwrap().len()
    );
}
#[test]
fn local_declaration_grouping_is_encoding_evidence_not_a_type_change() {
    let pair = Pair::new(
        "(module (func (export \"f\") (local i32 i32)))",
        "(module (func (export \"f\") (local i32 i32)))",
    );
    let mut new = fs::read(&pair.new).unwrap();
    // Replace the one [2 x i32] locals group with two [1 x i32] groups,
    // increasing both the body and code payload lengths by two bytes.
    let at = new
        .windows(5)
        .position(|w| w == [4, 1, 2, 0x7f, 0x0b])
        .unwrap();
    new[at] = 6;
    new.splice(at + 1..at + 4, [2, 1, 0x7f, 1, 0x7f]);
    let code = new
        .iter()
        .enumerate()
        .find_map(|(i, b)| if *b == 10 && i > 8 { Some(i) } else { None })
        .unwrap();
    new[code + 1] += 2;
    fs::write(&pair.new, new).unwrap();
    let r = pair.diff("1", false);
    assert_eq!(r["binary_changed"], true);
    assert_eq!(r["summary"]["functions_changed"], 0);
}

// The full report is an independent oracle for every retained field, including
// validation failures, disabled features, opaque custom bytes and exact edits.
#[test]
fn compact_json_is_exactly_the_documented_projection_of_full_audit() {
    let cases = [
        ("(module (func (export \"f\") (param v128 v128) (result v128) local.get 0 local.get 1 i8x16.add_sat_s))",
         "(module (func (export \"f\") (param v128 v128) (result v128) local.get 0 local.get 1 i8x16.sub_sat_s))"),
        ("(module (memory 1 2 shared) (func i32.const 0 i32.atomic.load drop))",
         "(module (memory 1 2 shared) (func i32.const 0 i32.atomic.load offset=4 drop))"),
        ("(module (func i32.const 1))", "(module (func i32.const 2))"),
    ];
    for (a, b) in cases {
        let pair = Pair::new(a, b);
        // A noncanonical name convention remains opaque rather than refusing
        // core decoding. The payload's complete edit must survive compaction.
        for (path, byte) in [(&pair.old, 255), (&pair.new, 254)] {
            let mut bytes = fs::read(path).unwrap();
            bytes.extend([0, 7, 4, b'n', b'a', b'm', b'e', 1, byte]);
            fs::write(path, bytes).unwrap();
        }
        for standard in ["core1", "core2", "core3", "all-known"] {
            let mut expected = pair.diff_profile("1", false, standard);
            let audit = &mut expected["audit"];
            let positional = audit["semantic_changes"].as_array().unwrap().len();
            for side in ["old", "new"] {
                audit[side].as_object_mut().unwrap().remove("sections");
                let evidence = audit[side]["standard"]["evidence"].as_array_mut().unwrap();
                let before = evidence.len();
                evidence.retain(|entry| entry["status"] != "supported");
                let omitted = before - evidence.len();
                audit[side]["standard"]["supported_evidence_omitted"] = omitted.into();
            }
            audit.as_object_mut().unwrap().remove("semantic_changes");
            audit["view"] = serde_json::json!({"kind":"compact_lossless_edits",
                "region_trees_omitted":true,"positional_changes_omitted":positional});
            assert_eq!(
                pair.diff_profile("0", true, standard),
                expected,
                "{standard}"
            );
        }
    }
}
