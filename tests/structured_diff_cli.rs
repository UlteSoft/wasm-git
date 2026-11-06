#![cfg(feature = "client")]
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Pair {
    root: PathBuf,
    old: Vec<u8>,
    new: Vec<u8>,
}
impl Pair {
    fn new(old: &str, new: &str) -> Self {
        let old = wat::parse_str(old).unwrap();
        let new = wat::parse_str(new).unwrap();
        for bytes in [&old, &new] {
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
                .validate_all(bytes)
                .unwrap();
        }
        let root = std::env::temp_dir().join(format!(
            "uwgit-structured-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("old.wasm"), &old).unwrap();
        fs::write(root.join("new.wasm"), &new).unwrap();
        Self { root, old, new }
    }
    fn report(&self, threads: &str) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(["--threads", threads, "diff"])
            .arg(self.root.join("old.wasm"))
            .arg(self.root.join("new.wasm"))
            .args(["--format", "json", "--compact"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn verify(&self) -> Value {
        let report = self.report("1");
        assert_eq!(report, self.report("0"));
        // Physical replay remains independent of heuristic region matching.
        let patch = wasm_git::patch::create(&self.old, &self.new).unwrap();
        let binary = patch.to_bytes().unwrap();
        assert_eq!(
            wasm_git::patch::BytePatch::from_bytes(&binary)
                .unwrap()
                .apply(&self.old)
                .unwrap(),
            self.new
        );
        report
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn sibling_moves_report_regions_and_accurate_old_new_instruction_positions() {
    let pair = Pair::new(
        "(module (func (export \"f\") block i32.const 10 drop end block i64.const 20 drop end))",
        "(module (func (export \"f\") block i64.const 20 drop end block i32.const 11 drop end))",
    );
    let r = pair.verify();
    assert_eq!(r["summary"]["functions_changed"], 1);
    assert_eq!(r["summary"]["operator_replacements"], 1);
    assert_eq!(r["summary"]["operator_insertions"], 0);
    assert_eq!(r["summary"]["operator_deletions"], 0);
    assert_eq!(r["summary"]["control_region_moves"], 1);
    let f = &r["changes"][0]["function"];
    assert_eq!(f["operators"][0]["index"], 1);
    assert_eq!(f["operators"][0]["new_index"], 5);
    assert_eq!(f["instruction_alignment"]["complete"], true);
    assert_eq!(f["instruction_alignment"]["identity_unproven"], true);
}

#[test]
fn a_call_moved_into_else_is_not_silently_paired_with_the_old_then_site() {
    let pair = Pair::new(
        "(module (import \"m\" \"c\" (func)) (func (export \"f\") (param i32) local.get 0 if call 0 else end))",
        "(module (import \"m\" \"c\" (func)) (func (export \"f\") (param i32) local.get 0 if else call 0 end))",
    );
    let r = pair.verify();
    let f = &r["changes"][0]["function"];
    assert_eq!(r["summary"]["operator_deletions"], 1);
    assert_eq!(r["summary"]["operator_insertions"], 1);
    let blocks = f["instruction_alignment"]["basic_blocks"]
        .as_array()
        .unwrap();
    assert_eq!(blocks[0]["arm"], "then");
    assert_eq!(blocks[1]["arm"], "else");
    let notes = f["call_bindings"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0]["relation"], "site_removed");
    assert_eq!(notes[1]["relation"], "site_added");
    assert!(notes
        .iter()
        .all(|n| n["pairing"] == "wasm_region_instruction_alignment"));
}

#[test]
fn ordered_typed_neighborhoods_distinguish_unexported_functions_with_equal_histograms() {
    let pair = Pair::new(
        "(module (func i32.const 1 drop i32.const 2 drop i32.const 3 drop) (func i32.const 3 drop i32.const 2 drop i32.const 1 drop))",
        "(module (func i32.const 3 drop i32.const 4 drop i32.const 1 drop) (func i32.const 1 drop i32.const 4 drop i32.const 3 drop))",
    );
    let r = pair.verify();
    assert_eq!(r["summary"]["functions_changed"], 2);
    assert_eq!(r["summary"]["operator_replacements"], 2);
    let matches = r["function_matches"].as_array().unwrap();
    for m in matches {
        assert_eq!(
            m["old_source_index"].as_u64().unwrap() + m["new_source_index"].as_u64().unwrap(),
            1
        );
        assert_eq!(m["reason"], "similarity_fallback");
    }
}

#[test]
fn repeated_call_sites_with_changed_context_keep_positional_uncertainty() {
    let pair = Pair::new(
        "(module (import \"m\" \"a\" (func)) (import \"m\" \"b\" (func)) (func (export \"f\") call 0 call 0))",
        "(module (import \"m\" \"a\" (func)) (import \"m\" \"b\" (func)) (func (export \"f\") call 1 call 1 call 1))",
    );
    let r = pair.verify();
    let f = &r["changes"][0]["function"];
    let notes = f["call_bindings"].as_array().unwrap();
    assert_eq!(
        notes
            .iter()
            .filter(|n| n["relation"] == "ambiguous_site")
            .count(),
        2
    );
    assert!(notes.iter().all(|n| n["target_changed"].is_null()));
    assert_eq!(r["summary"]["operator_replacements"], 2);
    assert_eq!(r["summary"]["operator_insertions"], 1);
}
