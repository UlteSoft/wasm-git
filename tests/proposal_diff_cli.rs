#![cfg(feature = "client")]
use serde_json::Value;
use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use wasm_git::{audit, parse, patch, standard::Standard};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn compare(old: &str, new: &str, feature: &str, core3: bool) -> Value {
    let a = wat::parse_str(old).unwrap();
    let b = wat::parse_str(new).unwrap();
    for bytes in [&a, &b] {
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(bytes)
            .unwrap();
        let all = audit::inspect(bytes).unwrap();
        assert_eq!(all.standard.validation, "valid");
        assert!(all.standard.evidence.iter().any(|e| e.feature == feature));
        let core = audit::inspect_with_standard(bytes, Standard::Core3).unwrap();
        assert_eq!(core.standard.validation == "valid", core3);
        for f in &parse::parse_module(bytes).unwrap().functions {
            for op in &f.operators {
                assert!(
                    !matches!(op.opcode, wasm_git::ir::Opcode::Unrecognized(_)),
                    "{:?}",
                    op
                );
                assert!(
                    !matches!(op.immediate, wasm_git::ir::Immediate::Unrecognized(_)),
                    "{:?}",
                    op
                );
            }
        }
    }
    let root = std::env::temp_dir().join(format!(
        "uwgit-proposal-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a"), &a).unwrap();
    fs::write(root.join("b"), &b).unwrap();
    let run = |threads: &str| {
        let o = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(["--threads", threads, "diff"])
            .arg(root.join("a"))
            .arg(root.join("b"))
            .args(["--compact", "--format", "json"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice::<Value>(&o.stdout).unwrap()
    };
    let r = run("1");
    assert_eq!(r, run("0"));
    assert_eq!(r["summary"]["functions_changed"], 1);
    assert_eq!(r["summary"]["operator_replacements"], 1);
    assert_eq!(r["summary"]["operator_insertions"], 0);
    assert_eq!(r["summary"]["operator_deletions"], 0);
    assert_eq!(
        r["changes"][0]["function"]["instruction_alignment"]["complete"],
        true
    );
    let p = patch::create(&a, &b).unwrap().to_bytes().unwrap();
    assert_eq!(
        patch::BytePatch::from_bytes(&p).unwrap().apply(&a).unwrap(),
        b
    );
    fs::remove_dir_all(root).unwrap();
    r
}
#[test]
fn threads_keep_memory_index_offset_and_block_structure() {
    let a="(module (memory 1 2 shared) (memory 1 2 shared) (func (export \"f\") block i32.const 0 i32.atomic.load 1 offset=16 drop atomic.fence end))";
    let r = compare(a, &a.replace("offset=16", "offset=20"), "threads", false);
    let op = &r["changes"][0]["function"]["operators"][0];
    assert_eq!(op["old"]["opcode"], "I32AtomicLoad");
    assert!(op["new"]["text"].as_str().unwrap().contains("memory: 1"));
    assert!(op["new"]["text"].as_str().unwrap().contains("offset: 20"));
}
#[test]
fn wait_notify_and_compare_exchange_remain_local_instruction_edits() {
    for body in [
        "i32.const 0 i32.const 1 memory.atomic.notify offset=4 drop",
        "i32.const 0 i32.const 1 i64.const -1 memory.atomic.wait32 offset=4 drop",
        "i32.const 0 i64.const 1 i64.const -1 memory.atomic.wait64 offset=8 drop",
        "i32.const 0 i32.const 1 i32.const 2 i32.atomic.rmw.cmpxchg offset=4 drop",
    ] {
        let a = format!("(module (memory 1 2 shared) (func (export \"f\") block {body} end))");
        let b = a
            .replace("offset=4", "offset=12")
            .replace("offset=8", "offset=16");
        compare(&a, &b, "threads", false);
    }
}
#[test]
fn simd_arithmetic_lane_and_memory_updates_keep_block_alignment() {
    let a="(module (func (export \"f\") (param v128 v128) (result v128) block (result v128) local.get 0 local.get 1 i8x16.add_sat_s end))";
    compare(a, &a.replace("add_sat_s", "sub_sat_s"), "simd", true);
    let a="(module (func (export \"f\") (param v128) (result i32) local.get 0 i16x8.extract_lane_s 2))";
    compare(a, &a.replace("lane_s 2", "lane_s 3"), "simd", true);
    let a="(module (memory 1) (memory 1) (func (export \"f\") (param v128) (result v128) i32.const 0 local.get 0 v128.load8_lane 1 offset=16 1))";
    let r = compare(a, &a.replace("offset=16 1", "offset=16 2"), "simd", true);
    assert!(r["changes"][0]["function"]["operators"][0]["new"]["text"]
        .as_str()
        .unwrap()
        .contains("memory: 1"));
}
#[test]
fn core3_gc_array_types_and_nullable_casts_have_complete_typed_operands() {
    let a="(module (type $a (array (mut i32))) (type $b (array (mut i32))) (func (export \"f\") unreachable array.copy $a $a))";
    compare(
        a,
        &a.replace("array.copy $a $a", "array.copy $a $b"),
        "gc",
        true,
    );
    for op in ["ref.test", "ref.cast"] {
        let a = format!(
            "(module (func (export \"f\") (param anyref) local.get 0 {op} (ref null i31) drop))"
        );
        compare(&a, &a.replace("ref null i31", "ref null eq"), "gc", true);
    }
}
#[test]
fn shared_global_atomic_order_is_part_of_the_diff() {
    let a="(module (global (shared mut i32) (i32.const 0)) (func (export \"f\") global.atomic.get seq_cst 0 drop))";
    let r = compare(
        a,
        &a.replace("seq_cst", "acq_rel"),
        "shared_everything_threads",
        false,
    );
    let op = &r["changes"][0]["function"]["operators"][0];
    assert!(op["old"]["text"].as_str().unwrap().contains("SeqCst"));
    assert!(op["new"]["text"].as_str().unwrap().contains("AcqRel"));
}
#[test]
fn wide_arithmetic_and_memory_control_are_known_proposals_not_core3() {
    let a="(module (func (export \"f\") (param i64 i64) (result i64 i64) local.get 0 local.get 1 i64.mul_wide_s))";
    compare(
        a,
        &a.replace("mul_wide_s", "mul_wide_u"),
        "wide_arithmetic",
        false,
    );
    let a="(module (memory 1) (memory 1) (func (export \"f\") i32.const 0 i32.const 0 memory.discard 0))";
    compare(
        a,
        &a.replace("discard 0", "discard 1"),
        "memory_control",
        false,
    );
}
