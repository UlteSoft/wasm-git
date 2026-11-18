//! A local, OS-authenticated maintenance boundary. No upload payload, retained
//! file read or new publish decision is accepted through this entry point.
use super::*;
use protocol::{
    AuditEvidenceObservation, AuditObservation, AuditObservationScope, AuditTarget,
    OperationRecoveryReply, OperationRecoveryRequest, RecoveryAuditAction, RecoveryState,
    RecoveryUploadState, RetentionCommand, RetentionReply, RetentionState, ScopedChunkIdentity,
    ScopedChunkStatus, ScopedUploadState, ScopedUploadStatus,
};
use sha2::{Digest, Sha256};

pub(super) fn observation_scope(operation: &Operation) -> Option<AuditObservationScope> {
    match operation {
        Operation::UploadChunk { .. } | Operation::QueryChunk { .. } => {
            Some(AuditObservationScope::Chunk)
        }
        Operation::ScopedUploadSlice { .. }
        | Operation::ApplyScopedUpload { .. }
        | Operation::CancelScopedUpload { .. }
        | Operation::QueryScopedUpload { .. } => Some(AuditObservationScope::UploadSession),
        Operation::ReleaseRetention { .. } => Some(AuditObservationScope::Retention),
        _ => None,
    }
}

fn observed<T: serde::Serialize>(value: &T) -> Result<AuditEvidenceObservation, String> {
    Ok(AuditEvidenceObservation::Observed {
        sha256: format!("{:x}", Sha256::digest(crate::binary::encode(value)?)),
    })
}

fn chunk_evidence(
    status: &ScopedChunkStatus,
    chunk: &ScopedChunkIdentity,
) -> Result<AuditEvidenceObservation, String> {
    status.validate_for(chunk)?;
    if matches!(status.state, protocol::ScopedChunkState::Unobserved) {
        Ok(AuditEvidenceObservation::Unobserved)
    } else {
        observed(status)
    }
}

fn upload_evidence(
    status: &ScopedUploadStatus,
    repository: &str,
    session: &str,
    binding: &protocol::UploadBinding,
) -> Result<AuditEvidenceObservation, String> {
    status.validate_for(repository, session, binding)?;
    match &status.state {
        None => Ok(AuditEvidenceObservation::Unobserved),
        Some(
            ScopedUploadState::Applying
            | ScopedUploadState::ApplyingV4 { .. }
            | ScopedUploadState::Finished { .. },
        ) => Ok(AuditEvidenceObservation::Unknown),
        _ => observed(status),
    }
}

fn retention_evidence(
    reply: &RetentionReply,
    repository: &str,
    actor: &str,
    original: &RetentionCommand,
) -> Result<AuditEvidenceObservation, String> {
    let operation = match original {
        RetentionCommand::Prepare { operation, .. }
        | RetentionCommand::Promote { operation }
        | RetentionCommand::Close { operation } => operation,
        _ => return Err("audit scope is not a retention mutation".into()),
    };
    let query = RetentionCommand::Query {
        operation: operation.clone(),
    };
    reply.validate_for(repository, &query)?;
    match reply {
        RetentionReply::State { record: None, .. } => Ok(AuditEvidenceObservation::Unobserved),
        RetentionReply::State {
            record: Some(record),
            ..
        } => {
            if record.actor != actor || record.repository.as_deref() != Some(repository) {
                return Err("retention observation cannot prove the original scope".into());
            }
            if let RetentionCommand::Prepare { commit, paths, .. } = original {
                if record.commit.is_some()
                    && (record.commit.as_ref() != Some(commit)
                        || record
                            .files
                            .keys()
                            .collect::<std::collections::BTreeSet<_>>()
                            != paths.iter().collect())
                {
                    return Err(
                        "original retention preparation does not match the durable scope".into(),
                    );
                }
            }
            observed(reply)
        }
        _ => Err("unexpected retention observation".into()),
    }
}

pub(super) fn observe_native(
    repository: &Repository,
    gate: &crate::repository::ServiceGuard,
    actor: &str,
    request: &Request,
) -> Result<AuditEvidenceObservation, String> {
    match &request.operation {
        Operation::UploadChunk { chunk } => {
            let status = repository
                .scoped_chunk_query(gate, chunk)
                .map_err(|e| e.message)?;
            chunk_evidence(&status, chunk)
        }
        Operation::ScopedUploadSlice {
            session, binding, ..
        }
        | Operation::ApplyScopedUpload { session, binding }
        | Operation::CancelScopedUpload { session, binding } => {
            let status = repository.scoped_upload_observe(
                gate,
                &request.repository,
                actor,
                session,
                binding,
            )?;
            upload_evidence(&status, &request.repository, session, binding)
        }
        Operation::ReleaseRetention { command } => {
            let operation = match command {
                RetentionCommand::Prepare { operation, .. }
                | RetentionCommand::Promote { operation }
                | RetentionCommand::Close { operation } => operation,
                _ => return Err("audit scope is not a retention mutation".into()),
            };
            let query = RetentionCommand::Query {
                operation: operation.clone(),
            };
            let reply = repository
                .release_retention_checked(&request.repository, actor, query, || Ok(()))
                .map_err(|e| e.message)?;
            retention_evidence(&reply, &request.repository, actor, command)
        }
        _ => Err("unsupported original operation observation".into()),
    }
}

pub(super) fn run(
    root: &Path,
    request: &OperationRecoveryRequest,
) -> Result<OperationRecoveryReply, String> {
    request.validate()?;
    let operator = admin_identity::authorize_recovery_root(root)?;
    // Exclusive root fencing drains earlier HTTP operations. The immutable
    // original credential supplies identity, never renewed action authority.
    let (_gate, attempt_id) = policy::recovery_begin(root, request, &operator)?;
    let scope = observation_scope(&request.original.request.operation)
        .ok_or("unsupported recovery scope")?;
    let target = AuditTarget::Recovery {
        recovery_id: request.recovery_id.clone(),
        attempt_id,
    };
    let result = native_state(root, request).and_then(|(state, evidence)| {
        let reply = OperationRecoveryReply {
            version: 1,
            recovery_id: request.recovery_id.clone(),
            attempt_id,
            original_operation_id: request.original.operation_id.clone(),
            actor: request.original.actor.clone(),
            repository: request.original.request.repository.clone(),
            operator,
            action: request.action,
            state,
        };
        reply.validate_for(request)?;
        Ok((reply, evidence))
    });
    let evidence = result
        .as_ref()
        .map_or(AuditEvidenceObservation::Unknown, |(_, evidence)| {
            evidence.clone()
        });
    let observation = AuditObservation {
        version: 1,
        scope,
        evidence,
    };
    if let Err(cause) = policy::audit_observe(root, &target, &observation) {
        return Err(format!(
            "recovery observation is unconfirmed; query the same original operation: {cause}"
        ));
    }
    let (reply, _) = result.map_err(|cause| {
        format!("recovery outcome is unknown; query the original operation: {cause}")
    })?;
    if !request.reconcile.is_empty() {
        policy::audit_reconcile_many(root, &request.recovery_id, attempt_id, &request.reconcile, &observation)
            .map_err(|cause| format!("recovery was observed but audit reconciliation is unconfirmed; query the audit index: {cause}"))?;
    }
    Ok(reply)
}

fn native_state(
    root: &Path,
    request: &OperationRecoveryRequest,
) -> Result<(RecoveryState, AuditEvidenceObservation), String> {
    let original = &request.original;
    let name = &original.request.repository;
    let actor = &original.actor;
    let (scope, chunk) = match &original.request.operation {
        Operation::UploadChunk { chunk } => (Some(chunk.scope.clone()), Some(chunk)),
        Operation::ScopedUploadSlice {
            session, binding, ..
        }
        | Operation::ApplyScopedUpload { session, binding }
        | Operation::CancelScopedUpload { session, binding } => (
            Some(crate::repository::ScopedExecution {
                actor: actor.clone(),
                repository: name.clone(),
                session: session.clone(),
                binding: binding.clone(),
            }),
            None,
        ),
        Operation::ReleaseRetention { .. } => (None, None),
        _ => return Err("unsupported recovery action".into()),
    };
    let path = repository_path_in(root, name)?;
    let repository = Repository::open_for_recovery(&path, scope.as_ref())?;
    let gate = repository.service_lock(TIMEOUT)?;
    if let Operation::ReleaseRetention { command } = &original.request.operation {
        return retention(root, &repository, name, actor, command, request.action);
    }
    let scope = scope.ok_or("unsupported recovery scope")?;
    let session = &scope.session;
    let binding = &scope.binding;
    let mut status = repository.scoped_upload_observe(&gate, name, actor, session, binding)?;
    if request.action == RecoveryAuditAction::Close {
        match &status.state {
            None | Some(ScopedUploadState::Open) => {
                repository
                    .scoped_upload_cancel_checked(name, actor, session, binding, || {
                        admin_identity::authorize_recovery_root(root).map(|_| ())
                    })
                    .map_err(|error| error.message)?;
                status = repository.scoped_upload_observe(&gate, name, actor, session, binding)?;
            }
            Some(ScopedUploadState::ApplyingV4 { .. }) => {
                repository
                    .scoped_upload_abort_checked(&gate, &scope, || {
                        admin_identity::authorize_recovery_root(root).map(|_| ())
                    })
                    .map_err(|error| error.message)?;
                status = repository.scoped_upload_observe(&gate, name, actor, session, binding)?;
            }
            Some(ScopedUploadState::Applying) => {
                return Err(
                    "original upload decision is unresolved; no terminal result can be asserted"
                        .into(),
                );
            }
            _ => (),
        }
    }
    // Cleanup uses only an already durable terminal fact. It cannot introduce
    // a new reference update, even when the original actor has been revoked.
    if matches!(
        &status.state,
        Some(
            ScopedUploadState::Canceled
                | ScopedUploadState::Aborted { .. }
                | ScopedUploadState::Finished { .. }
                | ScopedUploadState::FinishedV4 { .. }
        )
    ) {
        repository.scoped_upload_cleanup(session)?;
    }
    let chunk = chunk
        .map(|identity| {
            repository
                .scoped_chunk_query(&gate, identity)
                .map_err(|error| error.message)
        })
        .transpose()?;
    let evidence = match (&chunk, &original.request.operation) {
        (Some(status), Operation::UploadChunk { chunk }) => chunk_evidence(status, chunk)?,
        _ => upload_evidence(&status, name, session, binding)?,
    };
    let state = match status.state {
        None => RecoveryUploadState::Unobserved,
        Some(ScopedUploadState::Open) => RecoveryUploadState::Open,
        Some(ScopedUploadState::Canceled) => RecoveryUploadState::Canceled,
        Some(ScopedUploadState::Aborted { .. }) => RecoveryUploadState::Aborted,
        Some(ScopedUploadState::Applying | ScopedUploadState::ApplyingV4 { .. }) => {
            RecoveryUploadState::Unknown
        }
        Some(ScopedUploadState::Finished { .. }) => RecoveryUploadState::LegacyFinished,
        Some(ScopedUploadState::FinishedV4 { receipt }) => {
            receipt.validate()?;
            RecoveryUploadState::Finished { receipt }
        }
    };
    Ok((
        RecoveryState::Upload {
            session: session.clone(),
            state,
            chunk,
        },
        evidence,
    ))
}

fn retention(
    root: &Path,
    repository: &Repository,
    name: &str,
    actor: &str,
    original: &RetentionCommand,
    action: RecoveryAuditAction,
) -> Result<(RecoveryState, AuditEvidenceObservation), String> {
    let operation = match original {
        RetentionCommand::Prepare { operation, .. }
        | RetentionCommand::Promote { operation }
        | RetentionCommand::Close { operation } => operation,
        _ => return Err("retention recovery cannot read program bytes".into()),
    };
    let query = RetentionCommand::Query {
        operation: operation.clone(),
    };
    let mut reply = repository
        .release_retention_checked(name, actor, query.clone(), || Ok(()))
        .map_err(|e| e.message)?;
    reply.validate_for(name, &query)?;
    retention_evidence(&reply, name, actor, original)?;
    if action == RecoveryAuditAction::Close
        && !matches!(&reply, RetentionReply::State { record: Some(record), .. } if record.state == RetentionState::Promoted)
    {
        reply = repository
            .release_retention_checked(
                name,
                actor,
                RetentionCommand::Close {
                    operation: operation.clone(),
                },
                || admin_identity::authorize_recovery_root(root).map(|_| ()),
            )
            .map_err(|e| e.message)?;
    }
    let evidence = retention_evidence(&reply, name, actor, original)?;
    match reply {
        RetentionReply::State { record, .. } => Ok((
            RecoveryState::Retention {
                operation: operation.clone(),
                record,
            },
            evidence,
        )),
        _ => Err("unexpected retention recovery result".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "wasm-git-recovery-observation-{}-{}",
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

    #[test]
    fn recovery_chunk_observation_uses_the_same_native_status_as_rpc() {
        let temp = Temp::new();
        let repo = Repository::init(temp.0.join("project")).unwrap();
        let payload = b"frozen chunk";
        let binding = protocol::UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let chunk = ScopedChunkIdentity {
            version: 1,
            operation_id: "1".repeat(64),
            scope: crate::repository::ScopedExecution {
                actor: "alice".into(),
                repository: "project".into(),
                session: "2".repeat(64),
                binding,
            },
            offset: 0,
            payload_sha256: format!("{:x}", Sha256::digest(payload)),
        };
        let rpc = Request {
            version: 1,
            repository: "project".into(),
            operation: Operation::UploadChunk {
                chunk: chunk.clone(),
            },
        };
        let request = OperationRecoveryRequest {
            version: 1,
            recovery_id: "3".repeat(64),
            original: protocol::OperationCredentialRequest {
                version: 1,
                operation_id: chunk.operation_id.clone(),
                policy_generation: 1,
                actor: "alice".into(),
                token_sha256: "4".repeat(64),
                request: rpc.clone(),
                payload_sha256: chunk.payload_sha256.clone(),
                expires_at: 100,
            },
            action: RecoveryAuditAction::Inspect,
            reconcile: vec![],
        };
        let (_, evidence) = native_state(&temp.0, &request).unwrap();
        assert_eq!(evidence, AuditEvidenceObservation::Unobserved);

        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_chunk_write(&gate, &chunk, payload, || Ok(()))
            .unwrap();
        let rpc_evidence = observe_native(&repo, &gate, "alice", &rpc).unwrap();
        drop(gate);
        let (_, recovery_evidence) = native_state(&temp.0, &request).unwrap();
        assert!(matches!(
            recovery_evidence,
            AuditEvidenceObservation::Observed { .. }
        ));
        assert_eq!(recovery_evidence, rpc_evidence);
    }
}
