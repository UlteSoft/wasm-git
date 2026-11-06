//! The unreleased format is binary throughout; no JSON persistence fallback.
#[path = "support/native.rs"]
mod native;
use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};
use wasm_git::{
    binary,
    patch::BytePatch,
    repository::{Commit, Tree},
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run(args: &[&str]) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
#[test]
fn objects_index_state_and_patch_use_raw_binary_and_restore_exact_originals() {
    let t = Temp(PathBuf::from(format!(
        "/tmp/uwgit-native-storage-{}",
        std::process::id()
    )));
    fs::create_dir(&t.0).unwrap();
    let repo = t.0.join("repo");
    let r = repo.to_str().unwrap();
    run(&["init", r]);
    assert_eq!(
        fs::read(repo.join(".uwgit/format")).unwrap(),
        b"UWGRPO\0\x02"
    );
    let mut old =
        wat::parse_str("(module (func (export \"f\") (result i32) i32.const 1))").unwrap();
    // An opaque section containing non-UTF8 bytes must stay byte-exact.
    old.extend_from_slice(&[0, 6, 1, b'x', 0, 255, 128, 254]);
    fs::write(repo.join("a.wasm"), &old).unwrap();
    run(&["add", r, "a.wasm"]);
    run(&["commit", r, "-m", "first"]);
    let head = String::from_utf8(run(&["rev-parse", r, "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let raw = fs::read(repo.join(".uwgit/objects").join(&head)).unwrap();
    assert!(raw.starts_with(b"UWGO\0\0\0\x02\x02"));
    let commit = Commit::from_bytes(&raw[17..]).unwrap();
    let tree_bytes = fs::read(
        repo.join(".uwgit/objects")
            .join(commit.tree.as_ref().unwrap()),
    )
    .unwrap();
    let tree = Tree::from_bytes(&tree_bytes[17..]).unwrap();
    assert_eq!(tree_bytes.len(), 17 + 4 + 4 + 6 + 32);
    assert_eq!(
        fs::read(repo.join(".uwgit/objects").join(&tree.entries["a.wasm"])).unwrap()[17..],
        old
    );
    let index = fs::read(repo.join(".uwgit/index")).unwrap();
    assert!(index.starts_with(b"UWGITIDX\x06UWGM\0\0\0\x01"));
    assert_eq!(
        binary::decode::<native::Index>(&index[9..]).unwrap().files["a.wasm"],
        old
    );
    assert!(index.len() < old.len() + 128);
    run(&["config", r, "user.name", "binary-test"]);
    run(&["remote", r, "add", "origin", "http://localhost:1/project"]);
    for path in ["config", "reflog", "remotes/origin.bin"] {
        assert!(
            fs::read(repo.join(".uwgit").join(path))
                .unwrap()
                .starts_with(b"UWGM\0\0\0\x01"),
            "{path}"
        );
    }
    let mut new = old.clone();
    *new.last_mut().unwrap() ^= 1;
    fs::write(repo.join("a.wasm"), &new).unwrap();
    run(&["stash", r, "push", "-m", "raw snapshot"]);
    assert!(fs::read(repo.join(".uwgit/stash"))
        .unwrap()
        .starts_with(b"UWGM\0\0\0\x01"));
    run(&["stash", r, "apply", "0"]);
    assert_eq!(fs::read(repo.join("a.wasm")).unwrap(), new);
    let patch = wasm_git::patch::create(&old, &new)
        .unwrap()
        .to_bytes()
        .unwrap();
    assert!(patch.starts_with(b"UWGPAT01"));
    assert_eq!(patch.len(), 126);
    assert_eq!(
        BytePatch::from_bytes(&patch).unwrap().apply(&old).unwrap(),
        new
    );
    let tree_copy = Tree {
        format: 1,
        entries: BTreeMap::from([("a.wasm".into(), tree.entries["a.wasm"].clone())]),
    };
    assert_eq!(tree_copy.to_bytes().unwrap(), tree_bytes[17..]);
    for bytes in [&raw[17..], &tree_bytes[17..]] {
        for end in 0..bytes.len() {
            if bytes.len() == tree_bytes.len() - 17 {
                assert!(Tree::from_bytes(&bytes[..end]).is_err());
            } else {
                assert!(Commit::from_bytes(&bytes[..end]).is_err());
            }
        }
    }
    // A pre-release JSON repository is rejected without rewriting its marker.
    let marker = repo.join(".uwgit/format");
    fs::write(&marker, b"uwgit-repository-v1\n").unwrap();
    assert!(!Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(["fsck", r])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(fs::read(marker).unwrap(), b"uwgit-repository-v1\n");
}
