#![cfg(all(feature = "client", unix))]
//! Real OpenSSH/GnuPG interoperability, with isolated ephemeral test keys.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use wasm_git::repository::Repository;
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "sig-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        if self.0.join("gpg").exists() {
            let _ = Command::new("gpgconf")
                .env("GNUPGHOME", self.0.join("gpg"))
                .args(["--kill", "gpg-agent"])
                .output();
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn ok(out: Output) -> Vec<u8> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn cli(t: &Temp, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .env("GNUPGHOME", t.0.join("gpg"))
        .args(args)
        .output()
        .unwrap()
}
fn exercise(t: &Temp, format: &str, key: &str, public: &Path) -> (Repository, String, PathBuf) {
    let repo = Repository::init(t.0.join("repo")).unwrap();
    let first = wat::parse_str("(module (func (result i32) i32.const 1))").unwrap();
    let base = repo.commit(&first, "base").unwrap();
    let input = t.0.join("input.wasm");
    fs::write(
        &input,
        wat::parse_str("(module (func (result i32) i32.const 2))").unwrap(),
    )
    .unwrap();
    let trust = t.0.join("trust.bin");
    ok(cli(
        t,
        &[
            "trust-author",
            p(&trust),
            "Alice",
            p(public),
            "--format",
            format,
        ],
    ));
    let signed = String::from_utf8(ok(cli(
        t,
        &[
            "commit-signed",
            p(&repo.root),
            p(&input),
            "-m",
            "audited update",
            "--author",
            "Alice",
            "--key",
            key,
            "--format",
            format,
            "--timestamp",
            "123",
        ],
    )))
    .unwrap()
    .trim()
    .to_owned();
    let report = repo.verify_author(&signed, Some(&trust)).unwrap();
    assert_eq!(report["format"], format);
    assert_eq!(report["trusted"], true);
    let original = repo.read_commit(&signed).unwrap().commit;
    for field in [
        "message",
        "content",
        "parents",
        "author",
        "timestamp",
        "signature",
    ] {
        let mut commit = original.clone();
        match field {
            "message" => commit.message.push('!'),
            "content" => commit.wasm = repo.read_commit(&base).unwrap().commit.wasm,
            "parents" => commit.parents.clear(),
            "author" => commit.signature.as_mut().unwrap().author = "Mallory".into(),
            "timestamp" => commit.signature.as_mut().unwrap().timestamp += 1,
            "signature" => {
                *commit
                    .signature
                    .as_mut()
                    .unwrap()
                    .signature
                    .last_mut()
                    .unwrap() ^= 1
            }
            _ => unreachable!(),
        }
        // Recompute a valid object hash: integrity alone must not authenticate an author.
        let forged = repo
            .hash_payload("commit", &commit.to_bytes().unwrap(), true)
            .unwrap();
        assert!(
            repo.verify_author(&forged, None).is_err(),
            "unbound {format} field: {field}"
        );
    }
    // Structural history inspection remains available without launching key tools.
    repo.fsck().unwrap();
    repo.garbage_collect_mode(&[], true, true).unwrap();
    let fork = Repository::fork_local(&repo.root, t.0.join("fork")).unwrap();
    assert_eq!(
        fork.verify_author(&signed, Some(&trust)).unwrap()["author"],
        "Alice"
    );
    let wrong = t.0.join("wrong.bin");
    ok(cli(
        t,
        &[
            "trust-author",
            p(&wrong),
            "Bob",
            p(public),
            "--format",
            format,
        ],
    ));
    assert!(repo
        .verify_author(&signed, Some(&wrong))
        .unwrap_err()
        .contains("not trusted"));
    (repo, signed, trust)
}

#[test]
fn openssh_commit_signatures_bind_every_field_and_author_revocation() {
    let t = Temp::new();
    let key = t.0.join("id_ed25519");
    let public = t.0.join("id_ed25519.pub");
    ok(Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", p(&key)])
        .output()
        .expect("OpenSSH ssh-keygen is required for this integration test"));
    let (repo, id, trust) = exercise(&t, "ssh", p(&key), &public);
    let envelope = &repo
        .read_commit(&id)
        .unwrap()
        .commit
        .signature
        .unwrap()
        .signature;
    assert!(!envelope.windows(10).any(|w| w == b"-----BEGIN"));
    assert!(envelope.windows(6).any(|w| w == b"SSHSIG"));
    ok(cli(
        &t,
        &[
            "untrust-author",
            p(&trust),
            "Alice",
            "--public",
            p(&public),
            "--format",
            "ssh",
        ],
    ));
    assert!(repo
        .verify_author(&id, Some(&trust))
        .unwrap_err()
        .contains("not trusted"));
    assert_eq!(repo.verify_author(&id, None).unwrap()["trusted"], false);
}

#[test]
fn openpgp_signing_subkey_is_bound_to_primary_and_updated_revocation_is_authoritative() {
    use std::os::unix::fs::PermissionsExt;
    let t = Temp::new();
    let home = t.0.join("gpg");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let gpg = |args: &[&str]| {
        Command::new("gpg")
            .env("GNUPGHOME", &home)
            .args([
                "--no-options",
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
            ])
            .args(args)
            .output()
            .expect("GnuPG is required for this integration test")
    };
    ok(gpg(&[
        "--quick-generate-key",
        "Alice Test <alice@example.invalid>",
        "ed25519",
        "cert",
        "0",
    ]));
    let listing = String::from_utf8(ok(gpg(&["--with-colons", "--list-keys"]))).unwrap();
    let fingerprint = listing
        .lines()
        .find(|l| l.starts_with("fpr:"))
        .unwrap()
        .split(':')
        .nth(9)
        .unwrap();
    ok(gpg(&[
        "--quick-add-key",
        fingerprint,
        "ed25519",
        "sign",
        "0",
    ]));
    let public = t.0.join("public.pgp");
    fs::write(&public, ok(gpg(&["--export", fingerprint]))).unwrap();
    let (repo, id, trust) = exercise(&t, "openpgp", fingerprint, &public);
    // GnuPG creates an offline primary-key revocation certificate with a safety colon.
    let revocation = fs::read_to_string(
        home.join("openpgp-revocs.d")
            .join(format!("{fingerprint}.rev")),
    )
    .unwrap();
    let revocation = &revocation.as_bytes()[revocation
        .find(":-----BEGIN PGP PUBLIC KEY BLOCK-----")
        .unwrap()
        + 1..];
    let file = t.0.join("revoke.asc");
    fs::write(&file, revocation).unwrap();
    ok(gpg(&["--import", p(&file)]));
    fs::write(&public, ok(gpg(&["--export", fingerprint]))).unwrap();
    ok(cli(
        &t,
        &[
            "trust-author",
            p(&trust),
            "Alice",
            p(&public),
            "--format",
            "openpgp",
        ],
    ));
    assert!(
        repo.verify_author(&id, Some(&trust)).is_err(),
        "embedded old certificate bypassed revocation"
    );
    // Revocation affects trust/admission, not historical object readability.
    repo.fsck().unwrap();
    assert_eq!(
        repo.read_commit(&id).unwrap().commit.message,
        "audited update"
    );
}
