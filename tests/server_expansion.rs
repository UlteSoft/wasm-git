#![cfg(feature = "client")]
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use wasm_git::repository::{CloneFilter, PromiseSource, Repository, SelectOptions};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "wasm-git-expansion-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn p(&self, s: &str) -> PathBuf {
        self.0.join(s)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn p(p: &Path) -> &str {
    p.to_str().unwrap()
}
fn cmd(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap()
}
fn ok(args: &[&str]) -> String {
    let r = cmd(args);
    assert!(
        r.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    String::from_utf8(r.stdout).unwrap()
}
fn value(args: &[&str]) -> serde_json::Value {
    serde_json::from_str(&ok(args)).unwrap()
}
fn wasm(v: u8) -> Vec<u8> {
    wat::parse_str(format!(
        "(module (func (export \"f\") (result i32) i32.const {v}))"
    ))
    .unwrap()
}
fn history(t: &Temp, n: u8) -> (Repository, Vec<String>) {
    let r = Repository::init(t.p("source")).unwrap();
    let ids = (0..n)
        .map(|v| r.commit(&wasm(v), &format!("change {v}")).unwrap())
        .collect();
    (r, ids)
}
#[test]
fn shallow_partial_deepen_unshallow_and_gc_preserve_original_parents_and_bytes() {
    let t = Temp::new();
    let (source, ids) = history(&t, 6);
    let clone = t.p("clone");
    ok(&[
        "clone",
        p(&source.root),
        p(&clone),
        "--depth",
        "2",
        "--filter",
        "blob:none",
    ]);
    let repo = Repository::open(&clone).unwrap();
    let report = repo.fsck().unwrap();
    assert_eq!(report.commits, 2);
    assert_eq!(report.promised_wasm_blobs, 2);
    let log = repo.log(None).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].commit.parents, vec![ids[3].clone()]);
    assert!(log[1].commit.shallow);
    assert_eq!(repo.wasm_bytes("HEAD").unwrap(), wasm(5));
    assert_eq!(repo.fsck().unwrap().promised_wasm_blobs, 1);
    repo.repack().unwrap();
    assert_eq!(repo.wasm_bytes("HEAD").unwrap(), wasm(5));
    ok(&["fetch", p(&clone), "--deepen", "2"]);
    assert_eq!(repo.log(None).unwrap().len(), 4);
    ok(&["fetch", p(&clone), "--unshallow", "--filter", "all"]);
    assert_eq!(repo.log(None).unwrap().len(), 6);
    assert_eq!(repo.fsck().unwrap().promised_wasm_blobs, 0);
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(repo.wasm_bytes(id).unwrap(), wasm(n as u8));
    }
}
#[test]
fn promised_object_failure_is_explicit_and_replicas_fail_over() {
    let t = Temp::new();
    let (source, _) = history(&t, 3);
    let replica = Repository::clone_local(&source.root, t.p("replica")).unwrap();
    let selection = SelectOptions {
        filter: CloneFilter::BlobNone,
        ..Default::default()
    };
    let (_, bytes) = source.selective_bundle(&selection).unwrap();
    let clone = Repository::clone_bundle_bytes(
        &Repository::empty_network_bundle(&bytes).unwrap(),
        t.p("clone"),
    )
    .unwrap();
    clone
        .configure_promisor(
            vec![
                PromiseSource {
                    location: p(&source.root).into(),
                    allow_insecure_http: false,
                },
                PromiseSource {
                    location: p(&replica.root).into(),
                    allow_insecure_http: false,
                },
            ],
            Some(selection),
        )
        .unwrap();
    fs::rename(&source.root, t.p("offline")).unwrap();
    assert_eq!(clone.wasm_bytes("HEAD").unwrap(), wasm(2));
    fs::rename(&replica.root, t.p("offline2")).unwrap();
    let error = clone.wasm_bytes("HEAD~1").unwrap_err();
    assert!(error.contains("unavailable"), "{error}");
    assert_eq!(clone.fsck().unwrap().promised_wasm_blobs, 2);
}
#[test]
fn author_signature_binds_message_content_parents_and_trust() {
    let t = Temp::new();
    let repo = Repository::init(t.p("repo")).unwrap();
    let key = t.p("key");
    let public = t.p("public");
    let trust = t.p("trust");
    let input = t.p("input.wasm");
    fs::write(&input, wasm(7)).unwrap();
    ok(&["keygen", p(&key), p(&public)]);
    ok(&["trust-author", p(&trust), "Alice", p(&public)]);
    let id = ok(&[
        "commit-signed",
        p(&repo.root),
        p(&input),
        "-m",
        "signed",
        "--author",
        "Alice",
        "--key",
        p(&key),
        "--timestamp",
        "123",
    ])
    .trim()
    .to_owned();
    assert!(repo.verify_author(&id, Some(&trust)).unwrap()["trusted"]
        .as_bool()
        .unwrap());
    let original = repo.read_commit(&id).unwrap().commit;
    for field in ["message", "content", "parents", "timestamp"] {
        let mut commit = original.clone();
        match field {
            "message" => commit.message.push('!'),
            "content" => commit.wasm = "0".repeat(64),
            "parents" => commit.parents.push("0".repeat(64)),
            "timestamp" => commit.signature.as_mut().unwrap().timestamp += 1,
            _ => unreachable!(),
        }
        let payload = commit.to_bytes().unwrap();
        assert!(
            repo.hash_payload("commit", &payload, true)
                .unwrap_err()
                .contains("signature"),
            "unbound signed field: {field}"
        );
    }
    repo.repack().unwrap();
    let fork = Repository::fork_local(&repo.root, t.p("fork")).unwrap();
    assert_eq!(
        fork.verify_author(&id, Some(&trust)).unwrap()["author"],
        "Alice"
    );
    ok(&["trust-author", p(&t.p("wrong-trust")), "Bob", p(&public)]);
    assert!(repo
        .verify_author(&id, Some(&t.p("wrong-trust")))
        .unwrap_err()
        .contains("not trusted"));
    ok(&["untrust-author", p(&trust), "Alice", "--public", p(&public)]);
    assert!(repo
        .verify_author(&id, Some(&trust))
        .unwrap_err()
        .contains("not trusted"));
    assert!(
        repo.verify_author(&id, None).unwrap()["cryptographically_valid"]
            .as_bool()
            .unwrap()
    );
}
#[test]
fn relative_trust_paths_and_lock_suffixes_support_signing_and_revocation() {
    let t = Temp::new();
    let repo = Repository::init(t.p("repo")).unwrap();
    fs::write(t.p("input.wasm"), wasm(9)).unwrap();
    let relative = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .current_dir(&t.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    relative(&["keygen", "key", "public"]);
    relative(&["trust-author", "trust.lock", "Alice", "public"]);
    relative(&[
        "commit-signed",
        "repo",
        "input.wasm",
        "-m",
        "signed",
        "--author",
        "Alice",
        "--key",
        "key",
    ]);
    assert_eq!(
        repo.verify_author("HEAD", Some(&t.p("trust.lock")))
            .unwrap()["trusted"],
        true
    );
    relative(&["untrust-author", "trust.lock", "Alice"]);
    assert!(repo
        .verify_author("HEAD", Some(&t.p("trust.lock")))
        .is_err());
    assert!(!t.p("trust.lock.lock").exists());
}
fn large(v: u8) -> Vec<u8> {
    let mut bytes = b"\0asm\x01\0\0\0\0".to_vec();
    let mut raw = vec![1, b'x'];
    let mut n = 12345678u32;
    for _ in 0..100_000 {
        n ^= n << 13;
        n ^= n >> 17;
        n ^= n << 5;
        raw.push(n as u8);
    }
    raw[50000] = v;
    let mut len = raw.len();
    loop {
        let b = (len & 127) as u8;
        len >>= 7;
        bytes.push(b | if len > 0 { 128 } else { 0 });
        if len == 0 {
            break;
        }
    }
    bytes.extend(raw);
    bytes
}
#[test]
fn cross_pack_delta_survives_fork_base_deletion_and_repacking() {
    let t = Temp::new();
    let source = Repository::init(t.p("source")).unwrap();
    let root = source.commit(&large(0), "root").unwrap();
    source.repack().unwrap();
    let fork = Repository::fork_local(&source.root, t.p("fork")).unwrap();
    let tip = fork.commit(&large(1), "private").unwrap();
    let report = fork.repack().unwrap();
    assert_eq!(report["cross_pack_delta_objects"], 1, "{report}");
    assert!(report["pack_bytes"].as_u64().unwrap() < 5000);
    fs::remove_dir_all(&source.root).unwrap();
    assert_eq!(fork.wasm_bytes(&tip).unwrap(), large(1));
    assert_eq!(fork.wasm_bytes(&root).unwrap(), large(0));
    fork.fsck().unwrap();
    fork.repack().unwrap();
    assert_eq!(fork.wasm_bytes(&tip).unwrap(), large(1));
}
#[test]
fn two_node_pool_pins_gc_lazy_failover_and_release() {
    let t = Temp::new();
    let (source, ids) = history(&t, 3);
    let a = Repository::init(t.p("node-a")).unwrap();
    let b = Repository::init(t.p("node-b")).unwrap();
    ok(&["pool", "attach", p(&source.root), p(&a.root), p(&b.root)]);
    let report = value(&["pool", "sync", p(&source.root), "--evict"]);
    assert_eq!(report["replicas"], 2);
    assert_eq!(report["evicted_wasm_objects"], 3);
    assert_eq!(source.fsck().unwrap().promised_wasm_blobs, 3);
    ok(&["gc", p(&a.root), "--prune=now"]);
    ok(&["gc", p(&b.root), "--prune=now"]);
    fs::rename(&a.root, t.p("offline")).unwrap();
    assert_eq!(source.wasm_bytes(&ids[1]).unwrap(), wasm(1));
    fs::rename(t.p("offline"), &a.root).unwrap();
    ok(&["pool", "release", p(&source.root)]);
    assert_eq!(source.fsck().unwrap().promised_wasm_blobs, 0);
    assert!(a.branches().unwrap().is_empty());
    assert!(b.branches().unwrap().is_empty());
    assert_eq!(source.wasm_bytes(&ids[0]).unwrap(), wasm(0));
}
#[test]
fn pool_fork_acquires_independent_pins_without_materializing_shared_wasm() {
    let t = Temp::new();
    let (source, _) = history(&t, 3);
    let node = Repository::init(t.p("pool")).unwrap();
    ok(&["pool", "attach", p(&source.root), p(&node.root)]);
    ok(&["pool", "sync", p(&source.root), "--evict"]);
    let fork = Repository::fork_local(&source.root, t.p("fork")).unwrap();
    assert_eq!(fork.fsck().unwrap().promised_wasm_blobs, 3);
    assert_ne!(
        source.pool_config().unwrap().identity,
        fork.pool_config().unwrap().identity
    );
    ok(&["pool", "release", p(&source.root)]);
    fs::remove_dir_all(&source.root).unwrap();
    ok(&["gc", p(&node.root), "--prune=now"]);
    assert_eq!(fork.wasm_bytes("HEAD").unwrap(), wasm(2));
}
#[test]
fn concurrent_pool_maintenance_cannot_release_or_replace_replication_pins() {
    let t = Temp::new();
    let (source, _) = history(&t, 3);
    let node = Repository::init(t.p("pool")).unwrap();
    ok(&["pool", "attach", p(&source.root), p(&node.root)]);
    ok(&["pool", "sync", p(&source.root), "--evict"]);
    let pins = node.branches().unwrap();
    // Simulate another process holding the full replication/release operation.
    // The inode remains stable across process death and all linked worktrees.
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(source.root.join(".wasm-git/pool.gate"))
        .unwrap();
    gate.lock().unwrap();
    for args in [
        vec!["pool", "sync", p(&source.root), "--evict"],
        vec!["pool", "release", p(&source.root)],
        vec!["pool", "attach", p(&source.root), p(&node.root)],
    ] {
        let result = cmd(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("pool operation is busy"));
    }
    assert!(Repository::fork_local(&source.root, t.p("fork"))
        .unwrap_err()
        .contains("pool operation is busy"));
    assert!(!t.p("fork").exists());
    assert_eq!(node.branches().unwrap(), pins);
    assert_eq!(source.fsck().unwrap().promised_wasm_blobs, 3);
    drop(gate);
    ok(&["pool", "release", p(&source.root)]);
    assert!(node.branches().unwrap().is_empty());
    assert_eq!(source.fsck().unwrap().promised_wasm_blobs, 0);
    assert_eq!(source.wasm_bytes("HEAD").unwrap(), wasm(2));
}
#[test]
fn deepening_uses_previous_boundary_even_after_remote_advances() {
    let t = Temp::new();
    let (source, _) = history(&t, 8);
    let clone = t.p("clone");
    ok(&["clone", p(&source.root), p(&clone), "--depth", "2"]);
    source.commit(&wasm(8), "new tip").unwrap();
    source.commit(&wasm(9), "newer tip").unwrap();
    ok(&["fetch", p(&clone), "--deepen", "2"]);
    let repo = Repository::open(clone).unwrap();
    assert_eq!(repo.log(None).unwrap().len(), 4);
    assert_eq!(repo.fsck().unwrap().commits, 6);
}
#[test]
fn failed_pool_replication_keeps_every_local_blob() {
    let t = Temp::new();
    let (source, _) = history(&t, 3);
    let a = Repository::init(t.p("a")).unwrap();
    let b = Repository::init(t.p("b")).unwrap();
    ok(&["pool", "attach", p(&source.root), p(&a.root), p(&b.root)]);
    fs::rename(&b.root, t.p("offline")).unwrap();
    let result = cmd(&["pool", "sync", p(&source.root), "--evict"]);
    assert!(!result.status.success());
    assert_eq!(source.fsck().unwrap().promised_wasm_blobs, 0);
    assert_eq!(source.wasm_bytes("HEAD~2").unwrap(), wasm(0));
}

#[test]
fn path_filter_and_selected_file_audit_materialize_only_the_requested_module() {
    use std::collections::BTreeMap;
    let t = Temp::new();
    let source = Repository::init(t.p("source")).unwrap();
    source
        .commit_files_on_branch(
            &BTreeMap::from([
                ("a.wasm".into(), wasm(1)),
                ("b.wasm".into(), wasm(2)),
                ("lib/c.wasm".into(), wasm(3)),
            ]),
            "tree",
            &[],
            "main",
            None,
            None,
        )
        .unwrap();
    let clone = t.p("clone");
    ok(&[
        "clone",
        p(&source.root),
        p(&clone),
        "--filter",
        "path:a.wasm",
    ]);
    let repo = Repository::open(&clone).unwrap();
    assert_eq!(repo.fsck().unwrap().promised_wasm_blobs, 2);
    let out = t.p("b.wasm");
    ok(&[
        "show",
        p(&clone),
        "HEAD",
        "--file",
        "b.wasm",
        "--output",
        p(&out),
    ]);
    assert_eq!(fs::read(out).unwrap(), wasm(2));
    assert_eq!(repo.fsck().unwrap().promised_wasm_blobs, 1);
    let diff = value(&[
        "diff",
        "HEAD",
        "HEAD",
        "--repo",
        p(&clone),
        "--file",
        "a.wasm",
        "--format",
        "json",
    ]);
    assert_eq!(diff["summary"]["functions_changed"], 0);
    assert_eq!(repo.fsck().unwrap().promised_wasm_blobs, 1);
}
