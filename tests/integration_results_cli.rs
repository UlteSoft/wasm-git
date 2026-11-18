//! The integration consumes native machine results, not CLI prose.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use wasm_git::repository::Repository;

static NEXT: AtomicU64 = AtomicU64::new(0);
const WASM: &[u8] = b"\0asm\x01\0\0\0\0\x02\x01x";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "wasm-git-integration-results-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().into()
    }
    fn root(&self) -> &str {
        self.0.to_str().unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn invoke(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap()
}
fn success(args: &[&str]) -> String {
    let output = invoke(args);
    assert!(
        output.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn invoke_with_input(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
fn validate_request(input: &str) -> bool {
    invoke_with_input(&["--format", "json", "integration-validate"], input)
        .status
        .success()
}
fn validate(prefix: &str, raw_response: &str) -> bool {
    validate_request(&format!("{prefix},\"response\":{raw_response}}}"))
}

#[test]
fn actual_tree_sign_and_bundle_results_validate_and_match_published_bytes() {
    let t = Temp::new();
    success(&["init", t.root()]);
    fs::write(t.path("module.wasm"), WASM).unwrap();
    success(&["add", t.root(), "module.wasm"]);

    let commit_raw = success(&["--format", "json", "commit", t.root(), "-m", "initial"]);
    let commit_expected =
        r#"{"kind":"tree_commit_cli_reply","head":"main","previous":null,"process_exit":0"#;
    assert!(validate(commit_expected, &commit_raw));
    let commit: Value = serde_json::from_str(&commit_raw).unwrap();
    let commit_value = &commit["result"]["value"];
    assert_eq!(commit_value["version"], 1);
    let first = commit_value["commit"].as_str().unwrap();
    let repo = Repository::open(t.root()).unwrap();
    assert_eq!(repo.files_at(first).unwrap().files["module.wasm"], WASM);
    assert!(repo.read_commit(first).unwrap().commit.parents.is_empty());

    let key = t.path("author.key");
    let public = t.path("author.pub");
    success(&["keygen", &key, &public]);
    let sign_raw = success(&[
        "--format",
        "json",
        "sign-commit",
        t.root(),
        first,
        "--author",
        "Alice",
        "--key",
        &key,
        "--timestamp",
        "42",
    ]);
    let sign_expected = format!(
        r#"{{"kind":"sign_commit_cli_reply","head":"main","previous":"{first}","author":"Alice","process_exit":0"#
    );
    assert!(validate(&sign_expected, &sign_raw));
    let signed: Value = serde_json::from_str(&sign_raw).unwrap();
    let signed_id = signed["result"]["value"]["commit"].as_str().unwrap();
    assert_ne!(first, signed_id);
    assert_eq!(
        repo.read_commit(signed_id)
            .unwrap()
            .commit
            .signature
            .unwrap()
            .author,
        "Alice"
    );
    assert_eq!(repo.files_at(signed_id).unwrap().files["module.wasm"], WASM);

    let bundle = t.path("out.bundle");
    let bundle_raw = success(&["--format", "json", "bundle", t.root(), &bundle]);
    let bundle_expected = format!(
        r#"{{"kind":"bundle_cli_reply","head":"main","tip":"{signed_id}","process_exit":0"#
    );
    assert!(validate(&bundle_expected, &bundle_raw));
    let result: Value = serde_json::from_str(&bundle_raw).unwrap();
    let result = &result["result"]["value"];
    let actual_bytes = fs::read(&bundle).unwrap();
    assert_eq!(result["bytes"].as_u64().unwrap(), actual_bytes.len() as u64);
    assert_eq!(
        result["objects"].as_u64().unwrap(),
        repo.fsck().unwrap().objects as u64
    );
    assert_eq!(
        result["sha256"],
        format!("{:x}", Sha256::digest(&actual_bytes))
    );
    assert_eq!(
        Repository::bundle_state(&actual_bytes)
            .unwrap()
            .tip()
            .unwrap(),
        signed_id
    );
    let binding_request =
        r#"{"version":1,"operation":{"Push":{"branch":"main","expected":null,"force":false}}}"#;
    let binding_output = invoke_with_input(
        &["--format", "json", "upload-binding", &bundle],
        binding_request,
    );
    assert!(binding_output.status.success());
    let binding_raw = std::str::from_utf8(&binding_output.stdout).unwrap();
    let binding_expected = format!(
        r#"{{"kind":"upload_binding_cli_reply","request":{binding_request},"process_exit":0"#
    );
    assert!(validate(&binding_expected, binding_raw));
    let binding: Value = serde_json::from_str(binding_raw).unwrap();
    let mut pair = json!({"kind":"bundle_upload_binding", "bundle":result, "binding":binding["result"]["value"]});
    assert!(validate_request(&pair.to_string()));
    pair["binding"]["digest"] = json!("0".repeat(64));
    assert!(!validate_request(&pair.to_string()));

    assert!(!validate(
        &commit_expected.replace("main", "other"),
        &commit_raw
    ));
    assert!(!validate(&sign_expected.replace("Alice", "Bob"), &sign_raw));
    assert!(!validate(
        &bundle_expected.replace(signed_id, first),
        &bundle_raw
    ));
    let mut invalid_commit = commit.clone();
    invalid_commit["result"]["value"]["commit"] = json!("not-an-oid");
    assert!(!validate(commit_expected, &invalid_commit.to_string()));
    let mut invalid_bundle: Value = serde_json::from_str(&bundle_raw).unwrap();
    invalid_bundle["result"]["value"]["sha256"] = json!("invalid");
    assert!(!validate(&bundle_expected, &invalid_bundle.to_string()));
    invalid_bundle["result"]["value"]
        .as_object_mut()
        .unwrap()
        .remove("tip");
    assert!(!validate(&bundle_expected, &invalid_bundle.to_string()));
    assert!(!validate(
        &bundle_expected.replace("\"process_exit\":0", "\"process_exit\":1"),
        &bundle_raw
    ));

    let failed = invoke(&[
        "--format",
        "json",
        "sign-commit",
        t.root(),
        first,
        "--author",
        "Alice",
        "--key",
        &key,
        "--timestamp",
        "42",
    ]);
    assert!(!failed.status.success());
    assert!(validate(
        &sign_expected.replace("\"process_exit\":0", "\"process_exit\":1"),
        std::str::from_utf8(&failed.stdout).unwrap()
    ));
    let plain_bundle = t.path("plain.bundle");
    assert!(success(&["bundle", t.root(), &plain_bundle]).starts_with("wrote "));
    let plain_sign = success(&[
        "sign-commit",
        t.root(),
        signed_id,
        "--author",
        "Alice",
        "--key",
        &key,
        "--timestamp",
        "43",
    ]);
    assert_eq!(plain_sign.trim().len(), 64);
}

#[test]
fn empty_and_detached_snapshots_keep_native_head_identity() {
    let t = Temp::new();
    success(&["init", t.root()]);
    let empty_bundle = t.path("empty.bundle");
    let empty = success(&["--format", "json", "bundle", t.root(), &empty_bundle]);
    assert!(validate(
        r#"{"kind":"bundle_cli_reply","head":"main","tip":null,"process_exit":0"#,
        &empty
    ));
    fs::write(t.path("module.wasm"), WASM).unwrap();
    success(&["add", t.root(), "module.wasm"]);
    // The existing text output remains available for native callers.
    let plain = success(&["commit", t.root(), "-m", "initial"]);
    assert!(plain.starts_with("main "));
    let first = plain.split_whitespace().nth(1).unwrap();
    success(&["checkout", t.root(), "--detach", first]);
    fs::write(t.path("module.wasm"), b"\0asm\x01\0\0\0").unwrap();
    success(&["add", t.root(), "module.wasm"]);
    let raw = success(&["--format", "json", "commit", t.root(), "-m", "detached"]);
    let expected = format!(
        r#"{{"kind":"tree_commit_cli_reply","head":"{first}","previous":"{first}","process_exit":0"#
    );
    assert!(validate(&expected, &raw));
    let record: Value = serde_json::from_str(&raw).unwrap();
    let next = record["result"]["value"]["commit"].as_str().unwrap();
    let detached_bundle = t.path("detached.bundle");
    let raw_bundle = success(&["--format", "json", "bundle", t.root(), &detached_bundle]);
    let expected_bundle =
        format!(r#"{{"kind":"bundle_cli_reply","head":"{next}","tip":"{next}","process_exit":0"#);
    assert!(validate(&expected_bundle, &raw_bundle));
}
