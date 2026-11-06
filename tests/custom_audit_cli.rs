#![cfg(feature = "client")]
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use wasm_git::{audit, patch};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn uleb(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 127) as u8;
        n >>= 7;
        out.push(b | if n != 0 { 128 } else { 0 });
        if n == 0 {
            return out;
        }
    }
}
fn string(s: &str) -> Vec<u8> {
    [uleb(s.len()), s.as_bytes().to_vec()].concat()
}
fn custom(name: &str, data: &[u8]) -> Vec<u8> {
    let payload = [string(name), data.to_vec()].concat();
    [vec![0], uleb(payload.len()), payload].concat()
}
fn module(parts: &[Vec<u8>]) -> Vec<u8> {
    [vec![b"\0asm\x01\0\0\0".to_vec()], parts.to_vec()]
        .concat()
        .concat()
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn compare(a: &[u8], b: &[u8]) -> audit::AuditDiff {
    let patch = patch::create(a, b).unwrap().to_bytes().unwrap();
    assert_eq!(
        patch::BytePatch::from_bytes(&patch)
            .unwrap()
            .apply(a)
            .unwrap(),
        b
    );
    audit::compare(a, b).unwrap()
}
fn report(a: &[u8], b: &[u8]) -> Value {
    let dir = std::env::temp_dir().join(format!(
        "uwgit-custom-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("a"), a).unwrap();
    fs::write(dir.join("b"), b).unwrap();
    let run = |threads: &str, format: &str| {
        let o = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(["--threads", threads, "diff"])
            .arg(dir.join("a"))
            .arg(dir.join("b"))
            .args(["--format", format, "--compact"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o.stdout
    };
    let json: Value = serde_json::from_slice(&run("1", "json")).unwrap();
    assert_eq!(
        json,
        serde_json::from_slice::<Value>(&run("0", "json")).unwrap()
    );
    let text = String::from_utf8(run("0", "text")).unwrap();
    if !json["audit"]["custom_changes"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(text.contains("Custom "));
    }
    fs::remove_dir_all(dir).unwrap();
    json
}
#[test]
fn inserting_an_unknown_section_does_not_fabricate_name_content_changes() {
    let name = custom("name", &[1, 5, 1, 0, 2, b'f', b'n']);
    let a = module(std::slice::from_ref(&name));
    let b = module(&[custom("arbitrary\0\n", &[255, 0, 128]), name]);
    let d = compare(&a, &b);
    let c = d.custom_changes.iter().find(|c| c.name == "name").unwrap();
    assert!(!c.content_changed);
    assert!(!c.data_changed);
    assert!(c.position_changed);
    assert!(c.decoded_changes.is_empty());
    assert_eq!(c.pairing, "equal_payload_bytes");
    let r = report(&a, &b);
    assert_eq!(r["audit"]["custom_changes"].as_array().unwrap().len(), 2);
    assert!(!d.custom_summary().contains("arbitrary\0\n")); // control characters are escaped
}
#[test]
fn all_custom_payload_convention_failures_remain_valid_core_and_replay_exactly() {
    for name in [
        "name",
        "producers",
        "target_features",
        "sourceMappingURL",
        "external_debug_info",
        "dylink.0",
        "linking",
        "metadata.code.branch_hint",
        "wasm-cosign",
        "uwvm.wsea.v2",
        ".debug_info",
        "wasmedge",
        "future/community",
    ] {
        let a = module(&[custom(name, &[255, 128, 0, 254])]);
        let b = module(&[custom(name, &[255, 128, 0, 253])]);
        for bytes in [&a, &b] {
            wasmparser::Validator::new().validate_all(bytes).unwrap();
        }
        let d = compare(&a, &b);
        assert_eq!(d.old.sections[0].status, "opaque", "{name}");
        assert!(
            d.old.diagnostics.iter().all(|d| d.severity == "note"),
            "{name}"
        );
        assert!(d.custom_changes[0].data_changed);
        assert_eq!(
            d.custom_changes[0].old.as_ref().unwrap().data_sha256,
            hash(&[255, 128, 0, 254])
        );
    }
}
#[test]
fn duplicate_custom_names_use_equal_bytes_and_keep_uncertainty() {
    let a = module(&[custom("x", b"first"), custom("x", b"second")]);
    let b = module(&[custom("x", b"second"), custom("x", b"third")]);
    let d = compare(&a, &b);
    assert_eq!(d.custom_changes.len(), 2);
    let retained = d.custom_changes.iter().find(|c| !c.data_changed).unwrap();
    assert_eq!(retained.old.as_ref().unwrap().ordinal, 1);
    assert_eq!(retained.new.as_ref().unwrap().ordinal, 0);
    assert!(retained.ambiguous);
    assert!(d.custom_changes.iter().all(|c| c.ambiguous));
    let c = module(&[custom("x", b"third"), custom("x", b"fourth")]);
    let ambiguous = compare(&a, &c);
    assert_eq!(ambiguous.custom_changes.len(), 4);
    assert!(ambiguous
        .custom_changes
        .iter()
        .all(|c| c.pairing == "unpaired" && c.ambiguous));
}
#[test]
fn overlong_outer_encoding_is_a_content_change_without_payload_change() {
    let a = module(&[custom("x", b"a")]);
    let b = module(&[vec![0, 0x83, 0, 1, b'x', b'a']]);
    let d = compare(&a, &b);
    let c = &d.custom_changes[0];
    assert!(c.content_changed);
    assert!(!c.data_changed);
    assert_eq!(c.pairing, "equal_payload_bytes");
}
#[test]
fn source_locations_and_branch_hints_have_precise_field_evidence() {
    let a = module(&[
        custom("sourceMappingURL", &string("a.map")),
        custom("metadata.code.branch_hint", &[1, 0, 1, 7, 1, 0]),
    ]);
    let b = module(&[
        custom("sourceMappingURL", &string("b.map")),
        custom("metadata.code.branch_hint", &[1, 0, 1, 7, 1, 1]),
    ]);
    let d = compare(&a, &b);
    assert!(d
        .custom_changes
        .iter()
        .all(|c| c.old.as_ref().unwrap().interpretation == "decoded"));
    let hint = d
        .custom_changes
        .iter()
        .find(|c| c.name == "metadata.code.branch_hint")
        .unwrap();
    assert!(hint.decoded_changes[0]
        .new
        .as_ref()
        .unwrap()
        .description
        .contains("body_offset=7 taken=1"));
    let r = report(&a, &b);
    assert_eq!(r["summary"]["functions_changed"], 0);
}
#[test]
fn dylink_fields_and_unknown_subsections_are_audited_without_loading_them() {
    let a = module(&[custom("dylink.0", &[1, 4, 1, 0, 2, 0, 99, 2, 255, 0])]);
    let b = module(&[custom("dylink.0", &[1, 4, 2, 0, 2, 0, 99, 2, 255, 1])]);
    let d = compare(&a, &b);
    let c = &d.custom_changes[0];
    assert_eq!(c.old.as_ref().unwrap().interpretation, "decoded");
    assert_eq!(c.decoded_changes.len(), 2);
    assert!(c
        .decoded_changes
        .iter()
        .any(|c| c.path.ends_with("memory_size")));
    assert!(c.decoded_changes.iter().any(|c| c.path.ends_with("opaque")));
    let link = module(&[custom("linking", &[2, 99, 1, 255])]);
    assert_eq!(audit::inspect(&link).unwrap().sections[0].status, "decoded");
}
const WSEA: &[u8] = &[0x84, 1, 0x41, 0xa0, 0x41, 0xb0, 0x41, 0xc0];
#[test]
fn wsea_contribution_changes_are_separate_from_claimed_input_changes() {
    let a = module(&[custom("uwvm.wsea.v2", WSEA)]);
    let mut other = WSEA.to_vec();
    other[3] ^= 1;
    let b = module(&[custom("uwvm.wsea.v2", &other)]);
    let d = compare(&a, &b);
    let c = &d.custom_changes[0];
    assert_eq!(c.decoded_changes.len(), 1);
    assert!(c.decoded_changes[0].path.ends_with("/cose"));
    let scope = c.signature_input.as_ref().unwrap();
    assert_eq!(scope.changed, Some(false));
    assert_eq!(scope.verification, "not_performed");
    assert_eq!(scope.old_sha256, Some(hash(&module(&[]))));
    let c = module(&[custom("metadata", b"one"), custom("uwvm.wsea.v2", WSEA)]);
    let e = module(&[custom("metadata", b"two"), custom("uwvm.wsea.v2", WSEA)]);
    let d = compare(&c, &e);
    let sig = d
        .custom_changes
        .iter()
        .find(|c| c.name == "uwvm.wsea.v2")
        .unwrap();
    assert!(!sig.content_changed);
    assert!(!sig.position_changed);
    assert_eq!(sig.signature_input.as_ref().unwrap().changed, Some(true));
    assert!(d.custom_summary().contains("verification=not_performed"));
    report(&c, &e);
}
#[test]
fn wsea_nonfinal_duplicate_and_noncanonical_containers_have_unresolved_scope() {
    for b in [
        module(&[custom("uwvm.wsea.v2", WSEA), custom("x", b"data")]),
        module(&[custom("uwvm.wsea.v2", WSEA), custom("uwvm.wsea.v2", WSEA)]),
    ] {
        let d = compare(&module(&[]), &b);
        assert!(d
            .custom_changes
            .iter()
            .filter(|c| c.name == "uwvm.wsea.v2")
            .all(|c| c.signature_input.as_ref().unwrap().changed.is_none()));
    }
    let mut section = custom("uwvm.wsea.v2", WSEA);
    section[1] |= 128;
    section.insert(2, 0);
    let d = compare(&module(&[]), &module(&[section]));
    assert!(d.custom_changes[0]
        .signature_input
        .as_ref()
        .unwrap()
        .new_sha256
        .is_none());
}
#[test]
fn wsea_multiple_contributions_and_malformed_future_versions_remain_lossless() {
    let payload = [
        vec![0x82, 2, 0x82, 0x83],
        WSEA[2..].to_vec(),
        vec![0x83],
        WSEA[2..].to_vec(),
    ]
    .concat();
    let a = module(&[custom("uwvm.wsea.v2", &payload)]);
    assert_eq!(audit::inspect(&a).unwrap().sections[0].status, "decoded");
    let b = module(&[custom("uwvm.wsea.v2", &[0x82, 3, 0xff])]);
    let d = compare(&a, &b);
    assert_eq!(d.new.sections[0].status, "opaque");
    for end in 0..payload.len() {
        let bytes = module(&[custom("uwvm.wsea.v2", &payload[..end])]);
        assert_eq!(audit::inspect(&bytes).unwrap().sections[0].status, "opaque");
    }
}
#[test]
fn cosign_scope_excludes_all_named_containers_but_includes_other_custom_bytes() {
    let envelope=br#"{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","verificationMaterial":{},"messageSignature":{}}"#;
    let a = module(&[custom("wasm-cosign", envelope), custom("x", b"a")]);
    let b = module(&[
        custom("wasm-cosign", b"other-community-format"),
        custom("x", b"a"),
        custom("wasm-cosign", envelope),
    ]);
    let d = compare(&a, &b);
    assert!(d
        .custom_changes
        .iter()
        .filter(|c| c.name == "wasm-cosign")
        .all(|c| c.signature_input.as_ref().unwrap().changed == Some(false)));
    let expected = hash(&module(&[custom("x", b"a")]));
    assert!(d
        .custom_changes
        .iter()
        .filter(|c| c.name == "wasm-cosign")
        .all(|c| c.signature_input.as_ref().unwrap().old_sha256 == Some(expected.clone())));
    let c = module(&[custom("wasm-cosign", envelope), custom("x", b"b")]);
    let d = compare(&a, &c);
    let sig = d
        .custom_changes
        .iter()
        .find(|c| c.name == "wasm-cosign")
        .unwrap();
    assert!(!sig.content_changed);
    assert_eq!(sig.signature_input.as_ref().unwrap().changed, Some(true));
}
fn named_pair() -> (Vec<u8>, Vec<u8>) {
    (wat::parse_str("(module (func $alpha (result i32) i32.const 10) (func $beta (result i32) i32.const 20))").unwrap(),
     wat::parse_str("(module (func $beta (result i32) i32.const 21) (func $alpha (result i32) i32.const 11))").unwrap())
}
#[test]
fn unique_equal_symbols_improve_reordered_edited_function_pairing() {
    let (a, b) = named_pair();
    let r = report(&a, &b);
    let pairs = r["function_matches"].as_array().unwrap();
    assert_eq!(pairs.len(), 2);
    for p in pairs {
        assert_eq!(
            p["old_source_index"].as_u64().unwrap() + p["new_source_index"].as_u64().unwrap(),
            1
        );
        assert!(p["symbol_hint"].is_string());
        assert_eq!(p["reason"], "similarity_fallback");
        assert_eq!(p["ambiguous"], true);
    }
    assert_eq!(r["summary"]["operator_replacements"], 2);
}
#[test]
fn names_never_override_exact_body_evidence_or_admit_incompatible_types() {
    let a = wat::parse_str(
        "(module (func $alpha (result i32) i32.const 10) (func $beta (result i32) i32.const 20))",
    )
    .unwrap();
    let b = wat::parse_str(
        "(module (func $alpha (result i32) i32.const 20) (func $beta (result i32) i32.const 10))",
    )
    .unwrap();
    let r = report(&a, &b);
    assert_eq!(r["summary"]["functions_changed"], 0);
    assert!(r["function_matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["symbol_hint"].is_null()));
    let c = wat::parse_str("(module (func $alpha (result i64) i64.const 30))").unwrap();
    let r = report(&a, &c);
    assert!(r["function_matches"].as_array().unwrap().is_empty());
}
#[test]
fn duplicate_name_sections_or_indices_revoke_advice_without_rejecting_module() {
    let (a, b) = named_pair();
    for malformed in [
        vec![1, 7, 2, 0, 1, b'x', 0, 1, b'y'],
        vec![1, 3, 1, 0, 255],
        vec![],
    ] {
        let mut new = b.clone();
        new.extend(custom("name", &malformed));
        let r = report(&a, &new);
        assert_eq!(r["function_analysis"]["available"], true);
        assert!(r["function_matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["symbol_hint"].is_null()));
    }
}
#[test]
fn duplicate_symbols_do_not_receive_a_weight_bonus() {
    let a = wat::parse_str(
        "(module (func (result i32) i32.const 10) (func (result i32) i32.const 20))",
    )
    .unwrap();
    let b = wat::parse_str(
        "(module (func (result i32) i32.const 21) (func (result i32) i32.const 11))",
    )
    .unwrap();
    let mut names = vec![2];
    for i in 0..2 {
        names.extend(uleb(i));
        names.extend(string("same"));
    }
    let sub = [vec![1], uleb(names.len()), names].concat();
    let a = [a, custom("name", &sub)].concat();
    let b = [b, custom("name", &sub)].concat();
    let r = report(&a, &b);
    assert!(r["function_matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["symbol_hint"].is_null()));
}
#[test]
fn malformed_optional_formats_survive_repository_gc_and_restore() {
    let dir = std::env::temp_dir().join(format!("uwgit-custom-repo-{}", std::process::id()));
    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o.stdout
    };
    let r = dir.to_str().unwrap();
    run(&["init", r]);
    let a = module(&[
        custom("uwvm.wsea.v2", &[255, 0]),
        custom("name", &[128, 255]),
        custom("wasmedge", &[254, 0, 255]),
    ]);
    fs::write(dir.join("a.wasm"), &a).unwrap();
    run(&["add", r, "a.wasm"]);
    run(&["commit", r, "-m", "opaque custom payloads"]);
    let mut b = a.clone();
    *b.last_mut().unwrap() ^= 1;
    fs::write(dir.join("a.wasm"), &b).unwrap();
    run(&["add", r, "a.wasm"]);
    run(&["commit", r, "-m", "opaque update"]);
    run(&["gc", r]);
    run(&["fsck", r]);
    run(&["restore", r, "--source", "HEAD~1", "a.wasm"]);
    assert_eq!(fs::read(dir.join("a.wasm")).unwrap(), a);
    fs::remove_dir_all(dir).unwrap();
}

fn function_name_section(entries: &[(usize, &str)], future: bool) -> Vec<u8> {
    let mut payload = uleb(entries.len());
    for (i, name) in entries {
        payload.extend(uleb(*i));
        payload.extend(string(name));
    }
    let mut sub = [vec![1], uleb(payload.len()), payload].concat();
    if future {
        sub.extend([123, 2, 255, 0]);
    }
    custom("name", &sub)
}
#[test]
fn future_framed_name_subsections_do_not_discard_valid_function_advice() {
    let a = wat::parse_str(
        "(module (func (result i32) i32.const 10) (func (result i32) i32.const 20))",
    )
    .unwrap();
    let b = wat::parse_str(
        "(module (func (result i32) i32.const 21) (func (result i32) i32.const 11))",
    )
    .unwrap();
    let a = [a, function_name_section(&[(0, "alpha"), (1, "beta")], true)].concat();
    let b = [b, function_name_section(&[(0, "beta"), (1, "alpha")], true)].concat();
    let r = report(&a, &b);
    assert!(r["function_matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["symbol_hint"].is_string()));
    assert_eq!(
        audit::inspect(&a).unwrap().sections.last().unwrap().status,
        "opaque"
    );
}
#[test]
fn a_single_name_section_with_repeated_indices_is_opaque_and_gives_no_advice() {
    let a = wat::parse_str(
        "(module (func (result i32) i32.const 10) (func (result i32) i32.const 20))",
    )
    .unwrap();
    let b = wat::parse_str(
        "(module (func (result i32) i32.const 21) (func (result i32) i32.const 11))",
    )
    .unwrap();
    let a = [
        a,
        function_name_section(&[(0, "alpha"), (1, "beta")], false),
    ]
    .concat();
    let b = [
        b,
        function_name_section(&[(0, "beta"), (0, "alpha")], false),
    ]
    .concat();
    let r = report(&a, &b);
    assert!(r["function_matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["symbol_hint"].is_null()));
    assert_eq!(
        audit::inspect(&b).unwrap().sections.last().unwrap().status,
        "opaque"
    );
}
#[test]
fn future_core_grammar_does_not_prevent_custom_update_auditing() {
    let a = module(&[
        vec![1, 4, 1, 0x60, 0, 0],
        vec![3, 2, 1, 0],
        vec![10, 5, 1, 3, 0, 0xff, 0x0b],
        custom("future/community", &[255, 0]),
    ]);
    let mut b = a.clone();
    *b.last_mut().unwrap() = 1;
    let d = compare(&a, &b);
    assert_eq!(d.custom_changes.len(), 1);
    assert_eq!(d.custom_changes[0].name, "future/community");
    let r = report(&a, &b);
    assert_eq!(r["function_analysis"]["available"], false);
    assert_eq!(r["audit"]["custom_changes"][0]["data_changed"], true);
}
#[test]
fn arbitrary_custom_bytes_never_panic_or_become_invalid_core_under_interpretation() {
    let names = [
        "name",
        "producers",
        "target_features",
        "dylink.0",
        "linking",
        "metadata.code.branch_hint",
        "uwvm.wsea.v2",
        "wasm-cosign",
        "sourceMappingURL",
    ];
    let mut seed = 0x2f4a7c15u32;
    for name in names {
        for len in 0..64 {
            let mut data = Vec::new();
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                data.push(seed as u8);
            }
            let m = module(&[custom(name, &data)]);
            let audit = audit::inspect(&m).unwrap();
            assert!(
                audit.diagnostics.iter().all(|d| d.severity == "note"),
                "{name}: {:?}",
                audit.diagnostics
            );
            assert_ne!(audit.sections[0].status, "malformed");
            wasmparser::Validator::new().validate_all(&m).unwrap();
        }
    }
}

#[test]
fn symbol_only_updates_leave_function_and_dependency_evidence_unchanged() {
    let core = wat::parse_str(
        "(module (func (result i32) i32.const 10) (func (export \"entry\") (result i32) call 0))",
    )
    .unwrap();
    let a = [core.clone(), function_name_section(&[(0, "before")], false)].concat();
    let b = [core, function_name_section(&[(0, "after")], false)].concat();
    let r = report(&a, &b);
    assert_eq!(r["summary"]["functions_changed"], 0);
    assert!(r["call_binding_notes"].as_array().unwrap().is_empty());
    assert!(r["export_dependency_graph"]["nodes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(r["audit"]["custom_changes"][0]["name"], "name");
    assert_eq!(r["audit"]["custom_changes"][0]["data_changed"], true);
}

#[test]
fn custom_container_fields_are_reported_even_when_children_do_not_change() {
    // needed (2) -> runtime paths (5), both containing the same string.
    let payload = [vec![1], string("lib")].concat();
    let a = module(&[custom(
        "dylink.0",
        &[vec![2], uleb(payload.len()), payload.clone()].concat(),
    )]);
    let b = module(&[custom(
        "dylink.0",
        &[vec![5], uleb(payload.len()), payload].concat(),
    )]);
    let d = compare(&a, &b);
    let c = &d.custom_changes[0];
    assert_eq!(c.decoded_changes.len(), 1);
    assert_eq!(
        c.decoded_changes[0].old.as_ref().unwrap().description,
        "subsection_id=2"
    );
    assert_eq!(
        c.decoded_changes[0].new.as_ref().unwrap().description,
        "subsection_id=5"
    );
}
