#![cfg(all(feature = "client", unix))]

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use wasm_git::repository::Repository;

static NEXT: AtomicU64 = AtomicU64::new(0);
const WASM: &[u8] = b"\0asm\x01\0\0\0";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp/writer-lock-tests");
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(format!(
            "{}-{}",
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

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn seed(temp: &Temp) -> (PathBuf, String) {
    let root = temp.0.join("repo");
    let repo = Repository::init(&root).unwrap();
    let tip = repo.commit(WASM, "initial commit").unwrap();
    (root, tip)
}

fn linked(root: &Path, temp: &Temp) -> PathBuf {
    linked_named(root, temp, "linked")
}

fn linked_named(root: &Path, temp: &Temp, name: &str) -> PathBuf {
    let linked = temp.0.join(name);
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .arg("worktree")
        .arg(root)
        .arg("add")
        .arg(&linked)
        .args(["-b", name, "--legacy-path", "module.wasm"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    linked
}

fn start_batch(
    root: &Path,
    commands: &str,
) -> (Running, std::process::ChildStdin, Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .arg("update-ref")
        .arg(root)
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    write!(stdin, "start\n{commands}prepare\n").unwrap();
    stdin.flush().unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(30)).unwrap(),
        "start: ok"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(30)).unwrap(),
        "prepare: ok"
    );
    (Running(child), stdin, rx)
}

fn inode(path: &Path) -> (u64, u64) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

#[test]
fn killed_prepared_writer_releases_one_inode_shared_by_worktrees_and_aliases() {
    let temp = Temp::new();
    let (root, tip) = seed(&temp);
    let linked = linked(&root, &temp);
    let alias_parent = temp.0.join("alias");
    symlink(&temp.0, &alias_parent).unwrap();
    let alias = alias_parent.join("repo");
    let path = root.join(".wasm-git/write.lock");
    let before = inode(&path);
    let (mut writer, _stdin, _rx) =
        start_batch(&root, &format!("create refs/tags/prepared {tip}\n"));

    for worktree in [&root, &linked, &alias] {
        let repo = Repository::open(worktree).unwrap();
        assert!(repo.branches().unwrap_err().contains("locked"));
    }
    let hardlink = temp.0.join("lock-alias");
    fs::hard_link(&path, &hardlink).unwrap();
    let other = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&hardlink)
        .unwrap();
    assert!(other.try_lock().is_err());
    assert_eq!(inode(&hardlink), before);

    writer.0.kill().unwrap();
    assert!(!writer.0.wait().unwrap().success());
    assert_eq!(inode(&path), before);
    assert!(Repository::open(&linked)
        .unwrap()
        .resolve("prepared")
        .is_err());
    let repo = Repository::open(&alias).unwrap();
    assert_eq!(repo.create_tag("after-crash", "HEAD").unwrap(), tip);
    assert_eq!(inode(&path), before);
    assert!(path.is_file());
}

#[test]
fn concurrent_first_acquisition_publishes_only_one_stable_lock_inode() {
    let temp = Temp::new();
    let (root, tip) = seed(&temp);
    let path = root.join(".wasm-git/write.lock");
    let trees: Vec<_> = (0..4)
        .map(|index| linked_named(&root, &temp, &format!("linked-{index}")))
        .collect();
    let mut workers = Vec::new();
    for (index, tree) in trees.iter().enumerate() {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .arg("update-ref")
            .arg(tree)
            .arg("--stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        write!(stdin, "start\n").unwrap();
        stdin.flush().unwrap();
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => assert_eq!(line, "start: ok"),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let mut stderr = String::new();
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("worker {index} could not start ({error}): {stderr}");
            }
        }
        workers.push((Running(child), stdin, rx, index));
    }
    // Every process now holds its own worktree index, but no writer guard.
    // Reproduce a pre-upgrade repository with no writer lock file.
    fs::remove_file(&path).unwrap();
    for (_, stdin, _, index) in &mut workers {
        write!(stdin, "create refs/tags/race-{index} {tip}\nprepare\n").unwrap();
        stdin.flush().unwrap();
    }
    let mut acquired = 0;
    for (child, _, rx, index) in &mut workers {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => {
                assert_eq!(line, "prepare: ok");
                acquired += 1;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.0.wait().unwrap();
                let mut stderr = String::new();
                child
                    .0
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                eprintln!("worker {index} stderr: {stderr}");
                assert!(
                    !status.success(),
                    "worker {index} exited successfully: {stderr}"
                );
                assert!(
                    stderr.contains("resource is locked"),
                    "worker {index}: {stderr}"
                );
            }
            Err(error) => {
                let _ = child.0.kill();
                let _ = child.0.wait();
                let mut stderr = String::new();
                child
                    .0
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("worker {index} did not finish preparing ({error}): {stderr}");
            }
        }
    }
    assert_eq!(acquired, 1);
    let identity = inode(&path);
    for (mut child, stdin, _, _) in workers {
        drop(stdin);
        let _ = child.0.wait().unwrap();
    }
    assert!(Repository::open(&root).unwrap().branches().is_ok());
    assert_eq!(inode(&path), identity);
}

#[test]
fn killed_committed_writer_rolls_forward_its_original_journal() {
    const COUNT: usize = 512;
    let temp = Temp::new();
    let (root, tip) = seed(&temp);
    let linked = linked(&root, &temp);
    let lock = root.join(".wasm-git/write.lock");
    let identity = inode(&lock);
    let mut commands = String::new();
    for index in 0..COUNT {
        commands.push_str(&format!("create refs/tags/crash-{index:04} {tip}\n"));
    }
    let (mut writer, mut stdin, _rx) = start_batch(&root, &commands);
    stdin.write_all(b"commit\n").unwrap();
    stdin.flush().unwrap();
    let journal = root.join(".wasm-git/ref-transaction.bin");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !journal.is_file() {
        assert!(
            Instant::now() < deadline,
            "writer did not publish its journal"
        );
        assert!(
            writer.0.try_wait().unwrap().is_none(),
            "writer exited before its journal was observed"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    writer.0.kill().unwrap();
    assert!(!writer.0.wait().unwrap().success());
    assert!(
        journal.is_file(),
        "test did not interrupt the committed transaction"
    );
    assert_eq!(inode(&lock), identity);

    let recovered = Repository::open(&linked).unwrap();
    assert!(!journal.exists());
    let tags = recovered.tags().unwrap();
    assert_eq!(tags.len(), COUNT);
    for index in 0..COUNT {
        assert_eq!(tags.get(&format!("crash-{index:04}")), Some(&tip));
    }
    assert_eq!(inode(&lock), identity);
}

#[test]
fn legacy_and_redirected_lock_files_are_never_taken_over() {
    let temp = Temp::new();
    let (root, _) = seed(&temp);
    let lock = root.join(".wasm-git/write.lock");
    let legacy = [b"WGTLCK\0\x01".as_slice(), &42u32.to_be_bytes()].concat();
    // Controlled fixture conversion with no live owner simulates an old binary's lock.
    fs::write(&lock, &legacy).unwrap();
    let repo = Repository::open(&root).unwrap();
    assert!(repo.branches().unwrap_err().contains("legacy"));
    assert_eq!(fs::read(&lock).unwrap(), legacy);

    fs::remove_file(&lock).unwrap();
    let outside = temp.0.join("outside");
    fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, &lock).unwrap();
    assert!(repo.branches().is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    assert!(fs::symlink_metadata(&lock)
        .unwrap()
        .file_type()
        .is_symlink());

    fs::remove_file(&lock).unwrap();
    let fifo = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
    // SAFETY: fifo is a live NUL-terminated path in this test's directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(repo.branches().is_err());
    assert!(fs::symlink_metadata(&lock).unwrap().file_type().is_fifo());
}
