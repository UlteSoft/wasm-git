#![cfg(all(feature = "client", feature = "server", unix))]
//! Real strict HTTP/CLI tests for scoped chunk outcomes.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wasm_git::{
    binary,
    protocol::{
        self, Operation, OperationCredentialRequest, RefNamespace, Request, Response,
        ScopedChunkIdentity, ScopedChunkState, ScopedChunkStatus, UploadBinding,
    },
    repository::Repository,
};

const WASM: &[u8] = b"\0asm\x01\0\0\0";
#[path = "infrastructure/audit_faults.rs"]
mod audit_faults;
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Https {
    child: Child,
    address: String,
    certificate: PathBuf,
}
impl Https {
    fn start(root: &Path, backend: &str) -> Self {
        let certificate = root.join("test-ca.pem");
        let key = root.join("test-key.pem");
        let mut openssl = Command::new("openssl");
        openssl
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=IP:127.0.0.1,DNS:localhost",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate);
        let output = bounded_output(openssl, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut child = Command::new("python3")
            .arg("-u")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/https_proxy.py"))
            .arg(&certificate)
            .arg(&key)
            .arg(backend)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut address = String::new();
            let result = BufReader::new(stdout)
                .read_line(&mut address)
                .map(|_| address);
            let _ = send.send(result);
        });
        let address = match receive.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(address)) if !address.trim().is_empty() => address.trim().to_owned(),
            error => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                panic!("TLS proxy startup failed: {error:?}");
            }
        };
        reader.join().unwrap();
        Self {
            child,
            address,
            certificate,
        }
    }

    fn rpc(&self, token: &str, request: &Request, payload: &[u8]) -> Wire {
        let mut curl = Command::new("curl");
        curl.args([
            "-q",
            "--noproxy",
            "*",
            "--silent",
            "--show-error",
            "--max-time",
            "25",
            "--include",
            "--cacert",
        ])
        .arg(&self.certificate)
        .args([
            "--header",
            &format!("Authorization: Bearer {token}"),
            "--header",
            "Expect:",
            "--data-binary",
            "@-",
        ])
        .arg(format!("https://{}/v1/wasm-git", self.address));
        let output = bounded_output(curl, Some(&packet(request, payload)));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let offset = output
            .stdout
            .windows(4)
            .position(|v| v == b"\r\n\r\n")
            .unwrap()
            + 4;
        let status = std::str::from_utf8(&output.stdout[..offset])
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        Wire {
            status,
            body: output.stdout[offset..].to_vec(),
        }
    }
}
impl Drop for Https {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn recovery(
    root: &Path,
    recovery_id: u64,
    original: &OperationCredentialRequest,
    close: bool,
    reconcile: Vec<Value>,
) -> Value {
    let input = json!({"version":1,"recovery_id":id(recovery_id),"original":original,
        "action":if close {"Close"} else {"Inspect"},"reconcile":reconcile});
    let result = success(command(root, "operation-recover", &input));
    validate(
        json!({"kind":"operation_recovery_cli_reply","request":input,"process_exit":0,"response":result}),
    );
    result["result"]["value"].clone()
}

fn audit_rows(root: &Path) -> Vec<Value> {
    let mut items = Vec::new();
    let mut after = 0;
    let mut high_water = None;
    loop {
        let input = json!({"version":1,"after":after,"high_water":high_water,"limit":2});
        let result = success(command(root, "audit-query", &input));
        validate(
            json!({"kind":"audit_query_cli_reply","request":input,"process_exit":0,"response":result}),
        );
        let value = &result["result"]["value"];
        high_water = value["high_water"].as_u64();
        items.extend(value["items"].as_array().unwrap().iter().cloned());
        match value["next_after"].as_u64() {
            Some(next) => {
                assert!(next > after);
                after = next;
            }
            None => break,
        }
        assert!(items.len() < 1000, "audit pagination did not terminate");
    }
    items
}

#[test]
#[cfg(target_os = "linux")]
fn https_revoked_actor_recovery_closes_only_original_scope_and_keeps_chunk_evidence() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let head = repo.commit(WASM, "base").unwrap();
    Repository::init(temp.0.join("other")).unwrap();
    set_policy(&temp.0, 0, true, true);
    let server = Server::start(&temp.0);
    let https = Https::start(&temp.0, &server.address);
    let bytes = vec![42; 4096];
    let binding = UploadBinding::for_payload(
        Operation::UpdateRef {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: Some(head),
            force: false,
        },
        &bytes,
    )
    .unwrap();
    let original_chunk = chunk(301, "alice", "project", &id(401), &binding, 0, &bytes);
    let upload = issuance(
        301,
        "alice",
        "recover-alice",
        upload_request(&original_chunk),
        &bytes,
        now() + 120,
    );
    issue(&temp.0, &upload);
    // Do not retain the application's receipt: recovery must find the durable fact.
    assert_eq!(
        https.rpc("recover-alice", &upload.request, &bytes).status,
        200
    );
    let bob_query = issuance(
        302,
        "bob",
        "recover-bob",
        query_request(&original_chunk),
        &[],
        now() + 120,
    );
    assert!(!command(&temp.0, "operation-credential-issue", &bob_query)
        .status
        .success());
    set_policy(&temp.0, 1, false, false);
    https.rpc("recover-alice", &upload.request, &bytes).denied();
    let state = recovery(&temp.0, 501, &upload, false, vec![]);
    assert_eq!(state["state"]["Upload"]["state"], "Open");
    assert_eq!(
        state["state"]["Upload"]["chunk"]["state"],
        json!({"Accepted":{"data":"Available"}})
    );
    assert!(state["operator"]["identity"]
        .as_str()
        .unwrap()
        .starts_with("unix:uid:"));
    for field in ["actor", "repository", "binding"] {
        let mut changed = serde_json::to_value(&upload).unwrap();
        match field {
            "actor" => changed["actor"] = json!("bob"),
            "repository" => changed["request"]["repository"] = json!("other"),
            _ => {
                changed["request"]["operation"]["UploadChunk"]["chunk"]["scope"]["binding"]
                    ["digest"] = json!(id(999))
            }
        }
        assert!(!command(
            &temp.0,
            "operation-recover",
            &json!({"version":1,"recovery_id":id(502),"original":changed,"action":"Close"})
        )
        .status
        .success());
    }
    let closed = recovery(&temp.0, 503, &upload, true, vec![]);
    assert_eq!(closed["state"]["Upload"]["state"], "Canceled");
    assert_eq!(
        closed["state"]["Upload"]["chunk"]["state"],
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Canceled"}}}})
    );
    drop(https);
    drop(server);
    let _restarted = Server::start(&temp.0);
    assert_eq!(
        recovery(&temp.0, 503, &upload, true, vec![])["state"],
        closed["state"]
    );
    let rows = audit_rows(&temp.0);
    assert!(rows.iter().any(
        |row| row["item"]["Policy"]["attribution"]["Recorded"]["operator"]["identity"].is_string()
    ));
    assert!(rows
        .iter()
        .any(|row| row["item"]["RecoveryAttempt"]["original_operation_id"] == id(301)));
    let input = json!({"version":1,"repository":"other"});
    let capacity = success(command(&temp.0, "capacity-query", &input));
    validate(
        json!({"kind":"capacity_query_cli_reply","request":input,"process_exit":0,"response":capacity}),
    );
    assert_eq!(
        capacity["result"]["value"]["capacity"]["scoped"]["total_records"],
        0
    );
}

#[test]
#[cfg(target_os = "linux")]
fn close_before_first_chunk_fences_late_effect_and_audit_failures_are_reconciled() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let head = repo.commit(WASM, "base").unwrap();
    set_policy(&temp.0, 0, true, true);
    let server = Server::start(&temp.0);
    let https = Https::start(&temp.0, &server.address);
    let bytes = vec![42; 4096];
    let binding = UploadBinding::for_payload(
        Operation::UpdateRef {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: Some(head),
            force: false,
        },
        &bytes,
    )
    .unwrap();
    let original = chunk(601, "alice", "project", &id(701), &binding, 0, &bytes);
    let first = issuance(
        601,
        "alice",
        "late-chunk",
        upload_request(&original),
        &bytes,
        now() + 120,
    );
    issue(&temp.0, &first);
    let closed = recovery(&temp.0, 801, &first, true, vec![]);
    assert_eq!(closed["state"]["Upload"]["state"], "Canceled");
    https.rpc("late-chunk", &first.request, &bytes).denied();
    assert_eq!(
        recovery(&temp.0, 802, &first, false, vec![])["state"]["Upload"]["chunk"]["state"],
        "Unobserved"
    );

    let original = chunk(602, "alice", "project", &id(702), &binding, 0, &bytes);
    let second = issuance(
        602,
        "alice",
        "audit-failure",
        upload_request(&original),
        &bytes,
        now() + 120,
    );
    issue(&temp.0, &second);
    audit_faults::fail_observations(&temp.0);
    let response = https
        .rpc("audit-failure", &second.request, &bytes)
        .packet()
        .0;
    assert!(!response.ok && response.publication_unknown, "{response:?}");
    audit_faults::restore_observations(&temp.0);
    drop(https);
    drop(server);
    let rows = audit_rows(&temp.0);
    let pending = rows
        .iter()
        .find(|row| {
            row["item"]["OperationAttempt"]["operation_id"] == id(602)
                && row["item"]["OperationAttempt"]["observation"].is_null()
        })
        .unwrap();
    let attempt = pending["sequence"].as_u64().unwrap();
    let result = recovery(
        &temp.0,
        803,
        &second,
        true,
        vec![json!({"Operation":{"operation_id":id(602),"attempt_id":attempt}})],
    );
    assert_eq!(
        result["state"]["Upload"]["chunk"]["state"],
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Canceled"}}}})
    );
    let rows = audit_rows(&temp.0);
    let repaired = rows.iter().find(|row| row["sequence"] == attempt).unwrap();
    assert!(
        repaired["item"]["OperationAttempt"]["observation"]["source"]["RecoveryAttempt"]
            .is_object()
    );
    let repeated = recovery(
        &temp.0,
        803,
        &second,
        true,
        vec![json!({"Operation":{"operation_id":id(602),"attempt_id":attempt}})],
    );
    assert_eq!(repeated["state"], result["state"]);
    let input = json!({"version":1,"operation_id":id(602)});
    let context = success(command(&temp.0, "operation-context", &input));
    validate(
        json!({"kind":"operation_context_cli_reply","request":input,"process_exit":0,"response":context}),
    );
    assert_eq!(
        context["result"]["value"]["original"],
        serde_json::to_value(second).unwrap()
    );
}

#[test]
#[cfg(target_os = "linux")]
fn retention_recovery_never_promotes_or_releases_an_already_committed_reference() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let tree = repo
        .create_files_tree(&std::collections::BTreeMap::from([(
            "main.wasm".into(),
            WASM.to_vec(),
        )]))
        .unwrap();
    let commit = repo.create_commit_object(&tree, &[], "release").unwrap();
    set_policy(&temp.0, 0, true, true);
    let server = Server::start(&temp.0);
    let https = Https::start(&temp.0, &server.address);
    let prepare = issuance(
        901,
        "alice",
        "retention-prepare",
        request(
            "project",
            json!({"ReleaseRetention":{"command":{
        "Prepare":{"operation":id(902),"commit":commit,"paths":["main.wasm"]}}} }),
        ),
        &[],
        now() + 120,
    );
    issue(&temp.0, &prepare);
    assert!(
        https
            .rpc("retention-prepare", &prepare.request, &[])
            .packet()
            .0
            .ok
    );
    let promote = issuance(
        903,
        "alice",
        "retention-promote",
        request(
            "project",
            json!({"ReleaseRetention":{"command":{
        "Promote":{"operation":id(902)}}} }),
        ),
        &[],
        now() + 120,
    );
    issue(&temp.0, &promote);
    assert!(
        https
            .rpc("retention-promote", &promote.request, &[])
            .packet()
            .0
            .ok
    );
    let unseen = issuance(
        904,
        "alice",
        "unseen-retention",
        request(
            "project",
            json!({"ReleaseRetention":{"command":{
        "Prepare":{"operation":id(905),"commit":commit,"paths":["main.wasm"]}}} }),
        ),
        &[],
        now() + 120,
    );
    issue(&temp.0, &unseen);
    assert_eq!(
        recovery(&temp.0, 906, &unseen, true, vec![])["state"]["Retention"]["record"]["state"],
        "Closed"
    );
    https.rpc("unseen-retention", &unseen.request, &[]).denied();
    set_policy(&temp.0, 1, false, false);
    let result = recovery(&temp.0, 907, &prepare, true, vec![]);
    assert_eq!(result["state"]["Retention"]["record"]["state"], "Promoted");
    assert_eq!(result["state"]["Retention"]["record"]["commit"], commit);
    repo.garbage_collect_mode(&[], true, true).unwrap();
    repo.read_commit(&commit).unwrap();
    repo.query_tree(&commit).unwrap();
    let invalid = json!({"version":1,"recovery_id":id(908),"original":prepare,"action":"Promote"});
    assert!(!command(&temp.0, "operation-recover", &invalid)
        .status
        .success());
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "wasm-git-chunk-receipts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(value: u64) -> String {
    format!("{value:064x}")
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn wait_until(expiry: u64) {
    let deadline = Instant::now() + Duration::from_secs(25);
    while now() < expiry {
        assert!(
            Instant::now() < deadline,
            "clock did not reach credential expiry"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn bounded_output(mut command: Command, input: Option<&[u8]>) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        if let Err(error) = child.stdin.take().unwrap().write_all(input) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("write command input: {error}");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().unwrap(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "command did not finish ({result:?}): {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

fn command(root: &Path, name: &str, input: &impl serde::Serialize) -> Output {
    let path = root.join(format!(
        "request-{}.json",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, serde_json::to_vec(input).unwrap()).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    process.args(["--format", "json", name]).arg(root).arg(path);
    bounded_output(process, None)
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn validate(value: Value) {
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
    process.args(["--format", "json", "integration-validate"]);
    let output = bounded_output(process, Some(&serde_json::to_vec(&value).unwrap()));
    assert!(
        output.status.success(),
        "validator: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn reject_validation(input: &[u8]) {
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
    process.args(["--format", "json", "integration-validate"]);
    let output = bounded_output(process, Some(input));
    assert!(
        !output.status.success(),
        "invalid input passed validation: {}",
        String::from_utf8_lossy(input)
    );
}

fn set_policy(root: &Path, generation: u64, alice_exists: bool, alice_can_write: bool) {
    let grant = json!({"repository":"*", "read":true, "write":["refs/heads/*"],
        "force":false, "create":false, "release_retention":true});
    let mut principals =
        vec![json!({"name":"bob", "token_sha256":hash(b"bob-legacy"), "grants":[grant.clone()]})];
    if alice_exists {
        let mut alice_grant = grant;
        if !alice_can_write {
            alice_grant["write"] = json!([]);
        }
        principals.push(
            json!({"name":"alice", "token_sha256":hash(b"alice-legacy"), "grants":[alice_grant]}),
        );
    }
    let policy = json!({"version":3, "principals":principals});
    let result = success(command(
        root,
        "policy-set",
        &json!({"version":1,
        "operation_id":id(9000 + generation), "expected_generation":generation, "policy":policy}),
    ));
    assert_eq!(result["result"]["value"]["outcome"], "applied");
}

fn request(repository: &str, operation: Value) -> Request {
    serde_json::from_value(json!({"version":1, "repository":repository, "operation":operation}))
        .unwrap()
}

fn issuance(
    number: u64,
    actor: &str,
    token: &str,
    request: Request,
    payload: &[u8],
    expiry: u64,
) -> OperationCredentialRequest {
    OperationCredentialRequest {
        version: 1,
        operation_id: id(number),
        policy_generation: 1,
        actor: actor.into(),
        token_sha256: hash(token.as_bytes()),
        request,
        payload_sha256: hash(payload),
        expires_at: expiry,
    }
}

fn issue(root: &Path, request: &OperationCredentialRequest) {
    let result = success(command(root, "operation-credential-issue", request));
    validate(
        json!({"kind":"operation_credential_issue_cli_reply", "request":request,
        "process_exit":0, "response":result}),
    );
}

struct Server {
    child: Child,
    address: String,
}
impl Server {
    fn start(root: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
        command.args([
            "--root",
            root.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--allow-write",
            "--policy",
            root.join("unused-bootstrap").to_str().unwrap(),
            "--require-operation-credentials",
        ]);
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Wire {
    status: u16,
    body: Vec<u8>,
}
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
            assert!(
                matches!(self.status, 401 | 403),
                "unexpected HTTP {}: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            );
        }
    }
}

fn packet(request: &Request, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::new();
    protocol::write_packet(&mut packet, request, payload).unwrap();
    packet
}

fn connect(address: &str, token: &str, request: &Request, payload: &[u8]) -> TcpStream {
    let packet = packet(request, payload);
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(25)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(stream, "POST /v1/wasm-git HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n", packet.len()).unwrap();
    stream.write_all(&packet).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    stream
}

fn rpc(address: &str, token: &str, request: &Request, payload: &[u8]) -> Wire {
    let mut stream = connect(address, token, request, payload);
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let offset = bytes
        .windows(4)
        .position(|b| b == b"\r\n\r\n")
        .expect("HTTP headers")
        + 4;
    let status = std::str::from_utf8(&bytes[..offset])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    Wire {
        status,
        body: bytes[offset..].to_vec(),
    }
}

fn assert_unauthenticated_token(address: &str, token: &str) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(stream, "POST /v1/wasm-git HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n").unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert!(
        bytes.starts_with(b"HTTP/1.1 401"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}

// Consume only HTTP headers and deliberately discard the binary receipt. The
// server has completed the request, but the caller has no application result.
fn lose_reply(address: &str, token: &str, request: &Request, payload: &[u8]) {
    let mut stream = connect(address, token, request, payload);
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 8192, "oversized HTTP headers");
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    assert!(headers.starts_with(b"HTTP/1.1 200"));
    drop(stream);
}

fn chunk(
    number: u64,
    actor: &str,
    repository: &str,
    session: &str,
    binding: &UploadBinding,
    offset: u64,
    payload: &[u8],
) -> Value {
    json!({"version":1, "operation_id":id(number), "scope":{
        "actor":actor, "repository":repository, "session":session, "binding":binding
    }, "offset":offset, "payload_sha256":hash(payload)})
}

fn upload_request(chunk: &Value) -> Request {
    request(
        chunk["scope"]["repository"].as_str().unwrap(),
        json!({"UploadChunk":{"chunk":chunk}}),
    )
}

fn query_request(chunk: &Value) -> Request {
    request(
        chunk["scope"]["repository"].as_str().unwrap(),
        json!({"QueryChunk":{"chunk":chunk}}),
    )
}

fn chunk_cli(address: &str, repository: &str, token: &str, command: Value) -> Value {
    let input = json!({"version":1, "command":command});
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
    process.args([
        "--format",
        "json",
        "scoped-upload-rpc",
        &format!("http://{address}/{repository}"),
    ]);
    process.env("WASM_GIT_REMOTE_TOKEN", token);
    let result = success(bounded_output(
        process,
        Some(&serde_json::to_vec(&input).unwrap()),
    ));
    validate(
        json!({"kind":"upload_cli_reply", "repository":repository, "request":input,
        "process_exit":0, "response":result}),
    );
    result["result"]["value"]["Chunk"]["status"].clone()
}

fn query_cli(address: &str, repository: &str, token: &str, chunk: &Value) -> Value {
    chunk_cli(
        address,
        repository,
        token,
        json!({"QueryChunk":{"chunk":chunk}}),
    )
}

fn assert_status(actual: Value, chunk: &Value, state: Value) {
    assert_eq!(actual, json!({"version":1, "chunk":chunk, "state":state}));
}

fn assert_claim_denied(
    root: &Path,
    address: &str,
    chunk: &Value,
    actor: &str,
    number: u64,
) -> (u16, String) {
    let token = format!("wrong-claim-{number}");
    let request = query_request(chunk);
    issue(
        root,
        &issuance(number, actor, &token, request.clone(), &[], now() + 120),
    );
    let reply = rpc(address, &token, &request, &[]);
    if reply.status == 200 {
        let (response, payload) = reply.packet();
        assert!(
            !response.ok && !response.publication_unknown && payload.is_empty(),
            "{response:?}"
        );
        (200, response.error.unwrap_or_default())
    } else {
        assert!(
            matches!(reply.status, 401 | 403),
            "unexpected HTTP {}",
            reply.status
        );
        (reply.status, String::from_utf8(reply.body).unwrap())
    }
}

fn fake_chunk_reply(input: &Value, payload: Vec<u8>, outcome_unknown: bool) -> Output {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake HTTP server failed to accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            assert!(headers.len() < 8192, "oversized HTTP request headers");
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        let length: usize = std::str::from_utf8(&headers)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .expect("content length")
            .1
            .trim()
            .parse()
            .unwrap();
        assert!(length <= protocol::MAX_PACKET_BYTES);
        let mut request = vec![0; length];
        stream.read_exact(&mut request).unwrap();
        let (request, _): (Request, Vec<u8>) = protocol::read_complete_packet(&request).unwrap();
        assert!(matches!(request.operation, Operation::UploadChunk { .. }));
        let response = Response {
            version: 1,
            ok: !outcome_unknown,
            publication_unknown: outcome_unknown,
            error: outcome_unknown.then(|| "authorization unavailable before delivery".into()),
            state: None,
            tip: None,
        };
        let mut body = Vec::new();
        protocol::write_packet(&mut body, &response, &payload).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let mut process = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
    process.args([
        "--format",
        "json",
        "scoped-upload-rpc",
        &format!("http://{address}/project"),
    ]);
    process.env("WASM_GIT_REMOTE_TOKEN", "fake-token");
    let result = bounded_output(process, Some(&serde_json::to_vec(input).unwrap()));
    worker.join().unwrap();
    result
}

#[test]
fn chunk_cli_reports_unknown_after_invalid_or_uncertain_acknowledgement() {
    let bytes = b"original chunk";
    let binding = UploadBinding::for_payload(
        Operation::UpdateRef {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: None,
            force: false,
        },
        bytes,
    )
    .unwrap();
    let identity = chunk(41, "alice", "project", &id(104), &binding, 0, bytes);
    let input = json!({"version":1, "command":{"Chunk":{"chunk":identity, "bytes":bytes}}});
    let status = ScopedChunkStatus {
        version: 1,
        chunk: serde_json::from_value::<ScopedChunkIdentity>(identity).unwrap(),
        state: ScopedChunkState::Unobserved,
    };
    for (payload, outcome_unknown) in [
        (vec![0xff], false),
        (binary::encode(&status).unwrap(), false),
        (Vec::new(), true),
    ] {
        let output = fake_chunk_reply(&input, payload, outcome_unknown);
        assert!(
            !output.status.success(),
            "invalid acknowledgement was accepted"
        );
        let message = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_lowercase();
        assert!(
            message.contains("unknown") && message.contains("query"),
            "{message}"
        );
    }
}

#[test]
fn chunk_validator_rejects_unknown_duplicate_and_mismatched_fields() {
    let bytes = b"chunk";
    let binding = UploadBinding::for_payload(
        Operation::UpdateRef {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: None,
            force: false,
        },
        bytes,
    )
    .unwrap();
    let identity = chunk(51, "alice", "project", &id(105), &binding, 0, bytes);
    let valid = json!({"kind":"chunk_identity", "chunk":identity});
    validate(valid.clone());
    let mut unknown = valid.clone();
    unknown["chunk"]["scope"]["extra"] = json!(true);
    reject_validation(&serde_json::to_vec(&unknown).unwrap());
    let serialized = serde_json::to_string(&valid).unwrap();
    let duplicate = serialized.replacen(
        "\"operation_id\":",
        &format!("\"operation_id\":\"{}\",\"operation_id\":", id(51)),
        1,
    );
    assert_ne!(duplicate, serialized);
    reject_validation(duplicate.as_bytes());
    let mut wrong = identity.clone();
    wrong["scope"]["actor"] = json!("bob");
    let bad_status = json!({"kind":"chunk_status", "chunk":identity,
        "status":{"version":1, "chunk":wrong, "state":"Unobserved"}});
    reject_validation(&serde_json::to_vec(&bad_status).unwrap());
}

#[test]
fn lost_chunk_reply_can_be_queried_after_expiry_and_cancel() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let head = repo.commit(WASM, "base").unwrap();
    Repository::init(temp.0.join("other")).unwrap();
    set_policy(&temp.0, 0, true, true);
    let server = Server::start(&temp.0);

    let bytes = vec![42; protocol::TRANSFER_CHUNK + 17];
    let operation = Operation::UpdateRef {
        namespace: RefNamespace::Branch,
        name: "topic".into(),
        expected: None,
        new: Some(head),
        force: false,
    };
    let binding = UploadBinding::for_payload(operation, &bytes).unwrap();
    let session = id(100);
    let first = chunk(
        1,
        "alice",
        "project",
        &session,
        &binding,
        0,
        &bytes[..protocol::TRANSFER_CHUNK],
    );
    let first_request = upload_request(&first);
    let expiry = now() + 10;
    // Neither a replacement original operation id nor another payload digest
    // may mint a credential for this identity.
    assert!(!command(
        &temp.0,
        "operation-credential-issue",
        &issuance(
            9,
            "alice",
            "wrong-id",
            first_request.clone(),
            &bytes[..protocol::TRANSFER_CHUNK],
            now() + 120
        )
    )
    .status
    .success());
    assert!(!command(
        &temp.0,
        "operation-credential-issue",
        &issuance(
            1,
            "alice",
            "wrong-payload",
            first_request.clone(),
            b"different bytes",
            now() + 120
        )
    )
    .status
    .success());
    issue(
        &temp.0,
        &issuance(
            1,
            "alice",
            "first-upload",
            first_request.clone(),
            &bytes[..protocol::TRANSFER_CHUNK],
            expiry,
        ),
    );
    let mut replacement = first.clone();
    replacement["operation_id"] = json!(id(8));
    assert!(!command(
        &temp.0,
        "operation-credential-issue",
        &issuance(
            8,
            "alice",
            "replacement-id",
            upload_request(&replacement),
            &bytes[..protocol::TRANSFER_CHUNK],
            now() + 120
        )
    )
    .status
    .success());
    let mut changed_bytes = bytes[..protocol::TRANSFER_CHUNK].to_vec();
    changed_bytes[0] ^= 1;
    replacement["payload_sha256"] = json!(hash(&changed_bytes));
    assert!(!command(
        &temp.0,
        "operation-credential-issue",
        &issuance(
            8,
            "alice",
            "replacement-bytes",
            upload_request(&replacement),
            &changed_bytes,
            now() + 120
        )
    )
    .status
    .success());
    lose_reply(
        &server.address,
        "first-upload",
        &first_request,
        &bytes[..protocol::TRANSFER_CHUNK],
    );

    // This operation was issued but never sent. Unobserved is not a terminal
    // decision and must remain distinct from an accepted or cleaned chunk.
    let second = chunk(
        2,
        "alice",
        "project",
        &session,
        &binding,
        protocol::TRANSFER_CHUNK as u64,
        &bytes[protocol::TRANSFER_CHUNK..],
    );
    let second_request = upload_request(&second);
    issue(
        &temp.0,
        &issuance(
            2,
            "alice",
            "second-upload",
            second_request.clone(),
            &bytes[protocol::TRANSFER_CHUNK..],
            now() + 120,
        ),
    );
    let second_query = query_request(&second);
    issue(
        &temp.0,
        &issuance(201, "alice", "second-query", second_query, &[], now() + 120),
    );
    assert_status(
        query_cli(&server.address, "project", "second-query", &second),
        &second,
        json!("Unobserved"),
    );

    let legacy = request(
        "project",
        json!({"ScopedUploadSlice":{
        "session":session, "binding":binding, "offset":0}}),
    );
    assert!(!command(
        &temp.0,
        "operation-credential-issue",
        &issuance(
            3,
            "alice",
            "legacy-slice",
            legacy.clone(),
            &bytes[..protocol::TRANSFER_CHUNK],
            now() + 120
        )
    )
    .status
    .success());
    rpc(
        &server.address,
        "second-upload",
        &legacy,
        &bytes[..protocol::TRANSFER_CHUNK],
    )
    .denied();

    wait_until(expiry);
    assert_unauthenticated_token(&server.address, "first-upload");
    drop(server);
    let server = Server::start(&temp.0);
    let query = query_request(&first);
    issue(
        &temp.0,
        &issuance(200, "alice", "first-query", query, &[], now() + 120),
    );
    assert_status(
        query_cli(&server.address, "project", "first-query", &first),
        &first,
        json!({"Accepted":{"data":"Available"}}),
    );

    let mut altered = first.clone();
    altered["operation_id"] = json!(id(700));
    altered["scope"]["actor"] = json!("bob");
    let absent = assert_claim_denied(&temp.0, &server.address, &altered, "bob", 210);
    altered = first.clone();
    altered["scope"]["actor"] = json!("bob");
    let other_actor = assert_claim_denied(&temp.0, &server.address, &altered, "bob", 211);
    assert_eq!(
        absent, other_actor,
        "query must not disclose another actor's original operation"
    );
    altered = first.clone();
    altered["scope"]["repository"] = json!("other");
    assert_claim_denied(&temp.0, &server.address, &altered, "alice", 212);
    altered = first.clone();
    altered["payload_sha256"] = json!(id(701));
    assert_claim_denied(&temp.0, &server.address, &altered, "alice", 213);
    altered = first.clone();
    altered["scope"]["binding"]["digest"] = json!(id(702));
    assert_claim_denied(&temp.0, &server.address, &altered, "alice", 214);
    altered = first.clone();
    altered["scope"]["session"] = json!(id(703));
    assert_claim_denied(&temp.0, &server.address, &altered, "alice", 215);
    altered = first.clone();
    altered["offset"] = json!(protocol::TRANSFER_CHUNK as u64);
    assert_claim_denied(&temp.0, &server.address, &altered, "alice", 216);

    let cancel = request(
        "project",
        json!({"CancelScopedUpload":{"session":session, "binding":binding}}),
    );
    issue(
        &temp.0,
        &issuance(
            300,
            "alice",
            "cancel-upload",
            cancel.clone(),
            &[],
            now() + 120,
        ),
    );
    let response = rpc(&server.address, "cancel-upload", &cancel, &[])
        .packet()
        .0;
    assert!(response.ok, "{response:?}");
    assert_status(
        query_cli(&server.address, "project", "first-query", &first),
        &first,
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Canceled"}}}}),
    );
    // The second original credential is still valid, but a delayed original
    // request cannot recreate data in a canceled upload.
    rpc(
        &server.address,
        "second-upload",
        &second_request,
        &bytes[protocol::TRANSFER_CHUNK..],
    )
    .denied();
    assert_status(
        query_cli(&server.address, "project", "second-query", &second),
        &second,
        json!("Unobserved"),
    );
    assert_status(
        query_cli(&server.address, "project", "first-query", &first),
        &first,
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Canceled"}}}}),
    );

    // A query requires current authorization, but a new query credential can
    // still recover the original generation-one, expired operation afterward.
    set_policy(&temp.0, 1, false, false);
    assert_unauthenticated_token(&server.address, "first-query");
    set_policy(&temp.0, 2, true, true);
    assert_unauthenticated_token(&server.address, "first-query");
    let mut renewed_query = issuance(
        301,
        "alice",
        "generation-three-query",
        query_request(&first),
        &[],
        now() + 120,
    );
    renewed_query.policy_generation = 3;
    issue(&temp.0, &renewed_query);
    assert_status(
        query_cli(&server.address, "project", "generation-three-query", &first),
        &first,
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Canceled"}}}}),
    );
}

#[test]
fn completed_apply_keeps_chunk_tombstone_and_prevents_rewrite() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let base = repo.commit(WASM, "base").unwrap();
    let fork = Repository::fork_local(temp.0.join("project"), temp.0.join("fork")).unwrap();
    let mut next = WASM.to_vec();
    next.extend([0, 3, 1, b'x', 1]);
    let head = fork.commit(&next, "feature").unwrap();
    let bytes = fork
        .network_bundle(std::slice::from_ref(&head), std::slice::from_ref(&base))
        .unwrap()
        .1;
    assert!(bytes.len() <= protocol::TRANSFER_CHUNK);
    let binding = UploadBinding::for_payload(
        Operation::UpdateRef {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: Some(head.clone()),
            force: false,
        },
        &bytes,
    )
    .unwrap();
    let session = id(101);
    set_policy(&temp.0, 0, true, true);
    let server = Server::start(&temp.0);

    let original = chunk(11, "alice", "project", &session, &binding, 0, &bytes);
    let upload = upload_request(&original);
    issue(
        &temp.0,
        &issuance(
            11,
            "alice",
            "completed-upload",
            upload.clone(),
            &bytes,
            now() + 120,
        ),
    );
    assert_status(
        chunk_cli(
            &server.address,
            "project",
            "completed-upload",
            json!({"Chunk":{"chunk":original, "bytes":bytes}}),
        ),
        &original,
        json!({"Accepted":{"data":"Available"}}),
    );
    let query = query_request(&original);
    issue(
        &temp.0,
        &issuance(202, "alice", "completed-query", query, &[], now() + 120),
    );
    assert_status(
        query_cli(&server.address, "project", "completed-query", &original),
        &original,
        json!({"Accepted":{"data":"Available"}}),
    );

    let apply = request(
        "project",
        json!({"ApplyScopedUpload":{"session":session, "binding":binding}}),
    );
    let original_apply = issuance(12, "alice", "apply-upload", apply.clone(), &[], now() + 120);
    issue(&temp.0, &original_apply);
    let response = rpc(&server.address, "apply-upload", &apply, &[]).packet().0;
    assert!(response.ok, "{response:?}");
    assert_eq!(repo.resolve("topic").unwrap(), head);
    assert_status(
        query_cli(&server.address, "project", "completed-query", &original),
        &original,
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Completed"}}}}),
    );
    // Idempotent replay returns the original cleaned fact and never recreates
    // the transferred bytes.
    let (response, payload) = rpc(&server.address, "completed-upload", &upload, &bytes).packet();
    assert!(response.ok, "{response:?}");
    let replay: ScopedChunkStatus = binary::decode(&payload).unwrap();
    assert_status(
        serde_json::to_value(replay).unwrap(),
        &original,
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Completed"}}}}),
    );
    assert_status(
        query_cli(&server.address, "project", "completed-query", &original),
        &original,
        json!({"Accepted":{"data":{"Cleaned":{"reason":"Completed"}}}}),
    );
    #[cfg(target_os = "linux")]
    {
        set_policy(&temp.0, 1, false, false);
        let result = recovery(&temp.0, 990, &original_apply, true, vec![]);
        assert_eq!(
            result["state"]["Upload"]["state"]["Finished"]["receipt"]["response"]["ok"],
            true
        );
        assert_eq!(repo.resolve("topic").unwrap(), head);
    }
}

#[test]
fn current_read_grant_can_query_own_chunk_but_not_write_another() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let head = repo.commit(WASM, "base").unwrap();
    set_policy(&temp.0, 0, true, true);
    let server = Server::start(&temp.0);
    let bytes = b"a scoped upload chunk";
    let binding = UploadBinding::for_payload(
        Operation::UpdateRef {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: Some(head),
            force: false,
        },
        bytes,
    )
    .unwrap();
    let session = id(102);
    let original = chunk(31, "alice", "project", &session, &binding, 0, bytes);
    let upload = upload_request(&original);
    issue(
        &temp.0,
        &issuance(
            31,
            "alice",
            "read-test-upload",
            upload.clone(),
            bytes,
            now() + 120,
        ),
    );
    let response = rpc(&server.address, "read-test-upload", &upload, bytes)
        .packet()
        .0;
    assert!(response.ok, "{response:?}");

    set_policy(&temp.0, 1, true, false);
    assert_unauthenticated_token(&server.address, "read-test-upload");
    let query = query_request(&original);
    let mut credential = issuance(32, "alice", "read-test-query", query, &[], now() + 120);
    credential.policy_generation = 2;
    issue(&temp.0, &credential);
    assert_status(
        query_cli(&server.address, "project", "read-test-query", &original),
        &original,
        json!({"Accepted":{"data":"Available"}}),
    );

    let scope_query = request(
        "project",
        json!({"QueryScopedUpload":{"session":session, "binding":binding}}),
    );
    let mut credential = issuance(
        34,
        "alice",
        "read-test-scope",
        scope_query.clone(),
        &[],
        now() + 120,
    );
    credential.policy_generation = 2;
    issue(&temp.0, &credential);
    let response = rpc(&server.address, "read-test-scope", &scope_query, &[])
        .packet()
        .0;
    assert!(response.ok, "{response:?}");

    let next = chunk(33, "alice", "project", &id(103), &binding, 0, bytes);
    let next_upload = upload_request(&next);
    let mut credential = issuance(
        33,
        "alice",
        "read-test-denied",
        next_upload.clone(),
        bytes,
        now() + 120,
    );
    credential.policy_generation = 2;
    issue(&temp.0, &credential);
    rpc(&server.address, "read-test-denied", &next_upload, bytes).denied();
    assert_status(
        query_cli(&server.address, "project", "read-test-query", &original),
        &original,
        json!({"Accepted":{"data":"Available"}}),
    );
}
