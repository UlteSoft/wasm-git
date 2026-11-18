#![cfg(all(feature = "client", feature = "server", unix))]
//! Real CLI and HTTP coverage for immutable, request-bound credentials.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{atomic::{AtomicU64, Ordering}, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wasm_git::{
    binary,
    protocol::{
        self, CommandEnvelope, Operation, OperationCredentialQuery,
        OperationCredentialQueryReply, OperationCredentialReceipt, OperationCredentialRequest,
        RefNamespace, Request, Response, RetentionCommand, RetentionReply, ScopedUploadState,
        ScopedUploadStatus, UploadBinding,
    },
    repository::Repository,
};

const WASM: &[u8] = b"\0asm\x01\0\0\0";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("wasm-git-operation-credentials-{}-{}",
            std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn id(value: u64) -> String { format!("{value:064x}") }
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }

fn wait_until(expiry: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while now() < expiry {
        assert!(Instant::now() < deadline, "clock did not reach credential expiry");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn bounded_output(mut command: Command, input: Option<&[u8]>) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        if let Err(error) = child.stdin.take().unwrap().write_all(input) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("write command input: {error}");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().unwrap(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("command did not finish ({result:?}): {}", String::from_utf8_lossy(&output.stderr));
            }
        }
    }
}

fn command(root: &Path, name: &str, input: &impl serde::Serialize) -> Output {
    let path = root.join(format!("request-{}.json", NEXT.fetch_add(1, Ordering::Relaxed)));
    fs::write(&path, serde_json::to_vec(input).unwrap()).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    process.args(["--format", "json", name]).arg(root).arg(path);
    bounded_output(process, None)
}

fn success(output: Output) -> Value {
    assert!(output.status.success(), "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn validate(value: Value, expected: bool) {
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
    process.args(["--format", "json", "integration-validate"]);
    let result = bounded_output(process, Some(&serde_json::to_vec(&value).unwrap()));
    assert_eq!(result.status.success(), expected, "{}", String::from_utf8_lossy(&result.stdout));
}

fn policy(alice_exists: bool, alice_allowed: bool) -> Value {
    let grant = json!({"repository":"*", "read":true, "write":["refs/heads/*"],
        "force":false, "create":false, "release_retention":true});
    let mut principals = vec![json!({"name":"bob", "token_sha256":hash(b"bob-legacy"), "grants":[grant.clone()]})];
    if alice_exists {
        principals.push(json!({"name":"alice", "token_sha256":hash(b"alice-legacy"),
            "grants":if alice_allowed { vec![grant] } else { vec![] }}));
    }
    json!({"version":3, "principals":principals})
}

fn set_policy(root: &Path, generation: u64, value: Value) {
    let result = success(command(root, "policy-set", &json!({"version":1,
        "operation_id":id(1000 + generation), "expected_generation":generation, "policy":value})));
    assert_eq!(result["result"]["value"]["outcome"], "applied");
}

fn request(repository: &str, operation: Operation) -> Request {
    Request { version: 1, repository: repository.into(), operation }
}

fn issuance(number: u64, actor: &str, token: &str, request: Request,
    payload: &[u8], expiry: u64) -> OperationCredentialRequest {
    OperationCredentialRequest { version: 1, operation_id: id(number), actor: actor.into(),
        token_sha256: hash(token.as_bytes()), request, payload_sha256: hash(payload),
        expires_at: expiry, policy_generation: 1 }
}

fn issue(root: &Path, request: &OperationCredentialRequest) -> OperationCredentialReceipt {
    let result = success(command(root, "operation-credential-issue", request));
    let envelope: CommandEnvelope<OperationCredentialReceipt> = serde_json::from_value(result.clone()).unwrap();
    let receipt = envelope.validate_for("operation-credential-issue", 0).unwrap().unwrap().clone();
    receipt.validate_request(request).unwrap();
    validate(json!({"kind":"operation_credential_issue_cli_reply", "request":request,
        "process_exit":0, "response":result}), true);
    receipt
}

fn query_credential(root: &Path, original: &OperationCredentialRequest) -> OperationCredentialReceipt {
    let query = OperationCredentialQuery { version: 1, operation_id: original.operation_id.clone() };
    let result = success(command(root, "operation-credential-query", &query));
    let envelope: CommandEnvelope<OperationCredentialQueryReply> = serde_json::from_value(result.clone()).unwrap();
    let reply = envelope.validate_for("operation-credential-query", 0).unwrap().unwrap();
    reply.validate_request(original).unwrap();
    validate(json!({"kind":"operation_credential_query_cli_reply", "request":original,
        "process_exit":0, "response":result}), true);
    reply.receipt.clone().expect("credential receipt must remain queryable")
}

struct Server { child: Child, address: String }
impl Server {
    fn start(root: &Path, strict: bool, hook: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
        command.args(["--root", root.to_str().unwrap(), "--listen", "127.0.0.1:0",
            "--allow-write", "--policy", root.join("unused-bootstrap").to_str().unwrap()]);
        if strict { command.arg("--require-operation-credentials"); }
        if let Some(hook) = hook { command.arg("--pre-receive").arg(hook); }
        let mut child = command.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        let line = match receive.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(line)) => line,
            other => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                panic!("server readiness failed: {other:?}");
            }
        };
        reader.join().unwrap();
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("invalid server readiness {line:?}: {error}");
            }
        };
        let address = match parsed["listen"].as_str() {
            Some(address) => address.to_owned(),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("missing listen address in readiness: {parsed}");
            }
        };
        Self { child, address }
    }
}
impl Drop for Server {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

struct Wire { status: u16, body: Vec<u8> }
impl Wire {
    fn packet(self) -> (Response, Vec<u8>) {
        assert_eq!(self.status, 200, "{}", String::from_utf8_lossy(&self.body));
        protocol::read_complete_packet(&self.body).unwrap()
    }
    fn denied(self) {
        if self.status == 200 {
            let (response, _) = protocol::read_complete_packet::<Response>(&self.body).unwrap();
            assert!(!response.ok, "unexpected successful response: {response:?}");
        } else {
            assert!(matches!(self.status, 401 | 403 | 503), "unexpected HTTP {}", self.status);
        }
    }
}

fn rpc(address: &str, token: &str, request: &Request, payload: &[u8]) -> Wire {
    let mut packet = Vec::new();
    protocol::write_packet(&mut packet, request, payload).unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(25))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
    write!(stream, "POST /v1/wasm-git HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n", packet.len()).unwrap();
    stream.write_all(&packet).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let offset = bytes.windows(4).position(|b| b == b"\r\n\r\n").expect("HTTP headers") + 4;
    let status = std::str::from_utf8(&bytes[..offset]).unwrap().lines().next().unwrap()
        .split_whitespace().nth(1).unwrap().parse().unwrap();
    Wire { status, body: bytes[offset..].to_vec() }
}

struct HookGuard { root: PathBuf }
impl HookGuard {
    fn install(root: &Path) -> (PathBuf, Self) {
        let hook = root.join("receive-hook.sh");
        fs::write(&hook, b"#!/bin/sh\ntouch \"$WASM_GIT_REPOSITORY/../hook-started\"\nfor i in $(seq 1 900); do if test -f \"$WASM_GIT_REPOSITORY/../hook-release\"; then touch \"$WASM_GIT_REPOSITORY/../hook-finished\"; exit 0; fi; sleep 0.02; done\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        (hook, Self { root: root.into() })
    }
    fn release(&self) { fs::write(self.root.join("hook-release"), b"release").unwrap(); }
}
impl Drop for HookGuard {
    fn drop(&mut self) {
        if self.root.join("hook-started").exists() {
            let _ = fs::write(self.root.join("hook-release"), b"release");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !self.root.join("hook-finished").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn scoped_fixture() -> (Temp, Repository, Vec<u8>, UploadBinding, String) {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let base = repo.commit(WASM, "base").unwrap();
    let fork = Repository::fork_local(temp.0.join("project"), temp.0.join("fork")).unwrap();
    let mut next = WASM.to_vec();
    next.extend([0, 3, 1, b'x', 1]);
    let head = fork.commit(&next, "feature").unwrap();
    let bytes = fork.network_bundle(std::slice::from_ref(&head), std::slice::from_ref(&base)).unwrap().1;
    let operation = Operation::UpdateRef { namespace: RefNamespace::Branch, name: "topic".into(),
        expected: None, new: Some(head.clone()), force: false };
    let binding = UploadBinding::for_payload(operation, &bytes).unwrap();
    set_policy(&temp.0, 0, policy(true, true));
    (temp, repo, bytes, binding, head)
}

#[test]
fn strict_activation_binds_request_payload_actor_and_current_policy() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    repo.commit(WASM, "base").unwrap();
    let tree = repo.create_files_tree(&BTreeMap::from([("module.wasm".into(), WASM.to_vec())])).unwrap();
    let commit = repo.create_commit_object(&tree, &[], "release").unwrap();
    set_policy(&temp.0, 0, policy(true, true));

    let mut missing_policy = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    missing_policy.args(["--root", temp.0.to_str().unwrap(), "--require-operation-credentials", "--allow-write"]);
    let missing_policy = bounded_output(missing_policy, None);
    assert!(!missing_policy.status.success());
    let mut strict_stdio = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    strict_stdio.args(["--stdio", "--root", temp.0.to_str().unwrap(), "--policy",
            temp.0.join("unused-bootstrap").to_str().unwrap(), "--principal", "alice",
            "--require-operation-credentials"]);
    let strict_stdio = bounded_output(strict_stdio, None);
    assert!(!strict_stdio.status.success());
    assert!(String::from_utf8_lossy(&strict_stdio.stderr).contains("operation credentials require an HTTP policy entry point"));

    let legacy = Server::start(&temp.0, false, None);
    let strict = Server::start(&temp.0, true, None);
    let advertise = request("project", Operation::Advertise {});
    assert!(rpc(&legacy.address, "alice-legacy", &advertise, &[]).packet().0.ok);
    // A legacy request may already have passed header authentication while
    // still waiting for its body. Activation must fence that request too.
    let mut pending_packet = Vec::new();
    protocol::write_packet(&mut pending_packet, &advertise, &[]).unwrap();
    let mut pending = TcpStream::connect(&legacy.address).unwrap();
    pending.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    pending.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
    write!(pending, "POST /v1/wasm-git HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer alice-legacy\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n", pending_packet.len()).unwrap();
    let mut interim = Vec::new();
    while !interim.ends_with(b"\r\n\r\n") {
        assert!(interim.len() < 1024, "oversized interim response");
        let mut byte = [0];
        pending.read_exact(&mut byte).unwrap();
        interim.push(byte[0]);
    }
    assert!(interim.starts_with(b"HTTP/1.1 100"));
    let read = issuance(1, "alice", "alice-read-credential", advertise.clone(), &[], now() + 120);
    let first = issue(&temp.0, &read);
    pending.write_all(&pending_packet).unwrap();
    pending.shutdown(Shutdown::Write).unwrap();
    let mut rejected = Vec::new();
    pending.read_to_end(&mut rejected).unwrap();
    assert!(rejected.starts_with(b"HTTP/1.1 503"), "{}", String::from_utf8_lossy(&rejected));
    assert_eq!(query_credential(&temp.0, &read), first);
    assert_eq!(issue(&temp.0, &read), first);
    let mut altered = read.clone();
    altered.expires_at += 1;
    assert!(!command(&temp.0, "operation-credential-issue", &altered).status.success());
    assert_eq!(rpc(&legacy.address, "alice-legacy", &advertise, &[]).status, 503);
    let mut legacy_restart = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    legacy_restart.args(["--root", temp.0.to_str().unwrap(), "--policy",
        temp.0.join("unused-bootstrap").to_str().unwrap()]);
    let legacy_restart = bounded_output(legacy_restart, None);
    assert!(!legacy_restart.status.success());
    assert!(String::from_utf8_lossy(&legacy_restart.stderr).contains("strict credential entry point"));
    let mut legacy_stdio = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    legacy_stdio.args(["--stdio", "--root", temp.0.to_str().unwrap(), "--policy",
        temp.0.join("unused-bootstrap").to_str().unwrap(), "--principal", "alice"]);
    let legacy_stdio = bounded_output(legacy_stdio, None);
    assert!(!legacy_stdio.status.success());
    assert!(String::from_utf8_lossy(&legacy_stdio.stderr).contains("strict credential entry point"));
    rpc(&strict.address, "alice-legacy", &advertise, &[]).denied();
    assert!(rpc(&strict.address, "alice-read-credential", &advertise, &[]).packet().0.ok);
    rpc(&strict.address, "alice-read-credential", &request("other", Operation::Advertise {}), &[]).denied();
    rpc(&strict.address, "alice-read-credential", &request("project", Operation::Fetch {}), &[]).denied();
    rpc(&strict.address, "alice-read-credential", &advertise, b"changed payload").denied();

    let retention_id = id(77);
    let prepare = request("project", Operation::ReleaseRetention { command: RetentionCommand::Prepare {
        operation: retention_id.clone(), commit, paths: vec!["module.wasm".into()] } });
    let permit = issuance(2, "alice", "alice-retention", prepare.clone(), &[], now() + 120);
    let receipt = issue(&temp.0, &permit);
    assert!(receipt.effect_key.is_some());
    assert!(rpc(&strict.address, "alice-retention", &prepare, &[]).packet().0.ok);
    let mut renewed = permit.clone();
    renewed.operation_id = id(3);
    renewed.token_sha256 = hash(b"different-token");
    renewed.expires_at += 1;
    assert!(!command(&temp.0, "operation-credential-issue", &renewed).status.success());
    let retention_query = request("project", Operation::ReleaseRetention { command:
        RetentionCommand::Query { operation: retention_id } });
    let bob = issuance(4, "bob", "bob-query", retention_query.clone(), &[], now() + 120);
    issue(&temp.0, &bob);
    rpc(&strict.address, "bob-query", &retention_query, &[]).denied();
    let alice = issuance(5, "alice", "alice-query", retention_query.clone(), &[], now() + 120);
    issue(&temp.0, &alice);
    let (response, payload) = rpc(&strict.address, "alice-query", &retention_query, &[]).packet();
    assert!(response.ok, "{response:?}");
    let reply: RetentionReply = binary::decode(&payload).unwrap();
    assert!(matches!(reply, RetentionReply::State { record: Some(record), .. } if record.actor == "alice"));

    let raw_write = issuance(6, "alice", "raw-write", request("project", Operation::UpdateRef {
        namespace: RefNamespace::Branch, name: "topic".into(), expected: None, new: None, force: false }), &[], now() + 120);
    assert!(!command(&temp.0, "operation-credential-issue", &raw_write).status.success());
    set_policy(&temp.0, 1, policy(true, false));
    assert_eq!(rpc(&strict.address, "alice-read-credential", &advertise, &[]).status, 401);
    set_policy(&temp.0, 2, policy(true, true));
    // Restoring the same grants cannot resurrect credentials from generation 1.
    assert_eq!(rpc(&strict.address, "alice-read-credential", &advertise, &[]).status, 401);
    assert_eq!(rpc(&strict.address, "alice-retention", &prepare, &[]).status, 401);
    let stale = issuance(7, "alice", "stale-generation", advertise.clone(), &[], now() + 120);
    assert!(!command(&temp.0, "operation-credential-issue", &stale).status.success());
    let mut fresh = issuance(8, "alice", "generation-three-read", advertise.clone(), &[], now() + 120);
    fresh.policy_generation = 3;
    issue(&temp.0, &fresh);
    assert!(rpc(&strict.address, "generation-three-read", &advertise, &[]).packet().0.ok);
    let mut renewed_after_regrant = permit.clone();
    renewed_after_regrant.operation_id = id(9);
    renewed_after_regrant.token_sha256 = hash(b"regrant-write-token");
    renewed_after_regrant.expires_at += 2;
    renewed_after_regrant.policy_generation = 3;
    assert_eq!(renewed_after_regrant.receipt().unwrap().effect_key, receipt.effect_key);
    assert!(!command(&temp.0, "operation-credential-issue", &renewed_after_regrant).status.success());
    set_policy(&temp.0, 3, policy(false, false));
    assert_eq!(rpc(&strict.address, "generation-three-read", &advertise, &[]).status, 401);
    assert_eq!(query_credential(&temp.0, &read), first);
    assert_eq!(query_credential(&temp.0, &permit), receipt);
    assert_eq!(issue(&temp.0, &read), first);
}

#[test]
fn scoped_apply_keeps_its_result_after_credential_expiry() {
    let (temp, repo, bytes, binding, head) = scoped_fixture();
    let server = Server::start(&temp.0, true, None);
    let session = id(200);
    let slice = request("project", Operation::UploadChunk { chunk: protocol::ScopedChunkIdentity {
        version: 1, operation_id: id(20), scope: protocol::ScopedExecution { actor: "alice".into(), repository: "project".into(),
            session: session.clone(), binding: binding.clone() }, offset: 0, payload_sha256: hash(&bytes) } });
    let upload = issuance(20, "alice", "slice-token", slice.clone(), &bytes, now() + 120);
    issue(&temp.0, &upload);
    let mut changed = bytes.clone();
    changed[0] ^= 1;
    rpc(&server.address, "slice-token", &slice, &changed).denied();
    assert!(rpc(&server.address, "slice-token", &slice, &bytes).packet().0.ok);

    let apply = request("project", Operation::ApplyScopedUpload { session: session.clone(), binding: binding.clone() });
    let expiry = now() + 5;
    let issued = issuance(21, "alice", "apply-token", apply.clone(), &[], expiry);
    let original = issue(&temp.0, &issued);
    let response = rpc(&server.address, "apply-token", &apply, &[]).packet().0;
    assert!(response.ok, "{response:?}");
    assert_eq!(repo.resolve("topic").unwrap(), head);
    wait_until(expiry);
    rpc(&server.address, "apply-token", &apply, &[]).denied();
    assert_eq!(query_credential(&temp.0, &issued), original);
    assert_eq!(issue(&temp.0, &issued), original);

    let query = request("project", Operation::QueryScopedUpload { session: session.clone(), binding: binding.clone() });
    let fresh = issuance(22, "alice", "fresh-query", query.clone(), &[], now() + 120);
    issue(&temp.0, &fresh);
    let (response, payload) = rpc(&server.address, "fresh-query", &query, &[]).packet();
    assert!(response.ok, "{response:?}");
    let status: ScopedUploadStatus = binary::decode(&payload).unwrap();
    status.validate_for("project", &session, &binding).unwrap();
    assert!(matches!(status.state, Some(ScopedUploadState::FinishedV4 { receipt })
        if receipt.response.ok && receipt.scope.actor == "alice" && receipt.scope.session == session));
}

#[test]
fn scoped_apply_expiring_during_hook_does_not_publish_reference() {
    let (temp, repo, bytes, binding, _) = scoped_fixture();
    let (hook, hook_guard) = HookGuard::install(&temp.0);
    let server = Server::start(&temp.0, true, Some(&hook));
    let session = id(300);
    let slice = request("project", Operation::UploadChunk { chunk: protocol::ScopedChunkIdentity {
        version: 1, operation_id: id(30), scope: protocol::ScopedExecution { actor: "alice".into(), repository: "project".into(),
            session: session.clone(), binding: binding.clone() }, offset: 0, payload_sha256: hash(&bytes) } });
    issue(&temp.0, &issuance(30, "alice", "hook-slice", slice.clone(), &bytes, now() + 120));
    assert!(rpc(&server.address, "hook-slice", &slice, &bytes).packet().0.ok);
    let apply = request("project", Operation::ApplyScopedUpload { session: session.clone(), binding: binding.clone() });
    let expiry = now() + 8;
    let issued = issuance(31, "alice", "hook-apply", apply.clone(), &[], expiry);
    let original = issue(&temp.0, &issued);
    let address = server.address.clone();
    let worker = std::thread::spawn(move || rpc(&address, "hook-apply", &apply, &[]));
    wait_for(&temp.0.join("hook-started"));
    wait_until(expiry);
    hook_guard.release();
    let response = worker.join().unwrap().packet().0;
    assert!(!response.ok, "{response:?}");
    assert!(repo.resolve("topic").is_err());
    assert_eq!(query_credential(&temp.0, &issued), original);

    let query = request("project", Operation::QueryScopedUpload { session: session.clone(), binding: binding.clone() });
    issue(&temp.0, &issuance(32, "alice", "hook-query", query.clone(), &[], now() + 120));
    let (response, payload) = rpc(&server.address, "hook-query", &query, &[]).packet();
    assert!(response.ok, "{response:?}");
    let status: ScopedUploadStatus = binary::decode(&payload).unwrap();
    status.validate_for("project", &session, &binding).unwrap();
    assert!(matches!(&status.state, Some(ScopedUploadState::FinishedV4 { receipt }) if !receipt.response.ok)
        || matches!(&status.state, Some(ScopedUploadState::Aborted { .. })));
}
