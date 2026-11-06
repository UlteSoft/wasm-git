#![cfg(feature = "client")]
#[path = "support/native.rs"]
mod native;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "uwgit-pack-fork-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn command(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(args)
        .output()
        .unwrap()
}
fn p(p: &Path) -> &str {
    p.to_str().unwrap()
}
fn ok(args: &[&str]) -> String {
    let r = command(args);
    assert!(
        r.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    String::from_utf8(r.stdout).unwrap().trim().into()
}
fn js(args: &[&str]) -> Value {
    serde_json::from_str(&ok(args)).unwrap()
}
fn leb(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 127) as u8;
        n >>= 7;
        out.push(b | if n == 0 { 0 } else { 128 });
        if n == 0 {
            return out;
        }
    }
}
fn wasm(version: u8) -> Vec<u8> {
    let mut payload = b"\x01x".to_vec();
    let mut state = 0x12345678_u32;
    for _ in 0..100_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        payload.push(state as u8);
    }
    payload[50_000] = version;
    [
        b"\0asm\x01\0\0\0\0".as_slice(),
        &leb(payload.len()),
        &payload,
    ]
    .concat()
}
fn seed(repo: &Path, version: u8) -> String {
    fs::write(repo.join("a.wasm"), wasm(version)).unwrap();
    ok(&["add", p(repo), "a.wasm"]);
    ok(&["commit", p(repo), "-m", &format!("version {version}")]);
    ok(&["rev-parse", p(repo), "HEAD"])
}
fn packs(repo: &Path) -> Vec<PathBuf> {
    fs::read_dir(repo.join(".uwgit/packs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

#[test]
fn indexed_delta_repack_preserves_every_revision_and_future_writes() {
    let t = Temp::new();
    let repo = t.path("repo");
    ok(&["init", p(&repo)]);
    let versions: Vec<_> = (0..6).map(|i| (i, seed(&repo, i))).collect();
    let before = js(&["fsck", p(&repo)]);
    let report = js(&["repack", p(&repo)]);
    assert_eq!(report["packing"], true);
    assert!(report["delta_objects"].as_u64().unwrap() > 0);
    assert!(
        report["pack_bytes"].as_u64().unwrap() < report["raw_object_bytes"].as_u64().unwrap() / 2
    );
    assert_eq!(
        fs::read_dir(repo.join(".uwgit/objects")).unwrap().count(),
        0
    );
    assert_eq!(before, js(&["fsck", p(&repo)]));
    for (version, id) in versions {
        let output = t.path(&format!("version-{version}.wasm"));
        ok(&[
            "show",
            p(&repo),
            &id,
            "--file",
            "a.wasm",
            "--output",
            p(&output),
        ]);
        assert_eq!(fs::read(output).unwrap(), wasm(version));
    }
    seed(&repo, 7);
    ok(&["gc", p(&repo), "--pack", "--prune=now"]);
    ok(&["fsck", p(&repo)]);
    assert_eq!(packs(&repo).len(), 1);
    let bundle = t.path("history.uwgb");
    ok(&["bundle", p(&repo), p(&bundle)]);
    let peer = t.path("peer");
    ok(&["clone", p(&repo), p(&peer)]);
    assert_eq!(js(&["fsck", p(&peer)]), js(&["fsck", p(&repo)]));
}

#[test]
#[cfg(unix)]
fn independent_forks_share_loose_and_packed_inodes_and_survive_source_removal() {
    use std::os::unix::fs::MetadataExt;
    let t = Temp::new();
    let repo = t.path("source");
    ok(&["init", p(&repo)]);
    let first = seed(&repo, 1);
    let loose = t.path("loose");
    ok(&["fork", p(&repo), p(&loose)]);
    for e in fs::read_dir(repo.join(".uwgit/objects")).unwrap() {
        let e = e.unwrap();
        assert_eq!(
            e.metadata().unwrap().ino(),
            fs::metadata(loose.join(".uwgit/objects").join(e.file_name()))
                .unwrap()
                .ino()
        );
    }
    seed(&repo, 2);
    ok(&["repack", p(&repo)]);
    let packed = t.path("packed");
    ok(&["fork", p(&repo), p(&packed)]);
    for a in packs(&repo) {
        let b = packed.join(".uwgit/packs").join(a.file_name().unwrap());
        assert_eq!(
            fs::metadata(a).unwrap().ino(),
            fs::metadata(b).unwrap().ino()
        );
    }
    seed(&packed, 3);
    let source_packs: Vec<_> = packs(&repo)
        .iter()
        .map(|p| fs::metadata(p).unwrap().ino())
        .collect();
    let gc = js(&["gc", p(&packed)]);
    assert_eq!(
        gc["repack"]["shared_base_packs"].as_u64().unwrap() as usize,
        source_packs.len()
    );
    assert!(
        packs(&packed)
            .iter()
            .any(|p| source_packs.contains(&fs::metadata(p).unwrap().ino())),
        "GC duplicated the fork base"
    );
    ok(&["gc", p(&repo)]);
    assert!(packs(&repo)
        .iter()
        .any(|p| source_packs.contains(&fs::metadata(p).unwrap().ino())));
    ok(&["fsck", p(&repo)]);
    ok(&["fsck", p(&packed)]);
    assert_ne!(
        ok(&["rev-parse", p(&packed), "HEAD"]),
        ok(&["rev-parse", p(&repo), "HEAD"])
    );
    fs::remove_dir_all(&repo).unwrap();
    ok(&["fsck", p(&packed)]);
    ok(&["fsck", p(&loose)]);
    assert_eq!(ok(&["rev-parse", p(&loose), "HEAD"]), first);
}

#[test]
fn pack_corruption_and_truncation_are_rejected_without_changing_references() {
    let t = Temp::new();
    let repo = t.path("repo");
    ok(&["init", p(&repo)]);
    let tip = seed(&repo, 1);
    ok(&["repack", p(&repo)]);
    let pack = packs(&repo).pop().unwrap();
    let original = fs::read(&pack).unwrap();
    for broken in [original[..original.len() - 1].to_vec(), {
        let mut b = original.clone();
        let n = b.len();
        b[n - 5] ^= 0xff;
        b
    }] {
        fs::write(&pack, broken).unwrap();
        assert!(!command(&["fsck", p(&repo)]).status.success());
        assert_eq!(
            fs::read(repo.join(".uwgit/refs/main")).unwrap(),
            native::reference(&tip)
        );
    }
    fs::write(pack, original).unwrap();
    ok(&["fsck", p(&repo)]);
}

#[test]
fn a_fork_target_cannot_create_foreign_entries_inside_source_metadata() {
    let t = Temp::new();
    let repo = t.path("source");
    ok(&["init", p(&repo)]);
    seed(&repo, 1);
    for directory in [
        repo.join(".uwgit/objects/foreign"),
        repo.join(".uwgit/tmp/foreign"),
    ] {
        assert!(!command(&["fork", p(&repo), p(&directory)]).status.success());
        assert!(!directory.exists());
    }
    ok(&["fsck", p(&repo)]);
}

#[test]
fn packed_gc_removes_only_unretained_objects_and_leaves_a_complete_pack() {
    let t = Temp::new();
    let repo = t.path("repo");
    ok(&["init", p(&repo)]);
    seed(&repo, 1);
    let input = t.path("unreferenced.wasm");
    fs::write(&input, wasm(2)).unwrap();
    let dangling = ok(&["hash-object", p(&repo), "-w", p(&input)]);
    ok(&["repack", p(&repo)]);
    assert!(js(&["gc", p(&repo)])["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i == &dangling));
    let result = js(&["gc", p(&repo), "--prune=now"]);
    assert!(result["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i == &dangling));
    ok(&["fsck", p(&repo)]);
    assert!(!command(&["cat-file", p(&repo), "-t", &dangling])
        .status
        .success());
}

#[test]
#[cfg(feature = "server")]
fn standalone_http_server_creates_an_authenticated_independent_fork() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let t = Temp::new();
    let repo = t.path("upstream");
    ok(&["init", p(&repo)]);
    let base = seed(&repo, 1);
    ok(&["repack", p(&repo)]);
    let token = t.path("token");
    fs::write(&token, "fork-test-token\n").unwrap();
    let mut server = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
            .args([
                "--root",
                p(&t.0),
                "--listen",
                "127.0.0.1:0",
                "--token-file",
                p(&token),
                "--allow-write",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address: Value = serde_json::from_str(&line).unwrap();
    let url = format!("http://{}/upstream", address["listen"].as_str().unwrap());
    let invoke = |name: &str, authenticated: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wasm-git"));
        command
            .args(["fork", &url, name])
            .env_remove("UWGIT_REMOTE_TOKEN");
        if authenticated {
            command.env("UWGIT_REMOTE_TOKEN", "fork-test-token");
        }
        command.output().unwrap()
    };
    assert!(!invoke("denied", false).status.success());
    assert!(!t.path("denied").exists());
    let result = invoke("child", true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let child = t.path("child");
    assert_eq!(ok(&["rev-parse", p(&child), "HEAD"]), base);
    ok(&["fsck", p(&child)]);
    assert!(!invoke("child", true).status.success());
    assert!(!invoke("../escape", true).status.success());
    ok(&[
        "restore",
        p(&child),
        "--source",
        "HEAD",
        "--staged",
        "--worktree",
    ]);
    seed(&child, 2);
    assert_eq!(ok(&["rev-parse", p(&repo), "HEAD"]), base);
    assert_ne!(ok(&["rev-parse", p(&child), "HEAD"]), base);
}

#[test]
#[cfg(feature = "server")]
fn clone_and_push_transfer_large_bundles_in_bounded_frames() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let t = Temp::new();
    let source = t.path("large");
    let target = t.path("target");
    ok(&["init", p(&source)]);
    ok(&["init", p(&target)]);
    let payload = [b"\x01x".as_slice(), &vec![0x52; 65 * 1024 * 1024]].concat();
    let bytes = [
        b"\0asm\x01\0\0\0\0".as_slice(),
        &leb(payload.len()),
        &payload,
    ]
    .concat();
    let input = t.path("large.wasm");
    fs::write(&input, &bytes).unwrap();
    ok(&["commit", p(&source), p(&input), "-m", "large original"]);
    let token = t.path("token");
    fs::write(&token, b"large-transfer-token").unwrap();
    let mut server = Guard(
        Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
            .args([
                "--root",
                p(&t.0),
                "--listen",
                "127.0.0.1:0",
                "--token-file",
                p(&token),
                "--allow-write",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address: Value = serde_json::from_str(&line).unwrap();
    let address = address["listen"].as_str().unwrap();
    let run = |args: &[&str]| {
        let r = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(args)
            .env("UWGIT_REMOTE_TOKEN", "large-transfer-token")
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&r.stderr)
        );
    };
    let peer = t.path("peer");
    run(&["clone", &format!("http://{address}/large"), p(&peer)]);
    let restored = t.path("restored.wasm");
    ok(&["show", p(&peer), "HEAD", "--output", p(&restored)]);
    assert_eq!(fs::read(&restored).unwrap(), bytes);
    // A second large blob makes the ancestor closure exceed 128 MiB, so the
    // push must stage and verify multiple frames before its one leased update.
    let mut changed = bytes;
    let end = changed.len() - 1;
    changed[end] ^= 1;
    fs::write(&input, &changed).unwrap();
    ok(&["commit", p(&peer), p(&input), "-m", "large changed"]);
    run(&[
        "remote",
        p(&peer),
        "add",
        "destination",
        &format!("http://{address}/target"),
    ]);
    run(&["push", p(&peer), "destination", "main:main"]);
    ok(&["fsck", p(&target)]);
    let published = t.path("published.wasm");
    ok(&["show", p(&target), "HEAD", "--output", p(&published)]);
    assert_eq!(fs::read(published).unwrap(), changed);
    assert_eq!(
        ok(&["rev-parse", p(&target), "HEAD"]),
        ok(&["rev-parse", p(&peer), "HEAD"])
    );
    assert!(!fs::read_dir(target.join(".uwgit/tmp")).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("upload-")));
}
