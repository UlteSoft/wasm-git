#![cfg(all(feature = "client", feature = "server", unix))]
//! Native policy decisions and the cross-process fence use real servers/stdio.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use wasm_git::{
    protocol::{self, Operation, RefNamespace, Request, Response},
    repository::Repository,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
const WASM: &[u8] = b"\0asm\x01\0\0\0";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "wasm-git-policy-fence-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn policy(write: bool, read: bool) -> Value {
    json!({"version":3,"principals":[{"name":"alice","token_sha256":format!("{:x}",Sha256::digest(b"alice")),
        "grants":[{"repository":"project","read":read,"write":if write { vec!["refs/heads/*"] } else { vec![] },
          "force":false,"create":false}]}]})
}
fn update(id: char, generation: u64, policy: Value) -> Value {
    json!({"version":1,"operation_id":id.to_string().repeat(64),"expected_generation":generation,"policy":policy})
}
fn input(root: &Path, name: &str, value: &Value) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    path
}
fn server_command(command: &str, root: &Path, input: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    cmd.args([
        "--format",
        "json",
        command,
        root.to_str().unwrap(),
        input.to_str().unwrap(),
    ]);
    cmd
}
fn validate(value: Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(["--format", "json", "integration-validate"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
fn set(root: &Path, name: &str, request: &Value) -> Value {
    let file = input(root, name, request);
    let output = success(server_command("policy-set", root, &file).output().unwrap());
    validate(
        json!({"kind":"policy_update_cli_reply","request":request,"process_exit":0,"response":output}),
    );
    output
}
fn query(root: &Path, request: &Value) -> Value {
    let file = input(
        root,
        "query.json",
        &json!({"version":1,"operation_id":request["operation_id"]}),
    );
    let output = success(
        server_command("policy-query", root, &file)
            .output()
            .unwrap(),
    );
    validate(
        json!({"kind":"policy_query_cli_reply","request":request,"process_exit":0,"response":output}),
    );
    output
}
struct Server {
    child: Child,
    address: String,
}
impl Server {
    fn start(root: &Path, hook: Option<&Path>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
        cmd.args([
            "--root",
            root.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--allow-write",
            "--policy",
            root.join("unused-bootstrap").to_str().unwrap(),
        ]);
        if let Some(hook) = hook {
            cmd.arg("--pre-receive").arg(hook);
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        Self {
            child,
            address: value["listen"].as_str().unwrap().into(),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn packet(op: Operation) -> Vec<u8> {
    let mut bytes = Vec::new();
    protocol::write_packet(
        &mut bytes,
        &Request {
            version: 1,
            repository: "project".into(),
            operation: op,
        },
        &[],
    )
    .unwrap();
    bytes
}
fn rpc(address: &str, op: Operation) -> Response {
    let body = packet(op);
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(stream, "POST /v1/wasm-git HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer alice\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
    stream.write_all(&body).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    let offset = reply.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
    protocol::read_complete_packet::<Response>(&reply[offset..])
        .unwrap()
        .0
}
fn reference(name: &str, tip: &str) -> Operation {
    Operation::UpdateRef {
        namespace: RefNamespace::Branch,
        name: name.into(),
        expected: None,
        new: Some(tip.into()),
        force: false,
    }
}
fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn hook(root: &Path) -> PathBuf {
    let path = root.join("hook.sh");
    fs::write(&path, b"#!/bin/sh\ntouch \"$WASM_GIT_REPOSITORY/../hook-started\"\nfor i in $(seq 1 500); do if test -f \"$WASM_GIT_REPOSITORY/../hook-release\"; then touch \"$WASM_GIT_REPOSITORY/../hook-finished\"; exit 0; fi; sleep 0.02; done\nexit 1\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[test]
fn fence_drains_existing_http_and_blocks_all_instances_and_stdio() {
    eprintln!("tested client: {}", env!("CARGO_BIN_EXE_wasm-git"));
    eprintln!("tested server: {}", env!("CARGO_BIN_EXE_wasm-git-server"));
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let tip = repo.commit(WASM, "base").unwrap();
    let first = update('a', 0, policy(true, true));
    assert_eq!(
        set(&temp.0, "first.json", &first)["result"]["value"]["outcome"],
        "applied"
    );
    let hook = hook(&temp.0);
    let one = Server::start(&temp.0, Some(&hook));
    let two = Server::start(&temp.0, None);
    let address = one.address.clone();
    let old_tip = tip.clone();
    let active = std::thread::spawn(move || rpc(&address, reference("in-flight", &old_tip)));
    wait_for(&temp.0.join("hook-started"));
    let revoked = update('b', 1, policy(false, true));
    let file = input(&temp.0, "revoke.json", &revoked);
    let mut pending = server_command("policy-set", &temp.0, &file)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(pending.try_wait().unwrap().is_none());
    fs::write(temp.0.join("hook-release"), b"release").unwrap();
    assert!(active.join().unwrap().ok);
    let receipt = success(pending.wait_with_output().unwrap());
    assert_eq!(receipt["result"]["value"]["outcome"], "applied");
    validate(
        json!({"kind":"policy_update_cli_reply","request":revoked,"process_exit":0,"response":receipt}),
    );
    assert!(!rpc(&two.address, reference("after-fence", &tip)).ok);
    assert!(!rpc(&one.address, reference("also-denied", &tip)).ok);
    assert!(Repository::open(temp.0.join("project"))
        .unwrap()
        .resolve("after-fence")
        .is_err());

    let mut ssh = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--stdio",
            "--root",
            temp.0.to_str().unwrap(),
            "--policy",
            temp.0.join("unused-bootstrap").to_str().unwrap(),
            "--principal",
            "alice",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    ssh.stdin
        .take()
        .unwrap()
        .write_all(&packet(reference("stdio-denied", &tip)))
        .unwrap();
    let output = ssh.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        !protocol::read_complete_packet::<Response>(&output.stdout)
            .unwrap()
            .0
            .ok
    );

    // Replaying an old decision after a newer generation must not install it.
    let third = update('c', 2, policy(false, false));
    set(&temp.0, "third.json", &third);
    assert_eq!(
        query(&temp.0, &revoked)["result"]["value"]["receipt"]["generation"],
        2
    );
    let replay = set(&temp.0, "replay.json", &revoked);
    assert_eq!(replay["result"]["value"]["generation"], 2);
    let conflict = update('b', 2, policy(true, true));
    let file = input(&temp.0, "conflict.json", &conflict);
    assert!(!server_command("policy-set", &temp.0, &file)
        .output()
        .unwrap()
        .status
        .success());
    let mut status = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
    let status = success(
        status
            .args([
                "--format",
                "json",
                "policy-status",
                temp.0.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(status["result"]["value"]["generation"], 3);
    validate(json!({"kind":"policy_status_cli_reply","process_exit":0,"response":status}));
    assert!(!rpc(&two.address, Operation::Advertise {}).ok);

    let token = temp.0.join("admin-token");
    fs::write(&token, b"admin").unwrap();
    let bypass = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--root",
            temp.0.to_str().unwrap(),
            "--token-file",
            token.to_str().unwrap(),
            "--allow-write",
        ])
        .output()
        .unwrap();
    assert!(!bypass.status.success());
}

#[test]
fn generation_conflict_is_a_durable_identity_bound_decision() {
    let temp = Temp::new();
    let stale = update('d', 1, policy(true, true));
    let rejected = set(&temp.0, "stale.json", &stale);
    assert_eq!(
        rejected["result"]["value"]["outcome"],
        "generation_conflict"
    );
    assert_eq!(rejected["result"]["value"]["generation"], 0);
    assert_eq!(
        query(&temp.0, &stale)["result"]["value"]["receipt"]["outcome"],
        "generation_conflict"
    );
    let changed = update('d', 0, policy(true, true));
    let file = input(&temp.0, "changed.json", &changed);
    assert!(!server_command("policy-set", &temp.0, &file)
        .output()
        .unwrap()
        .status
        .success());
    let initial = update('e', 0, policy(false, true));
    assert_eq!(
        set(&temp.0, "initial.json", &initial)["result"]["value"]["outcome"],
        "applied"
    );
    assert_eq!(
        set(&temp.0, "retry.json", &stale)["result"]["value"],
        rejected["result"]["value"]
    );
}

#[test]
fn bootstrap_receipt_survives_lost_readiness_and_later_revisions() {
    let temp = Temp::new();
    let initial_policy = policy(true, true);
    let input = input(&temp.0, "bootstrap.json", &initial_policy);
    let compiled = temp.0.join("bootstrap.bin");
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "compile-policy",
            input.to_str().unwrap(),
            compiled.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let operation = format!("{:x}", Sha256::digest(fs::read(&compiled).unwrap()));
    let original = json!({"version":1,"operation_id":operation,"expected_generation":0,"policy":initial_policy});
    // EOF after initialization models a lost server readiness/reply.
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--stdio",
            "--root",
            temp.0.to_str().unwrap(),
            "--policy",
            compiled.to_str().unwrap(),
            "--principal",
            "alice",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        query(&temp.0, &original)["result"]["value"]["receipt"]["outcome"],
        "applied"
    );
    set(&temp.0, "next.json", &update('f', 1, policy(false, true)));
    assert_eq!(
        query(&temp.0, &original)["result"]["value"]["receipt"]["generation"],
        1
    );
}

#[test]
fn fence_waits_for_stdio_response_output() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let mut module = WASM.to_vec();
    let size = 2 * 1024 * 1024;
    module.push(0);
    let mut n = size + 2;
    loop {
        let byte = (n & 127) as u8;
        n >>= 7;
        module.push(byte | if n != 0 { 128 } else { 0 });
        if n == 0 {
            break;
        }
    }
    module.extend_from_slice(b"\x01x");
    let mut random = 0x1234_5678u32;
    for _ in 0..size {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        module.push(random as u8);
    }
    repo.commit(&module, "large").unwrap();
    set(&temp.0, "first.json", &update('a', 0, policy(true, true)));
    let mut ssh = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--stdio",
            "--root",
            temp.0.to_str().unwrap(),
            "--policy",
            temp.0.join("unused-bootstrap").to_str().unwrap(),
            "--principal",
            "alice",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    ssh.stdin
        .take()
        .unwrap()
        .write_all(&packet(Operation::Fetch {}))
        .unwrap();
    let mut output = ssh.stdout.take().unwrap();
    let mut first_byte = [0u8; 1];
    output.read_exact(&mut first_byte).unwrap();
    let revoked = update('b', 1, policy(false, false));
    let file = input(&temp.0, "revoke.json", &revoked);
    let mut pending = server_command("policy-set", &temp.0, &file)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(pending.try_wait().unwrap().is_none());
    let mut response = first_byte.to_vec();
    output.read_to_end(&mut response).unwrap();
    assert!(ssh.wait().unwrap().success());
    assert!(
        protocol::read_complete_packet::<Response>(&response)
            .unwrap()
            .0
            .ok
    );
    let receipt = success(pending.wait_with_output().unwrap());
    validate(
        json!({"kind":"policy_update_cli_reply","request":revoked,"process_exit":0,"response":receipt}),
    );
}

#[test]
fn crashed_worker_cannot_publish_after_fence() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let tip = repo.commit(WASM, "base").unwrap();
    set(&temp.0, "first.json", &update('a', 0, policy(true, true)));
    let hook = hook(&temp.0);
    let mut one = Server::start(&temp.0, Some(&hook));
    let two = Server::start(&temp.0, None);
    let address = one.address.clone();
    let thread = std::thread::spawn(move || {
        let body = packet(reference("crashed", &tip));
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "POST /v1/wasm-git HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer alice\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
    });
    wait_for(&temp.0.join("hook-started"));
    for name in ["index.lock", "write.lock"] {
        let held = fs::OpenOptions::new().read(true).write(true)
            .open(temp.0.join("project/.wasm-git").join(name)).unwrap();
        assert!(held.try_lock().is_err(), "{name} must exclude another process");
    }
    one.child.kill().unwrap();
    one.child.wait().unwrap();
    fs::write(temp.0.join("hook-release"), b"release").unwrap();
    wait_for(&temp.0.join("hook-finished"));
    thread.join().unwrap();
    let error = Repository::open(temp.0.join("project"))
        .and_then(|r| r.resolve("crashed"))
        .unwrap_err();
    assert!(!error.contains("locked"));
    assert!(rpc(&two.address, reference("after-crash", &repo.resolve("HEAD").unwrap())).ok);
    set(&temp.0, "revoke.json", &update('b', 1, policy(false, true)));
    assert!(
        !rpc(
            &two.address,
            reference("crashed", &repo.resolve("HEAD").unwrap())
        )
        .ok
    );
}

#[test]
fn killed_stdio_receive_releases_repository_and_index_locks() {
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let tip = repo.commit(WASM, "base").unwrap();
    set(&temp.0, "first.json", &update('a', 0, policy(true, true)));
    let hook = hook(&temp.0);
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args(["--stdio", "--root", temp.0.to_str().unwrap(), "--policy", temp.0.join("unused-bootstrap").to_str().unwrap(), "--principal", "alice", "--pre-receive", hook.to_str().unwrap()])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&packet(reference("stdio-crashed", &tip))).unwrap();
    let mut process = Process(child);
    wait_for(&temp.0.join("hook-started"));
    for name in ["index.lock", "write.lock"] {
        let held = fs::OpenOptions::new().read(true).write(true)
            .open(temp.0.join("project/.wasm-git").join(name)).unwrap();
        assert!(held.try_lock().is_err(), "{name} must exclude another process");
    }
    process.0.kill().unwrap();
    assert!(!process.0.wait().unwrap().success());
    fs::write(temp.0.join("hook-release"), b"release").unwrap();
    wait_for(&temp.0.join("hook-finished"));
    let error = Repository::open(temp.0.join("project")).and_then(|repo| repo.resolve("stdio-crashed")).unwrap_err();
    assert!(!error.contains("locked"));

    let mut retry = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args(["--stdio", "--root", temp.0.to_str().unwrap(), "--policy", temp.0.join("unused-bootstrap").to_str().unwrap(), "--principal", "alice"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    retry.stdin.take().unwrap().write_all(&packet(reference("stdio-after-crash", &tip))).unwrap();
    let output = retry.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(protocol::read_complete_packet::<Response>(&output.stdout).unwrap().0.ok);
}

#[test]
fn grant_expiring_during_hook_is_checked_before_reference_journal() {
    let temp = Temp::new();
    let repo = Repository::init(temp.0.join("project")).unwrap();
    let tip = repo.commit(WASM, "base").unwrap();
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 2;
    let mut expiring = policy(true, true);
    expiring["principals"][0]["grants"][0]["expires_at"] = json!(expiry);
    set(&temp.0, "first.json", &update('a', 0, expiring));
    let hook = hook(&temp.0);
    let server = Server::start(&temp.0, Some(&hook));
    let address = server.address.clone();
    let thread = std::thread::spawn(move || rpc(&address, reference("expired", &tip)));
    wait_for(&temp.0.join("hook-started"));
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        < expiry
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::write(temp.0.join("hook-release"), b"release").unwrap();
    let response = thread.join().unwrap();
    assert!(!response.ok);
    assert!(!response.publication_unknown);
    assert!(response.error.unwrap().contains("authorization unavailable before delivery"));
    assert!(Repository::open(temp.0.join("project"))
        .unwrap()
        .resolve("expired")
        .is_err());
}
