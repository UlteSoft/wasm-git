//! SQLite persistence for the root policy and its immutable operation receipts.
//! The caller holds the root authorization gate around every operation.

#[cfg(test)]
use crate::protocol::AuditEvidenceObservation;
use crate::protocol::{
    AuditAttribution, AuditItem, AuditObservation, AuditObservationScope, AuditObservationSource,
    AuditOperationScope, AuditOperator, AuditQueryReply, AuditQueryRequest,
    AuditRecordedObservation, AuditSummary, AuditTarget, Operation, OperationCredentialReceipt,
    OperationCredentialRequest, PolicyMigrationReceipt, PolicyMigrationRequest, PolicyQueryReply,
    PolicyQueryRequest, PolicyStatus, PolicyUpdateOutcome, PolicyUpdateReceipt,
    PolicyUpdateRequest, RecoveryAuditAction, Request, ServerPolicy, AUDIT_PAGE_MAX_BYTES,
    NODE_SAFE_INTEGER_MAX,
};
use rusqlite::{
    limits::Limit, params, Connection, OpenFlags, OptionalExtension, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

const FILE: &str = ".wasm-git-authorization.bin";
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
const LEGACY_HEADER: &[u8; 8] = b"WGTM\0\0\0\x01";
const APPLICATION_ID: i64 = 0x5747_504c;
const SCHEMA_VERSION: i64 = 2;
const CREDENTIAL_SCHEMA_VERSION: i64 = 3;
const CHUNK_RECEIPT_SCHEMA_VERSION: i64 = 4;
const AUDIT_SCHEMA_VERSION: i64 = 5;
const AUDIT_CREDENTIAL_SCHEMA_VERSION: i64 = 6;
const MAX_POLICY_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECEIPT_BYTES: usize = 4096;
const MAX_AUDIT_RECORD_BYTES: usize = 128 * 1024;
const MAX_AUDIT_OBSERVATION_BYTES: usize = 4096;
// Admission watermarks stop new work, never revocation or original-operation
// recovery. A finite shared reserve cannot bound delayed closes/retries.
const MAX_AUDIT_ADMISSION_ENTRIES: u64 = 1_000_000;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// SQLite's Unix file locks are process-scoped: closing another descriptor for
// the same database can release its locks. Serialize all local connections and
// the extra descriptor used for durable-publication recovery. The caller's
// root gate still excludes writers across processes.
static PROCESS_DATABASE_LOCK: Mutex<()> = Mutex::new(());

fn process_lock() -> Result<MutexGuard<'static, ()>, String> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match PROCESS_DATABASE_LOCK.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(format!("authorization database busy: {error}")),
        }
    }
}

const SCHEMA: &str = r#"
    PRAGMA application_id = 1464291404;
    PRAGMA user_version = 2;
    CREATE TABLE policy_receipts (
        operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) = 64),
        applied_generation INTEGER UNIQUE CHECK(applied_generation IS NULL OR applied_generation > 0),
        receipt BLOB NOT NULL CHECK(length(receipt) BETWEEN 1 AND 4096)
    ) WITHOUT ROWID;
    CREATE TRIGGER policy_receipts_no_update BEFORE UPDATE ON policy_receipts
        BEGIN SELECT RAISE(ABORT, 'policy receipts are immutable'); END;
    CREATE TRIGGER policy_receipts_no_delete BEFORE DELETE ON policy_receipts
        BEGIN SELECT RAISE(ABORT, 'policy receipts are immutable'); END;
    CREATE TABLE policy_migration (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        receipt BLOB NOT NULL CHECK(length(receipt) BETWEEN 1 AND 4096)
    );
    CREATE TRIGGER policy_migration_no_update BEFORE UPDATE ON policy_migration
        BEGIN SELECT RAISE(ABORT, 'policy migration receipt is immutable'); END;
    CREATE TRIGGER policy_migration_no_delete BEFORE DELETE ON policy_migration
        BEGIN SELECT RAISE(ABORT, 'policy migration receipt is immutable'); END;
    CREATE TABLE policy_current (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        generation INTEGER NOT NULL CHECK(generation >= 0 AND generation <= 9007199254740991),
        policy BLOB,
        policy_sha256 TEXT,
        current_operation TEXT REFERENCES policy_receipts(operation_id),
        CHECK((generation = 0 AND policy IS NULL AND policy_sha256 IS NULL AND current_operation IS NULL)
           OR (generation > 0 AND policy IS NOT NULL AND length(policy) <= 16777216
               AND length(policy_sha256) = 64 AND current_operation IS NOT NULL))
    );
    INSERT INTO policy_current(singleton, generation) VALUES (1, 0);
"#;

// Old v2 databases have none of these objects; v3 and v4 use the same objects.
// Creating the first new credential upgrades v2/v3 to v4 in the same
// transaction, so earlier binaries fail closed. Exact SQL verifies both unique
// indexes and immutable triggers rather than trusting their names alone.
const CREDENTIAL_TABLE_SQL: &str = "CREATE TABLE operation_credentials (\n\
    operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) = 64),\n\
    token_sha256 TEXT NOT NULL CHECK(length(token_sha256) = 64),\n\
    effect_key TEXT CHECK(effect_key IS NULL OR length(effect_key) = 64),\n\
    receipt BLOB NOT NULL CHECK(length(receipt) BETWEEN 1 AND 4096)\n\
) WITHOUT ROWID";
const CREDENTIAL_INDEX_SQL: &str =
    "CREATE UNIQUE INDEX operation_credentials_token ON operation_credentials(token_sha256)";
const CREDENTIAL_EFFECT_SQL: &str =
    "CREATE UNIQUE INDEX operation_credentials_effect ON operation_credentials(effect_key)";
const CREDENTIAL_UPDATE_SQL: &str = "CREATE TRIGGER operation_credentials_no_update BEFORE UPDATE ON operation_credentials BEGIN SELECT RAISE(ABORT, 'operation credentials are immutable'); END";
const CREDENTIAL_DELETE_SQL: &str = "CREATE TRIGGER operation_credentials_no_delete BEFORE DELETE ON operation_credentials BEGIN SELECT RAISE(ABORT, 'operation credentials are immutable'); END";
const CREDENTIAL_OBJECTS: [(&str, &str, &str); 5] = [
    ("operation_credentials", "table", CREDENTIAL_TABLE_SQL),
    ("operation_credentials_token", "index", CREDENTIAL_INDEX_SQL),
    (
        "operation_credentials_effect",
        "index",
        CREDENTIAL_EFFECT_SQL,
    ),
    (
        "operation_credentials_no_update",
        "trigger",
        CREDENTIAL_UPDATE_SQL,
    ),
    (
        "operation_credentials_no_delete",
        "trigger",
        CREDENTIAL_DELETE_SQL,
    ),
];

const AUDIT_TABLE_SQL: &str = "CREATE TABLE audit_index (\n\
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK(sequence BETWEEN 1 AND 9007199254740991),\n\
    kind INTEGER NOT NULL CHECK(kind BETWEEN 1 AND 5),\n\
    identity TEXT NOT NULL CHECK(length(identity) = 64),\n\
    record BLOB NOT NULL CHECK(length(record) BETWEEN 1 AND 131072),\n\
    observation BLOB CHECK(observation IS NULL OR length(observation) BETWEEN 1 AND 4096),\n\
    CHECK(kind IN (4, 5) OR observation IS NULL)\n\
)";
const AUDIT_UNIQUE_SQL: &str = "CREATE UNIQUE INDEX audit_original_identity ON audit_index(kind, identity) WHERE kind IN (1, 2, 3)";
const AUDIT_LOOKUP_SQL: &str = "CREATE INDEX audit_lookup ON audit_index(kind, identity)";
const AUDIT_IMMUTABLE_SQL: &str = "CREATE TRIGGER audit_identity_no_update BEFORE UPDATE OF sequence, kind, identity, record ON audit_index BEGIN SELECT RAISE(ABORT, 'audit identity is immutable'); END";
const AUDIT_NO_DELETE_SQL: &str = "CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit_index BEGIN SELECT RAISE(ABORT, 'audit records are immutable'); END";
const AUDIT_OBSERVATION_SQL: &str = "CREATE TRIGGER audit_observation_once BEFORE UPDATE OF observation ON audit_index WHEN OLD.observation IS NOT NULL AND NEW.observation IS NOT OLD.observation BEGIN SELECT RAISE(ABORT, 'audit observation is immutable'); END";
const AUDIT_OBJECTS: [(&str, &str, &str); 6] = [
    ("audit_index", "table", AUDIT_TABLE_SQL),
    ("audit_original_identity", "index", AUDIT_UNIQUE_SQL),
    ("audit_lookup", "index", AUDIT_LOOKUP_SQL),
    ("audit_identity_no_update", "trigger", AUDIT_IMMUTABLE_SQL),
    ("audit_no_delete", "trigger", AUDIT_NO_DELETE_SQL),
    ("audit_observation_once", "trigger", AUDIT_OBSERVATION_SQL),
];

fn audit_schema(connection: &Connection) -> Result<bool, String> {
    let mut statement = database(connection.prepare(
        "SELECT name, type, tbl_name, sql FROM sqlite_schema WHERE name IN (?1, ?2, ?3, ?4, ?5, ?6)",
    ))?;
    let mut rows = database(statement.query(params![
        AUDIT_OBJECTS[0].0,
        AUDIT_OBJECTS[1].0,
        AUDIT_OBJECTS[2].0,
        AUDIT_OBJECTS[3].0,
        AUDIT_OBJECTS[4].0,
        AUDIT_OBJECTS[5].0,
    ]))?;
    let mut found = [false; AUDIT_OBJECTS.len()];
    while let Some(row) = database(rows.next())? {
        let name: String = database(row.get(0))?;
        let kind: String = database(row.get(1))?;
        let table: String = database(row.get(2))?;
        let sql: Option<String> = database(row.get(3))?;
        let index = AUDIT_OBJECTS
            .iter()
            .position(|object| object.0 == name)
            .ok_or("unknown audit schema object")?;
        let (_, expected_kind, expected_sql) = AUDIT_OBJECTS[index];
        if found[index]
            || kind != expected_kind
            || table != "audit_index"
            || sql.as_deref() != Some(expected_sql)
        {
            return Err("audit schema is invalid".into());
        }
        found[index] = true;
    }
    if found.iter().all(|present| *present) {
        Ok(true)
    } else if found.iter().all(|present| !*present) {
        Ok(false)
    } else {
        Err("audit schema is incomplete".into())
    }
}

/// The caller is already inside an immediate transaction. Older binaries reject
/// both new versions; a policy-only root does not acquire credential support.
fn ensure_audit_schema(connection: &Connection) -> Result<(), String> {
    let version: i64 = database(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
    if matches!(
        version,
        AUDIT_SCHEMA_VERSION | AUDIT_CREDENTIAL_SCHEMA_VERSION
    ) {
        if !audit_schema(connection)? {
            return Err("audit schema is incomplete".into());
        }
        return Ok(());
    }
    if audit_schema(connection)? {
        return Err("unexpected audit schema".into());
    }
    let target = match version {
        SCHEMA_VERSION => AUDIT_SCHEMA_VERSION,
        CREDENTIAL_SCHEMA_VERSION | CHUNK_RECEIPT_SCHEMA_VERSION => AUDIT_CREDENTIAL_SCHEMA_VERSION,
        _ => return Err("unsupported audit schema upgrade".into()),
    };
    for (_, _, sql) in AUDIT_OBJECTS {
        database(connection.execute_batch(sql))?;
    }
    match target {
        AUDIT_SCHEMA_VERSION => database(connection.execute_batch("PRAGMA user_version = 5"))?,
        AUDIT_CREDENTIAL_SCHEMA_VERSION => {
            database(connection.execute_batch("PRAGMA user_version = 6"))?
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn credential_schema(connection: &Connection) -> Result<bool, String> {
    let mut statement = database(connection.prepare(
        "SELECT name, type, tbl_name, sql FROM sqlite_schema WHERE name IN (?1, ?2, ?3, ?4, ?5)",
    ))?;
    let mut rows = database(statement.query(params![
        CREDENTIAL_OBJECTS[0].0,
        CREDENTIAL_OBJECTS[1].0,
        CREDENTIAL_OBJECTS[2].0,
        CREDENTIAL_OBJECTS[3].0,
        CREDENTIAL_OBJECTS[4].0,
    ]))?;
    let mut found = [false; CREDENTIAL_OBJECTS.len()];
    while let Some(row) = database(rows.next())? {
        let name: String = database(row.get(0))?;
        let kind: String = database(row.get(1))?;
        let table: String = database(row.get(2))?;
        let sql: Option<String> = database(row.get(3))?;
        let index = CREDENTIAL_OBJECTS
            .iter()
            .position(|object| object.0 == name)
            .ok_or("unknown credential schema object")?;
        let (_, expected_kind, expected_sql) = CREDENTIAL_OBJECTS[index];
        if found[index]
            || kind != expected_kind
            || table != "operation_credentials"
            || sql.as_deref() != Some(expected_sql)
        {
            return Err("operation credential schema is invalid".into());
        }
        found[index] = true;
    }
    if found.iter().all(|present| *present) {
        Ok(true)
    } else if found.iter().all(|present| !*present) {
        Ok(false)
    } else {
        Err("operation credential schema is incomplete".into())
    }
}

fn create_credential_schema(connection: &Connection) -> Result<(), String> {
    for (_, _, sql) in CREDENTIAL_OBJECTS {
        database(connection.execute_batch(sql))?;
    }
    Ok(())
}

fn credential_features(connection: &Connection) -> Result<(bool, bool), String> {
    let version: i64 = database(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
    match (version, credential_schema(connection)?) {
        (SCHEMA_VERSION, false) => Ok((false, false)),
        (CREDENTIAL_SCHEMA_VERSION, true) => Ok((true, false)),
        (CHUNK_RECEIPT_SCHEMA_VERSION, true) => Ok((true, true)),
        (AUDIT_SCHEMA_VERSION, false) => Ok((false, false)),
        (AUDIT_CREDENTIAL_SCHEMA_VERSION, true) => Ok((true, true)),
        _ => Err("operation credential database version or schema is invalid".into()),
    }
}

fn database<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("authorization database: {error}"))
}

fn configure(connection: &Connection) -> Result<(), String> {
    database(connection.busy_timeout(BUSY_TIMEOUT))?;
    database(connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, (MAX_POLICY_BYTES + 8192) as i32))?;
    database(connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = EXTRA;"))?;
    let synchronous: i64 = database(connection.query_row("PRAGMA synchronous", [], |r| r.get(0)))?;
    let foreign_keys: i64 =
        database(connection.query_row("PRAGMA foreign_keys", [], |r| r.get(0)))?;
    let journal_mode: String =
        database(connection.query_row("PRAGMA journal_mode", [], |r| r.get(0)))?;
    if synchronous != 3 || foreign_keys != 1 || journal_mode != "delete" {
        return Err("authorization database durability configuration unavailable".into());
    }
    Ok(())
}

fn connect(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = database(Connection::open_with_flags(path, flags))?;
    if database(connection.is_readonly("main"))? {
        return Err("authorization database requires writable crash recovery".into());
    }
    configure(&connection)?;
    Ok(connection)
}

fn check_schema(connection: &Connection) -> Result<(), String> {
    let application: i64 =
        database(connection.query_row("PRAGMA application_id", [], |r| r.get(0)))?;
    let version: i64 = database(connection.query_row("PRAGMA user_version", [], |r| r.get(0)))?;
    if application != APPLICATION_ID
        || !matches!(
            version,
            SCHEMA_VERSION
                | CREDENTIAL_SCHEMA_VERSION
                | CHUNK_RECEIPT_SCHEMA_VERSION
                | AUDIT_SCHEMA_VERSION
                | AUDIT_CREDENTIAL_SCHEMA_VERSION
        )
    {
        return Err("unsupported authorization database identity or version".into());
    }
    let objects: i64 = database(connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE (type = 'table' AND name IN ('policy_receipts','policy_current','policy_migration')) OR (type = 'trigger' AND name IN ('policy_receipts_no_update','policy_receipts_no_delete','policy_migration_no_update','policy_migration_no_delete'))",
        [], |r| r.get(0),
    ))?;
    if objects != 7 {
        return Err("authorization database schema is incomplete".into());
    }
    credential_features(connection)?;
    let audit_present = audit_schema(connection)?;
    if audit_present
        != matches!(
            version,
            AUDIT_SCHEMA_VERSION | AUDIT_CREDENTIAL_SCHEMA_VERSION
        )
    {
        return Err("authorization database audit version or schema is invalid".into());
    }
    Ok(())
}

fn existing(root: &Path) -> Result<Option<Connection>, String> {
    existing_with_sync(root, sync_existing)
}

fn sync_existing(path: &Path, root: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync authorization database: {error}"))?;
    crate::output::sync_directory(root)
}

fn existing_with_sync(
    root: &Path,
    sync: impl FnOnce(&Path, &Path) -> Result<(), String>,
) -> Result<Option<Connection>, String> {
    let path = root.join(FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect authorization database: {error}")),
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err("authorization database must be a regular file".into())
        }
        Ok(_) => (),
    }
    let mut file =
        File::open(&path).map_err(|error| format!("open authorization database: {error}"))?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header[..8])
        .map_err(|error| format!("invalid authorization database header: {error}"))?;
    if header.starts_with(LEGACY_HEADER) {
        return Err(
            "legacy policy ledger requires an explicit maintenance-window migration".into(),
        );
    }
    file.read_exact(&mut header[8..])
        .map_err(|error| format!("invalid authorization database header: {error}"))?;
    if &header != SQLITE_HEADER {
        return Err("invalid authorization database header".into());
    }
    drop(file);
    let connection = connect(&path)?;
    check_schema(&connection)?;
    // A previous create/link or migration/rename may have succeeded while its
    // directory sync failed. Do not return a receipt or use its policy until
    // recovery and both syncs have succeeded, including on a later retry.
    sync(&path, root).map_err(|error| {
        format!("authorization durability unknown; retry the original operation: {error}")
    })?;
    Ok(Some(connection))
}

pub(super) struct Current {
    pub generation: u64,
    pub policy: Option<ServerPolicy>,
    pub operation_credentials: bool,
    pub chunk_receipts: bool,
    digest: Option<String>,
}

fn receipt(connection: &Connection, id: &str) -> Result<Option<PolicyUpdateReceipt>, String> {
    let value: Option<(Option<i64>, Vec<u8>)> = database(
        connection
            .query_row(
                "SELECT applied_generation, receipt FROM policy_receipts WHERE operation_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional(),
    )?;
    let Some((applied_generation, bytes)) = value else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err("authorization receipt exceeds bound".into());
    }
    let receipt: PolicyUpdateReceipt = crate::binary::decode(&bytes)?;
    receipt.validate()?;
    let expected_generation = match receipt.outcome {
        PolicyUpdateOutcome::Applied => Some(receipt.generation as i64),
        PolicyUpdateOutcome::GenerationConflict => None,
    };
    if receipt.operation_id != id || applied_generation != expected_generation {
        return Err("authorization receipt identity or index mismatch".into());
    }
    Ok(Some(receipt))
}

fn current_in(connection: &Connection) -> Result<Current, String> {
    let (operation_credentials, chunk_receipts) = credential_features(connection)?;
    let (generation, policy, digest, operation): (i64, Option<Vec<u8>>, Option<String>, Option<String>) =
        database(connection.query_row(
            "SELECT generation, policy, policy_sha256, current_operation FROM policy_current WHERE singleton = 1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ))?;
    let generation = u64::try_from(generation).map_err(|_| "invalid policy generation")?;
    if generation > NODE_SAFE_INTEGER_MAX {
        return Err("invalid policy generation".into());
    }
    if generation == 0 {
        if policy.is_some() || digest.is_some() || operation.is_some() {
            return Err("invalid uninitialized policy state".into());
        }
        return Ok(Current {
            generation,
            policy: None,
            operation_credentials,
            chunk_receipts,
            digest: None,
        });
    }
    let bytes = policy.ok_or("missing current policy")?;
    if bytes.len() > MAX_POLICY_BYTES {
        return Err("policy exceeds 16 MiB".into());
    }
    let policy: ServerPolicy = crate::binary::decode(&bytes)?;
    policy.validate()?;
    let digest = digest.ok_or("missing policy digest")?;
    if policy.digest()? != digest {
        return Err("current policy digest mismatch".into());
    }
    let operation = operation.ok_or("missing current policy operation")?;
    let applied = receipt(connection, &operation)?.ok_or("current policy receipt is missing")?;
    if applied.outcome != PolicyUpdateOutcome::Applied
        || applied.generation != generation
        || applied.policy_sha256 != digest
    {
        return Err("current policy receipt mismatch".into());
    }
    Ok(Current {
        generation,
        policy: Some(policy),
        operation_credentials,
        chunk_receipts,
        digest: Some(digest),
    })
}

pub(super) fn current(root: &Path) -> Result<Option<Current>, String> {
    let _process_lock = process_lock()?;
    existing(root)?
        .map(|connection| current_in(&connection))
        .transpose()
}

pub(super) fn status(root: &Path) -> Result<PolicyStatus, String> {
    let current = current(root)?.ok_or("authorization store is not initialized")?;
    Ok(PolicyStatus {
        version: 1,
        generation: current.generation,
        policy_sha256: current
            .digest
            .ok_or("authorization store is not initialized")?,
    })
}

pub(super) fn query(root: &Path, request: &PolicyQueryRequest) -> Result<PolicyQueryReply, String> {
    request.validate()?;
    let _process_lock = process_lock()?;
    let receipt = match existing(root)? {
        Some(connection) => {
            current_in(&connection)?;
            receipt(&connection, &request.operation_id)?
        }
        None => None,
    };
    Ok(PolicyQueryReply {
        version: 1,
        operation_id: request.operation_id.clone(),
        receipt,
    })
}

#[derive(Clone, Copy)]
enum CredentialIndex {
    Operation,
    Token,
    Effect,
}

fn credential_lookup(
    connection: &Connection,
    key: &str,
    index: CredentialIndex,
) -> Result<Option<OperationCredentialReceipt>, String> {
    let sql = match index {
        CredentialIndex::Operation => "SELECT operation_id, token_sha256, effect_key, receipt FROM operation_credentials WHERE operation_id = ?1",
        CredentialIndex::Token => "SELECT operation_id, token_sha256, effect_key, receipt FROM operation_credentials WHERE token_sha256 = ?1",
        CredentialIndex::Effect => "SELECT operation_id, token_sha256, effect_key, receipt FROM operation_credentials WHERE effect_key = ?1",
    };
    let value: Option<(String, String, Option<String>, Vec<u8>)> = database(
        connection
            .query_row(sql, [key], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional(),
    )?;
    let Some((operation_id, token_sha256, effect_key, bytes)) = value else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err("operation credential receipt exceeds bound".into());
    }
    let receipt: OperationCredentialReceipt = crate::binary::decode(&bytes)?;
    receipt.validate()?;
    if receipt.operation_id != operation_id
        || receipt.token_sha256 != token_sha256
        || receipt.effect_key != effect_key
        || match index {
            CredentialIndex::Operation => operation_id != key,
            CredentialIndex::Token => token_sha256 != key,
            CredentialIndex::Effect => effect_key.as_deref() != Some(key),
        }
    {
        return Err("operation credential identity or index mismatch".into());
    }
    Ok(Some(receipt))
}

fn check_policy_credentials(connection: &Connection, policy: &ServerPolicy) -> Result<(), String> {
    if credential_features(connection)?.0 {
        for principal in &policy.principals {
            if let Some(token) = &principal.token_sha256 {
                if credential_lookup(connection, token, CredentialIndex::Token)?.is_some() {
                    return Err("policy credential collides with an operation credential".into());
                }
            }
        }
    }
    Ok(())
}

fn credential_decide(
    connection: &Connection,
    proposed: &OperationCredentialReceipt,
) -> Result<OperationCredentialReceipt, String> {
    credential_decide_with_check(connection, proposed, || {
        super::AuthorizationLimit::until(proposed.expires_at).require_current()
    })
}

fn credential_decide_with_check(
    connection: &Connection,
    proposed: &OperationCredentialReceipt,
    check: impl FnOnce() -> Result<(), String>,
) -> Result<OperationCredentialReceipt, String> {
    proposed.validate()?;
    let current = current_in(connection)?;
    let exists = current.operation_credentials;
    let chunk_receipts = current.chunk_receipts;
    let policy = current
        .policy
        .ok_or("authorization store is not initialized")?;
    if exists {
        if let Some(prior) = credential_lookup(
            connection,
            &proposed.operation_id,
            CredentialIndex::Operation,
        )? {
            if &prior != proposed {
                return Err("operation credential identity conflict".into());
            }
            return Ok(prior);
        }
    }
    if current.generation != proposed.policy_generation {
        return Err("operation credential policy generation is no longer current".into());
    }
    if !policy
        .principals
        .iter()
        .any(|principal| principal.name == proposed.actor)
    {
        return Err("operation credential principal does not exist".into());
    }
    if policy
        .principals
        .iter()
        .any(|principal| principal.token_sha256.as_deref() == Some(proposed.token_sha256.as_str()))
    {
        return Err("operation credential collides with a policy credential".into());
    }
    if exists
        && credential_lookup(connection, &proposed.token_sha256, CredentialIndex::Token)?.is_some()
    {
        return Err("operation credential token is already assigned".into());
    }
    if let Some(effect) = &proposed.effect_key {
        if exists && credential_lookup(connection, effect, CredentialIndex::Effect)?.is_some() {
            return Err("operation credential effect is already assigned".into());
        }
    }
    if !exists {
        create_credential_schema(connection)?;
    }
    if !chunk_receipts {
        let version: i64 =
            database(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
        if version == AUDIT_SCHEMA_VERSION {
            database(connection.execute_batch("PRAGMA user_version = 6"))?;
        } else {
            database(connection.execute_batch("PRAGMA user_version = 4"))?;
        }
    }
    let bytes = crate::binary::encode(proposed)?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err("operation credential receipt exceeds bound".into());
    }
    // Replays return above, but a new immutable decision must not reserve the
    // effect identity if authority expired while waiting for the transaction.
    check()?;
    database(connection.execute(
        "INSERT INTO operation_credentials(operation_id, token_sha256, effect_key, receipt) VALUES (?1, ?2, ?3, ?4)",
        params![proposed.operation_id, proposed.token_sha256, proposed.effect_key, bytes],
    ))?;
    Ok(proposed.clone())
}

#[cfg(test)]
pub(super) fn credential_issue(
    root: &Path,
    proposed: &OperationCredentialReceipt,
) -> Result<OperationCredentialReceipt, String> {
    proposed.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let result = credential_decide(&tx, proposed)?;
    tx.commit().map_err(|error| {
        format!("operation credential outcome unknown; query the original operation: {error}")
    })?;
    Ok(result)
}

fn credential_read(
    root: &Path,
    key: &str,
    by_token: bool,
) -> Result<Option<OperationCredentialReceipt>, String> {
    crate::binary::oid(key)?;
    let _process_lock = process_lock()?;
    let connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let current = current_in(&connection)?;
    if current.policy.is_none() {
        return Err("authorization store is not initialized".into());
    }
    if current.operation_credentials {
        credential_lookup(
            &connection,
            key,
            if by_token {
                CredentialIndex::Token
            } else {
                CredentialIndex::Operation
            },
        )
    } else {
        Ok(None)
    }
}

pub(super) fn credential_by_token(
    root: &Path,
    token_sha256: &str,
) -> Result<Option<OperationCredentialReceipt>, String> {
    credential_read(root, token_sha256, true)
}

pub(super) fn credential_query(
    root: &Path,
    operation_id: &str,
) -> Result<Option<OperationCredentialReceipt>, String> {
    credential_read(root, operation_id, false)
}

fn decide(
    connection: &Connection,
    request: &PolicyUpdateRequest,
) -> Result<PolicyUpdateReceipt, String> {
    let current = current_in(connection)?;
    if let Some(receipt) = receipt(connection, &request.operation_id)? {
        receipt
            .validate_update(request)
            .map_err(|_| "policy operation identity conflict")?;
        return Ok(receipt);
    }
    let outcome = if current.generation == request.expected_generation {
        PolicyUpdateOutcome::Applied
    } else {
        PolicyUpdateOutcome::GenerationConflict
    };
    if outcome == PolicyUpdateOutcome::Applied {
        check_policy_credentials(connection, &request.policy)?;
    }
    let receipt = PolicyUpdateReceipt {
        version: 1,
        operation_id: request.operation_id.clone(),
        expected_generation: request.expected_generation,
        outcome,
        generation: if outcome == PolicyUpdateOutcome::Applied {
            current.generation + 1
        } else {
            current.generation
        },
        policy_sha256: request.policy.digest()?,
    };
    receipt.validate()?;
    let bytes = crate::binary::encode(&receipt)?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err("authorization receipt exceeds bound".into());
    }
    let applied_generation =
        (outcome == PolicyUpdateOutcome::Applied).then_some(receipt.generation as i64);
    database(connection.execute(
        "INSERT INTO policy_receipts(operation_id, applied_generation, receipt) VALUES (?1, ?2, ?3)",
        params![receipt.operation_id, applied_generation, bytes],
    ))?;
    if outcome == PolicyUpdateOutcome::Applied {
        let policy = crate::binary::encode(&request.policy)?;
        if policy.len() > MAX_POLICY_BYTES {
            return Err("policy exceeds 16 MiB".into());
        }
        let changed = database(connection.execute(
            "UPDATE policy_current SET generation = ?1, policy = ?2, policy_sha256 = ?3, current_operation = ?4 WHERE singleton = 1 AND generation = ?5",
            params![receipt.generation as i64, policy, receipt.policy_sha256, receipt.operation_id, current.generation as i64],
        ))?;
        if changed != 1 {
            return Err("policy generation changed during transaction".into());
        }
    }
    Ok(receipt)
}

#[cfg(test)]
fn transaction(
    connection: &mut Connection,
    request: &PolicyUpdateRequest,
) -> Result<PolicyUpdateReceipt, String> {
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    let result = decide(&tx, request)?;
    tx.commit().map_err(|error| {
        format!("policy decision outcome unknown; query the original operation: {error}")
    })?;
    Ok(result)
}

struct Staging {
    path: PathBuf,
    cleanup: bool,
}
impl Drop for Staging {
    fn drop(&mut self) {
        if self.cleanup {
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let mut name = self.path.as_os_str().to_os_string();
                name.push(suffix);
                if let Err(error) = fs::remove_file(PathBuf::from(name)) {
                    if error.kind() != io::ErrorKind::NotFound {
                        crate::cli_error(&format!("policy staging cleanup failed: {error}"));
                    }
                }
            }
        }
    }
}

fn stage(root: &Path) -> Result<Staging, String> {
    let random = getrandom::u64().map_err(|error| error.to_string())?;
    let path = root.join(format!(
        ".wasm-git-policy-{}-{random:x}.tmp",
        crate::platform_fs::process_id()
    ));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|error| format!("create policy staging: {error}"))?;
    drop(file);
    Ok(Staging {
        path,
        cleanup: true,
    })
}

fn seal(connection: Connection, path: &Path) -> Result<(), String> {
    connection
        .close()
        .map_err(|(_, error)| format!("close policy staging: {error}"))?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(PathBuf::from(name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(format!("inspect policy staging journal: {error}")),
            Ok(_) => return Err("policy staging retained an unresolved journal".into()),
        }
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync policy staging: {error}"))
}

#[cfg(test)]
fn create(root: &Path, request: &PolicyUpdateRequest) -> Result<PolicyUpdateReceipt, String> {
    create_with_hooks(
        root,
        request,
        |path| fs::remove_file(path),
        crate::output::sync_directory,
    )
}

#[cfg(test)]
fn create_with_hooks(
    root: &Path,
    request: &PolicyUpdateRequest,
    remove_staging: impl FnOnce(&Path) -> io::Result<()>,
    sync_directory: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PolicyUpdateReceipt, String> {
    create_with_decision(
        root,
        |connection| decide(connection, request),
        remove_staging,
        sync_directory,
    )
}

fn create_with_decision(
    root: &Path,
    decision: impl FnOnce(&Connection) -> Result<PolicyUpdateReceipt, String>,
    remove_staging: impl FnOnce(&Path) -> io::Result<()>,
    mut sync_directory: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PolicyUpdateReceipt, String> {
    let mut staging = stage(root)?;
    let mut connection = connect(&staging.path)?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    database(tx.execute_batch(SCHEMA))?;
    let result = decision(&tx)?;
    database(tx.commit())?;
    seal(connection, &staging.path)?;
    fs::hard_link(&staging.path, root.join(FILE))
        .map_err(|error| format!("publish policy database: {error}"))?;
    sync_directory(root).map_err(|error| {
        format!("policy outcome unknown; query the original operation: {error}")
    })?;
    remove_staging(&staging.path).map_err(|error| {
        format!("authorization ledger published; query the original operation; staging cleanup failed: {error}")
    })?;
    staging.cleanup = false;
    sync_directory(root).map_err(|error| {
        format!("policy outcome unknown; query the original operation: {error}")
    })?;
    Ok(result)
}

#[cfg(test)]
pub(super) fn set(
    root: &Path,
    request: &PolicyUpdateRequest,
) -> Result<PolicyUpdateReceipt, String> {
    request.validate()?;
    let _process_lock = process_lock()?;
    match existing(root)? {
        Some(mut connection) => transaction(&mut connection, request),
        None => create(root, request),
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAuditRecord {
    version: u32,
    detail: StoredAuditDetail,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum StoredAuditDetail {
    Policy {
        attribution: AuditAttribution,
    },
    Credential {
        original: Box<OperationCredentialRequest>,
        attribution: AuditAttribution,
    },
    Recovery {
        original_operation_id: String,
        action: RecoveryAuditAction,
        operator: AuditOperator,
        recorded_at_unix_millis: u64,
    },
    OperationAttempt {
        started_at_unix_millis: u64,
    },
    RecoveryAttempt {
        started_at_unix_millis: u64,
    },
}

impl StoredAuditRecord {
    fn kind(&self) -> i64 {
        match &self.detail {
            StoredAuditDetail::Policy { .. } => 1,
            StoredAuditDetail::Credential { .. } => 2,
            StoredAuditDetail::Recovery { .. } => 3,
            StoredAuditDetail::OperationAttempt { .. } => 4,
            StoredAuditDetail::RecoveryAttempt { .. } => 5,
        }
    }

    fn validate(&self, identity: &str) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported audit record version".into());
        }
        crate::binary::oid(identity)?;
        match &self.detail {
            StoredAuditDetail::Policy { attribution } => attribution.validate(),
            StoredAuditDetail::Credential {
                original,
                attribution,
            } => {
                original.validate()?;
                if original.operation_id != identity {
                    return Err("audit credential identity mismatch".into());
                }
                attribution.validate()
            }
            StoredAuditDetail::Recovery {
                original_operation_id,
                operator,
                recorded_at_unix_millis,
                ..
            } => {
                crate::binary::oid(original_operation_id)?;
                operator.validate()?;
                audit_time_valid(*recorded_at_unix_millis)
            }
            StoredAuditDetail::OperationAttempt {
                started_at_unix_millis,
            }
            | StoredAuditDetail::RecoveryAttempt {
                started_at_unix_millis,
            } => audit_time_valid(*started_at_unix_millis),
        }
    }
}

struct AuditRow {
    sequence: u64,
    kind: i64,
    identity: String,
    record: StoredAuditRecord,
    observation: Option<AuditRecordedObservation>,
}

type RawAuditRow = (i64, i64, String, Vec<u8>, Option<Vec<u8>>);

fn raw_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAuditRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn decode_audit_row(raw: RawAuditRow) -> Result<AuditRow, String> {
    let (sequence, kind, identity, bytes, observed) = raw;
    let sequence = u64::try_from(sequence).map_err(|_| "invalid audit sequence")?;
    if sequence == 0 || sequence > NODE_SAFE_INTEGER_MAX || bytes.len() > MAX_AUDIT_RECORD_BYTES {
        return Err("invalid audit record bounds".into());
    }
    let record: StoredAuditRecord = crate::binary::decode(&bytes)?;
    record.validate(&identity)?;
    if kind != record.kind() {
        return Err("audit record kind mismatch".into());
    }
    let observation = observed
        .map(|bytes| -> Result<AuditRecordedObservation, String> {
            if bytes.len() > MAX_AUDIT_OBSERVATION_BYTES {
                return Err("audit observation exceeds bounds".into());
            }
            let value: AuditRecordedObservation = crate::binary::decode(&bytes)?;
            value.validate()?;
            Ok(value)
        })
        .transpose()?;
    if observation.is_some() && !matches!(kind, 4 | 5) {
        return Err("audit observation has no attempt".into());
    }
    Ok(AuditRow {
        sequence,
        kind,
        identity,
        record,
        observation,
    })
}

fn audit_row_by_key(
    connection: &Connection,
    kind: i64,
    identity: &str,
) -> Result<Option<AuditRow>, String> {
    let row: Option<RawAuditRow> = database(connection.query_row(
        "SELECT sequence, kind, identity, record, observation FROM audit_index WHERE kind = ?1 AND identity = ?2",
        params![kind, identity], raw_audit_row,
    ).optional())?;
    row.map(decode_audit_row).transpose()
}

fn audit_row_by_sequence(
    connection: &Connection,
    sequence: u64,
) -> Result<Option<AuditRow>, String> {
    if sequence == 0 || sequence > NODE_SAFE_INTEGER_MAX {
        return Err("invalid audit attempt".into());
    }
    let row: Option<RawAuditRow> = database(connection.query_row(
        "SELECT sequence, kind, identity, record, observation FROM audit_index WHERE sequence = ?1",
        [sequence as i64], raw_audit_row,
    ).optional())?;
    row.map(decode_audit_row).transpose()
}

fn audit_time_valid(value: u64) -> Result<(), String> {
    if value > NODE_SAFE_INTEGER_MAX {
        return Err("audit time exceeds bound".into());
    }
    Ok(())
}

fn audit_now() -> Result<u64, String> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("audit clock: {error}"))?
        .as_millis();
    let time = u64::try_from(millis).map_err(|_| "audit clock exceeds bound")?;
    audit_time_valid(time)?;
    Ok(time)
}

fn audit_high_water(connection: &Connection) -> Result<u64, String> {
    let maximum: i64 = database(connection.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM audit_index",
        [],
        |row| row.get(0),
    ))?;
    let allocated: Option<i64> = database(
        connection
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'audit_index'",
                [],
                |row| row.get(0),
            )
            .optional(),
    )?;
    if maximum < 0 || allocated.unwrap_or(0) != maximum || maximum as u64 > NODE_SAFE_INTEGER_MAX {
        return Err("audit sequence history is incomplete".into());
    }
    Ok(maximum as u64)
}

// Imports must not turn a missing new audit record into apparent legacy
// history. This bounded-capacity check detects internal sequence holes before
// the first import, without scanning every record for ordinary operations.
fn audit_verify_contiguous(connection: &Connection) -> Result<(), String> {
    let maximum = audit_high_water(connection)?;
    let count: i64 =
        database(connection.query_row("SELECT COUNT(*) FROM audit_index", [], |row| row.get(0)))?;
    if count < 0 || count as u64 != maximum {
        return Err("audit sequence history is incomplete".into());
    }
    Ok(())
}

fn audit_insert(
    connection: &Connection,
    identity: &str,
    detail: StoredAuditDetail,
    reserved: bool,
) -> Result<u64, String> {
    audit_insert_with_limit(
        connection,
        identity,
        detail,
        reserved,
        MAX_AUDIT_ADMISSION_ENTRIES,
    )
}

fn audit_insert_with_limit(
    connection: &Connection,
    identity: &str,
    detail: StoredAuditDetail,
    reserved: bool,
    admission_limit: u64,
) -> Result<u64, String> {
    let record = StoredAuditRecord { version: 1, detail };
    record.validate(identity)?;
    let maximum = audit_high_water(connection)?;
    let limit = if reserved {
        NODE_SAFE_INTEGER_MAX
    } else {
        admission_limit
    };
    if maximum >= limit {
        return Err("audit capacity exceeded".into());
    }
    let bytes = crate::binary::encode(&record)?;
    if bytes.is_empty() || bytes.len() > MAX_AUDIT_RECORD_BYTES {
        return Err("audit record exceeds bound".into());
    }
    database(connection.execute(
        "INSERT INTO audit_index(kind, identity, record) VALUES (?1, ?2, ?3)",
        params![record.kind(), identity, bytes],
    ))?;
    let sequence =
        u64::try_from(connection.last_insert_rowid()).map_err(|_| "invalid audit sequence")?;
    if sequence != maximum + 1 {
        return Err("audit sequence is not contiguous".into());
    }
    Ok(sequence)
}

fn require_audit(connection: &Connection) -> Result<(), String> {
    let version: i64 = database(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
    if !matches!(
        version,
        AUDIT_SCHEMA_VERSION | AUDIT_CREDENTIAL_SCHEMA_VERSION
    ) || !audit_schema(connection)?
    {
        return Err("audit index is unavailable".into());
    }
    Ok(())
}

fn audited_credential(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<(OperationCredentialRequest, AuditAttribution)>, String> {
    let Some(row) = audit_row_by_key(connection, 2, operation_id)? else {
        return Ok(None);
    };
    let StoredAuditDetail::Credential {
        original,
        attribution,
    } = row.record.detail
    else {
        return Err("invalid audit credential record".into());
    };
    let receipt = credential_lookup(connection, operation_id, CredentialIndex::Operation)?
        .ok_or("audit credential receipt is missing")?;
    receipt.validate_request(&original)?;
    Ok(Some((*original, attribution)))
}

fn audited_recovery(
    connection: &Connection,
    recovery_id: &str,
) -> Result<Option<(String, RecoveryAuditAction, AuditOperator, u64)>, String> {
    let Some(row) = audit_row_by_key(connection, 3, recovery_id)? else {
        return Ok(None);
    };
    let StoredAuditDetail::Recovery {
        original_operation_id,
        action,
        operator,
        recorded_at_unix_millis,
    } = row.record.detail
    else {
        return Err("invalid recovery audit record".into());
    };
    if audited_credential(connection, &original_operation_id)?.is_none() {
        return Err("recovery audit original evidence is missing".into());
    }
    Ok(Some((
        original_operation_id,
        action,
        operator,
        recorded_at_unix_millis,
    )))
}

fn audited_policy_decide(
    connection: &Connection,
    request: &PolicyUpdateRequest,
    operator: &AuditOperator,
) -> Result<PolicyUpdateReceipt, String> {
    ensure_audit_schema(connection)?;
    let prior = receipt(connection, &request.operation_id)?;
    let result = decide(connection, request)?;
    if let Some(row) = audit_row_by_key(connection, 1, &request.operation_id)? {
        let StoredAuditDetail::Policy { attribution } = row.record.detail else {
            return Err("invalid audit policy record".into());
        };
        attribution.validate()?;
        if prior.is_none() {
            return Err("audit policy receipt is missing".into());
        }
        return Ok(result);
    }
    let time = audit_now()?;
    let attribution = if prior.is_some() {
        audit_verify_contiguous(connection)?;
        AuditAttribution::LegacyImported {
            observer: operator.clone(),
            observed_at_unix_millis: time,
        }
    } else {
        AuditAttribution::Recorded {
            operator: operator.clone(),
            unix_millis: time,
        }
    };
    audit_insert(
        connection,
        &request.operation_id,
        StoredAuditDetail::Policy { attribution },
        true,
    )?;
    Ok(result)
}

pub(super) fn set_audited(
    root: &Path,
    request: &PolicyUpdateRequest,
    operator: &AuditOperator,
) -> Result<PolicyUpdateReceipt, String> {
    request.validate()?;
    operator.validate()?;
    let _process_lock = process_lock()?;
    match existing(root)? {
        Some(mut connection) => {
            let tx =
                database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
            let result = audited_policy_decide(&tx, request, operator)?;
            tx.commit().map_err(|error| {
                format!("policy and audit outcome unknown; query the original operation: {error}")
            })?;
            Ok(result)
        }
        None => create_with_decision(
            root,
            |connection| audited_policy_decide(connection, request, operator),
            |path| fs::remove_file(path),
            crate::output::sync_directory,
        ),
    }
}

pub(super) fn bootstrap_audited(
    root: &Path,
    policy: ServerPolicy,
    operator: &AuditOperator,
) -> Result<(), String> {
    let digest = policy.digest()?;
    let request = PolicyUpdateRequest {
        version: 1,
        operation_id: digest,
        expected_generation: 0,
        policy,
    };
    let result = set_audited(root, &request, operator)?;
    if result.outcome != PolicyUpdateOutcome::Applied || result.generation != 1 {
        return Err("policy bootstrap operation identity or generation conflict".into());
    }
    Ok(())
}

fn audit_import_original(
    connection: &Connection,
    original: &OperationCredentialRequest,
    operator: &AuditOperator,
) -> Result<(), String> {
    original.validate()?;
    operator.validate()?;
    let expected = original.receipt()?;
    let actual = credential_lookup(
        connection,
        &original.operation_id,
        CredentialIndex::Operation,
    )?
    .ok_or("original credential receipt is missing")?;
    if actual != expected {
        return Err("original credential request does not match its receipt".into());
    }
    if let Some((stored, _)) = audited_credential(connection, &original.operation_id)? {
        if crate::binary::encode(&stored)? != crate::binary::encode(original)? {
            return Err("original credential evidence conflict".into());
        }
        return Ok(());
    }
    audit_verify_contiguous(connection)?;
    let attribution = AuditAttribution::LegacyImported {
        observer: operator.clone(),
        observed_at_unix_millis: audit_now()?,
    };
    audit_insert(
        connection,
        &original.operation_id,
        StoredAuditDetail::Credential {
            original: Box::new(original.clone()),
            attribution,
        },
        true,
    )?;
    Ok(())
}

#[cfg(test)]
pub(super) fn audit_import_legacy(
    root: &Path,
    original: &OperationCredentialRequest,
    operator: &AuditOperator,
) -> Result<(), String> {
    original.validate()?;
    operator.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    ensure_audit_schema(&tx)?;
    current_in(&tx)?;
    audit_import_original(&tx, original, operator)?;
    tx.commit().map_err(|error| {
        format!("legacy audit import outcome unknown; query the original operation: {error}")
    })
}

pub(super) fn credential_issue_audited(
    root: &Path,
    original: &OperationCredentialRequest,
    operator: &AuditOperator,
) -> Result<OperationCredentialReceipt, String> {
    let proposed = original.receipt()?;
    operator.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    ensure_audit_schema(&tx)?;
    let exists = current_in(&tx)?.operation_credentials;
    let prior = if exists {
        credential_lookup(&tx, &proposed.operation_id, CredentialIndex::Operation)?
    } else {
        None
    };
    let result = credential_decide(&tx, &proposed)?;
    if let Some((stored, _)) = audited_credential(&tx, &proposed.operation_id)? {
        if crate::binary::encode(&stored)? != crate::binary::encode(original)? {
            return Err("original credential evidence conflict".into());
        }
    } else if prior.is_some() {
        audit_import_original(&tx, original, operator)?;
    } else {
        let attribution = AuditAttribution::Recorded {
            operator: operator.clone(),
            unix_millis: audit_now()?,
        };
        audit_insert(
            &tx,
            &proposed.operation_id,
            StoredAuditDetail::Credential {
                original: Box::new(original.clone()),
                attribution,
            },
            false,
        )?;
    }
    tx.commit().map_err(|error| {
        format!("credential and audit outcome unknown; query the original operation: {error}")
    })?;
    Ok(result)
}

pub(super) fn audit_start_operation(
    root: &Path,
    credential: &OperationCredentialReceipt,
    request: &Request,
    payload_sha256: &str,
) -> Result<u64, String> {
    credential.validate()?;
    crate::binary::oid(payload_sha256)?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    require_audit(&tx)?;
    let (original, attribution) = audited_credential(&tx, &credential.operation_id)?
        .ok_or("original operation audit evidence is missing")?;
    if matches!(attribution, AuditAttribution::LegacyImported { .. }) {
        return Err("legacy imported credentials are limited to recovery".into());
    }
    if original.receipt()? != *credential
        || original.payload_sha256 != payload_sha256
        || crate::binary::encode(&original.request)? != crate::binary::encode(request)?
        || request.credential_digest(payload_sha256)? != credential.request_sha256
        || request.credential_effect_key(&credential.actor)? != credential.effect_key
        || credential.effect_key.is_none()
    {
        return Err("operation does not match its audited credential".into());
    }
    let current = current_in(&tx)?;
    let policy = current
        .policy
        .ok_or("authorization store is not initialized")?;
    if current.generation != credential.policy_generation
        || !policy
            .principals
            .iter()
            .any(|principal| principal.name == credential.actor)
        || audit_now()? / 1000 >= credential.expires_at
    {
        return Err("operation credential is no longer current".into());
    }
    let attempt = audit_insert(
        &tx,
        &credential.operation_id,
        StoredAuditDetail::OperationAttempt {
            started_at_unix_millis: audit_now()?,
        },
        false,
    )?;
    tx.commit().map_err(|error| {
        format!("operation audit start outcome unknown; inspect the audit index: {error}")
    })?;
    Ok(attempt)
}

pub(super) fn audit_operation_context(
    root: &Path,
    operation_id: &str,
) -> Result<Option<OperationCredentialRequest>, String> {
    crate::binary::oid(operation_id)?;
    let _process_lock = process_lock()?;
    let connection = existing(root)?.ok_or("authorization store is not initialized")?;
    current_in(&connection)?;
    let version: i64 = database(connection.query_row("PRAGMA user_version", [], |row| row.get(0)))?;
    if !matches!(
        version,
        AUDIT_SCHEMA_VERSION | AUDIT_CREDENTIAL_SCHEMA_VERSION
    ) {
        return Ok(None);
    }
    require_audit(&connection)?;
    Ok(audited_credential(&connection, operation_id)?.map(|(original, _)| original))
}

pub(super) fn audit_begin_recovery(
    root: &Path,
    recovery_id: &str,
    original: &OperationCredentialRequest,
    action: RecoveryAuditAction,
    operator: &AuditOperator,
) -> Result<u64, String> {
    crate::binary::oid(recovery_id)?;
    original.validate()?;
    operator.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    ensure_audit_schema(&tx)?;
    current_in(&tx)?;
    audit_import_original(&tx, original, operator)?;
    if let Some((prior_original, prior_action, prior_operator, _)) =
        audited_recovery(&tx, recovery_id)?
    {
        if prior_original != original.operation_id
            || prior_action != action
            || prior_operator != *operator
        {
            return Err("recovery audit identity conflict".into());
        }
    } else {
        audit_insert(
            &tx,
            recovery_id,
            StoredAuditDetail::Recovery {
                original_operation_id: original.operation_id.clone(),
                action,
                operator: operator.clone(),
                recorded_at_unix_millis: audit_now()?,
            },
            true,
        )?;
    }
    let attempt = audit_insert(
        &tx,
        recovery_id,
        StoredAuditDetail::RecoveryAttempt {
            started_at_unix_millis: audit_now()?,
        },
        true,
    )?;
    tx.commit().map_err(|error| {
        format!("recovery audit start outcome unknown; inspect the audit index: {error}")
    })?;
    Ok(attempt)
}

fn audit_target_context(
    connection: &Connection,
    target: &AuditTarget,
) -> Result<(AuditRow, OperationCredentialRequest), String> {
    target.validate()?;
    let (kind, identity, sequence) = match target {
        AuditTarget::Operation {
            operation_id,
            attempt_id,
        } => (4, operation_id, *attempt_id),
        AuditTarget::Recovery {
            recovery_id,
            attempt_id,
        } => (5, recovery_id, *attempt_id),
    };
    let row = audit_row_by_sequence(connection, sequence)?.ok_or("audit attempt is missing")?;
    if row.kind != kind || row.identity != *identity {
        return Err("audit attempt identity mismatch".into());
    }
    let operation_id = if kind == 4 {
        identity.clone()
    } else {
        audited_recovery(connection, identity)?
            .ok_or("recovery audit is missing")?
            .0
    };
    let (original, _) = audited_credential(connection, &operation_id)?
        .ok_or("original operation audit evidence is missing")?;
    Ok((row, original))
}

fn audit_validate_scope(
    original: &OperationCredentialRequest,
    observation: &AuditObservation,
) -> Result<(), String> {
    observation.validate()?;
    let allowed = match &original.request.operation {
        Operation::UploadChunk { .. }
        | Operation::QueryChunk { .. }
        | Operation::ScopedUploadSlice { .. } => matches!(
            observation.scope,
            AuditObservationScope::Chunk | AuditObservationScope::UploadSession
        ),
        Operation::ApplyScopedUpload { .. }
        | Operation::CancelScopedUpload { .. }
        | Operation::QueryScopedUpload { .. } => {
            observation.scope == AuditObservationScope::UploadSession
        }
        Operation::ReleaseRetention { .. } => observation.scope == AuditObservationScope::Retention,
        _ => false,
    };
    if !allowed {
        return Err("audit observation scope does not match the original operation".into());
    }
    Ok(())
}

fn audit_write_observation(
    connection: &Connection,
    row: &AuditRow,
    source: AuditObservationSource,
    observation: &AuditObservation,
) -> Result<(), String> {
    source.validate()?;
    observation.validate()?;
    if let Some(prior) = &row.observation {
        if prior.source == source && prior.observation == *observation {
            return Ok(());
        }
        return Err("audit attempt observation conflict".into());
    }
    let record = AuditRecordedObservation {
        observed_at_unix_millis: audit_now()?,
        source,
        observation: observation.clone(),
    };
    record.validate()?;
    let bytes = crate::binary::encode(&record)?;
    if bytes.len() > MAX_AUDIT_OBSERVATION_BYTES {
        return Err("audit observation exceeds bound".into());
    }
    let changed = database(connection.execute(
        "UPDATE audit_index SET observation = ?1 WHERE sequence = ?2 AND observation IS NULL",
        params![bytes, row.sequence as i64],
    ))?;
    if changed != 1 {
        return Err("audit attempt observation changed concurrently".into());
    }
    Ok(())
}

pub(super) fn audit_observe(
    root: &Path,
    target: &AuditTarget,
    observation: &AuditObservation,
) -> Result<(), String> {
    target.validate()?;
    observation.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    require_audit(&tx)?;
    let (row, original) = audit_target_context(&tx, target)?;
    audit_validate_scope(&original, observation)?;
    let source = match target {
        AuditTarget::Operation { .. } => AuditObservationSource::OriginalAttempt,
        AuditTarget::Recovery {
            recovery_id,
            attempt_id,
        } => AuditObservationSource::RecoveryAttempt {
            recovery_id: recovery_id.clone(),
            attempt_id: *attempt_id,
        },
    };
    audit_write_observation(&tx, &row, source, observation)?;
    tx.commit().map_err(|error| {
        format!("audit observation outcome unknown; inspect the original attempt: {error}")
    })
}

/// The recovery attempt must itself already contain exactly this scoped
/// observation. Reconciliation records who made that observation and does not
/// assert that the target attempt caused the observed native state.
#[cfg(test)]
pub(super) fn audit_reconcile(
    root: &Path,
    recovery_id: &str,
    recovery_attempt_id: u64,
    target: &AuditTarget,
    observation: &AuditObservation,
) -> Result<(), String> {
    audit_reconcile_many(
        root,
        recovery_id,
        recovery_attempt_id,
        std::slice::from_ref(target),
        observation,
    )
}

pub(super) fn audit_reconcile_many(
    root: &Path,
    recovery_id: &str,
    recovery_attempt_id: u64,
    targets: &[AuditTarget],
    observation: &AuditObservation,
) -> Result<(), String> {
    let recovery = AuditTarget::Recovery {
        recovery_id: recovery_id.into(),
        attempt_id: recovery_attempt_id,
    };
    recovery.validate()?;
    if targets.len() > crate::protocol::AUDIT_PAGE_MAX_ITEMS as usize {
        return Err("too many audit reconciliation targets".into());
    }
    for target in targets {
        target.validate()?;
        if &recovery == target {
            return Err("recovery cannot reconcile itself".into());
        }
    }
    observation.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    require_audit(&tx)?;
    let (source_row, source_original) = audit_target_context(&tx, &recovery)?;
    audit_validate_scope(&source_original, observation)?;
    let source = AuditObservationSource::RecoveryAttempt {
        recovery_id: recovery_id.into(),
        attempt_id: recovery_attempt_id,
    };
    match &source_row.observation {
        Some(record) if record.source == source && record.observation == *observation => (),
        _ => return Err("recovery attempt has no matching observation".into()),
    }
    for target in targets {
        let (target_row, target_original) = audit_target_context(&tx, target)?;
        if crate::binary::encode(&source_original)? != crate::binary::encode(&target_original)? {
            return Err("recovery and target original operations differ".into());
        }
        if let Some(prior) = &target_row.observation {
            if matches!(&prior.source, AuditObservationSource::RecoveryAttempt { recovery_id: prior_id, .. } if prior_id == recovery_id)
            {
                // A retried recovery has a new observation attempt. Keep the
                // earlier validated historical observation, not a rewritten
                // claim that the original action caused today's state.
                audit_check_observation_source(&tx, &target_row, &target_original)?;
                continue;
            }
        }
        audit_write_observation(&tx, &target_row, source.clone(), observation)?;
    }
    tx.commit().map_err(|error| {
        format!("audit reconciliation outcome unknown; inspect the original attempt: {error}")
    })
}

fn audit_check_observation_source(
    connection: &Connection,
    row: &AuditRow,
    original: &OperationCredentialRequest,
) -> Result<(), String> {
    let Some(observation) = &row.observation else {
        return Ok(());
    };
    audit_validate_scope(original, &observation.observation)?;
    match &observation.source {
        AuditObservationSource::OriginalAttempt if row.kind == 4 => Ok(()),
        AuditObservationSource::OriginalAttempt => Err("invalid audit observation source".into()),
        AuditObservationSource::RecoveryAttempt {
            recovery_id,
            attempt_id,
        } => {
            let source = AuditTarget::Recovery {
                recovery_id: recovery_id.clone(),
                attempt_id: *attempt_id,
            };
            let (source_row, source_original) = audit_target_context(connection, &source)?;
            if crate::binary::encode(&source_original)? != crate::binary::encode(original)? {
                return Err("audit observation source has a different original operation".into());
            }
            if source_row.sequence != row.sequence {
                let expected = AuditObservationSource::RecoveryAttempt {
                    recovery_id: recovery_id.clone(),
                    attempt_id: *attempt_id,
                };
                match source_row.observation {
                    Some(source_observation)
                        if source_observation.source == expected
                            && source_observation.observation == observation.observation => {}
                    _ => return Err("audit recovery observation source is missing".into()),
                }
            }
            Ok(())
        }
    }
}

fn audit_summary(connection: &Connection, row: AuditRow) -> Result<AuditSummary, String> {
    let item = match &row.record.detail {
        StoredAuditDetail::Policy { attribution } => {
            let receipt =
                receipt(connection, &row.identity)?.ok_or("audit policy receipt is missing")?;
            AuditItem::Policy {
                receipt,
                attribution: attribution.clone(),
            }
        }
        StoredAuditDetail::Credential { .. } => {
            let (original, attribution) = audited_credential(connection, &row.identity)?
                .ok_or("audit credential evidence is missing")?;
            let receipt = original.receipt()?;
            AuditItem::Credential {
                operation_id: row.identity.clone(),
                scope: AuditOperationScope::from_original(&original)?,
                request_sha256: receipt.request_sha256,
                attribution,
            }
        }
        StoredAuditDetail::Recovery { .. } => {
            let (operation_id, action, operator, recorded_at_unix_millis) =
                audited_recovery(connection, &row.identity)?.ok_or("recovery audit is missing")?;
            let (original, _) = audited_credential(connection, &operation_id)?
                .ok_or("original audit evidence is missing")?;
            AuditItem::Recovery {
                recovery_id: row.identity.clone(),
                original_operation_id: operation_id,
                scope: AuditOperationScope::from_original(&original)?,
                action,
                operator,
                recorded_at_unix_millis,
            }
        }
        StoredAuditDetail::OperationAttempt {
            started_at_unix_millis,
        } => {
            let (original, _) = audited_credential(connection, &row.identity)?
                .ok_or("original audit evidence is missing")?;
            audit_check_observation_source(connection, &row, &original)?;
            AuditItem::OperationAttempt {
                operation_id: row.identity.clone(),
                scope: AuditOperationScope::from_original(&original)?,
                started_at_unix_millis: *started_at_unix_millis,
                observation: row.observation.clone(),
            }
        }
        StoredAuditDetail::RecoveryAttempt {
            started_at_unix_millis,
        } => {
            let (operation_id, action, operator, _) =
                audited_recovery(connection, &row.identity)?.ok_or("recovery audit is missing")?;
            let (original, _) = audited_credential(connection, &operation_id)?
                .ok_or("original audit evidence is missing")?;
            audit_check_observation_source(connection, &row, &original)?;
            AuditItem::RecoveryAttempt {
                recovery_id: row.identity.clone(),
                original_operation_id: operation_id,
                scope: AuditOperationScope::from_original(&original)?,
                action,
                operator,
                started_at_unix_millis: *started_at_unix_millis,
                observation: row.observation.clone(),
            }
        }
    };
    let summary = AuditSummary {
        version: 1,
        sequence: row.sequence,
        item,
    };
    summary.validate()?;
    Ok(summary)
}

pub(super) fn audit_query(
    root: &Path,
    request: &AuditQueryRequest,
) -> Result<AuditQueryReply, String> {
    request.validate()?;
    let _process_lock = process_lock()?;
    let mut connection = existing(root)?.ok_or("authorization store is not initialized")?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Deferred))?;
    require_audit(&tx)?;
    current_in(&tx)?;
    let current_high_water = audit_high_water(&tx)?;
    let high_water = request.high_water.unwrap_or(current_high_water);
    if high_water > current_high_water || request.after > high_water {
        return Err("invalid audit page high-water mark".into());
    }
    let mut items = Vec::new();
    let mut last = request.after;
    {
        let mut statement = database(tx.prepare(
            "SELECT sequence, kind, identity, record, observation FROM audit_index WHERE sequence > ?1 AND sequence <= ?2 ORDER BY sequence LIMIT ?3",
        ))?;
        let mut rows = database(statement.query(params![
            request.after as i64,
            high_water as i64,
            request.limit as i64
        ]))?;
        while let Some(raw) = database(rows.next())? {
            let row = decode_audit_row(database(raw_audit_row(raw))?)?;
            if row.sequence != last + 1 {
                return Err("audit page sequence gap".into());
            }
            let summary = audit_summary(&tx, row)?;
            let candidate_last = summary.sequence;
            items.push(summary);
            let candidate = AuditQueryReply {
                version: 1,
                high_water,
                max_admission_entries: MAX_AUDIT_ADMISSION_ENTRIES,
                items: items.clone(),
                next_after: (candidate_last < high_water).then_some(candidate_last),
            };
            if serde_json::to_vec(&candidate)
                .map_err(|error| error.to_string())?
                .len()
                > AUDIT_PAGE_MAX_BYTES
            {
                items.pop();
                if items.is_empty() {
                    return Err("audit summary exceeds page bound".into());
                }
                break;
            }
            last = candidate_last;
        }
    }
    if items.len() < request.limit as usize && last < high_water {
        // A byte-bounded page may stop before its item limit. The next row must
        // nevertheless exist; a later page validates and returns it.
        if audit_row_by_sequence(&tx, last + 1)?.is_none() {
            return Err("audit page sequence gap".into());
        }
    }
    let reply = AuditQueryReply {
        version: 1,
        high_water,
        max_admission_entries: MAX_AUDIT_ADMISSION_ENTRIES,
        items,
        next_after: (last < high_water).then_some(last),
    };
    reply.validate_for(request)?;
    database(tx.commit())?;
    Ok(reply)
}

pub(super) fn migration_receipt(
    root: &Path,
    request: &PolicyMigrationRequest,
) -> Result<PolicyMigrationReceipt, String> {
    request.validate()?;
    let _process_lock = process_lock()?;
    let connection = existing(root)?.ok_or("authorization store is not initialized")?;
    current_in(&connection)?;
    let bytes: Vec<u8> = database(connection.query_row(
        "SELECT receipt FROM policy_migration WHERE singleton = 1",
        [],
        |row| row.get(0),
    ))?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err("policy migration receipt exceeds bound".into());
    }
    let receipt: PolicyMigrationReceipt = crate::binary::decode(&bytes)?;
    receipt.validate_request(request)?;
    Ok(receipt)
}

fn source_digest(path: &Path) -> Result<String, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("inspect legacy policy: {error}"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_POLICY_BYTES as u64
    {
        return Err("legacy policy must be a bounded regular file".into());
    }
    let bytes = fs::read(path).map_err(|error| format!("read legacy policy: {error}"))?;
    if !bytes.starts_with(LEGACY_HEADER) {
        return Err("legacy policy format changed during migration".into());
    }
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

pub(super) fn migrate(
    root: &Path,
    policy: Option<&ServerPolicy>,
    generation: u64,
    receipts: &BTreeMap<String, PolicyUpdateReceipt>,
    migration: &PolicyMigrationReceipt,
) -> Result<(), String> {
    let _process_lock = process_lock()?;
    migrate_with_sync(
        root,
        policy,
        generation,
        receipts,
        migration,
        crate::output::sync_directory,
    )
}

fn migrate_with_sync(
    root: &Path,
    policy: Option<&ServerPolicy>,
    generation: u64,
    receipts: &BTreeMap<String, PolicyUpdateReceipt>,
    migration: &PolicyMigrationReceipt,
    sync_directory: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    migration.validate()?;
    if migration.generation != generation
        || migration.receipts != receipts.len() as u64
        || migration.policy_sha256 != policy.map(ServerPolicy::digest).transpose()?
    {
        return Err("legacy policy migration receipt does not match the source".into());
    }
    let source = root.join(FILE);
    if source_digest(&source)? != migration.source_sha256 {
        return Err("legacy policy source digest mismatch".into());
    }
    let mut staging = stage(root)?;
    let mut connection = connect(&staging.path)?;
    let tx = database(connection.transaction_with_behavior(TransactionBehavior::Immediate))?;
    database(tx.execute_batch(SCHEMA))?;
    let mut current_operation = None;
    for (id, receipt) in receipts {
        receipt.validate()?;
        if &receipt.operation_id != id {
            return Err("legacy policy receipt identity mismatch".into());
        }
        let applied = if receipt.outcome == PolicyUpdateOutcome::Applied {
            if receipt.generation == generation {
                current_operation = Some(id.clone());
            }
            Some(receipt.generation as i64)
        } else {
            None
        };
        let bytes = crate::binary::encode(receipt)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err("legacy policy receipt exceeds bound".into());
        }
        database(tx.execute("INSERT INTO policy_receipts(operation_id, applied_generation, receipt) VALUES (?1, ?2, ?3)", params![id, applied, bytes]))?;
    }
    if generation > 0 {
        let policy = policy.ok_or("legacy current policy is missing")?;
        let bytes = crate::binary::encode(policy)?;
        if bytes.len() > MAX_POLICY_BYTES {
            return Err("legacy policy exceeds bound".into());
        }
        let digest = policy.digest()?;
        let operation = current_operation.ok_or("legacy current policy receipt is missing")?;
        database(tx.execute("UPDATE policy_current SET generation = ?1, policy = ?2, policy_sha256 = ?3, current_operation = ?4 WHERE singleton = 1 AND generation = 0",
            params![generation as i64, bytes, digest, operation]))?;
    } else if policy.is_some() {
        return Err("legacy uninitialized policy is invalid".into());
    }
    current_in(&tx)?;
    database(tx.execute(
        "INSERT INTO policy_migration(singleton, receipt) VALUES (1, ?1)",
        [crate::binary::encode(migration)?],
    ))?;
    database(tx.commit())?;
    seal(connection, &staging.path)?;
    if source_digest(&source)? != migration.source_sha256 {
        return Err("legacy policy changed during migration".into());
    }
    fs::rename(&staging.path, &source)
        .map_err(|error| format!("publish migrated policy database: {error}"))?;
    staging.cleanup = false;
    sync_directory(root).map_err(|error| {
        format!("policy migration outcome unknown; retry the original migration: {error}")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "wasm-git-policy-sqlite-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn policy() -> ServerPolicy {
        ServerPolicy {
            version: crate::protocol::SERVER_POLICY_VERSION,
            principals: Vec::new(),
        }
    }
    fn update(id: String, generation: u64, policy: ServerPolicy) -> PolicyUpdateRequest {
        PolicyUpdateRequest {
            version: 1,
            operation_id: id,
            expected_generation: generation,
            policy,
        }
    }

    fn policy_with_principals() -> ServerPolicy {
        use crate::protocol::PolicyPrincipal;
        ServerPolicy {
            version: crate::protocol::SERVER_POLICY_VERSION,
            principals: vec![
                PolicyPrincipal {
                    name: "alice".into(),
                    token_sha256: Some("a".repeat(64)),
                    grants: Vec::new(),
                },
                PolicyPrincipal {
                    name: "bob".into(),
                    token_sha256: None,
                    grants: Vec::new(),
                },
            ],
        }
    }

    fn credential(index: u64) -> OperationCredentialReceipt {
        OperationCredentialReceipt {
            version: 1,
            operation_id: format!("{index:064x}"),
            actor: "alice".into(),
            policy_generation: 1,
            token_sha256: format!("{:064x}", index + 10_000),
            request_sha256: format!("{:064x}", index + 20_000),
            effect_key: Some(format!("{:064x}", index + 30_000)),
            expires_at: NODE_SAFE_INTEGER_MAX,
        }
    }

    fn audit_operator(name: &str) -> AuditOperator {
        AuditOperator {
            version: 1,
            identity: name.into(),
        }
    }

    fn audit_original(index: u64) -> OperationCredentialRequest {
        use crate::protocol::RetentionCommand;
        OperationCredentialRequest {
            version: 1,
            operation_id: format!("{index:064x}"),
            policy_generation: 1,
            actor: "alice".into(),
            token_sha256: format!("{:064x}", index + 10_000),
            request: Request {
                version: 1,
                repository: "project".into(),
                operation: Operation::ReleaseRetention {
                    command: RetentionCommand::Prepare {
                        operation: format!("{:064x}", index + 20_000),
                        commit: "c".repeat(64),
                        paths: vec!["module.wasm".into()],
                    },
                },
            },
            payload_sha256: format!("{:x}", Sha256::digest([])),
            expires_at: NODE_SAFE_INTEGER_MAX,
        }
    }

    fn audit_page(root: &Path) -> AuditQueryReply {
        audit_query(
            root,
            &AuditQueryRequest {
                version: 1,
                after: 0,
                high_water: None,
                limit: 100,
            },
        )
        .unwrap()
    }

    #[test]
    fn audited_policy_and_credentials_commit_together_and_page_at_a_fixed_watermark() {
        let root = Temp::new();
        let operator = audit_operator("unix:uid:1000");
        let policy = update("f".repeat(64), 0, policy_with_principals());
        assert_eq!(
            set_audited(&root.0, &policy, &operator).unwrap().generation,
            1
        );
        let state = current(&root.0).unwrap().unwrap();
        assert!(!state.operation_credentials && !state.chunk_receipts);
        assert_eq!(
            existing(&root.0)
                .unwrap()
                .unwrap()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            AUDIT_SCHEMA_VERSION
        );
        let original = audit_original(1);
        let receipt = credential_issue_audited(&root.0, &original, &operator).unwrap();
        assert_eq!(receipt, original.receipt().unwrap());
        assert_eq!(
            crate::binary::encode(
                &audit_operation_context(&root.0, &original.operation_id)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            crate::binary::encode(&original).unwrap()
        );
        assert!(current(&root.0).unwrap().unwrap().chunk_receipts);
        let first_request = AuditQueryRequest {
            version: 1,
            after: 0,
            high_water: None,
            limit: 1,
        };
        let first = audit_query(&root.0, &first_request).unwrap();
        assert_eq!(first.high_water, 2);
        assert_eq!(first.next_after, Some(1));
        assert!(
            matches!(&first.items[0].item, AuditItem::Policy { attribution: AuditAttribution::Recorded { operator: actual, .. }, .. } if actual == &operator)
        );
        let attempt = audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256,
        )
        .unwrap();
        assert_eq!(attempt, 3);
        let next = audit_query(
            &root.0,
            &AuditQueryRequest {
                version: 1,
                after: 1,
                high_water: Some(first.high_water),
                limit: 2,
            },
        )
        .unwrap();
        assert_eq!(next.items.len(), 1);
        assert_eq!(next.items[0].sequence, 2);
        assert_eq!(next.next_after, None);
        assert_eq!(audit_page(&root.0).high_water, 3);
        let changed_operator = audit_operator("unix:uid:2000");
        credential_issue_audited(&root.0, &original, &changed_operator).unwrap();
        set_audited(&root.0, &policy, &changed_operator).unwrap();
        let records = audit_page(&root.0);
        assert_eq!(records.items.len(), 3);
        assert!(
            matches!(&records.items[1].item, AuditItem::Credential { attribution: AuditAttribution::Recorded { operator: actual, .. }, .. } if actual == &operator)
        );
        assert!(
            audit_start_operation(&root.0, &receipt, &original.request, &"0".repeat(64)).is_err()
        );
    }

    #[test]
    fn attempts_are_independent_and_recovery_observes_without_claiming_the_effect() {
        let root = Temp::new();
        let issuer = audit_operator("unix:uid:1000");
        let recovery_operator = audit_operator("unix:uid:2000");
        bootstrap_audited(&root.0, policy_with_principals(), &issuer).unwrap();
        let original = audit_original(2);
        let receipt = credential_issue_audited(&root.0, &original, &issuer).unwrap();
        let first = audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256,
        )
        .unwrap();
        let second = audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256,
        )
        .unwrap();
        assert_ne!(first, second);
        let observation = AuditObservation {
            version: 1,
            scope: AuditObservationScope::Retention,
            evidence: AuditEvidenceObservation::Unobserved,
        };
        let first_target = AuditTarget::Operation {
            operation_id: original.operation_id.clone(),
            attempt_id: first,
        };
        let second_target = AuditTarget::Operation {
            operation_id: original.operation_id.clone(),
            attempt_id: second,
        };
        audit_observe(&root.0, &first_target, &observation).unwrap();
        audit_observe(&root.0, &first_target, &observation).unwrap();
        let different = AuditObservation {
            evidence: AuditEvidenceObservation::Unknown,
            ..observation.clone()
        };
        assert!(audit_observe(&root.0, &first_target, &different).is_err());
        assert!(matches!(
            &audit_page(&root.0).items[(second - 1) as usize].item,
            AuditItem::OperationAttempt {
                observation: None,
                ..
            }
        ));
        let revoked = update("d".repeat(64), 1, policy());
        set_audited(&root.0, &revoked, &issuer).unwrap();
        assert!(audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256
        )
        .is_err());
        let recovery_id = "e".repeat(64);
        let recovery_attempt = audit_begin_recovery(
            &root.0,
            &recovery_id,
            &original,
            RecoveryAuditAction::Inspect,
            &recovery_operator,
        )
        .unwrap();
        assert!(audit_reconcile(
            &root.0,
            &recovery_id,
            recovery_attempt,
            &second_target,
            &observation
        )
        .is_err());
        let recovery_target = AuditTarget::Recovery {
            recovery_id: recovery_id.clone(),
            attempt_id: recovery_attempt,
        };
        audit_observe(&root.0, &recovery_target, &observation).unwrap();
        audit_reconcile(
            &root.0,
            &recovery_id,
            recovery_attempt,
            &second_target,
            &observation,
        )
        .unwrap();
        audit_reconcile(
            &root.0,
            &recovery_id,
            recovery_attempt,
            &second_target,
            &observation,
        )
        .unwrap();
        assert!(audit_begin_recovery(
            &root.0,
            &recovery_id,
            &original,
            RecoveryAuditAction::Close,
            &recovery_operator
        )
        .is_err());
        let repeated = audit_begin_recovery(
            &root.0,
            &recovery_id,
            &original,
            RecoveryAuditAction::Inspect,
            &recovery_operator,
        )
        .unwrap();
        assert_ne!(recovery_attempt, repeated);
        let page = audit_page(&root.0);
        assert!(matches!(&page.items[(second - 1) as usize].item,
            AuditItem::OperationAttempt { observation: Some(AuditRecordedObservation {
                source: AuditObservationSource::RecoveryAttempt { recovery_id: actual, attempt_id },
                observation: AuditObservation { evidence: AuditEvidenceObservation::Unobserved, .. }, .. }), .. }
                if actual == &recovery_id && *attempt_id == recovery_attempt));
        assert!(matches!(
            &page.items[(repeated - 1) as usize].item,
            AuditItem::RecoveryAttempt {
                observation: None,
                ..
            }
        ));
    }

    #[test]
    fn legacy_credential_import_validates_the_exact_original_and_never_invents_issuer() {
        let root = Temp::new();
        old_v2(&root.0);
        let original = audit_original(3);
        let receipt = credential_issue(&root.0, &original.receipt().unwrap()).unwrap();
        assert!(audit_operation_context(&root.0, &original.operation_id)
            .unwrap()
            .is_none());
        let operator = audit_operator("unix:uid:2000");
        let mut changed = original.clone();
        changed.payload_sha256 = "0".repeat(64);
        assert!(audit_import_legacy(&root.0, &changed, &operator).is_err());
        audit_import_legacy(&root.0, &original, &operator).unwrap();
        assert!(
            matches!(&audit_page(&root.0).items[0].item, AuditItem::Credential {
            attribution: AuditAttribution::LegacyImported { observer, .. }, .. } if observer == &operator)
        );
        assert!(audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256
        )
        .is_err());
        assert!(audit_begin_recovery(
            &root.0,
            &"d".repeat(64),
            &original,
            RecoveryAuditAction::Inspect,
            &operator
        )
        .is_ok());
    }

    #[test]
    fn recovery_reconciliation_is_atomic_and_replay_preserves_prior_observations() {
        let root = Temp::new();
        let operator = audit_operator("unix:uid:1000");
        bootstrap_audited(&root.0, policy_with_principals(), &operator).unwrap();
        let original = audit_original(72);
        let receipt = credential_issue_audited(&root.0, &original, &operator).unwrap();
        let first = audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256,
        )
        .unwrap();
        let second = audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256,
        )
        .unwrap();
        let targets = [first, second].map(|attempt_id| AuditTarget::Operation {
            operation_id: original.operation_id.clone(),
            attempt_id,
        });
        let recovery_id = "c".repeat(64);
        let attempt = audit_begin_recovery(
            &root.0,
            &recovery_id,
            &original,
            RecoveryAuditAction::Inspect,
            &operator,
        )
        .unwrap();
        let observation = AuditObservation {
            version: 1,
            scope: AuditObservationScope::Retention,
            evidence: AuditEvidenceObservation::Unobserved,
        };
        audit_observe(
            &root.0,
            &AuditTarget::Recovery {
                recovery_id: recovery_id.clone(),
                attempt_id: attempt,
            },
            &observation,
        )
        .unwrap();
        let bad = AuditTarget::Operation {
            operation_id: original.operation_id.clone(),
            attempt_id: 9999,
        };
        assert!(audit_reconcile_many(
            &root.0,
            &recovery_id,
            attempt,
            &[targets[0].clone(), bad],
            &observation
        )
        .is_err());
        assert!(matches!(
            &audit_page(&root.0).items[(first - 1) as usize].item,
            AuditItem::OperationAttempt {
                observation: None,
                ..
            }
        ));
        audit_reconcile_many(&root.0, &recovery_id, attempt, &targets, &observation).unwrap();
        let repeated = audit_begin_recovery(
            &root.0,
            &recovery_id,
            &original,
            RecoveryAuditAction::Inspect,
            &operator,
        )
        .unwrap();
        let later = AuditObservation {
            evidence: AuditEvidenceObservation::Unknown,
            ..observation
        };
        audit_observe(
            &root.0,
            &AuditTarget::Recovery {
                recovery_id: recovery_id.clone(),
                attempt_id: repeated,
            },
            &later,
        )
        .unwrap();
        audit_reconcile_many(&root.0, &recovery_id, repeated, &targets, &later).unwrap();
        assert!(
            matches!(&audit_page(&root.0).items[(first - 1) as usize].item,
            AuditItem::OperationAttempt { observation: Some(AuditRecordedObservation {
                source: AuditObservationSource::RecoveryAttempt { attempt_id, .. },
                observation: AuditObservation { evidence: AuditEvidenceObservation::Unobserved, .. }, .. }), .. } if *attempt_id == attempt)
        );
    }

    #[test]
    fn audit_admission_watermark_does_not_exhaust_recovery_capacity() {
        let root = Temp::new();
        let operator = audit_operator("unix:uid:1000");
        bootstrap_audited(&root.0, policy_with_principals(), &operator).unwrap();
        let original = audit_original(73);
        credential_issue_audited(&root.0, &original, &operator).unwrap();
        let recovery_id = "b".repeat(64);
        audit_begin_recovery(
            &root.0,
            &recovery_id,
            &original,
            RecoveryAuditAction::Close,
            &operator,
        )
        .unwrap();
        let connection = existing(&root.0).unwrap().unwrap();
        let watermark = audit_high_water(&connection).unwrap();
        let start = audit_now().unwrap();
        assert!(audit_insert_with_limit(
            &connection,
            &original.operation_id,
            StoredAuditDetail::OperationAttempt {
                started_at_unix_millis: start
            },
            false,
            watermark
        )
        .is_err());
        audit_insert_with_limit(
            &connection,
            &recovery_id,
            StoredAuditDetail::RecoveryAttempt {
                started_at_unix_millis: start,
            },
            true,
            watermark,
        )
        .unwrap();
        assert_eq!(audit_high_water(&connection).unwrap(), watermark + 1);
        drop(connection);
        assert_eq!(audit_page(&root.0).items.len() as u64, watermark + 1);
    }

    #[test]
    fn failed_audit_insert_rolls_back_policy_and_credential_decisions() {
        let root = Temp::new();
        let operator = audit_operator("unix:uid:1000");
        bootstrap_audited(&root.0, policy_with_principals(), &operator).unwrap();
        let connection = existing(&root.0).unwrap().unwrap();
        connection.execute_batch("CREATE TRIGGER injected_audit_failure BEFORE INSERT ON audit_index BEGIN SELECT RAISE(ABORT, 'injected audit failure'); END").unwrap();
        drop(connection);
        let original = audit_original(4);
        assert!(credential_issue_audited(&root.0, &original, &operator).is_err());
        assert!(!current(&root.0).unwrap().unwrap().operation_credentials);
        let policy = update("b".repeat(64), 1, policy_with_principals());
        assert!(set_audited(&root.0, &policy, &operator).is_err());
        assert_eq!(status(&root.0).unwrap().generation, 1);
        assert!(query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: policy.operation_id.clone()
            }
        )
        .unwrap()
        .receipt
        .is_none());
        let connection = existing(&root.0).unwrap().unwrap();
        connection
            .execute_batch("DROP TRIGGER injected_audit_failure")
            .unwrap();
        drop(connection);
        assert!(credential_issue_audited(&root.0, &original, &operator).is_ok());
        assert_eq!(
            set_audited(&root.0, &policy, &operator).unwrap().generation,
            2
        );
    }

    #[test]
    fn corrupt_or_missing_audit_entries_fail_closed() {
        let root = Temp::new();
        let operator = audit_operator("unix:uid:1000");
        bootstrap_audited(&root.0, policy_with_principals(), &operator).unwrap();
        let original = audit_original(5);
        let receipt = credential_issue_audited(&root.0, &original, &operator).unwrap();
        audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256,
        )
        .unwrap();
        let connection = existing(&root.0).unwrap().unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER audit_no_delete; DELETE FROM audit_index WHERE sequence = 2",
            )
            .unwrap();
        connection.execute_batch(AUDIT_NO_DELETE_SQL).unwrap();
        drop(connection);
        assert!(audit_query(
            &root.0,
            &AuditQueryRequest {
                version: 1,
                after: 0,
                high_water: None,
                limit: 100
            }
        )
        .is_err());
        assert!(audit_import_legacy(&root.0, &original, &operator).is_err());
        assert!(audit_start_operation(
            &root.0,
            &receipt,
            &original.request,
            &original.payload_sha256
        )
        .is_err());
    }

    fn old_v2(root: &Path) {
        let path = root.join(FILE);
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap(),
        );
        let mut connection = connect(&path).unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute_batch(SCHEMA).unwrap();
        decide(&tx, &update("f".repeat(64), 0, policy_with_principals())).unwrap();
        tx.commit().unwrap();
    }

    fn install_v3_credential(root: &Path, record: &OperationCredentialReceipt) {
        record.validate().unwrap();
        let mut connection = existing(root).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(credential_features(&tx).unwrap(), (false, false));
        create_credential_schema(&tx).unwrap();
        tx.execute_batch("PRAGMA user_version = 3").unwrap();
        tx.execute(
            "INSERT INTO operation_credentials(operation_id, token_sha256, effect_key, receipt) VALUES (?1, ?2, ?3, ?4)",
            params![record.operation_id, record.token_sha256, record.effect_key, crate::binary::encode(record).unwrap()],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn old_v2_credentials_initialize_atomically_without_changing_policy() {
        let root = Temp::new();
        old_v2(&root.0);
        let record = credential(1);
        assert!(credential_query(&root.0, &record.operation_id)
            .unwrap()
            .is_none());
        assert!(credential_by_token(&root.0, &record.token_sha256)
            .unwrap()
            .is_none());
        assert!(!credential_schema(&existing(&root.0).unwrap().unwrap()).unwrap());
        let initial = current(&root.0).unwrap().unwrap();
        assert!(!initial.operation_credentials);
        assert!(!initial.chunk_receipts);
        let before = status(&root.0).unwrap();
        assert_eq!(credential_issue(&root.0, &record).unwrap(), record);
        assert_eq!(
            credential_query(&root.0, &record.operation_id).unwrap(),
            Some(record.clone())
        );
        assert_eq!(
            credential_by_token(&root.0, &record.token_sha256).unwrap(),
            Some(record)
        );
        let after = status(&root.0).unwrap();
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.policy_sha256, after.policy_sha256);
        let connection = existing(&root.0).unwrap().unwrap();
        assert!(credential_schema(&connection).unwrap());
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CHUNK_RECEIPT_SCHEMA_VERSION);
        assert_ne!(version, SCHEMA_VERSION);
        assert_ne!(version, CREDENTIAL_SCHEMA_VERSION);
        let current = current_in(&connection).unwrap();
        assert!(current.operation_credentials);
        assert!(current.chunk_receipts);
        assert!(connection
            .execute(
                "UPDATE operation_credentials SET token_sha256 = ?1",
                ["b".repeat(64)]
            )
            .is_err());
        assert!(connection
            .execute("DELETE FROM operation_credentials", [])
            .is_err());
    }

    #[test]
    fn v3_history_replays_without_upgrade_and_fresh_issue_upgrades_atomically() {
        let root = Temp::new();
        old_v2(&root.0);
        let mut original = credential(1);
        original.expires_at = 1;
        install_v3_credential(&root.0, &original);

        let before = status(&root.0).unwrap();
        let state = current(&root.0).unwrap().unwrap();
        assert!(state.operation_credentials);
        assert!(!state.chunk_receipts);
        assert_eq!(
            credential_query(&root.0, &original.operation_id).unwrap(),
            Some(original.clone())
        );
        assert_eq!(
            credential_by_token(&root.0, &original.token_sha256).unwrap(),
            Some(original.clone())
        );
        assert_eq!(credential_issue(&root.0, &original).unwrap(), original);
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);

        let mut duplicate_token = credential(3);
        duplicate_token.token_sha256 = original.token_sha256.clone();
        assert!(credential_issue(&root.0, &duplicate_token).is_err());
        let mut duplicate_effect = credential(4);
        duplicate_effect.effect_key = original.effect_key.clone();
        assert!(credential_issue(&root.0, &duplicate_effect).is_err());
        let mut stale = credential(5);
        stale.policy_generation = 2;
        assert!(credential_issue(&root.0, &stale).is_err());
        let mut missing_actor = credential(6);
        missing_actor.actor = "missing".into();
        assert!(credential_issue(&root.0, &missing_actor).is_err());
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);

        let fresh = credential(2);
        let mut connection = existing(&root.0).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let error = credential_decide_with_check(&tx, &fresh, || {
            assert_eq!(credential_features(&tx).unwrap(), (true, true));
            Err("injected expired authority".into())
        })
        .unwrap_err();
        assert!(error.contains("expired"));
        drop(tx);
        drop(connection);
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);
        assert!(credential_query(&root.0, &fresh.operation_id)
            .unwrap()
            .is_none());
        assert_eq!(
            credential_query(&root.0, &original.operation_id).unwrap(),
            Some(original.clone())
        );

        let mut expired = fresh.clone();
        expired.expires_at = 1;
        assert!(credential_issue(&root.0, &expired).is_err());
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);
        assert_eq!(credential_issue(&root.0, &fresh).unwrap(), fresh);
        let state = current(&root.0).unwrap().unwrap();
        assert!(state.operation_credentials && state.chunk_receipts);
        assert_eq!(
            credential_query(&root.0, &original.operation_id).unwrap(),
            Some(original)
        );
        let after = status(&root.0).unwrap();
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.policy_sha256, after.policy_sha256);
    }

    #[test]
    fn v3_to_v4_disk_full_rolls_back_version_and_new_receipts() {
        let root = Temp::new();
        old_v2(&root.0);
        let original = credential(1);
        install_v3_credential(&root.0, &original);
        let mut connection = existing(&root.0).unwrap().unwrap();
        let pages: i64 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        let maximum: i64 = connection
            .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(maximum, pages);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut full = false;
        for index in 2..=1000 {
            if let Err(error) = credential_decide(&tx, &credential(index)) {
                assert!(error.to_ascii_lowercase().contains("full"), "{error}");
                full = true;
                break;
            }
        }
        assert!(full, "database did not reach its fixed page limit");
        drop(tx);
        drop(connection);
        let state = current(&root.0).unwrap().unwrap();
        assert!(state.operation_credentials);
        assert!(!state.chunk_receipts);
        assert_eq!(
            credential_query(&root.0, &original.operation_id).unwrap(),
            Some(original)
        );
        assert!(credential_query(&root.0, &credential(2).operation_id)
            .unwrap()
            .is_none());
        assert_eq!(
            credential_issue(&root.0, &credential(2)).unwrap(),
            credential(2)
        );
        assert!(current(&root.0).unwrap().unwrap().chunk_receipts);
    }

    #[test]
    fn v3_policy_updates_keep_history_and_reject_credential_token_collisions() {
        let root = Temp::new();
        old_v2(&root.0);
        let original = credential(1);
        install_v3_credential(&root.0, &original);
        let mut collision = policy_with_principals();
        collision.principals[1].token_sha256 = Some(original.token_sha256.clone());
        assert!(set(&root.0, &update("a".repeat(64), 1, collision))
            .unwrap_err()
            .contains("collides with an operation credential"));
        assert_eq!(status(&root.0).unwrap().generation, 1);
        let applied = set(
            &root.0,
            &update("b".repeat(64), 1, policy_with_principals()),
        )
        .unwrap();
        assert_eq!(applied.generation, 2);
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);
        assert_eq!(credential_issue(&root.0, &original).unwrap(), original);
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);
        let prior = query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: "f".repeat(64),
            },
        )
        .unwrap()
        .receipt
        .unwrap();
        assert_eq!(prior.generation, 1);
        let mut fresh = credential(2);
        fresh.policy_generation = 2;
        credential_issue(&root.0, &fresh).unwrap();
        assert!(current(&root.0).unwrap().unwrap().chunk_receipts);
        assert_eq!(status(&root.0).unwrap().generation, 2);
    }

    #[test]
    fn v1_migration_receipt_remains_queryable_in_v3_and_v4() {
        let root = Temp::new();
        let source = include_bytes!("../../../tests/fixtures/policy-v1.bin");
        fs::write(root.0.join(FILE), source).unwrap();
        let request = PolicyMigrationRequest {
            version: 1,
            source_sha256: format!("{:x}", Sha256::digest(source)),
        };
        let initial = super::super::migrate(&root.0, &request).unwrap();
        set(
            &root.0,
            &update("d".repeat(64), 1, policy_with_principals()),
        )
        .unwrap();
        let mut original = credential(1);
        original.policy_generation = 2;
        install_v3_credential(&root.0, &original);
        assert!(current(&root.0).unwrap().unwrap().operation_credentials);
        assert!(!current(&root.0).unwrap().unwrap().chunk_receipts);
        let v3 = super::super::migrate(&root.0, &request).unwrap();
        assert_eq!(v3.generation, initial.generation);
        assert_eq!(v3.receipts, initial.receipts);
        assert_eq!(v3.policy_sha256, initial.policy_sha256);
        assert_eq!(
            credential_query(&root.0, &original.operation_id).unwrap(),
            Some(original)
        );
        let mut fresh = credential(2);
        fresh.policy_generation = 2;
        credential_issue(&root.0, &fresh).unwrap();
        assert!(current(&root.0).unwrap().unwrap().chunk_receipts);
        let v4 = super::super::migrate(&root.0, &request).unwrap();
        assert_eq!(v4.generation, initial.generation);
        assert_eq!(v4.receipts, initial.receipts);
        assert_eq!(v4.policy_sha256, initial.policy_sha256);
        assert_eq!(status(&root.0).unwrap().generation, 2);
    }

    #[test]
    fn credential_replay_and_collision_rules_survive_principal_removal() {
        let root = Temp::new();
        set(
            &root.0,
            &update("f".repeat(64), 0, policy_with_principals()),
        )
        .unwrap();
        let mut first = credential(1);
        first.expires_at = 1;
        // Simulate issuance while this deadline was current, then exercise
        // replay and uniqueness after its deadline has passed.
        let mut connection = existing(&root.0).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(
            credential_decide_with_check(&tx, &first, || Ok(())).unwrap(),
            first
        );
        tx.commit().unwrap();
        drop(connection);
        assert_eq!(credential_issue(&root.0, &first).unwrap(), first);
        for field in 0..6 {
            let mut changed = first.clone();
            match field {
                0 => changed.actor = "bob".into(),
                1 => changed.token_sha256 = "b".repeat(64),
                2 => changed.request_sha256 = "c".repeat(64),
                3 => changed.expires_at += 1,
                4 => changed.effect_key = None,
                _ => changed.policy_generation += 1,
            }
            assert!(credential_issue(&root.0, &changed)
                .unwrap_err()
                .contains("identity conflict"));
        }
        let mut same_token = credential(2);
        same_token.token_sha256 = first.token_sha256.clone();
        assert!(credential_issue(&root.0, &same_token)
            .unwrap_err()
            .contains("token is already assigned"));
        let mut same_effect = credential(3);
        same_effect.effect_key = first.effect_key.clone();
        assert!(credential_issue(&root.0, &same_effect)
            .unwrap_err()
            .contains("effect is already assigned"));
        let mut same_principal_token = credential(4);
        same_principal_token.token_sha256 = "a".repeat(64);
        assert!(credential_issue(&root.0, &same_principal_token)
            .unwrap_err()
            .contains("policy credential"));
        let mut absent = credential(5);
        absent.actor = "missing".into();
        assert!(credential_issue(&root.0, &absent)
            .unwrap_err()
            .contains("principal does not exist"));

        let mut changed_policy = policy_with_principals();
        changed_policy.principals.remove(0);
        let mut policy_token_collision = changed_policy.clone();
        policy_token_collision.principals[0].token_sha256 = Some(first.token_sha256.clone());
        let rejected = update("e".repeat(64), 1, policy_token_collision);
        assert!(set(&root.0, &rejected)
            .unwrap_err()
            .contains("collides with an operation credential"));
        assert_eq!(status(&root.0).unwrap().generation, 1);
        assert!(query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: rejected.operation_id
            }
        )
        .unwrap()
        .receipt
        .is_none());
        set(&root.0, &update("d".repeat(64), 1, changed_policy)).unwrap();
        assert_eq!(credential_issue(&root.0, &first).unwrap(), first);
        assert_eq!(
            credential_query(&root.0, &first.operation_id).unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            credential_by_token(&root.0, &first.token_sha256).unwrap(),
            Some(first.clone())
        );
        let mut renewed = credential(9);
        renewed.actor = "bob".into();
        renewed.policy_generation = 2;
        renewed.effect_key = first.effect_key.clone();
        assert!(credential_issue(&root.0, &renewed)
            .unwrap_err()
            .contains("effect is already assigned"));
        let mut removed_actor = credential(6);
        removed_actor.actor = "alice".into();
        removed_actor.policy_generation = 2;
        assert!(credential_issue(&root.0, &removed_actor)
            .unwrap_err()
            .contains("principal does not exist"));
        let mut second_query = credential(7);
        second_query.actor = "bob".into();
        second_query.effect_key = None;
        assert!(credential_issue(&root.0, &second_query)
            .unwrap_err()
            .contains("policy generation"));
        assert!(credential_query(&root.0, &second_query.operation_id)
            .unwrap()
            .is_none());
        second_query.policy_generation = 2;
        let mut third_query = credential(8);
        third_query.actor = "bob".into();
        third_query.policy_generation = 2;
        third_query.effect_key = None;
        credential_issue(&root.0, &second_query).unwrap();
        credential_issue(&root.0, &third_query).unwrap();
    }

    #[test]
    fn credential_indexes_retain_more_than_the_old_capacity() {
        let root = Temp::new();
        set(
            &root.0,
            &update("f".repeat(64), 0, policy_with_principals()),
        )
        .unwrap();
        let mut connection = existing(&root.0).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        for index in 0..=4100 {
            let record = credential(index);
            assert_eq!(credential_decide(&tx, &record).unwrap(), record);
        }
        tx.commit().unwrap();
        for (sql, key, index_name) in [
            ("EXPLAIN QUERY PLAN SELECT receipt FROM operation_credentials WHERE operation_id = ?1", credential(1).operation_id, "PRIMARY KEY"),
            ("EXPLAIN QUERY PLAN SELECT receipt FROM operation_credentials WHERE token_sha256 = ?1", credential(1).token_sha256, "operation_credentials_token"),
            ("EXPLAIN QUERY PLAN SELECT receipt FROM operation_credentials WHERE effect_key = ?1", credential(1).effect_key.unwrap(), "operation_credentials_effect"),
        ] {
            let plan: String = connection.query_row(sql, [key], |row| row.get(3)).unwrap();
            assert!(plan.contains(index_name), "{plan}");
        }
        drop(connection);
        for index in [0, 4100] {
            let record = credential(index);
            assert_eq!(
                credential_query(&root.0, &record.operation_id).unwrap(),
                Some(record.clone())
            );
            assert_eq!(
                credential_by_token(&root.0, &record.token_sha256).unwrap(),
                Some(record)
            );
        }
        credential_issue(&root.0, &credential(4101)).unwrap();
    }

    #[test]
    fn credential_schema_and_indexes_fail_closed_on_corruption() {
        for damage in [
            "partial",
            "present_in_v2",
            "v3_without",
            "v4_without",
            "unknown_version",
            "nonunique",
            "index_mismatch",
            "effect_mismatch",
        ] {
            let root = Temp::new();
            old_v2(&root.0);
            let first = credential(1);
            if matches!(damage, "index_mismatch" | "effect_mismatch") {
                credential_issue(&root.0, &first).unwrap();
            }
            let connection = connect(&root.0.join(FILE)).unwrap();
            if damage == "partial" {
                connection.execute_batch(CREDENTIAL_TABLE_SQL).unwrap();
            } else if damage == "present_in_v2" {
                create_credential_schema(&connection).unwrap();
            } else if damage == "v3_without" {
                connection.execute_batch("PRAGMA user_version = 3").unwrap();
            } else if damage == "v4_without" {
                connection.execute_batch("PRAGMA user_version = 4").unwrap();
            } else if damage == "unknown_version" {
                connection.execute_batch("PRAGMA user_version = 5").unwrap();
            } else if damage == "nonunique" {
                create_credential_schema(&connection).unwrap();
                connection.execute_batch("DROP INDEX operation_credentials_token; CREATE INDEX operation_credentials_token ON operation_credentials(token_sha256)").unwrap();
            } else {
                connection
                    .execute_batch("DROP TRIGGER operation_credentials_no_update")
                    .unwrap();
                let column = if damage == "index_mismatch" {
                    "token_sha256"
                } else {
                    "effect_key"
                };
                connection
                    .execute(
                        &format!("UPDATE operation_credentials SET {column} = ?1"),
                        ["b".repeat(64)],
                    )
                    .unwrap();
                connection.execute_batch(CREDENTIAL_UPDATE_SQL).unwrap();
            }
            drop(connection);
            assert!(
                credential_query(&root.0, &first.operation_id).is_err(),
                "{damage}"
            );
            let mut second = credential(2);
            if damage == "index_mismatch" {
                second.token_sha256 = "b".repeat(64);
            } else if damage == "effect_mismatch" {
                second.effect_key = Some("b".repeat(64));
            }
            assert!(credential_issue(&root.0, &second).is_err(), "{damage}");
            if !matches!(damage, "index_mismatch" | "effect_mismatch") {
                assert!(status(&root.0).is_err(), "{damage}");
            }
        }
    }

    #[test]
    fn credential_schema_and_first_receipt_roll_back_when_disk_is_full() {
        let root = Temp::new();
        old_v2(&root.0);
        let mut connection = existing(&root.0).unwrap().unwrap();
        let pages: i64 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        let maximum: i64 = connection
            .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(maximum, pages);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(credential_decide(&tx, &credential(1)).is_err());
        drop(tx);
        drop(connection);
        assert!(credential_query(&root.0, &credential(1).operation_id)
            .unwrap()
            .is_none());
        assert!(!credential_schema(&existing(&root.0).unwrap().unwrap()).unwrap());
        assert!(!current(&root.0).unwrap().unwrap().operation_credentials);
        assert_eq!(
            credential_issue(&root.0, &credential(1)).unwrap(),
            credential(1)
        );
    }

    #[test]
    fn expiry_at_first_insert_rolls_back_the_schema_and_effect_identity() {
        let root = Temp::new();
        old_v2(&root.0);
        let record = credential(1);
        let mut connection = existing(&root.0).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let error =
            credential_decide_with_check(&tx, &record, || Err("injected expired authority".into()))
                .unwrap_err();
        assert!(error.contains("expired"));
        drop(tx);
        drop(connection);
        assert!(credential_query(&root.0, &record.operation_id)
            .unwrap()
            .is_none());
        assert!(!current(&root.0).unwrap().unwrap().operation_credentials);
        assert_eq!(credential_issue(&root.0, &record).unwrap(), record);

        let mut expired = credential(2);
        expired.expires_at = 1;
        assert!(credential_issue(&root.0, &expired).is_err());
        assert!(credential_query(&root.0, &expired.operation_id)
            .unwrap()
            .is_none());
        assert_eq!(
            credential_issue(&root.0, &credential(2)).unwrap(),
            credential(2)
        );
    }

    #[cfg(unix)]
    #[test]
    fn crash_credential_writer_child() {
        use std::io::Write;
        let Some(root) = std::env::var_os("WASM_GIT_CREDENTIAL_CRASH_ROOT") else {
            return;
        };
        let mut connection = existing(Path::new(&root)).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let index = std::env::var("WASM_GIT_CREDENTIAL_CRASH_INDEX")
            .ok()
            .map(|value| value.parse().unwrap())
            .unwrap_or(1);
        credential_decide(&tx, &credential(index)).unwrap();
        tx.cache_flush().unwrap();
        println!("credential-ledger-ready");
        std::io::stdout().flush().unwrap();
        let _ = std::io::stdin().read(&mut [0u8]);
    }

    #[cfg(unix)]
    fn kill_credential_writer(root: &Path, index: u64) {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::policy::ledger::tests::crash_credential_writer_child",
                "--nocapture",
            ])
            .env("WASM_GIT_CREDENTIAL_CRASH_ROOT", root)
            .env("WASM_GIT_CREDENTIAL_CRASH_INDEX", index.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if line.unwrap_or_default().contains("credential-ledger-ready") {
                    let _ = send.send(());
                    return;
                }
            }
        });
        if receive.recv_timeout(Duration::from_secs(10)).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            panic!("credential writer did not reach its transaction boundary");
        }
        child.kill().unwrap();
        child.wait().unwrap();
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn killed_first_credential_writer_rolls_back_schema_and_receipt() {
        let root = Temp::new();
        old_v2(&root.0);
        kill_credential_writer(&root.0, 1);
        let record = credential(1);
        assert!(credential_query(&root.0, &record.operation_id)
            .unwrap()
            .is_none());
        assert!(!credential_schema(&existing(&root.0).unwrap().unwrap()).unwrap());
        assert!(!current(&root.0).unwrap().unwrap().operation_credentials);
        assert_eq!(credential_issue(&root.0, &record).unwrap(), record);
    }

    #[cfg(unix)]
    #[test]
    fn killed_v3_upgrade_rolls_back_version_and_preserves_history() {
        let root = Temp::new();
        old_v2(&root.0);
        let original = credential(1);
        install_v3_credential(&root.0, &original);
        kill_credential_writer(&root.0, 2);
        let state = current(&root.0).unwrap().unwrap();
        assert!(state.operation_credentials);
        assert!(!state.chunk_receipts);
        assert_eq!(
            credential_query(&root.0, &original.operation_id).unwrap(),
            Some(original)
        );
        assert!(credential_query(&root.0, &credential(2).operation_id)
            .unwrap()
            .is_none());
        assert_eq!(
            credential_issue(&root.0, &credential(2)).unwrap(),
            credential(2)
        );
        assert!(current(&root.0).unwrap().unwrap().chunk_receipts);
    }

    #[test]
    fn indexed_receipts_continue_beyond_the_old_capacity() {
        let root = Temp::new();
        set(&root.0, &update(format!("{:064x}", 0), 0, policy())).unwrap();
        let mut connection = existing(&root.0).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        for index in 1..=4100 {
            let request = update(format!("{index:064x}"), 0, policy());
            assert_eq!(
                decide(&tx, &request).unwrap().outcome,
                PolicyUpdateOutcome::GenerationConflict
            );
        }
        tx.commit().unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT receipt FROM policy_receipts WHERE operation_id = ?1",
                [format!("{:064x}", 1)],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("PRIMARY KEY"), "{plan}");
        assert!(connection
            .execute(
                "DELETE FROM policy_receipts WHERE operation_id = ?1",
                [format!("{:064x}", 1)]
            )
            .is_err());
        drop(connection);
        let request = update(format!("{:064x}", 4101), 1, policy());
        assert_eq!(set(&root.0, &request).unwrap().generation, 2);
        let query = PolicyQueryRequest {
            version: 1,
            operation_id: format!("{:064x}", 1),
        };
        assert_eq!(
            self::query(&root.0, &query)
                .unwrap()
                .receipt
                .unwrap()
                .outcome,
            PolicyUpdateOutcome::GenerationConflict
        );
        assert_eq!(set(&root.0, &request).unwrap().generation, 2);
    }

    #[test]
    fn failed_staging_cleanup_keeps_the_original_conflict_receipt() {
        let root = Temp::new();
        let request = update("e".repeat(64), 7, policy());
        let error = create_with_hooks(
            &root.0,
            &request,
            |_| Err(io::Error::other("injected unlink failure")),
            crate::output::sync_directory,
        )
        .unwrap_err();
        assert!(error.contains("authorization ledger published"));
        assert!(error.contains("query the original operation"));
        let current = current(&root.0).unwrap().unwrap();
        assert_eq!(current.generation, 0);
        assert!(current.policy.is_none());
        let reply = query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: request.operation_id.clone(),
            },
        )
        .unwrap();
        let receipt = reply.receipt.unwrap();
        receipt.validate_update(&request).unwrap();
        assert_eq!(receipt.outcome, PolicyUpdateOutcome::GenerationConflict);
        assert_eq!(receipt.generation, 0);
    }

    #[test]
    fn failed_initial_publication_sync_requires_durable_retry() {
        let root = Temp::new();
        let request = update("c".repeat(64), 0, policy());
        let error = create_with_hooks(
            &root.0,
            &request,
            |path| fs::remove_file(path),
            |_| Err("injected publication directory sync failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("outcome unknown"));
        assert!(root.0.join(FILE).exists());

        let error = existing_with_sync(&root.0, |path, _| {
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .sync_all()
                .unwrap();
            Err("injected retry directory sync failure".into())
        })
        .err()
        .unwrap();
        assert!(error.contains("durability unknown"));
        assert!(error.contains("injected retry"));

        let reply = query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: request.operation_id.clone(),
            },
        )
        .unwrap();
        let receipt = reply.receipt.unwrap();
        receipt.validate_update(&request).unwrap();
        assert_eq!(receipt.outcome, PolicyUpdateOutcome::Applied);
        assert_eq!(receipt.generation, 1);
        assert_eq!(set(&root.0, &request).unwrap().generation, 1);
    }

    #[test]
    fn failed_migration_sync_requires_durable_retry() {
        let root = Temp::new();
        let source = include_bytes!("../../../tests/fixtures/policy-v1.bin");
        fs::write(root.0.join(FILE), source).unwrap();
        let legacy: super::super::LegacyStore = crate::binary::decode(source).unwrap();
        legacy.validate().unwrap();
        let migration = PolicyMigrationReceipt {
            version: 1,
            source_sha256: format!("{:x}", Sha256::digest(source)),
            source_version: 1,
            target_version: 2,
            generation: legacy.generation,
            policy_sha256: legacy
                .policy
                .as_ref()
                .map(ServerPolicy::digest)
                .transpose()
                .unwrap(),
            receipts: legacy.receipts.len() as u64,
        };
        let request = PolicyMigrationRequest {
            version: 1,
            source_sha256: migration.source_sha256.clone(),
        };
        let error = migrate_with_sync(
            &root.0,
            legacy.policy.as_ref(),
            legacy.generation,
            &legacy.receipts,
            &migration,
            |_| Err("injected migration directory sync failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("migration outcome unknown"));
        assert!(fs::read(root.0.join(FILE))
            .unwrap()
            .starts_with(SQLITE_HEADER));
        let error = existing_with_sync(&root.0, |path, _| {
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .sync_all()
                .unwrap();
            Err("injected retry directory sync failure".into())
        })
        .err()
        .unwrap();
        assert!(error.contains("durability unknown"));

        let recovered = migration_receipt(&root.0, &request).unwrap();
        recovered.validate_request(&request).unwrap();
        assert_eq!(recovered.generation, 1);
        assert_eq!(recovered.receipts, 2);
        assert_eq!(
            set(
                &root.0,
                &update("d".repeat(64), 1, policy_with_principals())
            )
            .unwrap()
            .generation,
            2
        );
        let mut new_credential = credential(1);
        new_credential.policy_generation = 2;
        credential_issue(&root.0, &new_credential).unwrap();
        assert!(current(&root.0).unwrap().unwrap().operation_credentials);
        let repeated = migration_receipt(&root.0, &request).unwrap();
        assert_eq!(repeated.generation, 1);
        assert_eq!(repeated.receipts, 2);
    }

    #[test]
    fn sqlite_full_rolls_back_policy_and_receipt_together() {
        use crate::protocol::PolicyPrincipal;
        let root = Temp::new();
        set(&root.0, &update("a".repeat(64), 0, policy())).unwrap();
        let mut connection = existing(&root.0).unwrap().unwrap();
        let pages: i64 = connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();
        let maximum: i64 = connection
            .query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(maximum, pages);
        let many = (0..4096)
            .map(|index| PolicyPrincipal {
                name: format!("user{index:04}{}", "x".repeat(90)),
                token_sha256: None,
                grants: Vec::new(),
            })
            .collect();
        let request = update(
            "b".repeat(64),
            1,
            ServerPolicy {
                version: crate::protocol::SERVER_POLICY_VERSION,
                principals: many,
            },
        );
        assert!(transaction(&mut connection, &request).is_err());
        drop(connection);
        assert_eq!(status(&root.0).unwrap().generation, 1);
        assert!(query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: request.operation_id.clone()
            }
        )
        .unwrap()
        .receipt
        .is_none());
        assert_eq!(set(&root.0, &request).unwrap().generation, 2);
    }

    #[cfg(unix)]
    #[test]
    fn crash_writer_child() {
        use std::io::Write;
        let Some(root) = std::env::var_os("WASM_GIT_LEDGER_CRASH_ROOT") else {
            return;
        };
        let mut connection = existing(Path::new(&root)).unwrap().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        decide(&tx, &update("f".repeat(64), 1, policy())).unwrap();
        tx.cache_flush().unwrap();
        println!("policy-ledger-ready");
        std::io::stdout().flush().unwrap();
        let _ = std::io::stdin().read(&mut [0u8]);
    }

    #[cfg(unix)]
    #[test]
    fn killed_writer_rolls_back_hot_journal_and_allows_original_retry() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let root = Temp::new();
        set(&root.0, &update("a".repeat(64), 0, policy())).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::policy::ledger::tests::crash_writer_child",
                "--nocapture",
            ])
            .env("WASM_GIT_LEDGER_CRASH_ROOT", &root.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if line.unwrap_or_default().contains("policy-ledger-ready") {
                    let _ = send.send(());
                    return;
                }
            }
        });
        if receive.recv_timeout(Duration::from_secs(10)).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            panic!("writer did not reach its transaction boundary");
        }
        child.kill().unwrap();
        child.wait().unwrap();
        reader.join().unwrap();
        assert_eq!(status(&root.0).unwrap().generation, 1);
        let request = update("f".repeat(64), 1, policy());
        assert!(query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: request.operation_id.clone()
            }
        )
        .unwrap()
        .receipt
        .is_none());
        assert_eq!(set(&root.0, &request).unwrap().generation, 2);
    }
}
