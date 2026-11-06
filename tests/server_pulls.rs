#![cfg(all(feature = "client", feature = "server"))]
//! Exercise authenticated hosting and the actual HTTP RPC, not a mock policy.
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use wasm_git::{
    binary,
    protocol::{self, BatchPublication, Operation, RefNamespace, RefUpdate, Request, Response},
    pull::*,
    repository::Repository,
};
const WASM: &[u8] = b"\0asm\x01\0\0\0";
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "uwgit-pulls-{}-{}",
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
fn success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn compile(root: &Path, name: &str, value: serde_json::Value) -> PathBuf {
    let input = root.join(format!("{name}.json"));
    let output = root.join(format!("{name}.bin"));
    fs::write(&input, value.to_string()).unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
            .args([
                format!("compile-{name}").as_str(),
                input.to_str().unwrap(),
                output.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    fs::remove_file(input).unwrap();
    assert!(fs::read(&output).unwrap().starts_with(b"UWGM"));
    output
}
struct Server {
    child: Child,
    address: String,
}
impl Server {
    fn start(root: &Path, protected: bool, extras: &[&str]) -> Self {
        let policy = compile(
            root,
            "policy",
            json!({"version":1,"principals":[
                {"name":"alice","token_sha256":format!("{:x}",Sha256::digest(b"alice")),"grants":[{"repository":"*","read":true,"write":["refs/heads/*"],"force":true,"create":true}]},
                {"name":"bob","token_sha256":format!("{:x}",Sha256::digest(b"bob")),"grants":[{"repository":"project","read":true,"write":["refs/heads/*"],"force":true,"create":false}]},
                {"name":"ci","token_sha256":format!("{:x}",Sha256::digest(b"ci")),"grants":[{"repository":"project","read":true,"write":["refs/heads/*"],"force":false,"create":false}]},
                {"name":"reader","token_sha256":format!("{:x}",Sha256::digest(b"reader")),"grants":[{"repository":"project","read":true,"write":[],"force":false,"create":false}]}
            ]}),
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"));
        cmd.args([
            "--root",
            root.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--allow-write",
            "--allow-force",
            "--policy",
            policy.to_str().unwrap(),
        ]);
        if protected {
            let protection = compile(
                root,
                "protection",
                json!({"version":1,"rules":[{"repository":"project","branches":["refs/heads/main"],"required_reviews":1,"required_checks":["build"],"check_issuers":{"build":["ci"]}}]}),
            );
            cmd.arg("--protection").arg(protection);
        }
        let mut child = cmd
            .args(extras)
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
    fn rpc(&self, actor: &str, op: Operation, payload: &[u8]) -> (Response, Vec<u8>) {
        let mut bytes = Vec::new();
        protocol::write_packet(
            &mut bytes,
            &Request {
                version: 1,
                repository: "project".into(),
                operation: op,
            },
            payload,
        )
        .unwrap();
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(stream,"POST /v1/uwgit HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {actor}\r\nContent-Length: {}\r\n\r\n",bytes.len()).unwrap();
        stream.write_all(&bytes).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        let begin = response.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
        protocol::read_complete_packet(&response[begin..]).unwrap()
    }
    fn pull(&self, actor: &str, command: PullCommand) -> Result<PullReply, String> {
        let raw = binary::encode(&command).unwrap();
        let (r, bytes) = self.rpc(actor, Operation::PullRequest {}, &raw);
        if !r.ok {
            return Err(r.error.unwrap());
        }
        assert_eq!(r.tip, Some(format!("{:x}", Sha256::digest(&raw))));
        Ok(binary::decode(&bytes).unwrap())
    }
    fn record(&self, actor: &str, command: PullCommand) -> PullRecord {
        self.pull(actor, command).unwrap().records.remove(0)
    }
    fn cli(&self, args: &[&str]) -> String {
        success(
            Command::new(env!("CARGO_BIN_EXE_wasm-git"))
                .args(["pr", &format!("http://{}/project", self.address)])
                .args(args)
                .env("UWGIT_REMOTE_TOKEN", "alice")
                .output()
                .unwrap(),
        )
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn create(draft: bool) -> PullCommand {
    PullCommand::Create {
        source_repository: "fork".into(),
        source_branch: "main".into(),
        base_branch: "main".into(),
        title: "Audit this Wasm update".into(),
        body: "Original bytes and exact identities".into(),
        draft,
    }
}
fn merge(p: &PullRecord, method: MergeMethod) -> PullCommand {
    PullCommand::Merge {
        number: p.number,
        version: p.version,
        head: p.head.clone(),
        base: p.base.clone(),
        method,
        message: "Merge audited update".into(),
    }
}
fn review(p: &PullRecord, decision: ReviewDecision) -> PullCommand {
    PullCommand::Review {
        number: p.number,
        version: p.version,
        decision,
        body: "Reviewed".into(),
    }
}
fn check(p: &PullRecord, state: CheckState) -> PullCommand {
    PullCommand::Check {
        number: p.number,
        version: p.version,
        context: "build".into(),
        state,
        description: "CI result".into(),
    }
}
fn fixture(t: &Temp) -> (Repository, Repository, String, String) {
    let repo = Repository::init(t.0.join("project")).unwrap();
    let base = repo.commit(WASM, "base").unwrap();
    let fork = Repository::fork_local(t.0.join("project"), t.0.join("fork")).unwrap();
    let head = fork.commit(&custom(b'x', 1), "feature").unwrap();
    (repo, fork, base, head)
}
fn custom(name: u8, value: u8) -> Vec<u8> {
    let mut v = WASM.to_vec();
    v.extend([0, 3, 1, name, value]);
    v
}

#[test]
fn cross_fork_protection_requires_fresh_independent_review_and_trusted_ci() {
    let t = Temp::new();
    let (repo, fork, base, head) = fixture(&t);
    let s = Server::start(&t.0, true, &[]);
    assert!(s
        .pull("reader", create(false))
        .unwrap_err()
        .contains("authorization"));
    let mut p = s.record("alice", create(true));
    assert_eq!((&p.base, &p.head), (&base, &head));
    assert!(s
        .pull("bob", merge(&p, MergeMethod::FastForward))
        .unwrap_err()
        .contains("ready"));
    p = s.record(
        "alice",
        PullCommand::Ready {
            number: p.number,
            version: p.version,
        },
    );
    assert!(s
        .pull("alice", review(&p, ReviewDecision::Approve))
        .unwrap_err()
        .contains("own"));
    assert!(s
        .pull("reader", review(&p, ReviewDecision::Approve))
        .unwrap_err()
        .contains("authorization"));
    assert!(s
        .pull("bob", merge(&p, MergeMethod::FastForward))
        .unwrap_err()
        .contains("approvals"));
    p = s.record("bob", review(&p, ReviewDecision::Approve));
    assert!(s
        .pull("bob", check(&p, CheckState::Success))
        .unwrap_err()
        .contains("issuer"));
    assert!(s
        .pull("bob", merge(&p, MergeMethod::FastForward))
        .unwrap_err()
        .contains("check"));
    p = s.record("ci", check(&p, CheckState::Failure));
    assert!(s.pull("bob", merge(&p, MergeMethod::FastForward)).is_err());
    p = s.record("ci", check(&p, CheckState::Success));
    let old_version = p.version;
    let newhead = fork.commit(&custom(b'x', 2), "refined feature").unwrap();
    p = s.record(
        "alice",
        PullCommand::Update {
            number: p.number,
            version: p.version,
            title: None,
            body: None,
            refresh: true,
        },
    );
    assert_eq!(p.head, newhead);
    assert!(s
        .pull("bob", merge(&p, MergeMethod::FastForward))
        .unwrap_err()
        .contains("approvals"));
    assert!(s
        .pull(
            "reader",
            PullCommand::Comment {
                number: p.number,
                version: old_version,
                body: "stale".into()
            }
        )
        .unwrap_err()
        .contains("version"));
    p = s.record("bob", review(&p, ReviewDecision::RequestChanges));
    assert!(s
        .pull("bob", merge(&p, MergeMethod::FastForward))
        .unwrap_err()
        .contains("requests changes"));
    p = s.record("bob", review(&p, ReviewDecision::Approve));
    p = s.record("ci", check(&p, CheckState::Success));
    let mut wrong = merge(&p, MergeMethod::FastForward);
    if let PullCommand::Merge { head, .. } = &mut wrong {
        *head = base.clone();
    }
    assert!(s.pull("bob", wrong).unwrap_err().contains("reviewed"));
    p = s.record("bob", merge(&p, MergeMethod::FastForward));
    assert_eq!(p.state, PullState::Merged);
    assert_eq!(repo.resolve("HEAD").unwrap(), newhead);
    assert_eq!(repo.wasm_bytes("HEAD").unwrap(), custom(b'x', 2));
    p = s.record(
        "reader",
        PullCommand::Comment {
            number: p.number,
            version: p.version,
            body: "Verified deployed bytes".into(),
        },
    );
    assert_eq!(p.state, PullState::Merged);
    assert!(s
        .pull(
            "alice",
            PullCommand::Reopen {
                number: p.number,
                version: p.version
            }
        )
        .is_err());
    assert!(s.pull("bob", merge(&p, MergeMethod::FastForward)).is_err());
    let raw = fs::read(t.0.join("project/.uwgit/pulls/1.bin")).unwrap();
    assert!(raw.starts_with(b"UWGPR001"));
    assert!(!raw.starts_with(b"{"));
    repo.fsck().unwrap();
}

#[test]
fn direct_atomic_and_partial_push_cannot_bypass_pr_protection() {
    let t = Temp::new();
    let (repo, fork, base, head) = fixture(&t);
    let s = Server::start(&t.0, true, &[]);
    let bundle = fork
        .network_bundle(std::slice::from_ref(&head), std::slice::from_ref(&base))
        .unwrap()
        .1;
    let update = RefUpdate {
        namespace: RefNamespace::Branch,
        name: "main".into(),
        expected: Some(base.clone()),
        new: Some(head.clone()),
        force: true,
    };
    let single = s
        .rpc(
            "alice",
            Operation::UpdateRef {
                namespace: RefNamespace::Branch,
                name: "main".into(),
                expected: Some(base.clone()),
                new: Some(head.clone()),
                force: true,
            },
            &bundle,
        )
        .0;
    assert!(!single.ok);
    assert!(single.error.unwrap().contains("protected"));
    let bytes = binary::encode(&BatchPublication {
        updates: vec![update],
        bundle,
    })
    .unwrap();
    assert!(!s.rpc("alice", Operation::PublishBatch {}, &bytes).0.ok);
    let upload_id = format!("{:x}", Sha256::digest(&bytes));
    assert!(
        s.rpc(
            "alice",
            Operation::UploadSlice {
                transfer: upload_id.clone(),
                total: bytes.len() as u64,
                offset: 0
            },
            &bytes
        )
        .0
        .ok
    );
    let uploaded = s
        .rpc(
            "alice",
            Operation::ApplyUpload {
                transfer: upload_id,
                operation: Box::new(Operation::PublishBatch {}),
            },
            &[],
        )
        .0;
    assert!(!uploaded.ok);
    assert!(uploaded.error.unwrap().contains("protected"));
    let partial = binary::encode(&protocol::OptionsPublication {
        publication: binary::decode(&bytes).unwrap(),
        options: vec![],
    })
    .unwrap();
    let r = s.rpc(
        "alice",
        Operation::PublishPartial { dry_run: false },
        &partial,
    );
    assert!(r.0.ok, "{:?}", r.0.error);
    let report: protocol::PartialReceipt = binary::decode(&r.1).unwrap();
    assert!(report.rejections[0].as_ref().unwrap().contains("protected"));
    assert_eq!(repo.resolve("HEAD").unwrap(), base);
}

#[test]
fn named_tree_pr_diff_keeps_additions_deletions_paths_and_version_leases() {
    use std::collections::BTreeMap;
    let t = Temp::new();
    let repo = Repository::init(t.0.join("project")).unwrap();
    let files = BTreeMap::from([
        ("keep.wasm".into(), WASM.to_vec()),
        ("remove.wasm".into(), custom(b'x', 1)),
    ]);
    let base = repo
        .commit_files_on_branch(&files, "initial tree", &[], "main", None, None)
        .unwrap();
    let fork = Repository::fork_local(&repo.root, t.0.join("fork")).unwrap();
    let changed = BTreeMap::from([
        ("keep.wasm".into(), WASM.to_vec()),
        ("add.wasm".into(), custom(b'x', 2)),
    ]);
    fork.commit_files_on_branch(
        &changed,
        "tree edits",
        std::slice::from_ref(&base),
        "main",
        Some(&base),
        None,
    )
    .unwrap();
    let s = Server::start(&t.0, false, &[]);
    let p = s.record("alice", create(false));
    let files = s
        .pull(
            "reader",
            PullCommand::Files {
                number: p.number,
                version: p.version,
                against: DiffBase::MergeBase,
            },
        )
        .unwrap()
        .files;
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "add.wasm");
    assert!(files[0].old.is_none());
    assert_eq!(files[1].path, "remove.wasm");
    assert!(files[1].new.is_none());
    for (path, status) in [("add.wasm", "added"), ("remove.wasm", "deleted")] {
        let audit: serde_json::Value =
            serde_json::from_str(&s.cli(&["diff", "1", "--file", path, "--format", "json"]))
                .unwrap();
        assert_eq!(audit["status"], status);
    }
    s.record(
        "reader",
        PullCommand::Comment {
            number: p.number,
            version: p.version,
            body: "read raced a comment".into(),
        },
    );
    assert!(s
        .pull(
            "reader",
            PullCommand::File {
                number: p.number,
                version: p.version,
                path: "add.wasm".into(),
                old: false,
                offset: 0,
                against: DiffBase::MergeBase
            }
        )
        .unwrap_err()
        .contains("version"));
    let p = repo.pull(p.number).unwrap();
    let p = s.record("bob", merge(&p, MergeMethod::Merge));
    assert_eq!(
        repo.files_at(&p.merge.unwrap().commit).unwrap().files,
        changed
    );
}

#[test]
fn changed_target_invalidates_approvals_even_when_the_source_is_unchanged() {
    let t = Temp::new();
    let (repo, _, _, head) = fixture(&t);
    let s = Server::start(&t.0, true, &[]);
    let mut p = s.record("alice", create(false));
    p = s.record("bob", review(&p, ReviewDecision::Approve));
    p = s.record("ci", check(&p, CheckState::Success));
    repo.commit(WASM, "target metadata update").unwrap();
    p = s.record(
        "alice",
        PullCommand::Update {
            number: p.number,
            version: p.version,
            title: Some("Refreshed proposal".into()),
            body: None,
            refresh: true,
        },
    );
    assert_eq!(p.head, head);
    assert!(s
        .pull("bob", merge(&p, MergeMethod::Merge))
        .unwrap_err()
        .contains("approvals"));
    assert!(
        matches!(&p.events.last().unwrap().kind,PullEventKind::Updated { title,.. } if title=="Refreshed proposal")
    );
    p = s.record("bob", review(&p, ReviewDecision::Approve));
    assert!(s
        .pull("bob", merge(&p, MergeMethod::Merge))
        .unwrap_err()
        .contains("check"));
    p = s.record("ci", check(&p, CheckState::Success));
    assert_eq!(
        s.record("bob", merge(&p, MergeMethod::Merge)).state,
        PullState::Merged
    );
}

#[test]
fn discussion_lifecycle_cas_and_pagination_are_durable() {
    let t = Temp::new();
    fixture(&t);
    let s = Server::start(&t.0, false, &[]);
    let first = s.record("alice", create(false));
    let second = s.record("alice", create(true));
    let page = s
        .pull("reader", PullCommand::List { after: 0, limit: 1 })
        .unwrap();
    assert_eq!(page.records[0].number, first.number);
    assert_eq!(page.next, Some(first.number));
    let page = s
        .pull(
            "reader",
            PullCommand::List {
                after: page.next.unwrap(),
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(page.records[0].number, second.number);
    assert_eq!(page.next, None);
    assert!(s
        .pull("reader", PullCommand::List { after: 0, limit: 0 })
        .is_err());
    assert!(s
        .pull(
            "reader",
            PullCommand::Close {
                number: first.number,
                version: first.version
            }
        )
        .is_err());
    let mut p = s.record(
        "alice",
        PullCommand::Close {
            number: first.number,
            version: first.version,
        },
    );
    assert_eq!(p.state, PullState::Closed);
    p = s.record(
        "reader",
        PullCommand::Comment {
            number: p.number,
            version: p.version,
            body: "Keep the history".into(),
        },
    );
    p = s.record(
        "alice",
        PullCommand::Reopen {
            number: p.number,
            version: p.version,
        },
    );
    p = s.record(
        "alice",
        PullCommand::Draft {
            number: p.number,
            version: p.version,
        },
    );
    assert_eq!(p.state, PullState::Draft);
    p = s.record(
        "alice",
        PullCommand::Ready {
            number: p.number,
            version: p.version,
        },
    );
    let output: serde_json::Value =
        serde_json::from_str(&s.cli(&["show", &p.number.to_string()])).unwrap();
    assert_eq!(output["records"][0]["version"], p.version);
    assert_eq!(
        p.events
            .iter()
            .filter(|e| matches!(e.kind, PullEventKind::Comment { .. }))
            .count(),
        1
    );
}

#[test]
fn frozen_pr_history_survives_fork_removal_and_gc_and_detects_record_corruption() {
    let t = Temp::new();
    let (repo, _, _, head) = fixture(&t);
    let s = Server::start(&t.0, false, &[]);
    let mut p = s.record("alice", create(false));
    p = s.record(
        "alice",
        PullCommand::Close {
            number: p.number,
            version: p.version,
        },
    );
    fs::remove_dir_all(t.0.join("fork")).unwrap();
    repo.garbage_collect_mode(&[], true, true).unwrap();
    assert_eq!(repo.wasm_bytes(&head).unwrap(), custom(b'x', 1));
    let files = s
        .pull(
            "reader",
            PullCommand::Files {
                number: p.number,
                version: p.version,
                against: DiffBase::MergeBase,
            },
        )
        .unwrap();
    assert_eq!(files.files.len(), 1);
    let chunk = s
        .pull(
            "reader",
            PullCommand::File {
                number: p.number,
                version: p.version,
                path: "".into(),
                old: false,
                offset: 0,
                against: DiffBase::MergeBase,
            },
        )
        .unwrap()
        .chunk
        .unwrap();
    assert_eq!(chunk.bytes, custom(b'x', 1));
    repo.fsck().unwrap();
    fs::write(t.0.join("project/.uwgit/pulls/1.bin"), b"corrupt").unwrap();
    assert!(repo.fsck().unwrap_err().contains("pull record"));
    assert!(repo.garbage_collect(&[], true).is_err());
}

#[test]
fn merge_and_squash_preserve_independent_changes_and_audit_the_proposed_diff() {
    for method in [MergeMethod::Merge, MergeMethod::Squash] {
        let t = Temp::new();
        let repo = Repository::init(t.0.join("project")).unwrap();
        let module = |a, b| {
            wat::parse_str(format!(
                "(module (func (result i32) i32.const {a}) (func (result i32) i32.const {b}))"
            ))
            .unwrap()
        };
        let ancestor = repo.commit(&module(1, 2), "base").unwrap();
        let fork = Repository::fork_local(t.0.join("project"), t.0.join("fork")).unwrap();
        let head = fork
            .commit(&module(3, 2), "proposed function edit")
            .unwrap();
        let target = repo
            .commit(&module(1, 4), "independent target function edit")
            .unwrap();
        let s = Server::start(&t.0, false, &[]);
        let p = s.record("alice", create(false));
        assert_eq!(p.base, target);
        assert!(s
            .pull("bob", merge(&p, MergeMethod::FastForward))
            .unwrap_err()
            .contains("fast-forward"));
        let files = s
            .pull(
                "reader",
                PullCommand::Files {
                    number: p.number,
                    version: p.version,
                    against: DiffBase::MergeBase,
                },
            )
            .unwrap();
        assert_eq!(files.diff_base, Some(ancestor.clone()));
        let old = s
            .pull(
                "reader",
                PullCommand::File {
                    number: p.number,
                    version: p.version,
                    path: "".into(),
                    old: true,
                    offset: 0,
                    against: DiffBase::MergeBase,
                },
            )
            .unwrap()
            .chunk
            .unwrap();
        assert_eq!(old.bytes, module(1, 2));
        let old = s
            .pull(
                "reader",
                PullCommand::File {
                    number: p.number,
                    version: p.version,
                    path: "".into(),
                    old: true,
                    offset: 0,
                    against: DiffBase::Target,
                },
            )
            .unwrap()
            .chunk
            .unwrap();
        assert_eq!(old.bytes, module(1, 4));
        let audit: serde_json::Value =
            serde_json::from_str(&s.cli(&["diff", "1", "--format", "json"])).unwrap();
        let rendered = audit.to_string();
        assert!(rendered.contains(&ancestor));
        assert!(rendered.contains(&head));
        let p = s.record("bob", merge(&p, method));
        let id = &p.merge.unwrap().commit;
        let c = repo.read_commit(id).unwrap().commit;
        assert_eq!(
            c.parents,
            if method == MergeMethod::Merge {
                vec![target, head]
            } else {
                vec![target]
            }
        );
        let bytes = repo.wasm_bytes(id).unwrap();
        assert_eq!(bytes, module(3, 4));
        repo.fsck().unwrap();
    }
}

#[test]
fn stale_target_and_semantic_conflicts_never_publish_a_merge() {
    let t = Temp::new();
    let (repo, _, base, _) = fixture(&t);
    let s = Server::start(&t.0, false, &[]);
    let mut p = s.record("alice", create(false));
    let changed = repo.commit(&custom(b'x', 3), "conflicting base").unwrap();
    assert!(s.pull("bob", merge(&p, MergeMethod::Merge)).is_err());
    assert_eq!(repo.pull(p.number).unwrap().state, PullState::Open);
    p = s.record(
        "alice",
        PullCommand::Update {
            number: p.number,
            version: p.version,
            title: None,
            body: None,
            refresh: true,
        },
    );
    assert_eq!(p.revisions[0].base, base);
    assert_eq!(p.base, changed);
    assert!(s
        .pull("bob", merge(&p, MergeMethod::Merge))
        .unwrap_err()
        .contains("conflict"));
    assert_eq!(repo.resolve("HEAD").unwrap(), changed);
    assert_eq!(repo.pull(p.number).unwrap().state, PullState::Open);
}

#[test]
fn lost_merge_acknowledgement_recovers_from_exact_ref_log_evidence() {
    let t = Temp::new();
    let (repo, _, base, head) = fixture(&t);
    let s = Server::start(&t.0, false, &[]);
    let p = s.record("alice", create(false));
    let prepare = |version| {
        repo.prepare_pull_merge(
            p.number,
            version,
            PullMergeOptions {
                method: MergeMethod::Merge,
                message: "Recover me",
                actor: "bob",
                signing: None,
            },
            |_| Ok(()),
        )
        .unwrap()
    };
    let (pending, _) = prepare(p.version);
    assert_eq!(pending.state, PullState::Merging);
    assert!(repo
        .edit_pull(
            p.number,
            pending.version,
            |_| Ok(PullEventKind::Comment {
                body: "interleave".into()
            }),
            "alice",
            None
        )
        .is_err());
    let aborted = repo.recover_pull_merge(p.number, pending.version).unwrap();
    assert_eq!(aborted.state, PullState::Open);
    let (pending, bundle) = prepare(aborted.version);
    let candidate = pending.merge.as_ref().unwrap().commit.clone();
    // Publish, then deliberately discard the completion record/acknowledgement.
    repo.receive_batch_checked(
        &bundle,
        &[RefUpdate {
            namespace: RefNamespace::Branch,
            name: "main".into(),
            expected: Some(base),
            new: Some(candidate.clone()),
            force: false,
        }],
        |_| Ok(()),
    )
    .unwrap();
    repo.commit(&custom(b'x', 4), "later commit").unwrap();
    let recovered = s.record(
        "bob",
        PullCommand::Recover {
            number: p.number,
            version: pending.version,
        },
    );
    assert_eq!(recovered.state, PullState::Merged);
    assert_eq!(recovered.merge.unwrap().commit, candidate);
    assert_ne!(repo.resolve("HEAD").unwrap(), candidate);
    assert_eq!(
        repo.read_commit(&candidate).unwrap().commit.parents[1],
        head
    );
}

#[cfg(unix)]
#[test]
fn receive_hooks_reject_without_ref_publication_and_post_failure_does_not_undo_merge() {
    use std::os::unix::fs::PermissionsExt;
    for reject in [true, false] {
        let t = Temp::new();
        let (repo, _, base, head) = fixture(&t);
        let hook = t.0.join("hook");
        fs::write(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        let s = Server::start(
            &t.0,
            false,
            &[
                if reject {
                    "--pre-receive"
                } else {
                    "--post-receive"
                },
                hook.to_str().unwrap(),
            ],
        );
        let p = s.record("alice", create(false));
        let result = s.pull("bob", merge(&p, MergeMethod::FastForward));
        if reject {
            assert!(result.is_err());
            assert_eq!(repo.resolve("HEAD").unwrap(), base);
            assert_eq!(repo.pull(p.number).unwrap().state, PullState::Open);
        } else {
            assert_eq!(result.unwrap().records[0].state, PullState::Merged);
            assert_eq!(repo.resolve("HEAD").unwrap(), head);
        }
    }
}

#[cfg(unix)]
#[test]
fn signed_admission_checks_cross_fork_commits_and_server_generated_merge() {
    let t = Temp::new();
    let (repo, fork, _, unsigned) = fixture(&t);
    let key = t.0.join("key");
    let public = t.0.join("key.pub");
    let trust = t.0.join("authors.bin");
    success(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    for author in ["Alice", "Merge service"] {
        success(
            Command::new(env!("CARGO_BIN_EXE_wasm-git"))
                .args([
                    "trust-author",
                    trust.to_str().unwrap(),
                    author,
                    public.to_str().unwrap(),
                    "--format",
                    "ssh",
                ])
                .output()
                .unwrap(),
        );
    }
    let s = Server::start(
        &t.0,
        false,
        &[
            "--signed-authors",
            trust.to_str().unwrap(),
            "--merge-signing-key",
            key.to_str().unwrap(),
            "--merge-author",
            "Merge service",
            "--merge-signing-format",
            "ssh",
        ],
    );
    let mut p = s.record("alice", create(false));
    assert!(s
        .pull("bob", merge(&p, MergeMethod::Merge))
        .unwrap_err()
        .contains("signature"));
    assert_eq!(repo.pull(p.number).unwrap().state, PullState::Open);
    let signed = fork
        .sign_head_format(
            &unsigned,
            &wasm_git::repository::SigningIdentity {
                author: "Alice".into(),
                key,
                timestamp: 123,
            },
            wasm_git::repository::SignatureFormat::Ssh,
        )
        .unwrap();
    p = repo.pull(p.number).unwrap();
    p = s.record(
        "alice",
        PullCommand::Update {
            number: p.number,
            version: p.version,
            title: None,
            body: None,
            refresh: true,
        },
    );
    assert_eq!(p.head, signed);
    let p = s.record("bob", merge(&p, MergeMethod::Merge));
    let commit = p.merge.unwrap().commit;
    assert_eq!(
        repo.verify_author(&commit, Some(&trust)).unwrap()["author"],
        "Merge service"
    );
    assert_eq!(
        repo.verify_author(&signed, Some(&trust)).unwrap()["format"],
        "ssh"
    );
}
