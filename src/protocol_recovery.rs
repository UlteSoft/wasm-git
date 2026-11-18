//! Restricted holder-side recovery. Issuance identifies an original mutation;
//! it never proves that mutation executed or renews its authorization.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationContextReply {
    pub version: u32,
    pub operation_id: String,
    #[serde(deserialize_with = "present_option")]
    pub original: Option<OperationCredentialRequest>,
}
impl OperationContextReply {
    pub fn validate_for(&self, request: &OperationCredentialQuery) -> Result<(), String> {
        request.validate()?;
        if self.version != 1 || self.operation_id != request.operation_id {
            return Err("operation context identity mismatch".into());
        }
        if let Some(original) = &self.original {
            original.validate()?;
            if original.operation_id != self.operation_id {
                return Err("original operation context mismatch".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityQueryRequest {
    pub version: u32,
    pub repository: String,
}
impl CapacityQueryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported capacity query version".into());
        }
        repository_name(&self.repository)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityQueryReply {
    pub version: u32,
    pub repository: String,
    pub capacity: RepositoryCapacity,
}
impl CapacityQueryReply {
    pub fn validate_for(&self, request: &CapacityQueryRequest) -> Result<(), String> {
        request.validate()?;
        if self.version != 1 || self.repository != request.repository {
            return Err("capacity result scope mismatch".into());
        }
        self.capacity.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecoveryRequest {
    pub version: u32,
    pub recovery_id: String,
    pub original: OperationCredentialRequest,
    pub action: RecoveryAuditAction,
    #[serde(default)]
    pub reconcile: Vec<AuditTarget>,
}

impl OperationRecoveryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported operation recovery version".into());
        }
        crate::binary::oid(&self.recovery_id)?;
        self.original.validate()?;
        if self.reconcile.len() > AUDIT_PAGE_MAX_ITEMS as usize {
            return Err("too many recovery audit targets".into());
        }
        for target in &self.reconcile {
            target.validate()?;
        }
        if self.original.receipt()?.effect_key.is_none() {
            return Err("recovery requires an original mutation credential".into());
        }
        match &self.original.request.operation {
            Operation::UploadChunk { chunk } => {
                chunk.validate_scope(&self.original.request.repository, &self.original.actor)?
            }
            Operation::ScopedUploadSlice {
                session, binding, ..
            } => {
                crate::binary::oid(session)?;
                binding.validate()?;
            }
            Operation::ApplyScopedUpload { session, binding }
            | Operation::CancelScopedUpload { session, binding } => {
                crate::binary::oid(session)?;
                binding.validate()?;
                self.require_empty_original_payload()?;
            }
            Operation::ReleaseRetention { command } => {
                if !matches!(
                    command,
                    RetentionCommand::Prepare { .. }
                        | RetentionCommand::Promote { .. }
                        | RetentionCommand::Close { .. }
                ) {
                    return Err("recovery does not authorize retained file reads".into());
                }
                RetentionRequest {
                    version: 1,
                    command: command.clone(),
                }
                .validate()?;
                self.require_empty_original_payload()?;
            }
            _ => return Err("operation has no restricted recovery contract".into()),
        }
        Ok(())
    }

    fn require_empty_original_payload(&self) -> Result<(), String> {
        if self.original.payload_sha256 != format!("{:x}", Sha256::digest([])) {
            return Err("original operation requires an empty payload".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryUploadState {
    /// No current scope decision. A delayed original request may still arrive.
    Unobserved,
    Open,
    /// An older applying format cannot establish either commit or termination.
    Unknown,
    Canceled,
    Aborted,
    Finished {
        receipt: Box<ScopedReceipt>,
    },
    /// Legacy terminal state without a verifiable scoped result.
    LegacyFinished,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryState {
    Upload {
        session: String,
        state: RecoveryUploadState,
        /// The original chunk's durable decision survives scope cancellation.
        #[serde(deserialize_with = "present_option")]
        chunk: Option<ScopedChunkStatus>,
    },
    Retention {
        operation: String,
        #[serde(deserialize_with = "present_option")]
        record: Option<RetentionRecord>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecoveryReply {
    pub version: u32,
    pub recovery_id: String,
    pub attempt_id: u64,
    pub original_operation_id: String,
    pub actor: String,
    pub repository: String,
    pub operator: AuditOperator,
    pub action: RecoveryAuditAction,
    pub state: RecoveryState,
}

impl OperationRecoveryReply {
    pub fn validate_for(&self, request: &OperationRecoveryRequest) -> Result<(), String> {
        request.validate()?;
        self.operator.validate()?;
        if self.version != 1
            || self.attempt_id == 0
            || self.attempt_id > NODE_SAFE_INTEGER_MAX
            || self.recovery_id != request.recovery_id
            || self.original_operation_id != request.original.operation_id
            || self.actor != request.original.actor
            || self.repository != request.original.request.repository
            || self.action != request.action
        {
            return Err("recovery result does not match original scope".into());
        }
        match (&self.state, &request.original.request.operation) {
            (
                RecoveryState::Upload {
                    session,
                    state,
                    chunk,
                },
                operation,
            ) => {
                let (expected, binding) = match operation {
                    Operation::UploadChunk { chunk } => {
                        (&chunk.scope.session, &chunk.scope.binding)
                    }
                    Operation::ScopedUploadSlice {
                        session, binding, ..
                    }
                    | Operation::ApplyScopedUpload { session, binding }
                    | Operation::CancelScopedUpload { session, binding } => (session, binding),
                    _ => return Err("recovery result has a different resource kind".into()),
                };
                if session != expected {
                    return Err("recovery upload session mismatch".into());
                }
                match (chunk, operation) {
                    (Some(status), Operation::UploadChunk { chunk }) => {
                        status.validate_for(chunk)?
                    }
                    (None, Operation::UploadChunk { .. }) | (Some(_), _) => {
                        return Err("recovery chunk decision mismatch".into())
                    }
                    _ => (),
                }
                if let RecoveryUploadState::Finished { receipt } = state {
                    receipt.validate()?;
                    if receipt.aborted
                        || receipt.scope.repository != self.repository
                        || receipt.scope.actor != self.actor
                        || receipt.scope.session != *session
                        || crate::binary::encode(&receipt.scope.binding)?
                            != crate::binary::encode(binding)?
                    {
                        return Err("recovered receipt does not match original scope".into());
                    }
                }
                if self.action == RecoveryAuditAction::Close
                    && matches!(
                        state,
                        RecoveryUploadState::Unobserved
                            | RecoveryUploadState::Open
                            | RecoveryUploadState::Unknown
                    )
                {
                    return Err("recovery close did not establish a terminal scope".into());
                }
            }
            (
                RecoveryState::Retention { operation, record },
                Operation::ReleaseRetention { command },
            ) => {
                let expected = match command {
                    RetentionCommand::Prepare { operation, .. }
                    | RetentionCommand::Promote { operation }
                    | RetentionCommand::Close { operation } => operation,
                    _ => return Err("unsupported original retention action".into()),
                };
                if operation != expected {
                    return Err("recovery retention operation mismatch".into());
                }
                if let Some(record) = record {
                    record.validate()?;
                    if record.actor != self.actor
                        || record.repository.as_deref() != Some(self.repository.as_str())
                        || record.operation != *operation
                    {
                        return Err("recovery retention scope mismatch".into());
                    }
                    if let RetentionCommand::Prepare { commit, paths, .. } = command {
                        if record.commit.is_some()
                            && (record.commit.as_ref() != Some(commit)
                                || record
                                    .files
                                    .keys()
                                    .collect::<std::collections::BTreeSet<_>>()
                                    != paths.iter().collect())
                        {
                            return Err("recovery retention preparation mismatch".into());
                        }
                    }
                }
                if self.action == RecoveryAuditAction::Close
                    && !record.as_ref().is_some_and(|r| {
                        matches!(r.state, RetentionState::Closed | RetentionState::Promoted)
                    })
                {
                    return Err("recovery close did not establish a terminal retention".into());
                }
            }
            _ => return Err("recovery result has a different resource kind".into()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original(operation: Operation) -> OperationCredentialRequest {
        OperationCredentialRequest {
            version: 1,
            operation_id: "1".repeat(64),
            policy_generation: 1,
            actor: "alice".into(),
            token_sha256: "2".repeat(64),
            request: Request {
                version: 1,
                repository: "project".into(),
                operation,
            },
            payload_sha256: format!("{:x}", Sha256::digest([])),
            expires_at: 100,
        }
    }

    fn reply(request: &OperationRecoveryRequest, state: RecoveryState) -> OperationRecoveryReply {
        OperationRecoveryReply {
            version: 1,
            recovery_id: request.recovery_id.clone(),
            attempt_id: 1,
            original_operation_id: request.original.operation_id.clone(),
            actor: request.original.actor.clone(),
            repository: request.original.request.repository.clone(),
            operator: AuditOperator {
                version: 1,
                identity: "unix:uid:1".into(),
            },
            action: request.action,
            state,
        }
    }

    #[test]
    fn recovery_reply_checks_full_receipt_scope_and_retention_preparation() {
        let update = RefUpdate {
            namespace: RefNamespace::Branch,
            name: "topic".into(),
            expected: None,
            new: Some("a".repeat(64)),
            force: false,
        };
        let binding = UploadBinding::for_payload(
            Operation::UpdateRef {
                namespace: update.namespace.clone(),
                name: update.name.clone(),
                expected: update.expected.clone(),
                new: update.new.clone(),
                force: false,
            },
            b"bundle",
        )
        .unwrap();
        let session = "3".repeat(64);
        let request = OperationRecoveryRequest {
            version: 1,
            recovery_id: "4".repeat(64),
            original: original(Operation::ApplyScopedUpload {
                session: session.clone(),
                binding: binding.clone(),
            }),
            action: RecoveryAuditAction::Inspect,
            reconcile: vec![],
        };
        let scope = ScopedExecution {
            actor: "alice".into(),
            repository: "project".into(),
            session: session.clone(),
            binding,
        };
        let receipt = ScopedReceipt::success(&scope, &[update], &[None], false).unwrap();
        let mut result = reply(
            &request,
            RecoveryState::Upload {
                session,
                state: RecoveryUploadState::Finished {
                    receipt: Box::new(receipt),
                },
                chunk: None,
            },
        );
        result.validate_for(&request).unwrap();
        let mut wrong_actor = result.clone();
        if let RecoveryState::Upload {
            state: RecoveryUploadState::Finished { receipt },
            ..
        } = &mut wrong_actor.state
        {
            receipt.scope.actor = "bob".into();
        }
        assert!(wrong_actor.validate_for(&request).is_err());
        let mut wrong_dry_run = result.clone();
        if let RecoveryState::Upload {
            state: RecoveryUploadState::Finished { receipt },
            ..
        } = &mut wrong_dry_run.state
        {
            receipt.dry_run = true;
        }
        assert!(wrong_dry_run.validate_for(&request).is_err());
        if let RecoveryState::Upload {
            state: RecoveryUploadState::Finished { receipt },
            ..
        } = &mut result.state
        {
            receipt.updates[0].name = "outside".into();
        }
        assert!(result.validate_for(&request).is_err());

        let operation = "5".repeat(64);
        let commit = "6".repeat(64);
        let request = OperationRecoveryRequest {
            version: 1,
            recovery_id: "7".repeat(64),
            original: original(Operation::ReleaseRetention {
                command: RetentionCommand::Prepare {
                    operation: operation.clone(),
                    commit: commit.clone(),
                    paths: vec!["app.wasm".into()],
                },
            }),
            action: RecoveryAuditAction::Inspect,
            reconcile: vec![],
        };
        let record = RetentionRecord {
            version: RETENTION_VERSION,
            operation: operation.clone(),
            actor: "alice".into(),
            repository: Some("project".into()),
            state: RetentionState::Prepared,
            commit: Some(commit),
            files: std::collections::BTreeMap::from([(
                "app.wasm".into(),
                RetainedFile {
                    blob: "8".repeat(64),
                    sha256: "9".repeat(64),
                    size: 8,
                },
            )]),
        };
        let mut result = reply(
            &request,
            RecoveryState::Retention {
                operation,
                record: Some(record),
            },
        );
        result.validate_for(&request).unwrap();
        let mut unbound = result.clone();
        if let RecoveryState::Retention {
            record: Some(record),
            ..
        } = &mut unbound.state
        {
            record.repository = None;
        }
        assert!(unbound.validate_for(&request).is_err());
        if let RecoveryState::Retention {
            record: Some(record),
            ..
        } = &mut result.state
        {
            record.commit = Some("a".repeat(64));
        }
        assert!(result.validate_for(&request).is_err());
    }
}
