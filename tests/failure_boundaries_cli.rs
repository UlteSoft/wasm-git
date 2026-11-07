#![cfg(feature = "client")]

use std::process::Command;

#[test]
fn diagnostics_cannot_execute_terminal_controls_or_reorder_text() {
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .arg("bad\u{1b}[2J\r\ncommand\u{202e}")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!error.contains(['\u{1b}', '\r', '\u{202e}']));
    assert!(error.contains(r"bad\u{1b}[2J\r\ncommand\u{202e}"));
    assert!(!error.contains("panicked"));
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_fail_without_panicking_in_both_entry_points() {
    use std::os::unix::ffi::OsStringExt;
    let mut binaries = vec![env!("CARGO_BIN_EXE_wasm-git")];
    if cfg!(feature = "server") {
        binaries.extend(option_env!("CARGO_BIN_EXE_wasm-git-server"));
    }
    for binary in binaries {
        let output = Command::new(binary)
            .arg(std::ffi::OsString::from_vec(vec![0xff, 0x80]))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("argument 1 is not valid UTF-8"));
        assert!(!error.contains("panicked"));
    }
}

#[cfg(unix)]
#[test]
fn a_closed_stdout_is_a_clean_early_consumer_exit() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let writer: OwnedFd = writer.into();
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .arg("version")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn help_and_errors_keep_their_exit_codes_when_stderr_is_closed() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    for (arg, expected) in [("--help", 0), ("invalid", 1)] {
        let (writer, reader) = UnixStream::pair().unwrap();
        drop(reader);
        let writer: OwnedFd = writer.into();
        let output = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .arg(arg)
            .stderr(Stdio::from(writer))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected));
    }
}

#[cfg(all(unix, feature = "server"))]
#[test]
fn server_logging_failures_preserve_publication_and_readiness_errors_are_controlled() {
    use std::fs;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use wasm_git::{
        binary,
        protocol::{self, BatchPublication, Operation, RefNamespace, RefUpdate, Request, Response},
        repository::Repository,
    };
    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let temp = Temp(std::env::temp_dir().join(format!(
        "wasm-git-server-log-boundary-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    let host = temp.0.join("host");
    fs::create_dir_all(&host).unwrap();
    let repo = Repository::init(host.join("project")).unwrap();
    let base = repo.commit(b"\0asm\x01\0\0\0", "base").unwrap();
    let client = Repository::clone_local(&repo.root, temp.0.join("client")).unwrap();
    let next = client
        .commit(b"\0asm\x01\0\0\0\0\x03\x01x\x42", "update")
        .unwrap();
    let payload = binary::encode(&BatchPublication {
        updates: vec![RefUpdate {
            namespace: RefNamespace::Branch,
            name: "main".into(),
            expected: Some(base.clone()),
            new: Some(next.clone()),
            force: false,
        }],
        bundle: client
            .network_bundle(std::slice::from_ref(&next), &[base])
            .unwrap()
            .1,
    })
    .unwrap();
    // An administrator hook fails after publication while its diagnostic
    // consumer is closed. The response and repository must still report success.
    let hook = temp.0.join("reject.sh");
    fs::write(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let writer: OwnedFd = writer.into();
    let mut child = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--stdio",
            "--root",
            host.to_str().unwrap(),
            "--allow-write",
            "--post-receive",
            hook.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(writer))
        .spawn()
        .unwrap();
    protocol::write_packet(
        &mut child.stdin.take().unwrap(),
        &Request {
            version: 1,
            repository: "project".into(),
            operation: Operation::PublishBatch {},
        },
        &payload,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let (response, _) = protocol::read_complete_packet::<Response>(&output.stdout).unwrap();
    assert!(response.ok && !response.publication_unknown);
    assert_eq!(repo.resolve("HEAD").unwrap(), next);
    repo.fsck().unwrap();

    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let writer: OwnedFd = writer.into();
    let output = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .args([
            "--root",
            host.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--max-connections",
            "1",
        ])
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("write server readiness"));
    assert!(!error.contains("panicked"));
}
