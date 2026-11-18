#![cfg(all(feature = "client", feature = "server"))]
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use wasm_git::protocol::{
    CommandEnvelope, PolicyMigrationReceipt, PolicyMigrationRequest, PolicyQueryReply,
    PolicyQueryRequest, PolicyStatus, PolicyUpdateOutcome, PolicyUpdateReceipt,
    PolicyUpdateRequest, ServerPolicy, SERVER_POLICY_VERSION,
};

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request_file(root: &Path, name: &str, request: &impl serde::Serialize) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
    path
}
fn call(command: &str, root: &Path, request: Option<&Path>) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    child.args(["--format", "json", command]).arg(root);
    if let Some(request) = request {
        child.arg(request);
    }
    child.output().unwrap()
}
fn validate(request: &PolicyMigrationRequest, response: &[u8], accepted: bool) {
    let value = json!({"kind":"policy_migration_cli_reply", "request":request, "process_exit":0,
        "response":serde_json::from_slice::<serde_json::Value>(response).unwrap()});
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(["--format", "json", "integration-validate"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(
        result.status.success(),
        accepted,
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn actual_v1_native_ledger_migrates_without_forgetting_original_decisions() {
    // Produced by the pinned auth-fence-v1 server through two real policy-set calls.
    let legacy = include_bytes!("fixtures/policy-v1.bin");
    let source_sha256 = format!("{:x}", Sha256::digest(legacy));
    assert_eq!(
        source_sha256,
        "13e3efb311e04223a598be44b27ef725b9342f64480d15a1400c17c473cafcbe"
    );
    let temporary = Temp(
        std::env::temp_dir().join(format!("wasm-git-policy-migration-{}", std::process::id())),
    );
    fs::create_dir(&temporary.0).unwrap();
    let root = &temporary.0;
    let ledger = root.join(".wasm-git-authorization.bin");
    fs::write(&ledger, legacy).unwrap();
    assert!(
        !call("policy-status", root, None).status.success(),
        "Legacy data must not be silently reset"
    );
    let wrong = PolicyMigrationRequest {
        version: 1,
        source_sha256: "0".repeat(64),
    };
    let wrong_path = request_file(root, "wrong.json", &wrong);
    assert!(!call("policy-migrate", root, Some(&wrong_path))
        .status
        .success());
    assert_eq!(fs::read(&ledger).unwrap(), legacy);

    let request = PolicyMigrationRequest {
        version: 1,
        source_sha256,
    };
    let path = request_file(root, "migration.json", &request);
    let result = call("policy-migrate", root, Some(&path));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let envelope: CommandEnvelope<PolicyMigrationReceipt> =
        serde_json::from_slice(&result.stdout).unwrap();
    let receipt = envelope.validate_for("policy-migrate", 0).unwrap().unwrap();
    receipt.validate_request(&request).unwrap();
    assert_eq!(receipt.generation, 1);
    assert_eq!(receipt.receipts, 2);
    validate(&request, &result.stdout, true);
    validate(&wrong, &result.stdout, false);

    let policy = ServerPolicy {
        version: SERVER_POLICY_VERSION,
        principals: Vec::new(),
    };
    for (id, expected_generation, expected_outcome) in [
        ('a', 0, PolicyUpdateOutcome::Applied),
        ('b', 7, PolicyUpdateOutcome::GenerationConflict),
    ] {
        let original = PolicyUpdateRequest {
            version: 1,
            operation_id: id.to_string().repeat(64),
            expected_generation,
            policy: policy.clone(),
        };
        let query = PolicyQueryRequest {
            version: 1,
            operation_id: original.operation_id.clone(),
        };
        let query_path = request_file(root, "query.json", &query);
        let queried = call("policy-query", root, Some(&query_path));
        assert!(queried.status.success());
        let envelope: CommandEnvelope<PolicyQueryReply> =
            serde_json::from_slice(&queried.stdout).unwrap();
        let reply = envelope.validate_for("policy-query", 0).unwrap().unwrap();
        reply.validate_update(&original).unwrap();
        assert_eq!(reply.receipt.as_ref().unwrap().outcome, expected_outcome);
    }
    let next = PolicyUpdateRequest {
        version: 1,
        operation_id: "c".repeat(64),
        expected_generation: 1,
        policy,
    };
    let next_path = request_file(root, "next.json", &next);
    let committed = call("policy-set", root, Some(&next_path));
    assert!(committed.status.success());
    let envelope: CommandEnvelope<PolicyUpdateReceipt> =
        serde_json::from_slice(&committed.stdout).unwrap();
    let applied = envelope.validate_for("policy-set", 0).unwrap().unwrap();
    applied.validate_update(&next).unwrap();
    assert_eq!(applied.generation, 2);
    assert_eq!(applied.outcome, PolicyUpdateOutcome::Applied);
    // Retrying a migration after later policy changes reports the original migration.
    let replay = call("policy-migrate", root, Some(&path));
    assert!(replay.status.success());
    assert_eq!(replay.stdout, result.stdout);
    assert!(!call("policy-migrate", root, Some(&wrong_path))
        .status
        .success());
    let status = call("policy-status", root, None);
    let envelope: CommandEnvelope<PolicyStatus> = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        envelope
            .validate_for("policy-status", 0)
            .unwrap()
            .unwrap()
            .generation,
        2
    );
}
