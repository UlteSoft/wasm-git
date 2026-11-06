//! Historical wire bytes stay interpretable after upstream withdrawal.
#![cfg(feature = "client")]
use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};
use wasm_git::{audit, features, patch, standard::Standard};

// Fixed legacy encoding, independent of future WAT producer support:
// (module (tag) (func try i32.const 1 drop throw 0 catch 0 i32.const 2 drop end))
const LEGACY: &[u8] = b"\0asm\x01\0\0\0\x01\x04\x01\x60\0\0\x03\x02\x01\0\x0d\x03\x01\0\0\x0a\x11\x01\x0f\0\x06\x40\x41\x01\x1a\x08\0\x07\0\x41\x02\x1a\x0b\x0b";

fn run(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn catalog_reports_community_status_without_claiming_unimplemented_grammar() {
    let report: Value =
        serde_json::from_str(&run(&["features", "--json", "--standard", "core3"])).unwrap();
    let entries = report["features"].as_array().unwrap();
    for id in [
        "threads",
        "compact_imports",
        "wide_arithmetic",
        "legacy_exceptions",
    ] {
        let feature = entries.iter().find(|f| f["id"] == id).unwrap();
        assert_eq!(feature["retained"], true);
        assert_eq!(feature["default_recognition"], true);
        assert_eq!(feature["selected_standard_allows"], false);
    }
    let fp16 = entries.iter().find(|f| f["id"] == "fp16").unwrap();
    assert_eq!(fp16["coverage"], "unreviewed");
    assert_eq!(fp16["default_recognition"], false);
    assert!(run(&["features", "legacy_exceptions"]).contains("deprecated"));
    let alias: Value =
        serde_json::from_str(&run(&["features", "compact-import-section", "--json"])).unwrap();
    assert_eq!(alias["features"][0]["id"], "compact_imports");
    assert!(!Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(["features", "invented-feature"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(entries.len(), features::catalog(Standard::Core3).len());
}

#[test]
fn deprecated_legacy_bytes_keep_exact_diff_patch_and_profile_evidence() {
    let offset = LEGACY
        .windows(3)
        .position(|w| w == [0x41, 1, 0x1a])
        .unwrap()
        + 1;
    let mut new = LEGACY.to_vec();
    new[offset] = 3;
    let old_audit = audit::inspect(LEGACY).unwrap();
    assert_eq!(old_audit.standard.validation, "valid");
    assert_eq!(old_audit.standard.deprecated_features.len(), 1);
    let notice = &old_audit.standard.deprecated_features[0];
    assert_eq!(notice.feature, "legacy_exceptions");
    assert_eq!(notice.lifecycle, "deprecated");
    assert!(notice.decoder_retained);
    let legacy = old_audit
        .standard
        .evidence
        .iter()
        .filter(|e| e.feature == "legacy_exceptions")
        .collect::<Vec<_>>();
    assert!(!legacy.is_empty());
    assert!(legacy.iter().all(|e| e.status == "supported"));
    let diff = audit::compare(LEGACY, &new).unwrap();
    assert_eq!(diff.edits.len(), 1);
    let patch = patch::create(LEGACY, &new).unwrap();
    assert_eq!(patch.apply(LEGACY).unwrap(), new);
    assert_eq!(
        patch::BytePatch::from_bytes(&patch.to_bytes().unwrap())
            .unwrap()
            .apply(LEGACY)
            .unwrap(),
        new
    );
    let strict = audit::inspect_with_standard(LEGACY, Standard::Core3).unwrap();
    assert_eq!(strict.standard.validation, "invalid");
    assert_eq!(strict.standard.deprecated_features.len(), 1);
    assert!(strict
        .standard
        .evidence
        .iter()
        .any(|e| e.feature == "legacy_exceptions" && e.status == "recognized_but_disabled"));
}

#[test]
fn compact_and_section_reports_keep_deprecation_in_the_displayed_scope() {
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = Temp(PathBuf::from(format!(
        "/tmp/uwgit-feature-report-{}",
        std::process::id()
    )));
    fs::create_dir(&root.0).unwrap();
    let a = root.0.join("old.wasm");
    let b = root.0.join("new.wasm");
    let mut changed = LEGACY.to_vec();
    let offset = changed
        .windows(3)
        .position(|w| w == [0x41, 1, 0x1a])
        .unwrap()
        + 1;
    changed[offset] = 3;
    fs::write(&a, LEGACY).unwrap();
    fs::write(&b, changed).unwrap();
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
    for selector in [None, Some("code")] {
        let mut args = vec!["diff", a, b, "--format", "json", "--compact"];
        if let Some(selector) = selector {
            args.extend(["--section", selector]);
        }
        let report: Value = serde_json::from_str(&run(&args)).unwrap();
        let audit = if selector.is_some() {
            &report
        } else {
            &report["audit"]
        };
        for side in ["old", "new"] {
            assert_eq!(
                audit[side]["standard"]["deprecated_features"][0]["feature"], "legacy_exceptions",
                "{report}"
            );
            assert!(audit[side]["standard"]["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["feature"] == "legacy_exceptions"));
        }
    }
    let report: Value = serde_json::from_str(&run(&[
        "diff",
        a,
        b,
        "--section",
        "type",
        "--format",
        "json",
    ]))
    .unwrap();
    assert!(report["old"]["standard"]
        .get("deprecated_features")
        .is_none());
    for args in [
        vec!["inspect", a],
        vec!["diff", a, b],
        vec!["diff", a, b, "--short"],
        vec!["diff", a, b, "--compact"],
        vec!["diff", a, b, "--section", "code"],
    ] {
        assert!(run(&args).contains("deprecated"), "{args:?}");
    }
}
