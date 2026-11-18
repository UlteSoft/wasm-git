//! Root-scoped authority. The stable gate protects both the current policy and
//! every served operation; an update takes the exclusive side as its fence.
use super::*;
use crate::protocol::{
    PolicyMigrationReceipt, PolicyMigrationRequest, PolicyQueryReply, PolicyQueryRequest,
    PolicyStatus, PolicyUpdateOutcome, PolicyUpdateReceipt, PolicyUpdateRequest, ServerPolicy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[path = "infrastructure/database/policy_ledger.rs"]
mod ledger;

const GATE_FILE: &str = ".wasm-git-authorization.lock";
const MAX_POLICY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LEGACY_RECEIPTS: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStore {
    version: u32,
    generation: u64,
    policy: Option<ServerPolicy>,
    receipts: BTreeMap<String, PolicyUpdateReceipt>,
}

impl LegacyStore {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.generation > protocol::NODE_SAFE_INTEGER_MAX
            || (self.generation == 0) != self.policy.is_none()
        {
            return Err("invalid authorization store version or generation".into());
        }
        if let Some(policy) = &self.policy {
            policy.validate()?;
        }
        if self.receipts.len() > MAX_LEGACY_RECEIPTS {
            return Err("policy receipt capacity exceeded".into());
        }
        let mut generations = std::collections::BTreeSet::new();
        for (id, receipt) in &self.receipts {
            receipt.validate()?;
            if id != &receipt.operation_id
                || receipt.generation > self.generation
                || (receipt.outcome == PolicyUpdateOutcome::Applied
                    && !generations.insert(receipt.generation))
                || (receipt.outcome == PolicyUpdateOutcome::Applied
                    && receipt.generation == self.generation
                    && receipt.policy_sha256
                        != self
                            .policy
                            .as_ref()
                            .ok_or("missing current policy")?
                            .digest()?)
            {
                return Err("invalid policy receipt ledger".into());
            }
        }
        if generations.len() as u64 != self.generation
            || generations
                .iter()
                .enumerate()
                .any(|(index, generation)| *generation != index as u64 + 1)
        {
            return Err("policy applied receipt history is incomplete".into());
        }
        Ok(())
    }
}

pub(super) struct Guard {
    _gate: fs::File,
    pub policy: Option<Arc<ServerPolicy>>,
    pub generation: u64,
    pub operation_credentials: bool,
    pub chunk_receipts: bool,
}

fn gate(root: &Path, exclusive: bool) -> Result<fs::File, String> {
    let path = root.join(GATE_FILE);
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|e| format!("open authorization gate: {e}"))?;
    real_path(&path, false)?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(format!("authorization gate busy: {e}")),
        }
    }
}

/// A bootstrap file is used only for an uninitialized root. Existing roots
/// always use their own durable store, never a stale process-local snapshot.
pub(super) fn bootstrap(root: &Path, input: &Path) -> Result<(), String> {
    let _gate = gate(root, true)?;
    let current = ledger::current(root)?;
    if current.as_ref().is_none_or(|store| store.policy.is_none()) {
        let policy = load_compiled(input)?;
        ledger::bootstrap_audited(root, policy, &admin_identity::current_operator()?)?;
    }
    Ok(())
}

pub(super) fn request_guard(root: &Path, policy_enabled: bool) -> Result<Guard, String> {
    let gate = gate(root, false)?;
    let store = ledger::current(root)?;
    let generation = store.as_ref().map_or(0, |store| store.generation);
    let operation_credentials = store
        .as_ref()
        .is_some_and(|store| store.operation_credentials);
    let chunk_receipts = store.as_ref().is_some_and(|store| store.chunk_receipts);
    let policy = match (policy_enabled, store) {
        (true, Some(store)) => Some(Arc::new(
            store
                .policy
                .ok_or("authorization store is not initialized")?,
        )),
        (true, None) => return Err("authorization store is not initialized".into()),
        (false, Some(_)) => return Err("this root requires its authorization policy".into()),
        (false, None) => None,
    };
    Ok(Guard {
        _gate: gate,
        policy,
        generation,
        operation_credentials,
        chunk_receipts,
    })
}

pub(super) fn set(
    root: &Path,
    request: &PolicyUpdateRequest,
) -> Result<PolicyUpdateReceipt, String> {
    request.validate()?;
    let _gate = gate(root, true)?;
    ledger::set_audited(root, request, &admin_identity::current_operator()?)
}

pub(super) fn status(root: &Path) -> Result<PolicyStatus, String> {
    let _gate = gate(root, false)?;
    ledger::status(root)
}

pub(super) fn query(root: &Path, request: &PolicyQueryRequest) -> Result<PolicyQueryReply, String> {
    let _gate = gate(root, false)?;
    ledger::query(root, request)
}

pub(super) fn issue_credential(
    root: &Path,
    request: &protocol::OperationCredentialRequest,
) -> Result<protocol::OperationCredentialReceipt, String> {
    let receipt = request.receipt()?;
    let _gate = gate(root, true)?;
    if matches!(
        &request.request.operation,
        Operation::ScopedUploadSlice { .. }
    ) {
        // Preserve historical replay without issuing a new way around the
        // original-operation chunk journal. Its execution is also disabled.
        return match ledger::credential_query(root, &receipt.operation_id)? {
            Some(prior) if prior == receipt => Ok(prior),
            _ => Err("strict uploads require an original-operation chunk request".into()),
        };
    }
    // The transaction checks expiry at first issuance's INSERT decision point;
    // historical replay returns the original receipt without renewing it.
    ledger::credential_issue_audited(root, request, &admin_identity::current_operator()?)
}

/// Caller owns the root authorization gate for the complete native attempt.
pub(super) fn audit_start(
    root: &Path,
    credential: &protocol::OperationCredentialReceipt,
    request: &Request,
    payload: &[u8],
) -> Result<u64, String> {
    ledger::audit_start_operation(
        root,
        credential,
        request,
        &format!("{:x}", Sha256::digest(payload)),
    )
}

pub(super) fn audit_observe(
    root: &Path,
    target: &protocol::AuditTarget,
    observation: &protocol::AuditObservation,
) -> Result<(), String> {
    ledger::audit_observe(root, target, observation)
}

pub(super) fn recovery_begin(
    root: &Path,
    request: &protocol::OperationRecoveryRequest,
    operator: &protocol::AuditOperator,
) -> Result<(fs::File, u64), String> {
    request.validate()?;
    let guard = gate(root, true)?;
    let original = ledger::credential_query(root, &request.original.operation_id)?
        .ok_or("original credential issuance is unavailable")?;
    original.validate_request(&request.original)?;
    let attempt = ledger::audit_begin_recovery(
        root,
        &request.recovery_id,
        &request.original,
        request.action,
        operator,
    )?;
    Ok((guard, attempt))
}

pub(super) fn audit_query(
    root: &Path,
    request: &protocol::AuditQueryRequest,
) -> Result<protocol::AuditQueryReply, String> {
    let _gate = gate(root, false)?;
    ledger::audit_query(root, request)
}

pub(super) fn audit_reconcile_many(
    root: &Path,
    recovery_id: &str,
    attempt_id: u64,
    targets: &[protocol::AuditTarget],
    observation: &protocol::AuditObservation,
) -> Result<(), String> {
    ledger::audit_reconcile_many(root, recovery_id, attempt_id, targets, observation)
}

pub(super) fn operation_context(
    root: &Path,
    operation_id: &str,
) -> Result<Option<protocol::OperationCredentialRequest>, String> {
    let _gate = gate(root, false)?;
    ledger::audit_operation_context(root, operation_id)
}

/// The caller retains the root's shared guard. Original issuance is identity
/// evidence, not proof of execution, and is readable after expiry/revocation.
pub(super) fn validate_original_chunk(
    root: &Path,
    chunk: &protocol::ScopedChunkIdentity,
) -> Result<(), String> {
    let credential = ledger::credential_query(root, &chunk.operation_id).map_err(|cause| {
        crate::cli_error(&format!("original chunk authority: {cause}"));
        "original chunk authority unavailable".to_owned()
    })?;
    if credential
        .as_ref()
        .is_some_and(|credential| chunk.validate_credential(credential).is_ok())
    {
        Ok(())
    } else {
        Err("original chunk is unavailable to this actor or scope".into())
    }
}

pub(super) fn query_credential(
    root: &Path,
    request: &protocol::OperationCredentialQuery,
) -> Result<protocol::OperationCredentialQueryReply, String> {
    request.validate()?;
    let _gate = gate(root, false)?;
    Ok(protocol::OperationCredentialQueryReply {
        version: 1,
        operation_id: request.operation_id.clone(),
        receipt: ledger::credential_query(root, &request.operation_id)?,
    })
}

/// The root guard is held by the transport throughout authentication, use and
/// output. A credential narrows the current principal policy; it never grants
/// rights by itself or falls back to a principal token after a failed match.
pub(super) type Authentication = (String, Option<protocol::OperationCredentialReceipt>);

pub(super) fn authenticate(
    root: &Path,
    policy: &ServerPolicy,
    generation: u64,
    token: &[u8],
    strict: bool,
) -> Result<Option<Authentication>, String> {
    // An absent/malformed Bearer value cannot become an identity even if a
    // trusted issuer accidentally stores the digest of such a value.
    if token.is_empty() || token.len() > 4096 || token.iter().any(|byte| !(33..=126).contains(byte))
    {
        return Ok(None);
    }
    let digest = format!("{:x}", Sha256::digest(token));
    if let Some(receipt) = ledger::credential_by_token(root, &digest)? {
        if receipt.policy_generation != generation
            || !policy.principal_exists(&receipt.actor)
            || now()? >= receipt.expires_at
        {
            return Ok(None);
        }
        return Ok(Some((receipt.actor.clone(), Some(receipt))));
    }
    if strict {
        return Ok(None);
    }
    Ok(policy.authenticate(token).map(|actor| (actor, None)))
}

/// Explicit maintenance-window migration. Old participants that do not honor
/// this gate must be stopped before invoking it.
pub(super) fn migrate(
    root: &Path,
    request: &PolicyMigrationRequest,
) -> Result<PolicyMigrationReceipt, String> {
    request.validate()?;
    let _gate = gate(root, true)?;
    let path = root.join(".wasm-git-authorization.bin");
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect policy migration source: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("policy migration source must be a regular file".into());
    }
    let mut file =
        fs::File::open(&path).map_err(|error| format!("open policy migration source: {error}"))?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header[..8])
        .map_err(|error| format!("read policy migration source: {error}"))?;
    if &header[..8] != b"WGTM\0\0\0\x01" {
        file.read_exact(&mut header[8..])
            .map_err(|error| format!("read policy migration source: {error}"))?;
        if &header == b"SQLite format 3\0" {
            return ledger::migration_receipt(root, request);
        }
        return Err("unsupported legacy policy migration source".into());
    }
    if metadata.len() > MAX_POLICY_BYTES {
        return Err("unsupported legacy policy migration source".into());
    }
    drop(file);
    let bytes = fs::read(&path).map_err(|error| format!("read legacy policy: {error}"))?;
    if format!("{:x}", Sha256::digest(&bytes)) != request.source_sha256 {
        return Err("legacy policy source digest mismatch".into());
    }
    let legacy: LegacyStore = crate::binary::decode(&bytes)?;
    legacy.validate()?;
    let receipt = PolicyMigrationReceipt {
        version: 1,
        source_sha256: request.source_sha256.clone(),
        source_version: 1,
        target_version: 2,
        generation: legacy.generation,
        policy_sha256: legacy
            .policy
            .as_ref()
            .map(ServerPolicy::digest)
            .transpose()?,
        receipts: legacy.receipts.len() as u64,
    };
    receipt.validate_request(request)?;
    ledger::migrate(
        root,
        legacy.policy.as_ref(),
        legacy.generation,
        &legacy.receipts,
        &receipt,
    )?;
    Ok(receipt)
}

fn load_compiled(path: &Path) -> Result<ServerPolicy, String> {
    let path = real_path(path, false)?;
    if fs::metadata(&path).map_err(|e| e.to_string())?.len() > MAX_POLICY_BYTES {
        return Err("policy exceeds 16 MiB".into());
    }
    let policy: ServerPolicy = crate::binary::decode(&fs::read(path).map_err(|e| e.to_string())?)?;
    policy.validate()?;
    Ok(policy)
}

fn matches(pattern: &str, value: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == value, |prefix| value.starts_with(prefix))
}

fn now() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_secs())
        .map_err(|_| "authorization clock unavailable".into())
}

/// None is unbounded; an absent matching grant is represented by Result, not
/// by None. Alternatives take the longest bound and required permissions the
/// shortest, so a batch compares one combined limit against one clock reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AuthorizationLimit(Option<u64>);

impl AuthorizationLimit {
    pub(super) fn unbounded() -> Self {
        Self(None)
    }
    pub(super) fn until(expiry: u64) -> Self {
        Self(Some(expiry))
    }
    pub(super) fn intersect(self, other: Self) -> Self {
        Self(match (self.0, other.0) {
            (None, value) | (value, None) => value,
            (Some(left), Some(right)) => Some(left.min(right)),
        })
    }

    fn alternative(self, other: Self) -> Self {
        Self(match (self.0, other.0) {
            (None, _) | (_, None) => None,
            (Some(left), Some(right)) => Some(left.max(right)),
        })
    }

    pub(super) fn require_current(self) -> Result<(), String> {
        let current = now()?;
        if self.0.is_none_or(|expiry| current < expiry) {
            Ok(())
        } else {
            Err("authorization denied for this repository or reference".into())
        }
    }
}

impl ServerPolicy {
    pub(super) fn authenticate(&self, token: &[u8]) -> Option<String> {
        let hash = format!("{:x}", Sha256::digest(token));
        let mut name = None;
        for principal in &self.principals {
            if principal
                .token_sha256
                .as_ref()
                .is_some_and(|expected| equal_secret(hash.as_bytes(), expected.as_bytes()))
            {
                name = Some(principal.name.clone());
            }
        }
        name
    }
    pub(super) fn principal_exists(&self, name: &str) -> bool {
        self.principals.iter().any(|p| p.name == name)
    }
    pub(super) fn retention_limit(
        &self,
        actor: &str,
        repo: &str,
    ) -> Result<AuthorizationLimit, String> {
        self.principals
            .iter()
            .filter(|p| p.name == actor)
            .flat_map(|p| &p.grants)
            .filter(|g| matches(&g.repository, repo) && g.read && g.release_retention)
            .map(|g| AuthorizationLimit(g.expires_at))
            .reduce(AuthorizationLimit::alternative)
            .ok_or_else(|| "release retention authorization denied".into())
    }
    pub(super) fn authorization_limit(
        &self,
        actor: &str,
        repo: &str,
        reference: Option<&str>,
        force: bool,
        create: bool,
    ) -> Result<AuthorizationLimit, String> {
        let mut limit: Option<AuthorizationLimit> = None;
        for grant in self
            .principals
            .iter()
            .filter(|p| p.name == actor)
            .flat_map(|p| &p.grants)
        {
            let allowed = matches(&grant.repository, repo)
                && if create {
                    grant.create
                } else {
                    grant.read
                        && (!force || grant.force)
                        && reference.is_none_or(|r| {
                            if r == "*" {
                                !grant.write.is_empty()
                            } else {
                                grant.write.iter().any(|p| matches(p, r))
                            }
                        })
                };
            if allowed {
                let candidate = AuthorizationLimit(grant.expires_at);
                limit = Some(limit.map_or(candidate, |prior| prior.alternative(candidate)));
            }
        }
        limit.ok_or_else(|| "authorization denied for this repository or reference".into())
    }

    pub(super) fn authorize(
        &self,
        actor: &str,
        repo: &str,
        reference: Option<&str>,
        force: bool,
        create: bool,
    ) -> Result<(), String> {
        self.authorization_limit(actor, repo, reference, force, create)?
            .require_current()
    }
}

pub(super) fn compile(input: &Path, output: &Path) -> Result<String, String> {
    let input = real_path(input, false)?;
    if fs::metadata(&input).map_err(|e| e.to_string())?.len() > MAX_POLICY_BYTES {
        return Err("policy input exceeds 16 MiB".into());
    }
    let policy: ServerPolicy = serde_json::from_slice(&fs::read(input).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    policy.validate()?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output).map_err(|e| e.to_string())?;
    file.write_all(&crate::binary::encode(&policy)?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(format!("compiled {} principals", policy.principals.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn missing_or_malformed_bearer_never_queries_a_credential() {
        let policy = ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: Vec::new(),
        };
        for token in [
            Vec::new(),
            b" ".to_vec(),
            b"token with spaces".to_vec(),
            vec![b'x'; 4097],
            vec![0xff],
        ] {
            assert!(
                authenticate(Path::new("absent-root"), &policy, 1, &token, true)
                    .unwrap()
                    .is_none()
            );
        }
    }
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "wasm-git-policy-migration-{}-{}",
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

    #[test]
    fn missing_applied_receipts_are_rejected() {
        let policy = ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: Vec::new(),
        };
        let digest = policy.digest().unwrap();
        let receipt = PolicyUpdateReceipt {
            version: 1,
            operation_id: "2".repeat(64),
            expected_generation: 1,
            outcome: PolicyUpdateOutcome::Applied,
            generation: 2,
            policy_sha256: digest,
        };
        let store = LegacyStore {
            version: 1,
            generation: 2,
            policy: Some(policy),
            receipts: BTreeMap::from([(receipt.operation_id.clone(), receipt)]),
        };
        assert!(store
            .validate()
            .unwrap_err()
            .contains("history is incomplete"));
    }

    #[test]
    fn alternative_grants_and_required_permissions_combine_expiry_once() {
        use crate::protocol::{PolicyGrant, PolicyPrincipal};
        let grant = |expiry, reference: &str| PolicyGrant {
            repository: "project".into(),
            read: true,
            write: vec![reference.into()],
            force: false,
            create: false,
            release_retention: false,
            expires_at: expiry,
        };
        let policy = ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: vec![PolicyPrincipal {
                name: "alice".into(),
                token_sha256: None,
                grants: vec![
                    grant(Some(10), "refs/heads/a"),
                    grant(Some(20), "refs/heads/a"),
                    grant(Some(15), "refs/heads/b"),
                ],
            }],
        };
        let a = policy
            .authorization_limit("alice", "project", Some("refs/heads/a"), false, false)
            .unwrap();
        let b = policy
            .authorization_limit("alice", "project", Some("refs/heads/b"), false, false)
            .unwrap();
        assert_eq!(a, AuthorizationLimit(Some(20)));
        assert_eq!(a.intersect(b), AuthorizationLimit(Some(15)));
        assert_eq!(AuthorizationLimit(None).intersect(a), a);
        assert!(a.require_current().is_err());
        assert!(policy
            .authorization_limit("alice", "project", Some("refs/heads/c"), false, false)
            .is_err());
    }

    #[test]
    fn migration_preserves_every_receipt_and_replays_after_later_updates() {
        let root = Temp::new();
        let policy = ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: Vec::new(),
        };
        let digest = policy.digest().unwrap();
        let applied = PolicyUpdateReceipt {
            version: 1,
            operation_id: "a".repeat(64),
            expected_generation: 0,
            outcome: PolicyUpdateOutcome::Applied,
            generation: 1,
            policy_sha256: digest.clone(),
        };
        let conflict = PolicyUpdateReceipt {
            version: 1,
            operation_id: "b".repeat(64),
            expected_generation: 0,
            outcome: PolicyUpdateOutcome::GenerationConflict,
            generation: 1,
            policy_sha256: digest,
        };
        let legacy = LegacyStore {
            version: 1,
            generation: 1,
            policy: Some(policy.clone()),
            receipts: BTreeMap::from([
                (applied.operation_id.clone(), applied.clone()),
                (conflict.operation_id.clone(), conflict.clone()),
            ]),
        };
        let bytes = crate::binary::encode(&legacy).unwrap();
        let path = root.0.join(".wasm-git-authorization.bin");
        fs::write(&path, &bytes).unwrap();
        assert!(status(&root.0)
            .unwrap_err()
            .contains("maintenance-window migration"));
        let mut request = PolicyMigrationRequest {
            version: 1,
            source_sha256: "0".repeat(64),
        };
        assert!(migrate(&root.0, &request).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        request.source_sha256 = format!("{:x}", Sha256::digest(&bytes));
        let migrated = migrate(&root.0, &request).unwrap();
        assert_eq!(migrated.generation, 1);
        assert_eq!(migrated.receipts, 2);
        let actual = query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: conflict.operation_id.clone(),
            },
        )
        .unwrap()
        .receipt
        .unwrap();
        assert_eq!(
            crate::binary::encode(&actual).unwrap(),
            crate::binary::encode(&conflict).unwrap()
        );
        set(
            &root.0,
            &PolicyUpdateRequest {
                version: 1,
                operation_id: "c".repeat(64),
                expected_generation: 1,
                policy,
            },
        )
        .unwrap();
        assert_eq!(status(&root.0).unwrap().generation, 2);
        let replay = migrate(&root.0, &request).unwrap();
        assert_eq!(
            crate::binary::encode(&replay).unwrap(),
            crate::binary::encode(&migrated).unwrap()
        );
        request.source_sha256 = "f".repeat(64);
        assert!(migrate(&root.0, &request).is_err());
    }

    #[test]
    fn migration_preserves_conflicts_before_first_policy() {
        let root = Temp::new();
        let policy = ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: Vec::new(),
        };
        let conflict = PolicyUpdateReceipt {
            version: 1,
            operation_id: "d".repeat(64),
            expected_generation: 1,
            outcome: PolicyUpdateOutcome::GenerationConflict,
            generation: 0,
            policy_sha256: policy.digest().unwrap(),
        };
        let legacy = LegacyStore {
            version: 1,
            generation: 0,
            policy: None,
            receipts: BTreeMap::from([(conflict.operation_id.clone(), conflict.clone())]),
        };
        let bytes = crate::binary::encode(&legacy).unwrap();
        fs::write(root.0.join(".wasm-git-authorization.bin"), &bytes).unwrap();
        let request = PolicyMigrationRequest {
            version: 1,
            source_sha256: format!("{:x}", Sha256::digest(&bytes)),
        };
        let migrated = migrate(&root.0, &request).unwrap();
        assert_eq!(migrated.generation, 0);
        assert_eq!(migrated.policy_sha256, None);
        assert!(status(&root.0).is_err());
        assert!(query(
            &root.0,
            &PolicyQueryRequest {
                version: 1,
                operation_id: conflict.operation_id.clone()
            }
        )
        .unwrap()
        .receipt
        .is_some());
        set(
            &root.0,
            &PolicyUpdateRequest {
                version: 1,
                operation_id: "e".repeat(64),
                expected_generation: 0,
                policy,
            },
        )
        .unwrap();
        assert_eq!(migrate(&root.0, &request).unwrap().generation, 0);
    }

    #[test]
    fn migration_refuses_incomplete_history_without_replacing_source() {
        let root = Temp::new();
        let legacy = LegacyStore {
            version: 1,
            generation: 1,
            policy: Some(ServerPolicy {
                version: protocol::SERVER_POLICY_VERSION,
                principals: Vec::new(),
            }),
            receipts: BTreeMap::new(),
        };
        let bytes = crate::binary::encode(&legacy).unwrap();
        let path = root.0.join(".wasm-git-authorization.bin");
        fs::write(&path, &bytes).unwrap();
        let request = PolicyMigrationRequest {
            version: 1,
            source_sha256: format!("{:x}", Sha256::digest(&bytes)),
        };
        assert!(migrate(&root.0, &request)
            .unwrap_err()
            .contains("history is incomplete"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
