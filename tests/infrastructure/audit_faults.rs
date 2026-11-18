//! Real SQLite fault injection at the holder's audit persistence boundary.
use std::path::Path;

pub fn fail_observations(root: &Path) {
    let connection = rusqlite::Connection::open(root.join(".wasm-git-authorization.bin")).unwrap();
    connection.execute_batch("CREATE TRIGGER test_observation_failure BEFORE UPDATE OF observation ON audit_index BEGIN SELECT RAISE(ABORT, 'injected observation failure'); END").unwrap();
}

pub fn restore_observations(root: &Path) {
    let connection = rusqlite::Connection::open(root.join(".wasm-git-authorization.bin")).unwrap();
    connection
        .execute_batch("DROP TRIGGER test_observation_failure")
        .unwrap();
}
