#![cfg(feature = "client")]
use std::process::Command;
#[test]
fn reported_build_features_match_the_compiled_client() {
    let r = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
        .args(["version", "--build-options"])
        .output()
        .unwrap();
    assert!(r.status.success());
    let v: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(v["client"], true);
    assert_eq!(v["server"], cfg!(feature = "server"));
    if !cfg!(feature = "server") {
        let r = Command::new(env!("CARGO_BIN_EXE_wasm-git"))
            .args(["server", "--help"])
            .output()
            .unwrap();
        assert!(!r.status.success());
        assert!(String::from_utf8_lossy(&r.stderr).contains("server support is not compiled"));
    }
}
#[test]
#[cfg(feature = "server")]
fn standalone_server_executable_has_its_own_entry_point() {
    let r = Command::new(env!("CARGO_BIN_EXE_wasm-git-server"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(r.status.success());
    assert!(String::from_utf8_lossy(&r.stdout).contains("--stdio --root"));
}
