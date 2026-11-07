#![cfg(feature = "client")]
//! Multi-site historical edits, physical/index order independence, deterministic
//! parallel compression, and lossless closure restoration through native GC.
#[path = "support/native.rs"]
mod native;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use wasm_git::repository::{Commit, Repository, Tree};
struct Temp(PathBuf);
impl Temp {
    fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "wasm-git-history-gc-{}-{label}",
            std::process::id()
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
fn run(threads: &str, args: &[&str]) -> serde_json::Value {
    let r = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(["--threads", threads])
        .args(args)
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    serde_json::from_slice(&r.stdout).unwrap()
}
fn object(repo: &Path, kind: u8, payload: &[u8]) -> String {
    let raw = [
        b"WGTO\0\0\0\x02".as_slice(),
        &[kind],
        &(payload.len() as u64).to_be_bytes(),
        payload,
    ]
    .concat();
    let id = format!("{:x}", Sha256::digest(&raw));
    fs::write(repo.join(".wasm-git/objects").join(&id), raw).unwrap();
    id
}
fn leb(mut n: usize) -> Vec<u8> {
    let mut b = Vec::new();
    loop {
        let v = (n & 127) as u8;
        n >>= 7;
        b.push(v | if n == 0 { 0 } else { 128 });
        if n == 0 {
            return b;
        }
    }
}
fn image(group: usize, rev: usize) -> Vec<u8> {
    let mut state = 0x12345678_u32.wrapping_add(group as u32);
    let mut bytes: Vec<_> = (0..128 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    for i in (2048..bytes.len()).step_by(4096) {
        bytes[i] ^= (rev + 1) as u8;
    }
    if rev % 2 == 1 {
        bytes.rotate_left(4099);
    }
    bytes.splice(8007..8007, [rev as u8; 13]);
    let payload = [b"\x01x".as_slice(), &bytes].concat();
    [
        b"\0asm\x01\0\0\0\0".as_slice(),
        &leb(payload.len()),
        &payload,
    ]
    .concat()
}
fn history(path: &Path, revisions: usize) -> (Repository, Vec<String>) {
    let repo = Repository::init(path).unwrap();
    let mut ids = Vec::new();
    for rev in 0..revisions {
        let entries = (0..4)
            .map(|g| (format!("modules/{g}.wasm"), object(path, 1, &image(g, rev))))
            .collect();
        let tree = object(path, 3, &Tree { format: 1, entries }.to_bytes().unwrap());
        let commit = Commit {
            shallow: false,
            signature: None,
            format: 2,
            wasm: String::new(),
            parents: ids.last().cloned().into_iter().collect(),
            message: format!("revision {rev}"),
            tree: Some(tree),
            legacy_path: None,
        };
        ids.push(object(path, 2, &commit.to_bytes().unwrap()));
    }
    fs::write(
        path.join(".wasm-git/refs/main"),
        native::reference(ids.last().unwrap()),
    )
    .unwrap();
    repo.fsck().unwrap();
    (repo, ids)
}
fn pack(path: &Path) -> PathBuf {
    let entries: Vec<_> = fs::read_dir(path.join(".wasm-git/packs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}
#[test]
fn scattered_history_compresses_deterministically_on_all_cpus_and_every_object_restores() {
    let t = Temp::new("roundtrip");
    let (repo, ids) = history(&t.0.join("source"), 24);
    let before = repo.bundle_bytes(None).unwrap();
    let serial = Repository::clone_local(&repo.root, t.0.join("serial")).unwrap();
    let a = run("1", &["repack", serial.root.to_str().unwrap()]);
    let b = run("0", &["gc", repo.root.to_str().unwrap()]);
    let b = &b["repack"];
    assert_eq!(a["threads"], 1);
    assert_eq!(
        b["threads"],
        std::thread::available_parallelism().unwrap().get()
    );
    assert_eq!(b["codec"], "zstd");
    assert_eq!(b["delta_algorithm"], "block_copy_insert");
    assert!(b["pack_bytes"].as_u64().unwrap() < b["raw_object_bytes"].as_u64().unwrap() / 10);
    assert!(b["max_delta_depth"].as_u64().unwrap() <= 8);
    assert_eq!(
        fs::read(pack(&repo.root)).unwrap(),
        fs::read(pack(&serial.root)).unwrap()
    );
    // This equality compares every original envelope and all history, including
    // every unselected version. No semantic or normalized representation helps.
    assert_eq!(repo.bundle_bytes(None).unwrap(), before);
    for rev in [0, 9, 23] {
        let files = repo.files_at(&ids[rev]).unwrap();
        for g in 0..4 {
            assert_eq!(files.files[&format!("modules/{g}.wasm")], image(g, rev));
        }
    }
    let fork = Repository::fork_local(&repo.root, t.0.join("fork")).unwrap();
    run("0", &["gc", repo.root.to_str().unwrap(), "--prune=now"]);
    fs::remove_dir_all(&repo.root).unwrap();
    assert_eq!(fork.bundle_bytes(None).unwrap(), before);
}
#[test]
fn gc_preview_is_read_only_default_compacts_and_pruning_is_explicit() {
    let t = Temp::new("policy");
    let (repo, _) = history(&t.0.join("repo"), 2);
    let dangling = object(&repo.root, 1, &image(9, 0));
    let before = repo.bundle_bytes(None).unwrap();
    let count = fs::read_dir(repo.root.join(".wasm-git/objects"))
        .unwrap()
        .count();
    let plan = run("0", &["gc", repo.root.to_str().unwrap(), "-n"]);
    assert_eq!(plan["dry_run"], true);
    assert!(!repo.root.join(".wasm-git/packs").exists());
    assert_eq!(
        fs::read_dir(repo.root.join(".wasm-git/objects"))
            .unwrap()
            .count(),
        count
    );
    let compact = run("0", &["gc", repo.root.to_str().unwrap()]);
    assert_eq!(compact["dry_run"], false);
    assert_eq!(compact["prune"], false);
    assert!(compact["removed"].as_array().unwrap().is_empty());
    assert_eq!(repo.bundle_bytes(None).unwrap(), before);
    assert_eq!(repo.cat_object(&dangling).unwrap().0, "wasm");
    let pruned = run("0", &["gc", repo.root.to_str().unwrap(), "--prune=now"]);
    assert!(pruned["removed"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(dangling)));
    assert!(repo.cat_object(&dangling).is_err());
    repo.fsck().unwrap();
}
