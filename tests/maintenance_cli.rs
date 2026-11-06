use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "uwgit-maintenance-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        ok(&["init", path(&root)]);
        Self(root)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn execute(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap()
}
fn ok(args: &[&str]) -> String {
    let output = execute(args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}
fn json(args: &[&str]) -> Value {
    serde_json::from_str(&ok(args)).unwrap()
}
fn wasm(value: u8) -> Vec<u8> {
    [b"\0asm\x01\0\0\0".as_slice(), &[0, 3, 1, b'x', value]].concat()
}
fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(
        root: &Path,
        directory: &Path,
        result: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), result);
            } else if entry.file_type().unwrap().is_file() {
                result.insert(
                    entry.path().strip_prefix(root).unwrap().into(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    visit(root, root, &mut result);
    result
}
fn seed(root: &Path, value: u8) -> String {
    fs::write(root.join("module.wasm"), wasm(value)).unwrap();
    ok(&["add", path(root), "module.wasm"]);
    ok(&["commit", path(root), "-m", &format!("seed {value}")]);
    ok(&["rev-parse", path(root), "HEAD"])
}

#[test]
fn cleanup_previews_and_force_removes_only_untracked_nonignored_regular_paths() {
    let temp = Temp::new();
    let root = &temp.0;
    seed(root, 1);
    fs::write(root.join(".uwgitignore"), "*.log\ncache/\n").unwrap();
    fs::write(root.join("keep.log"), b"keep").unwrap();
    fs::write(root.join("junk.txt"), b"remove").unwrap();
    fs::create_dir(root.join("cache")).unwrap();
    fs::write(root.join("cache/keep"), b"keep").unwrap();
    fs::create_dir(root.join("loose")).unwrap();
    fs::write(root.join("loose/remove"), b"remove").unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    ok(&["init", path(&root.join("nested"))]);
    fs::write(root.join("nested/keep"), b"keep").unwrap();
    let before = snapshot(root);
    assert!(!execute(&["clean", path(root)]).status.success());
    let plan = json(&["clean", path(root), "-n", "-d", "-e", ".uwgitignore"]);
    assert!(plan["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "junk.txt"));
    assert_eq!(snapshot(root), before);
    let removed = json(&["clean", path(root), "-f", "-d", "-e", ".uwgitignore"]);
    assert_eq!(removed["files"], plan["files"]);
    for keep in [
        "module.wasm",
        ".uwgitignore",
        "keep.log",
        "cache/keep",
        "nested/keep",
    ] {
        assert!(root.join(keep).exists(), "{keep}");
    }
    assert!(!root.join("junk.txt").exists());
    assert!(!root.join("loose").exists());
    ok(&["fsck", path(root)]);
}

#[test]
fn cleanup_ignored_only_exclusions_path_scopes_and_worktree_lock_are_exact() {
    let temp = Temp::new();
    let root = &temp.0;
    seed(root, 1);
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    for name in ["a.log", "b.log", "a.txt"] {
        fs::write(root.join(name), name).unwrap();
    }
    let before = snapshot(root);
    fs::write(root.join(".uwgit/index.lock"), b"held").unwrap();
    assert!(!execute(&["clean", path(root), "-f", "-x"]).status.success());
    fs::remove_file(root.join(".uwgit/index.lock")).unwrap();
    assert_eq!(snapshot(root), before);
    json(&["clean", path(root), "-f", "-X", "-e", "b.log"]);
    assert!(!root.join("a.log").exists());
    assert!(root.join("b.log").exists());
    assert!(root.join("a.txt").exists());
    json(&["clean", path(root), "-f", "-x", "--", "b.log"]);
    assert!(!root.join("b.log").exists());
    assert!(root.join("a.txt").exists());
    assert!(!execute(&["clean", path(root), "-f", "--", "../outside"])
        .status
        .success());
}

#[cfg(unix)]
#[test]
fn cleanup_never_follows_or_removes_symbolic_links() {
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let outside = Temp::new();
    let root = &temp.0;
    seed(root, 1);
    fs::write(outside.0.join("keep"), b"outside").unwrap();
    symlink(&outside.0, root.join("linked")).unwrap();
    symlink(outside.0.join("keep"), root.join("linked-file")).unwrap();
    let before = snapshot(&outside.0);
    json(&["clean", path(root), "-f", "-d", "-x"]);
    assert!(root
        .join("linked")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(root
        .join("linked-file")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(snapshot(&outside.0), before);
}

#[test]
fn gc_dry_run_plans_and_explicit_prune_retains_reflog_closures() {
    let temp = Temp::new();
    let root = &temp.0;
    let first = seed(root, 1);
    seed(root, 2);
    ok(&["reset", path(root), "--hard", &first]);
    let tree = ok(&["write-tree", path(root)]);
    let dangling = ok(&["commit-tree", path(root), &tree, "-m", "dangling"]);
    let child = ok(&[
        "commit-tree",
        path(root),
        &tree,
        "-p",
        &dangling,
        "-m",
        "child",
    ]);
    let before = snapshot(root);
    let plan = json(&["gc", path(root), "-n"]);
    assert_eq!(snapshot(root), before);
    let order: Vec<_> = plan["deletion_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(order, vec![child.clone(), dangling.clone()]);
    let removed = json(&["gc", path(root), "--prune=now"]);
    assert_eq!(removed["removed"], plan["deletion_order"]);
    assert!(!root.join(".uwgit/objects").join(dangling).exists());
    ok(&["fsck", path(root)]);
    let log = json(&["reflog", path(root)]);
    assert!(log.as_array().unwrap().len() >= 3);
}

#[test]
fn every_gc_deletion_prefix_retains_a_complete_object_graph() {
    let temp = Temp::new();
    let root = &temp.0;
    seed(root, 1);
    let tree = ok(&["write-tree", path(root)]);
    let a = ok(&["commit-tree", path(root), &tree, "-m", "a"]);
    let b = ok(&["commit-tree", path(root), &tree, "-p", &a, "-m", "b"]);
    let _c = ok(&["commit-tree", path(root), &tree, "-p", &b, "-m", "c"]);
    let plan = json(&["gc", path(root), "-n"]);
    for id in plan["deletion_order"].as_array().unwrap() {
        fs::remove_file(root.join(".uwgit/objects").join(id.as_str().unwrap())).unwrap();
        ok(&["fsck", path(root)]);
    }
}

#[test]
fn gc_pins_auxiliary_recovery_state_and_maintenance_rejects_unimplemented_tasks() {
    let temp = Temp::new();
    let root = &temp.0;
    seed(root, 1);
    let tree = ok(&["write-tree", path(root)]);
    ok(&["commit-tree", path(root), &tree, "-m", "dangling"]);
    fs::write(root.join(".uwgit/notes.bin"), b"future auxiliary metadata").unwrap();
    let index = fs::read(root.join(".uwgit/index")).unwrap();
    let result = json(&["gc", path(root), "--prune=now"]);
    assert!(result["removed"].as_array().unwrap().is_empty());
    assert_eq!(fs::read(root.join(".uwgit/index")).unwrap(), index);
    assert_eq!(
        fs::read(root.join(".uwgit/notes.bin")).unwrap(),
        b"future auxiliary metadata"
    );
    let before = snapshot(root);
    assert!(json(&["maintenance", path(root), "run", "--task=verify"])["verify"].is_object());
    assert!(
        !execute(&["maintenance", path(root), "run", "--task=pack-refs"])
            .status
            .success()
    );
    assert!(!execute(&[
        "maintenance",
        path(root),
        "run",
        "--task=verify",
        "--prune=now"
    ])
    .status
    .success());
    assert_eq!(snapshot(root), before);
}

#[test]
fn reflog_expiry_previews_exactly_then_releases_abandoned_history_for_gc() {
    let temp = Temp::new();
    let root = &temp.0;
    let first = seed(root, 1);
    let abandoned = seed(root, 2);
    ok(&["reset", path(root), "--hard", &first]);
    let before = snapshot(root);
    let plan = json(&[
        "reflog",
        path(root),
        "expire",
        "--all",
        "--expire=never",
        "--expire-unreachable=all",
        "--rewrite",
        "-n",
    ]);
    assert!(plan["expired"].as_u64().unwrap() > 0);
    assert_eq!(snapshot(root), before);
    assert!(json(&["gc", path(root), "-n"])["candidates"]
        .as_array()
        .unwrap()
        .is_empty());
    let actual = json(&[
        "reflog",
        path(root),
        "expire",
        "--all",
        "--expire=never",
        "--expire-unreachable=all",
        "--rewrite",
    ]);
    assert_eq!(actual["removed"], plan["removed"]);
    assert_eq!(ok(&["rev-parse", path(root), "HEAD"]), first);
    assert_eq!(fs::read(root.join("module.wasm")).unwrap(), wasm(1));
    let gc = json(&["gc", path(root), "--prune=now"]);
    assert!(gc["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == &abandoned));
    seed(root, 3);
    ok(&["fsck", path(root)]);
    assert!(
        json(&["reflog", path(root), "show"])[0]["unix_seconds"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn reflog_deletion_keeps_unselected_chains_and_appends_after_sequence_gaps() {
    let temp = Temp::new();
    let root = &temp.0;
    let first = seed(root, 1);
    ok(&["tag", path(root), "saved"]);
    let second = seed(root, 2);
    let third = seed(root, 3);
    let tag = json(&["reflog", path(root), "show", "saved"]);
    let before = snapshot(root);
    json(&[
        "reflog",
        path(root),
        "delete",
        "main@{1}",
        "--rewrite",
        "-n",
    ]);
    assert_eq!(snapshot(root), before);
    let deleted = json(&["reflog", path(root), "delete", "main@{1}", "--rewrite"]);
    assert_eq!(deleted["removed"]["new"], second);
    let entries = json(&["reflog", path(root), "show", "main"]);
    assert_eq!(entries[0]["new"], third);
    assert_eq!(entries[0]["old"], first);
    assert_eq!(json(&["reflog", path(root), "show", "saved"]), tag);
    seed(root, 4);
    let entries = json(&["reflog", path(root), "show", "main"]);
    assert!(entries[0]["sequence"].as_u64().unwrap() > entries[1]["sequence"].as_u64().unwrap());
    assert!(execute(&["reflog", path(root), "exists", "main"])
        .status
        .success());
    assert_eq!(
        execute(&["reflog", path(root), "exists", "missing"])
            .status
            .code(),
        Some(1)
    );
    assert!(json(&["reflog", path(root), "list"])
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "HEAD"));
    ok(&["fsck", path(root)]);
}

#[test]
fn timestamp_selectors_and_expiry_use_checked_utc_and_strict_boundaries() {
    use wasm_git::{binary, repository::ReflogEntry};
    let temp = Temp::new();
    let root = &temp.0;
    let first = seed(root, 1);
    let second = seed(root, 2);
    let third = seed(root, 3);
    for name in ["reflog", "head-reflog.bin"] {
        let file = root.join(".uwgit").join(name);
        let mut entries: Vec<ReflogEntry> = binary::decode(&fs::read(&file).unwrap()).unwrap();
        for entry in &mut entries {
            entry.unix_seconds = match entry.new.as_deref() {
                Some(id) if id == first => 1_704_067_200, // 2024-01-01 UTC
                Some(id) if id == second => 1_704_153_600,
                _ => 1_704_240_000,
            };
        }
        fs::write(file, binary::encode(&entries).unwrap()).unwrap();
    }
    assert_eq!(
        ok(&["rev-parse", path(root), "HEAD@{2024-01-02T00:00:00Z}"]),
        second
    );
    assert_eq!(ok(&["rev-parse", path(root), "main@{@1704067200}"]), first);
    assert_eq!(ok(&["rev-parse", path(root), "HEAD@{0}"]), third);
    let before = snapshot(root);
    for date in [
        "2023-02-29",
        "2024-13-01",
        "2024-01-01T24:00:00Z",
        "18446744073709551615.weeks.ago",
    ] {
        assert!(
            !execute(&["reflog", path(root), "expire", "--all", "--expire", date])
                .status
                .success()
        );
        assert_eq!(snapshot(root), before);
    }
    json(&[
        "reflog",
        path(root),
        "expire",
        "--all",
        "--expire=2024-01-02",
        "--expire-unreachable=never",
    ]);
    let log = json(&["reflog", path(root), "show", "main"]);
    assert_eq!(log.as_array().unwrap().len(), 2);
    assert_eq!(log[1]["new"], second);
    seed(root, 4);
    ok(&["fsck", path(root)]);
}

#[test]
fn reflog_expiry_validates_every_worktree_before_writes_and_respects_single_worktree() {
    let temp = Temp::new();
    let root = &temp.0;
    seed(root, 1);
    let linked = root.join("linked");
    json(&["worktree", path(root), "add", path(&linked), "-b", "topic"]);
    let log_path = linked.join(".uwgit/head-reflog.bin");
    let original = fs::read(&log_path).unwrap();
    fs::write(&log_path, b"corrupt").unwrap();
    let before = snapshot(root);
    assert!(
        !execute(&["reflog", path(root), "expire", "--all", "--expire=all"])
            .status
            .success()
    );
    assert_eq!(snapshot(root), before);
    fs::write(&log_path, &original).unwrap();
    fs::write(linked.join(".uwgit/pending.bin"), b"pending recovery").unwrap();
    let before = snapshot(root);
    assert!(
        !execute(&["reflog", path(root), "expire", "--all", "--expire=all"])
            .status
            .success()
    );
    assert_eq!(snapshot(root), before);
    fs::remove_file(linked.join(".uwgit/pending.bin")).unwrap();
    json(&[
        "reflog",
        path(root),
        "expire",
        "--all",
        "--single-worktree",
        "--expire=all",
    ]);
    assert_eq!(fs::read(&log_path).unwrap(), original);
    json(&["reflog", path(&linked), "expire", "--all", "--expire=all"]);
    assert_eq!(
        json(&["reflog", path(&linked), "show"]),
        serde_json::json!([])
    );
    ok(&["fsck", path(root)]);
    ok(&["fsck", path(&linked)]);
}

#[test]
fn reflog_write_validates_native_endpoints_and_never_moves_references() {
    let temp = Temp::new();
    let root = &temp.0;
    let id = seed(root, 1);
    let before = snapshot(root);
    assert!(
        !execute(&["reflog", path(root), "write", "main", &id, &id, "test"])
            .status
            .success()
    );
    assert!(!execute(&[
        "reflog",
        path(root),
        "write",
        "refs/heads/main",
        &id,
        &"f".repeat(64),
        "test"
    ])
    .status
    .success());
    assert_eq!(snapshot(root), before);
    json(&[
        "reflog",
        path(root),
        "write",
        "refs/heads/main",
        &"0".repeat(64),
        &id,
        "  recovered\n commit  ",
    ]);
    let entries = json(&["reflog", path(root), "show", "main"]);
    assert_eq!(entries[0]["operation"], "recovered commit");
    assert_eq!(entries[0]["old"], serde_json::Value::Null);
    assert_eq!(ok(&["rev-parse", path(root), "HEAD"]), id);
    json(&["maintenance", path(root), "run", "--task=reflog-expire"]);
    ok(&["fsck", path(root)]);
}
