#![cfg(all(feature = "client", feature = "server"))]
//! Hosting regressions exercise actual HTTP/stdio services and native frames.
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Barrier,
    },
    time::{Duration, Instant},
};
use wasm_git::{
    binary,
    protocol::{self, BatchPublication, Operation, RefNamespace, RefUpdate, Request, Response},
    repository::{FetchOptions, ReferenceEdit, ReferenceEditKind, Repository},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
const WASM: &[u8] = b"\0asm\x01\0\0\0";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "uwgit-hosting-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Server {
    child: Child,
    address: String,
}
impl Server {
    fn start(root: &Path, extra: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
        command.args(["--root", root.to_str().unwrap(), "--listen", "127.0.0.1:0"]);
        if !extra.contains(&"--policy") {
            let token = root.join("token");
            fs::write(&token, b"hosting-test").unwrap();
            command.args(["--token-file", token.to_str().unwrap()]);
        }
        let mut child = command
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        Self {
            child,
            address: value["listen"].as_str().unwrap().into(),
        }
    }
    fn rpc(&self, repo: &str, op: Operation, payload: &[u8]) -> (Response, Vec<u8>) {
        rpc(&self.address, "hosting-test", repo, op, payload)
    }
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(args)
            .env("UWGIT_REMOTE_TOKEN", "hosting-test")
            .output()
            .unwrap()
    }
    fn url(&self, name: &str) -> String {
        format!("http://{}/{name}", self.address)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn rpc(
    address: &str,
    token: &str,
    repo: &str,
    operation: Operation,
    payload: &[u8],
) -> (Response, Vec<u8>) {
    let mut body = Vec::new();
    protocol::write_packet(
        &mut body,
        &Request {
            version: 1,
            repository: repo.into(),
            operation,
        },
        payload,
    )
    .unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(stream,"POST /v1/uwgit HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",body.len()).unwrap();
    stream.write_all(&body).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    assert!(
        reply.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    let begin = reply.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
    protocol::read_complete_packet(&reply[begin..]).unwrap()
}
fn success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn update(name: &str, old: Option<&str>, new: Option<&str>) -> RefUpdate {
    RefUpdate {
        namespace: RefNamespace::Branch,
        name: name.into(),
        expected: old.map(str::to_owned),
        new: new.map(str::to_owned),
        force: false,
    }
}
fn batch(updates: Vec<RefUpdate>, bundle: Vec<u8>) -> Vec<u8> {
    binary::encode(&BatchPublication { updates, bundle }).unwrap()
}
fn custom(size: usize, byte: u8) -> Vec<u8> {
    let mut out = WASM.to_vec();
    out.push(0);
    let mut n = size + 2;
    loop {
        let b = (n & 127) as u8;
        n >>= 7;
        out.push(b | if n != 0 { 128 } else { 0 });
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(b"\x01x");
    out.resize(out.len() + size, byte);
    out
}

#[test]
fn concurrent_readers_succeed_and_a_slow_peer_does_not_block_later_arrivals() {
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    repo.commit(&custom(2 * 1024 * 1024, 7), "base").unwrap();
    let server = Server::start(&t.0, &[]);
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let address = server.address.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                assert!(
                    rpc(
                        &address,
                        "hosting-test",
                        "project",
                        Operation::Advertise {},
                        &[]
                    )
                    .0
                    .ok
                )
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap()
    }
    let slow = TcpStream::connect(&server.address).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    for _ in 0..15 {
        assert!(server.rpc("project", Operation::Advertise {}, &[]).0.ok)
    }
    let start = Instant::now();
    assert!(server.rpc("project", Operation::Advertise {}, &[]).0.ok);
    assert!(start.elapsed() < Duration::from_secs(5));
    drop(slow);
    assert_eq!(
        fs::read_dir(t.0.join("project/.uwgit/tmp"))
            .unwrap()
            .count(),
        0,
        "read-only requests wrote temporary state"
    );
}

#[test]
fn thousands_of_refs_are_paged_cloned_and_forked_without_large_acknowledgements() {
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    let id = repo.commit(WASM, "base").unwrap();
    let edits: Vec<_> = (0..1100)
        .map(|n| ReferenceEdit {
            kind: ReferenceEditKind::Create,
            reference: format!("refs/heads/topic-{n:04}"),
            new: Some(id.clone()),
            expected: None,
            no_deref: false,
        })
        .collect();
    repo.prepare_reference_edits(&edits, "many refs")
        .unwrap()
        .commit()
        .unwrap();
    let server = Server::start(&t.0, &["--allow-write"]);
    let url = server.url("project");
    let output = success(server.cli(&["ls-remote", &url, "--json"]));
    assert!(output.contains("topic-1099"));
    success(server.cli(&["clone", &url, t.0.join("clone").to_str().unwrap()]));
    success(server.cli(&["fork", &url, "fork"]));
    for name in ["clone", "fork"] {
        let copy = Repository::open(t.0.join(name)).unwrap();
        assert_eq!(copy.state().unwrap().branches.len(), 1101);
        copy.fsck().unwrap();
    }
    let (first, raw) = server.rpc(
        "project",
        Operation::RefsPage {
            after: None,
            expected: None,
        },
        &[],
    );
    assert!(first.ok);
    let page: protocol::RefPage = binary::decode(&raw).unwrap();
    repo.create_branch("changed", &id).unwrap();
    let stale = server
        .rpc(
            "project",
            Operation::RefsPage {
                after: page.next,
                expected: Some(page.generation),
            },
            &[],
        )
        .0;
    assert!(!stale.ok);
    assert!(stale.error.unwrap().contains("changed"));
}

#[test]
fn negotiation_compresses_history_sends_no_known_objects_and_hides_unreachable_history() {
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    let base = repo.commit(&custom(2 * 1024 * 1024, 1), "base").unwrap();
    repo.create_branch("discarded", &base).unwrap();
    repo.switch_branch("discarded").unwrap();
    let secret = repo.commit(&custom(2 * 1024 * 1024, 99), "hidden").unwrap();
    repo.switch_branch("main").unwrap();
    repo.delete_branch("discarded", true).unwrap();
    let server = Server::start(&t.0, &[]);
    let options = binary::encode(&FetchOptions {
        selection: None,
        have: vec![],
    })
    .unwrap();
    let (reply, wire) = server.rpc("project", Operation::Fetch {}, &options);
    assert!(reply.ok);
    assert!(wire.len() < 10_000);
    let full = Repository::empty_network_bundle(&wire).unwrap();
    let client = Repository::clone_bundle_bytes(&full, t.0.join("client")).unwrap();
    assert!(client.resolve(&secret).is_err());
    assert_eq!(client.fsck().unwrap().commits, 1);
    let options = binary::encode(&FetchOptions {
        selection: None,
        have: client.negotiation_haves().unwrap(),
    })
    .unwrap();
    let (reply, thin) = server.rpc("project", Operation::Fetch {}, &options);
    assert!(reply.ok);
    assert!(thin.len() < 256, "unchanged fetch: {}", thin.len());
    let restored = client.complete_network_bundle(&thin).unwrap();
    assert_eq!(
        Repository::bundle_state(&restored).unwrap(),
        client.state().unwrap()
    );
    assert!(
        Repository::empty_network_bundle(&thin).is_err(),
        "thin transfer invented missing bases"
    );
    let next = repo.commit(&custom(2 * 1024 * 1024, 2), "next").unwrap();
    let (reply, wire) = server.rpc("project", Operation::Fetch {}, &options);
    assert!(reply.ok);
    let full = client.complete_network_bundle(&wire).unwrap();
    client.install_bundle_bytes(&full).unwrap();
    assert_eq!(Repository::bundle_state(&full).unwrap().tip(), Some(&next));
    client.fsck().unwrap();
}

#[test]
fn terminal_upload_corruption_is_removed_reservations_are_bounded_and_cancel_is_explicit() {
    let t = Temp::new();
    Repository::init(t.0.join("project")).unwrap();
    let server = Server::start(&t.0, &["--allow-write"]);
    let bytes = vec![0; protocol::TRANSFER_CHUNK];
    let id = "1".repeat(64);
    assert!(
        server
            .rpc(
                "project",
                Operation::UploadSlice {
                    transfer: id.clone(),
                    total: bytes.len() as u64,
                    offset: 0
                },
                &bytes
            )
            .0
            .ok
    );
    let result = server
        .rpc(
            "project",
            Operation::ApplyUpload {
                transfer: id.clone(),
                operation: Box::new(Operation::Push {
                    branch: "main".into(),
                    expected: None,
                    force: false,
                }),
            },
            &[],
        )
        .0;
    assert!(!result.ok);
    assert!(result.error.unwrap().contains("checksum"));
    assert!(!t.0.join(format!("project/.uwgit/tmp/upload-{id}")).exists());
    let a = "2".repeat(64);
    let b = "3".repeat(64);
    let c = "4".repeat(64);
    for id in [&a, &b] {
        assert!(
            server
                .rpc(
                    "project",
                    Operation::UploadSlice {
                        transfer: id.clone(),
                        total: protocol::MAX_BUNDLE_BYTES,
                        offset: 0
                    },
                    &bytes
                )
                .0
                .ok
        )
    }
    let full = server
        .rpc(
            "project",
            Operation::UploadSlice {
                transfer: c.clone(),
                total: 1,
                offset: 0,
            },
            &[1],
        )
        .0;
    assert!(!full.ok);
    assert!(full.error.unwrap().contains("quota"));
    assert!(
        server
            .rpc("project", Operation::CancelUpload { transfer: a }, &[])
            .0
            .ok
    );
    assert!(
        server
            .rpc(
                "project",
                Operation::UploadSlice {
                    transfer: c,
                    total: 1,
                    offset: 0
                },
                &[1]
            )
            .0
            .ok
    );
}

#[test]
fn atomic_receive_rejects_every_ref_on_one_bad_lease_and_cli_publishes_multiple_refs() {
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    let base = repo.commit(WASM, "base").unwrap();
    let client = Repository::clone_local(repo.root.clone(), t.0.join("client")).unwrap();
    let next = client.commit(&custom(32, 7), "next").unwrap();
    let server = Server::start(&t.0, &["--allow-write"]);
    let bundle = client
        .network_bundle(std::slice::from_ref(&next), std::slice::from_ref(&base))
        .unwrap()
        .1;
    let bad = batch(
        vec![
            update("main", Some(&base), Some(&next)),
            update("topic", Some(&base), Some(&next)),
        ],
        bundle.clone(),
    );
    let result = server.rpc("project", Operation::PublishBatch {}, &bad).0;
    assert!(!result.ok);
    assert!(!result.publication_unknown);
    assert_eq!(repo.state().unwrap().tip(), Some(&base));
    assert!(repo.resolve(&next).is_err());
    let good = batch(
        vec![
            update("main", Some(&base), Some(&next)),
            update("topic", None, Some(&next)),
        ],
        bundle,
    );
    let result = server.rpc("project", Operation::PublishBatch {}, &good).0;
    assert!(result.ok, "{:?}", result.error);
    assert_eq!(result.tip, Some(format!("{:x}", Sha256::digest(&good))));
    assert_eq!(repo.resolve("topic").unwrap(), next);
    let root = client.root.to_str().unwrap();
    success(server.cli(&["remote", root, "add", "origin", &server.url("project")]));
    success(server.cli(&[
        "push",
        root,
        "origin",
        "HEAD:another",
        "HEAD:refs/tags/release",
        "--atomic",
    ]));
    assert_eq!(repo.resolve("another").unwrap(), next);
    assert_eq!(repo.resolve("refs/tags/release").unwrap(), next);
    let audit = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args(["audit-log", t.0.join("receive-audit.bin").to_str().unwrap()])
        .output()
        .unwrap();
    let rows: serde_json::Value = serde_json::from_str(&success(audit)).unwrap();
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["outcome"] == "accepted" && r["actor"] == "token"));
}

#[test]
fn policy_enforces_principals_repo_ref_force_and_fork_scopes_over_http_and_stdio() {
    let t = Temp::new();
    for name in ["alice.project", "bob.project"] {
        Repository::init(t.0.join(name))
            .unwrap()
            .commit(WASM, "base")
            .unwrap();
    }
    let input = t.0.join("policy-input.json");
    let output = t.0.join("policy.bin");
    fs::write(&input,serde_json::to_vec(&json!({"version":1,"principals":[{"name":"alice","token_sha256":format!("{:x}",Sha256::digest(b"hosting-test")),"grants":[{"repository":"alice.*","read":true,"write":["refs/heads/topic*"],"force":false,"create":true}]}]})).unwrap()).unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
            .args([
                "compile-policy",
                input.to_str().unwrap(),
                output.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    fs::remove_file(input).unwrap();
    assert!(fs::read(&output).unwrap().starts_with(b"UWGM"));
    let server = Server::start(
        &t.0,
        &[
            "--policy",
            output.to_str().unwrap(),
            "--allow-write",
            "--allow-force",
        ],
    );
    assert!(
        server
            .rpc("alice.project", Operation::Advertise {}, &[])
            .0
            .ok
    );
    assert!(!server.rpc("bob.project", Operation::Advertise {}, &[]).0.ok);
    let alice = Repository::open(t.0.join("alice.project")).unwrap();
    let tip = alice.resolve("HEAD").unwrap();
    let bundle = alice.bundle_bytes(Some(&tip)).unwrap().1;
    for (name, force, allowed) in [
        ("topic", false, true),
        ("main", false, false),
        ("topic-force", true, false),
    ] {
        let r = server
            .rpc(
                "alice.project",
                Operation::UpdateRef {
                    namespace: RefNamespace::Branch,
                    name: name.into(),
                    expected: None,
                    new: Some(tip.clone()),
                    force,
                },
                &bundle,
            )
            .0;
        assert_eq!(r.ok, allowed, "{:?}", r.error);
    }
    assert!(
        !server
            .rpc(
                "alice.project",
                Operation::Fork {
                    destination: "bob.fork".into()
                },
                &[]
            )
            .0
            .ok
    );
    assert!(
        server
            .rpc(
                "alice.project",
                Operation::Fork {
                    destination: "alice.fork".into()
                },
                &[]
            )
            .0
            .ok
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--stdio",
            "--root",
            t.0.to_str().unwrap(),
            "--policy",
            output.to_str().unwrap(),
            "--principal",
            "alice",
            "--read-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    protocol::write_packet(
        &mut child.stdin.take().unwrap(),
        &Request {
            version: 1,
            repository: "alice.project".into(),
            operation: Operation::Fork {
                destination: "alice.ssh".into(),
            },
        },
        &[],
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let (r, _) = protocol::read_complete_packet::<Response>(&out.stdout).unwrap();
    assert!(!r.ok);
    assert!(!t.0.join("alice.ssh").exists());
}

#[test]
#[cfg(unix)]
fn pre_receive_rejection_has_no_public_objects_and_post_receive_failure_preserves_success() {
    use std::os::unix::fs::PermissionsExt;
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    let base = repo.commit(WASM, "base").unwrap();
    let client = Repository::clone_local(&repo.root, t.0.join("client")).unwrap();
    let next = client.commit(&custom(64, 3), "next").unwrap();
    let script = t.0.join("reject.sh");
    fs::write(&script,b"#!/bin/sh\ntest -f \"$UWGIT_QUARANTINE_BUNDLE\" || exit 2\ntest -f \"$UWGIT_UPDATES_FILE\" || exit 3\nexit 1\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let bytes = batch(
        vec![update("main", Some(&base), Some(&next))],
        client
            .network_bundle(std::slice::from_ref(&next), std::slice::from_ref(&base))
            .unwrap()
            .1,
    );
    let server = Server::start(
        &t.0,
        &["--allow-write", "--pre-receive", script.to_str().unwrap()],
    );
    let rejected = server.rpc("project", Operation::PublishBatch {}, &bytes).0;
    assert!(!rejected.ok);
    assert!(!rejected.publication_unknown);
    assert!(repo.resolve(&next).is_err());
    assert_eq!(repo.resolve("HEAD").unwrap(), base);
    drop(server);
    assert!(fs::read_dir(t.0.join("project/.uwgit/tmp"))
        .unwrap()
        .all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("receive-")));
    let server = Server::start(
        &t.0,
        &["--allow-write", "--post-receive", script.to_str().unwrap()],
    );
    let accepted = server.rpc("project", Operation::PublishBatch {}, &bytes).0;
    assert!(accepted.ok, "{:?}", accepted.error);
    assert_eq!(repo.resolve("HEAD").unwrap(), next);
    repo.fsck().unwrap();
}

#[test]
fn small_changes_to_incompressible_wasm_use_verified_binary_delta_bases() {
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    let mut module = custom(2 * 1024 * 1024, 0);
    let mut random = 0x12345678_u32;
    for b in module.iter_mut().skip(16) {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *b = random as u8;
    }
    let base = repo.commit(&module, "random base").unwrap();
    let peer = Repository::clone_local(&repo.root, t.0.join("peer")).unwrap();
    for p in [12345, 890123, 1_900_001] {
        module[p] ^= 0x42;
    }
    let next = repo.commit(&module, "three small byte edits").unwrap();
    let (_, clone_wire) = repo.network_bundle(&[], &[]).unwrap();
    assert!(
        clone_wire.len() < module.len() + 65536,
        "clone duplicated similar historical modules"
    );
    let standalone = Repository::empty_network_bundle(&clone_wire).unwrap();
    let clone = Repository::clone_bundle_bytes(&standalone, t.0.join("standalone")).unwrap();
    assert_eq!(clone.resolve("HEAD").unwrap(), next);
    clone.fsck().unwrap();
    let (_, wire) = repo.network_bundle(&[], &[base]).unwrap();
    assert!(
        wire.len() < 4096,
        "small edits retransmitted the module: {}",
        wire.len()
    );
    assert!(Repository::empty_network_bundle(&wire)
        .unwrap_err()
        .contains("base"));
    let state = peer.install_network_bundle(&wire, None).unwrap();
    assert_eq!(state.tip(), Some(&next));
    // Verify by exporting the exact revision, independently of wire-size checks.
    let output = t.0.join("restored.wasm");
    let result = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args([
            "show",
            peer.root.to_str().unwrap(),
            &next,
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    success(result);
    assert_eq!(fs::read(output).unwrap(), module);
    peer.fsck().unwrap();
}

#[test]
fn a_lost_fork_receipt_is_unknown_even_when_the_repository_was_published() {
    let t = Temp::new();
    Repository::init(t.0.join("project"))
        .unwrap()
        .commit(WASM, "base")
        .unwrap();
    let server = Server::start(&t.0, &["--allow-write"]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/project", listener.local_addr().unwrap());
    let address = server.address.clone();
    let bridge = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            stream.read_exact(&mut b).unwrap();
            headers.push(b[0]);
            assert!(headers.len() < 16384);
        }
        let text = String::from_utf8(headers).unwrap();
        let length = text
            .lines()
            .find_map(|line| {
                let (k, v) = line.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let (request, payload): (Request, Vec<u8>) = protocol::read_complete_packet(&body).unwrap();
        assert!(
            rpc(
                &address,
                "hosting-test",
                &request.repository,
                request.operation,
                &payload
            )
            .0
            .ok
        );
        // Simulate a transport loss after the receiving service committed.
        drop(stream);
    });
    let output = server.cli(&["fork", &url, "published-without-receipt"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("publication outcome is unknown"));
    bridge.join().unwrap();
    Repository::open(t.0.join("published-without-receipt"))
        .unwrap()
        .fsck()
        .unwrap();
}

#[test]
fn http_selective_clone_lazy_object_authorization_and_shallow_push() {
    let temp = Temp::new();
    let source = Repository::init(temp.0.join("project")).unwrap();
    for n in 0..4 {
        source
            .commit(
                &wat::parse_str(format!("(module (func (result i32) i32.const {n}))")).unwrap(),
                "history",
            )
            .unwrap();
    }
    let hidden_blob = source
        .hash_payload(
            "wasm",
            &wat::parse_str("(module (func (result i32) i32.const 99))").unwrap(),
            true,
        )
        .unwrap();
    let server = Server::start(&temp.0, &["--allow-write"]);
    let clone = temp.0.join("clone");
    let url = server.url("project");
    let result = server.cli(&[
        "clone",
        &url,
        clone.to_str().unwrap(),
        "--depth",
        "2",
        "--filter",
        "blob:none",
        "--allow-insecure-http",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let local = Repository::open(&clone).unwrap();
    assert_eq!(local.fsck().unwrap().promised_wasm_blobs, 2);
    let log = server.cli(&["log", clone.to_str().unwrap()]);
    assert!(log.status.success());
    assert_eq!(local.fsck().unwrap().promised_wasm_blobs, 2);
    let output = temp.0.join("restored.wasm");
    let restored = server.cli(&[
        "show",
        clone.to_str().unwrap(),
        "HEAD",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(local.fsck().unwrap().promised_wasm_blobs, 1);
    let (response, _) = server.rpc(
        "project",
        Operation::ObjectSlice {
            id: hidden_blob,
            offset: 0,
        },
        &[],
    );
    assert!(!response.ok);
    let bytes = wat::parse_str("(module (func (result i32) i32.const 17))").unwrap();
    local.commit(&bytes, "shallow child").unwrap();
    let pushed = server.cli(&["push", clone.to_str().unwrap(), "origin", "main:main"]);
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert_eq!(source.wasm_bytes("HEAD").unwrap(), bytes);
}

#[test]
fn server_enforces_trusted_author_before_object_or_ref_publication() {
    let temp = Temp::new();
    let source = Repository::init(temp.0.join("client")).unwrap();
    let destination = Repository::init(temp.0.join("hosted")).unwrap();
    let key = temp.0.join("key");
    let public = temp.0.join("public");
    let trust = temp.0.join("trust");
    for args in [
        vec!["keygen", key.to_str().unwrap(), public.to_str().unwrap()],
        vec![
            "trust-author",
            trust.to_str().unwrap(),
            "Alice",
            public.to_str().unwrap(),
        ],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let signed = source
        .commit_signed(
            WASM,
            "signed root",
            &wasm_git::repository::SigningIdentity {
                author: "Alice".into(),
                key,
                timestamp: 123,
            },
        )
        .unwrap();
    let server = Server::start(
        &temp.0,
        &["--allow-write", "--signed-authors", trust.to_str().unwrap()],
    );
    let (_, bundle) = source.network_bundle(&[], &[]).unwrap();
    let batch = BatchPublication {
        updates: vec![RefUpdate {
            namespace: RefNamespace::Branch,
            name: "main".into(),
            expected: None,
            new: Some(signed.clone()),
            force: false,
        }],
        bundle,
    };
    let (accepted, _) = server.rpc(
        "hosted",
        Operation::PublishBatch {},
        &binary::encode(&batch).unwrap(),
    );
    assert!(accepted.ok, "{:?}", accepted.error);
    // Ref-only create/delete remains usable with signed-author enforcement.
    for (expected, new) in [(None, Some(signed.clone())), (Some(signed.clone()), None)] {
        let batch = BatchPublication {
            updates: vec![RefUpdate {
                namespace: RefNamespace::Tag,
                name: "signed-release".into(),
                expected,
                new,
                force: false,
            }],
            bundle: Vec::new(),
        };
        let (accepted, _) = server.rpc(
            "hosted",
            Operation::PublishBatch {},
            &binary::encode(&batch).unwrap(),
        );
        assert!(accepted.ok, "{:?}", accepted.error);
    }
    let unsigned = source.commit(WASM, "unsigned child").unwrap();
    let before = destination.bundle_bytes(None).unwrap().1;
    let (_, bundle) = source.network_bundle(&[], &[]).unwrap();
    let batch = BatchPublication {
        updates: vec![RefUpdate {
            namespace: RefNamespace::Branch,
            name: "main".into(),
            expected: Some(signed),
            new: Some(unsigned.clone()),
            force: false,
        }],
        bundle,
    };
    let (rejected, _) = server.rpc(
        "hosted",
        Operation::PublishBatch {},
        &binary::encode(&batch).unwrap(),
    );
    assert!(!rejected.ok);
    assert!(rejected.error.unwrap().contains("signature"));
    assert_eq!(destination.bundle_bytes(None).unwrap().1, before);
    // Installing a dangling object through local administration must not turn
    // a later ref-only publication into an author-policy bypass.
    destination
        .install_network_bundle(&batch.bundle, None)
        .unwrap();
    let before = destination.bundle_bytes(None).unwrap().1;
    let batch = BatchPublication {
        updates: vec![RefUpdate {
            namespace: RefNamespace::Tag,
            name: "hidden-unsigned".into(),
            expected: None,
            new: Some(unsigned),
            force: false,
        }],
        bundle: Vec::new(),
    };
    let (rejected, _) = server.rpc(
        "hosted",
        Operation::PublishBatch {},
        &binary::encode(&batch).unwrap(),
    );
    assert!(!rejected.ok);
    assert!(rejected.error.unwrap().contains("signature"));
    assert_eq!(destination.bundle_bytes(None).unwrap().1, before);
}

#[test]
fn replicated_http_pool_survives_one_server_exit_without_local_blob_copy() {
    let temp = Temp::new();
    let source = Repository::init(temp.0.join("source")).unwrap();
    source.commit(WASM, "root").unwrap();
    let ar = temp.0.join("a-root");
    let br = temp.0.join("b-root");
    fs::create_dir(&ar).unwrap();
    fs::create_dir(&br).unwrap();
    Repository::init(ar.join("pool")).unwrap();
    Repository::init(br.join("pool")).unwrap();
    let mut a = Server::start(&ar, &["--allow-write"]);
    let b = Server::start(&br, &["--allow-write"]);
    for args in [
        vec![
            "pool",
            "attach",
            source.root.to_str().unwrap(),
            &a.url("pool"),
            &b.url("pool"),
            "--allow-insecure-http",
        ],
        vec!["pool", "sync", source.root.to_str().unwrap(), "--evict"],
    ] {
        let result = a.cli(&args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(source.fsck().unwrap().promised_wasm_blobs, 1);
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    let output = temp.0.join("recovered.wasm");
    let result = b.cli(&[
        "show",
        source.root.to_str().unwrap(),
        "HEAD",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(output).unwrap(), WASM);
}
