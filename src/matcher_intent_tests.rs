//! Reduced regressions from actual Clang C++ workloads. The larger compile and
//! execution matrix is opt-in; these reproduce the matching failures without
//! requiring a native compiler in ordinary cargo test runs.
use crate::{diff, normalize, parse, resolve};
fn compare(a: &str, b: &str) -> diff::DiffReport {
    let module = |text| {
        normalize::normalize_module(&resolve::resolve_module(
            parse::parse_module(&wat::parse_str(text).unwrap()).unwrap(),
        ))
    };
    diff::diff_modules("old", &module(a), "new", &module(b))
}
#[test]
fn a_cpp_edit_that_equals_an_existing_body_keeps_the_changed_symbol_attached() {
    let old = "(module
        (func $domestic (param i32) (result i32) local.get 0 i32.const 7 i32.mul i32.const 3 i32.add)
        (func $foreign (param i32) (result i32) local.get 0 i32.const 11 i32.mul i32.const 3 i32.add))";
    let new = old.replace("i32.const 7", "i32.const 11");
    let report = compare(old, &new);
    assert_eq!(report.summary.functions_changed, 1);
    assert_eq!(report.summary.operator_replacements, 1);
    assert_eq!(report.function_matches.len(), 2);
    for pair in &report.function_matches {
        assert_eq!(pair.old_source_index, pair.new_source_index);
        // Two bodies coincide in the new version. Names select a candidate,
        // but cannot make that many-to-one history logically unique.
        assert!(pair.ambiguous);
        assert!(pair.symbol_hint.is_some());
    }
    let report = serde_json::to_value(report).unwrap();
    let changed = &report["changes"][0]["function"];
    assert!(changed["old_id"]
        .as_str()
        .unwrap()
        .contains("type:i32->i32"));
    assert_eq!(changed["operators"][0]["old"]["opcode"], "I32Const");
    assert_eq!(
        changed["operators"][0]["new"]["text"],
        "I32Const { value: 11 }"
    );
}
#[test]
fn name_removal_or_duplicate_names_do_not_make_colliding_bodies_certain() {
    for names in [false, true] {
        let old = "(module (func (result i32) i32.const 7) (func (result i32) i32.const 11))";
        let new = old.replace("const 7", "const 11");
        let mut a = wat::parse_str(old).unwrap();
        let mut b = wat::parse_str(&new).unwrap();
        if names {
            // Both functions have the same name: no global uniqueness.
            for bytes in [&mut a, &mut b] {
                bytes.extend([
                    0, 14, 4, b'n', b'a', b'm', b'e', 1, 7, 2, 0, 1, b'x', 1, 1, b'x',
                ]);
            }
        }
        let module = |bytes: &[u8]| {
            normalize::normalize_module(&resolve::resolve_module(
                parse::parse_module(bytes).unwrap(),
            ))
        };
        let report = diff::diff_modules("old", &module(&a), "new", &module(&b));
        assert_eq!(report.summary.functions_changed, 1);
        assert!(report
            .function_matches
            .iter()
            .all(|m| m.ambiguous && m.symbol_hint.is_none()));
    }
}

#[test]
fn compiler_index_drift_is_labelled_without_erasing_the_raw_edits() {
    let a = "(module (func $a (result i32) i32.const 7) (func $b (result i32) i32.const 11)
        (func (export \"entry\") (result i32) call $b))";
    let b = "(module (func $b (result i32) i32.const 11)
        (func (export \"entry\") (result i32) call $b))";
    let r = serde_json::to_value(compare(a, b)).unwrap();
    assert_eq!(r["summary"]["operator_replacements"], 1);
    let f = &r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "function_changed")
        .unwrap()["function"];
    assert_eq!(f["change_kind"], "confirmed_reference_relocation");
    assert_eq!(f["old_source_index"], 2);
    assert_eq!(f["new_source_index"], 1);
    assert_eq!(f["call_bindings"][0]["target_changed"], false);
    let b = b.replace("const 11", "const 13");
    let r = serde_json::to_value(compare(a, &b)).unwrap();
    let f = &r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["function"]["old_source_index"] == 2)
        .unwrap()["function"];
    assert_eq!(f["change_kind"], "unresolved_reference_change");
    assert!(f["call_bindings"][0]["target_changed"].is_null());
}

#[test]
fn deleting_a_callee_cannot_hide_retargeting_at_the_same_numeric_index() {
    let a =
        "(module (func $removed (result i32) i32.const 7) (func $kept (result i32) i32.const 11)
        (func (export \"entry\") (result i32) call $removed call $kept i32.add))";
    let b = "(module (func $kept (result i32) i32.const 11)
        (func (export \"entry\") (result i32) call $kept call $kept i32.add))";
    let r = serde_json::to_value(compare(a, b)).unwrap();
    let f = &r["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["function"]["old_source_index"] == 2)
        .unwrap()["function"];
    assert_eq!(f["change_kind"], "unresolved_reference_change");
    assert_eq!(f["call_bindings"].as_array().unwrap().len(), 2);
    let slot = &f["call_bindings"][0];
    assert_eq!(slot["old"]["function_index"], 0);
    assert_eq!(slot["new"]["function_index"], 0);
    assert_eq!(slot["relation"], "unresolved_target");
    assert!(slot["target_changed"].is_null());
    assert_eq!(f["call_bindings"][1]["target_changed"], false);
}
