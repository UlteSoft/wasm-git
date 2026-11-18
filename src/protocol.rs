//! Versioned, bounded binary RPC used identically by HTTP and SSH.

use crate::repository::RepositoryState;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use std::io::{Cursor, Read, Write};

const MAGIC: &[u8; 8] = b"WGTRPC02";
pub const TRANSFER_CHUNK: usize = 1024 * 1024;
pub const SCOPED_UPLOAD_STATUS_VERSION: u32 = 2;
pub const RETENTION_VERSION: u32 = 2;
pub const SERVER_POLICY_VERSION: u32 = 3;
pub const NODE_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;
pub const MAX_BUNDLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_PACKET_BYTES: usize = 64 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 65_536;
const HEADER_BYTES: usize = 20;

#[path = "protocol_operation_credentials.rs"]
mod operation_credentials;
pub use operation_credentials::{
    OperationCredentialQuery, OperationCredentialQueryReply, OperationCredentialReceipt,
    OperationCredentialRequest,
};
#[path = "protocol_chunks.rs"]
mod chunks;
pub use chunks::{
    ScopedChunkCleanupReason, ScopedChunkData, ScopedChunkIdentity, ScopedChunkState,
    ScopedChunkStatus,
};
#[path = "protocol_capacity.rs"]
mod capacity;
pub use capacity::{AdmissionCapacity, RepositoryCapacity};
#[path = "protocol_recovery.rs"]
mod recovery;
pub use recovery::{
    CapacityQueryReply, CapacityQueryRequest, OperationContextReply, OperationRecoveryReply,
    OperationRecoveryRequest, RecoveryState, RecoveryUploadState,
};
#[path = "protocol_audit.rs"]
mod audit;
pub use audit::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub repository: String,
    pub operation: Operation,
}

pub fn repository_name(name: &str) -> Result<(), String> {
    if name.len() > 512 || name.split('/').count() > 16 {
        return Err("repository paths are limited to 512 bytes and 16 components".into());
    }
    for component in name.split('/') {
        repository_component(component)?;
    }
    Ok(())
}

pub fn repository_component(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("invalid server repository name".into());
    }
    Ok(())
}

/// Native administrator input shared by the policy store and integrations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerPolicy {
    pub version: u32,
    pub principals: Vec<PolicyPrincipal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyPrincipal {
    pub name: String,
    pub token_sha256: Option<String>,
    pub grants: Vec<PolicyGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyGrant {
    pub repository: String,
    pub read: bool,
    pub write: Vec<String>,
    pub force: bool,
    pub create: bool,
    #[serde(default)]
    pub release_retention: bool,
    /// Exclusive UTC Unix-second boundary. None means no time limit.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl ServerPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SERVER_POLICY_VERSION || self.principals.len() > 4096 {
            return Err("invalid policy version or principal count".into());
        }
        let mut names = std::collections::BTreeSet::new();
        let mut tokens = std::collections::BTreeSet::new();
        for principal in &self.principals {
            repository_component(&principal.name)?;
            if !names.insert(&principal.name) {
                return Err("duplicate policy principal".into());
            }
            if let Some(hash) = &principal.token_sha256 {
                crate::binary::oid(hash)?;
                if !tokens.insert(hash) {
                    return Err("duplicate policy credential".into());
                }
            }
            for grant in &principal.grants {
                if grant.expires_at.is_some_and(|t| t > NODE_SAFE_INTEGER_MAX) {
                    return Err("policy expiry exceeds the interoperable integer bound".into());
                }
                let prefix = grant
                    .repository
                    .strip_suffix('*')
                    .unwrap_or(&grant.repository);
                if grant.repository != "*" {
                    repository_name(prefix.strip_suffix('/').unwrap_or(prefix))?;
                }
                for reference in &grant.write {
                    let prefix = reference.strip_suffix('*').unwrap_or(reference);
                    if !(prefix.starts_with("refs/heads/") || prefix.starts_with("refs/tags/"))
                        || prefix.contains('*')
                        || prefix.contains("..")
                        || prefix.bytes().any(|b| b <= 32 || b >= 127)
                    {
                        return Err("policy write rules require refs/heads/ or refs/tags/ prefixes and an optional final '*'".into());
                    }
                }
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, String> {
        use sha2::Digest;
        self.validate()?;
        Ok(format!(
            "{:x}",
            sha2::Sha256::digest(crate::binary::encode(self)?)
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyUpdateRequest {
    pub version: u32,
    pub operation_id: String,
    pub expected_generation: u64,
    pub policy: ServerPolicy,
}
impl PolicyUpdateRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.expected_generation >= NODE_SAFE_INTEGER_MAX {
            return Err("invalid policy update version or generation".into());
        }
        crate::binary::oid(&self.operation_id)?;
        self.policy.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyUpdateOutcome {
    Applied,
    GenerationConflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyUpdateReceipt {
    pub version: u32,
    pub operation_id: String,
    pub expected_generation: u64,
    pub outcome: PolicyUpdateOutcome,
    /// Applied generation, or current generation observed by a conflict.
    pub generation: u64,
    pub policy_sha256: String,
}
impl PolicyUpdateReceipt {
    pub fn validate(&self) -> Result<(), String> {
        crate::binary::oid(&self.operation_id)?;
        crate::binary::oid(&self.policy_sha256)?;
        if self.version != 1
            || self.expected_generation >= NODE_SAFE_INTEGER_MAX
            || self.generation > NODE_SAFE_INTEGER_MAX
            || match self.outcome {
                PolicyUpdateOutcome::Applied => self.generation != self.expected_generation + 1,
                PolicyUpdateOutcome::GenerationConflict => {
                    self.generation == self.expected_generation
                }
            }
        {
            return Err("invalid policy update receipt".into());
        }
        Ok(())
    }
    pub fn validate_update(&self, request: &PolicyUpdateRequest) -> Result<(), String> {
        request.validate()?;
        self.validate()?;
        if self.operation_id != request.operation_id
            || self.expected_generation != request.expected_generation
            || self.policy_sha256 != request.policy.digest()?
        {
            return Err("policy update receipt does not match the requested operation".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyQueryRequest {
    pub version: u32,
    pub operation_id: String,
}
impl PolicyQueryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("invalid policy query version".into());
        }
        crate::binary::oid(&self.operation_id)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyQueryReply {
    pub version: u32,
    pub operation_id: String,
    #[serde(deserialize_with = "present_option")]
    pub receipt: Option<PolicyUpdateReceipt>,
}
impl PolicyQueryReply {
    pub fn validate_update(&self, request: &PolicyUpdateRequest) -> Result<(), String> {
        request.validate()?;
        if self.version != 1 || self.operation_id != request.operation_id {
            return Err("policy query operation mismatch".into());
        }
        if let Some(receipt) = &self.receipt {
            receipt.validate_update(request)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyStatus {
    pub version: u32,
    pub generation: u64,
    pub policy_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyMigrationRequest {
    pub version: u32,
    pub source_sha256: String,
}
impl PolicyMigrationRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("invalid policy migration request version".into());
        }
        crate::binary::oid(&self.source_sha256).map(|_| ())
    }
}

/// Historical receipt for an exact source image, not the current policy state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyMigrationReceipt {
    pub version: u32,
    pub source_sha256: String,
    pub source_version: u32,
    pub target_version: u32,
    pub generation: u64,
    #[serde(deserialize_with = "present_option")]
    pub policy_sha256: Option<String>,
    pub receipts: u64,
}
impl PolicyMigrationReceipt {
    pub fn validate(&self) -> Result<(), String> {
        crate::binary::oid(&self.source_sha256)?;
        if self.version != 1
            || self.source_version != 1
            || self.target_version != 2
            || self.generation > NODE_SAFE_INTEGER_MAX
            || self.receipts > NODE_SAFE_INTEGER_MAX
            || self.receipts < self.generation
            || (self.generation == 0) != self.policy_sha256.is_none()
        {
            return Err("invalid policy migration receipt".into());
        }
        if let Some(digest) = &self.policy_sha256 {
            crate::binary::oid(digest)?;
        }
        Ok(())
    }
    pub fn validate_request(&self, request: &PolicyMigrationRequest) -> Result<(), String> {
        request.validate()?;
        self.validate()?;
        if self.source_sha256 != request.source_sha256 {
            return Err("policy migration source mismatch".into());
        }
        Ok(())
    }
}
impl PolicyStatus {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.generation == 0 || self.generation > NODE_SAFE_INTEGER_MAX {
            return Err("invalid policy status version or generation".into());
        }
        crate::binary::oid(&self.policy_sha256)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefNamespace {
    Branch,
    Tag,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Operation {
    Advertise {},
    Fetch {},
    FetchSlice {
        offset: u64,
        expected: Option<String>,
    },
    UploadSlice {
        transfer: String,
        total: u64,
        offset: u64,
    },
    ApplyUpload {
        transfer: String,
        #[serde(with = "publication")]
        operation: Box<Operation>,
    },
    Fork {
        destination: String,
    },
    Push {
        branch: String,
        #[serde(deserialize_with = "present_option")]
        expected: Option<String>,
        force: bool,
    },
    UpdateRef {
        namespace: RefNamespace,
        name: String,
        #[serde(deserialize_with = "present_option")]
        expected: Option<String>,
        #[serde(deserialize_with = "present_option")]
        new: Option<String>,
        force: bool,
    },
    RefsPage {
        after: Option<String>,
        expected: Option<String>,
    },
    CancelUpload {
        transfer: String,
    },
    PublishBatch {},
    ObjectSlice {
        id: String,
        offset: u64,
    },
    PublishOptions {},
    CheckBatch {},
    PublishPartial {
        dry_run: bool,
    },
    PullRequest {},
    ReleaseRetention {
        command: RetentionCommand,
    },
    ScopedUploadSlice {
        session: String,
        binding: UploadBinding,
        offset: u64,
    },
    ApplyScopedUpload {
        session: String,
        binding: UploadBinding,
    },
    CancelScopedUpload {
        session: String,
        binding: UploadBinding,
    },
    QueryScopedUpload {
        session: String,
        binding: UploadBinding,
    },
    UploadChunk {
        chunk: ScopedChunkIdentity,
    },
    QueryChunk {
        chunk: ScopedChunkIdentity,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ScopedUploadState {
    Open,
    Applying,
    Finished {
        response: Response,
        payload: Vec<u8>,
    },
    Canceled,
    /// A v4 operation has not yet recorded its native decision.
    ApplyingV4 {
        repository: String,
    },
    /// The v4 operation was interrupted before any native decision.
    Aborted {
        repository: String,
    },
    /// The durable native decision, including the exact reference results.
    FinishedV4 {
        receipt: Box<ScopedReceipt>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedUploadStatus {
    pub version: u32,
    #[serde(deserialize_with = "present_option")]
    pub state: Option<ScopedUploadState>,
}

impl ScopedUploadStatus {
    pub fn validate_for(
        &self,
        repository: &str,
        session: &str,
        binding: &UploadBinding,
    ) -> Result<(), String> {
        if self.version != SCOPED_UPLOAD_STATUS_VERSION {
            return Err("unsupported scoped upload status".into());
        }
        repository_name(repository)?;
        crate::binary::oid(session)?;
        binding.validate()?;
        match &self.state {
            Some(
                ScopedUploadState::ApplyingV4 { repository: actual }
                | ScopedUploadState::Aborted { repository: actual },
            ) if actual != repository => Err("scoped upload repository mismatch".into()),
            Some(ScopedUploadState::Finished { response, payload })
                if response.version != 1 || payload.len() > MAX_PACKET_BYTES =>
            {
                Err("invalid legacy scoped upload receipt".into())
            }
            Some(ScopedUploadState::FinishedV4 { receipt }) => {
                receipt.validate()?;
                if receipt.aborted
                    || receipt.scope.repository != repository
                    || receipt.scope.session != session
                    || crate::binary::encode(&receipt.scope.binding)?
                        != crate::binary::encode(binding)?
                {
                    return Err("scoped upload receipt identity mismatch".into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedExecution {
    pub actor: String,
    pub repository: String,
    pub session: String,
    pub binding: UploadBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedReceipt {
    pub version: u32,
    pub scope: ScopedExecution,
    pub updates: Vec<RefUpdate>,
    pub rejections: Vec<Option<String>>,
    pub dry_run: bool,
    pub aborted: bool,
    pub response: Response,
    pub payload: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadBindingRequest {
    pub version: u32,
    #[serde(with = "publication")]
    pub operation: Box<Operation>,
}

impl UploadBindingRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported upload binding request version".into());
        }
        match self.operation.as_ref() {
            Operation::Push { .. } | Operation::UpdateRef { .. } => {
                let targets = upload_targets(&self.operation, &[])?;
                UploadBinding {
                    version: 1,
                    digest: "0".repeat(64),
                    total: 1,
                    operation: self.operation.clone(),
                    targets,
                }
                .validate()
            }
            Operation::PublishBatch {}
            | Operation::PublishOptions {}
            | Operation::CheckBatch {}
            | Operation::PublishPartial { .. } => Ok(()),
            _ => Err("invalid upload binding operation".into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRecovery {
    pub version: u32,
    pub repository_url: String,
    pub session: String,
    pub binding: UploadBinding,
    pub payload_file: String,
}

impl UploadRecovery {
    pub fn validate(&self, payload: &[u8]) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported upload recovery version".into());
        }
        crate::remote::validate_remote_url(&self.repository_url)?;
        crate::binary::oid(&self.session)?;
        if self.payload_file != format!("upload-{}.bin", self.session) {
            return Err("invalid upload recovery payload name".into());
        }
        self.binding.validate_payload(payload)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ScopedUploadCommand {
    Slice {
        session: String,
        binding: UploadBinding,
        offset: u64,
        bytes: Vec<u8>,
    },
    Apply {
        session: String,
        binding: UploadBinding,
    },
    Cancel {
        session: String,
        binding: UploadBinding,
    },
    Query {
        session: String,
        binding: UploadBinding,
    },
    Chunk {
        chunk: ScopedChunkIdentity,
        bytes: Vec<u8>,
    },
    QueryChunk {
        chunk: ScopedChunkIdentity,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedUploadRequest {
    pub version: u32,
    pub command: ScopedUploadCommand,
}

impl ScopedUploadRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported scoped upload request version".into());
        }
        let (session, binding) = match &self.command {
            ScopedUploadCommand::Chunk { chunk, bytes } => {
                chunk.validate_payload(bytes)?;
                (&chunk.scope.session, &chunk.scope.binding)
            }
            ScopedUploadCommand::QueryChunk { chunk } => {
                chunk.validate()?;
                (&chunk.scope.session, &chunk.scope.binding)
            }
            ScopedUploadCommand::Slice {
                session,
                binding,
                offset,
                bytes,
            } => {
                binding.validate_slice(*offset, bytes.len())?;
                (session, binding)
            }
            ScopedUploadCommand::Apply { session, binding }
            | ScopedUploadCommand::Cancel { session, binding }
            | ScopedUploadCommand::Query { session, binding } => (session, binding),
        };
        crate::binary::oid(session)?;
        binding.validate()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ScopedUploadReply {
    Acknowledged {
        version: u32,
        session: String,
    },
    Applied {
        version: u32,
        session: String,
        response: Response,
        payload: Vec<u8>,
    },
    Status {
        version: u32,
        session: String,
        status: ScopedUploadStatus,
    },
    Chunk {
        version: u32,
        status: ScopedChunkStatus,
    },
}

impl ScopedUploadReply {
    pub fn validate_for(
        &self,
        repository: &str,
        request: &ScopedUploadRequest,
    ) -> Result<(), String> {
        repository_name(repository)?;
        request.validate()?;
        match (&request.command, self) {
            (
                ScopedUploadCommand::Chunk { chunk, .. },
                ScopedUploadReply::Chunk { version: 1, status },
            ) => {
                chunk.validate_scope(repository, &chunk.scope.actor)?;
                status.validate_for(chunk)?;
                if !matches!(status.state, ScopedChunkState::Accepted { .. }) {
                    return Err("chunk acknowledgement has no accepted decision".into());
                }
                Ok(())
            }
            (
                ScopedUploadCommand::QueryChunk { chunk },
                ScopedUploadReply::Chunk { version: 1, status },
            ) => {
                chunk.validate_scope(repository, &chunk.scope.actor)?;
                status.validate_for(chunk)
            }
            (
                ScopedUploadCommand::Slice { session, .. }
                | ScopedUploadCommand::Cancel { session, .. },
                ScopedUploadReply::Acknowledged {
                    version: 1,
                    session: actual,
                },
            ) if session == actual => Ok(()),
            (
                ScopedUploadCommand::Query { session, binding },
                ScopedUploadReply::Status {
                    version: 1,
                    session: actual,
                    status,
                },
            ) if session == actual => status.validate_for(repository, session, binding),
            (
                ScopedUploadCommand::Apply { session, binding },
                ScopedUploadReply::Applied {
                    version: 1,
                    session: actual,
                    response,
                    payload,
                },
            ) if session == actual => {
                if response.version != 1
                    || !response.ok
                    || response.publication_unknown
                    || response.error.is_some()
                    || response.state.is_some()
                {
                    return Err("invalid scoped apply response".into());
                }
                match binding.operation.as_ref() {
                    Operation::Push { .. } => {
                        crate::binary::oid(response.tip.as_deref().ok_or("missing push tip")?)?;
                    }
                    Operation::UpdateRef { new, .. } if response.tip == *new => (),
                    Operation::PublishBatch {}
                    | Operation::PublishOptions {}
                    | Operation::CheckBatch {}
                    | Operation::PublishPartial { .. }
                        if response.tip.as_deref() == Some(binding.digest.as_str()) => {}
                    _ => return Err("scoped apply response does not match operation".into()),
                }
                if matches!(*binding.operation, Operation::PublishPartial { .. }) {
                    let receipt: PartialReceipt = crate::binary::decode(payload)?;
                    if receipt.rejections.len() != binding.targets.len() {
                        return Err("partial receipt count mismatch".into());
                    }
                } else if !payload.is_empty() {
                    return Err("unexpected scoped apply payload".into());
                }
                Ok(())
            }
            _ => Err("scoped upload response does not match request".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadTarget {
    pub reference: String,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadBinding {
    pub version: u32,
    pub digest: String,
    pub total: u64,
    #[serde(with = "publication")]
    pub operation: Box<Operation>,
    pub targets: Vec<UploadTarget>,
}

impl UploadBinding {
    pub fn validate_slice(&self, offset: u64, length: usize) -> Result<(), String> {
        self.validate()?;
        if !offset.is_multiple_of(TRANSFER_CHUNK as u64)
            || offset >= self.total
            || length as u64 != (self.total - offset).min(TRANSFER_CHUNK as u64)
        {
            return Err("invalid scoped upload slice bounds".into());
        }
        Ok(())
    }

    pub fn for_payload(operation: Operation, payload: &[u8]) -> Result<Self, String> {
        use sha2::{Digest, Sha256};
        let targets = upload_targets(&operation, payload)?;
        let result = Self {
            version: 1,
            digest: format!("{:x}", Sha256::digest(payload)),
            total: payload.len() as u64,
            operation: Box::new(operation),
            targets,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        crate::binary::oid(&self.digest)?;
        if self.version != 1
            || self.total == 0
            || self.total > MAX_BUNDLE_BYTES
            || self.targets.is_empty()
            || self.targets.len() > 4096
        {
            return Err("invalid scoped upload binding".into());
        }
        for target in &self.targets {
            let name = target
                .reference
                .strip_prefix("refs/heads/")
                .or_else(|| target.reference.strip_prefix("refs/tags/"))
                .ok_or("invalid upload target namespace")?;
            crate::repository::validate_ref_name(name)?;
        }
        match self.operation.as_ref() {
            Operation::Push { .. } | Operation::UpdateRef { .. } => {
                if upload_targets(&self.operation, &[])? != self.targets {
                    return Err("upload target does not match operation".into());
                }
            }
            Operation::PublishBatch {}
            | Operation::PublishOptions {}
            | Operation::CheckBatch {}
            | Operation::PublishPartial { .. } => (),
            _ => return Err("invalid scoped upload operation".into()),
        }
        Ok(())
    }

    pub fn validate_payload(&self, payload: &[u8]) -> Result<(), String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        if payload.len() as u64 != self.total
            || format!("{:x}", Sha256::digest(payload)) != self.digest
        {
            return Err("uploaded bundle checksum mismatch".into());
        }
        if upload_targets(&self.operation, payload)? != self.targets {
            return Err("uploaded targets do not match binding".into());
        }
        Ok(())
    }
}

fn upload_targets(operation: &Operation, payload: &[u8]) -> Result<Vec<UploadTarget>, String> {
    match operation {
        Operation::Push { branch, force, .. } => Ok(vec![UploadTarget {
            reference: format!("refs/heads/{branch}"),
            force: *force,
        }]),
        Operation::UpdateRef {
            namespace,
            name,
            force,
            ..
        } => Ok(vec![UploadTarget {
            reference: format!(
                "refs/{}/{name}",
                if matches!(namespace, RefNamespace::Branch) {
                    "heads"
                } else {
                    "tags"
                }
            ),
            force: *force,
        }]),
        Operation::PublishBatch {}
        | Operation::PublishOptions {}
        | Operation::CheckBatch {}
        | Operation::PublishPartial { .. } => {
            let (publication, _) = publication_payload(operation, payload)?;
            Ok(publication
                .updates
                .into_iter()
                .map(|u| UploadTarget {
                    reference: format!(
                        "refs/{}/{}",
                        if matches!(u.namespace, RefNamespace::Branch) {
                            "heads"
                        } else {
                            "tags"
                        },
                        u.name
                    ),
                    force: u.force,
                })
                .collect())
        }
        _ => Err("invalid scoped upload operation".into()),
    }
}

/// The repository holder records protection; the coordinator owns the release
/// decision. IDs are 32-byte lowercase hexadecimal values scoped to a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RetentionCommand {
    Prepare {
        operation: String,
        commit: String,
        paths: Vec<String>,
    },
    Query {
        operation: String,
    },
    Promote {
        operation: String,
    },
    Close {
        operation: String,
    },
    Read {
        operation: String,
        path: String,
        offset: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionState {
    Prepared,
    Promoted,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedFile {
    pub blob: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionRecord {
    pub version: u32,
    pub operation: String,
    pub actor: String,
    pub state: RetentionState,
    #[serde(deserialize_with = "present_option")]
    pub repository: Option<String>,
    #[serde(deserialize_with = "present_option")]
    pub commit: Option<String>,
    pub files: std::collections::BTreeMap<String, RetainedFile>,
}

impl RetentionRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != RETENTION_VERSION
            || self.actor.is_empty()
            || self.actor.len() > 128
            || self.files.len() > 128
        {
            return Err("invalid release retention record".into());
        }
        crate::binary::oid(&self.operation)?;
        if let Some(repository) = &self.repository {
            repository_name(repository)?;
        }
        match (&self.state, &self.commit) {
            (RetentionState::Closed, None) if self.files.is_empty() => (),
            (_, Some(commit)) if !self.files.is_empty() => {
                crate::binary::oid(commit)?;
            }
            _ => return Err("invalid release retention contents".into()),
        }
        for (path, file) in &self.files {
            crate::repository::validate_tree_path(path)?;
            if path.len() > 1024 || file.size < 8 {
                return Err("invalid retained file".into());
            }
            crate::binary::oid(&file.blob)?;
            crate::binary::oid(&file.sha256)?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RetentionReply {
    State {
        version: u32,
        repository: String,
        operation: String,
        #[serde(deserialize_with = "present_option")]
        record: Option<RetentionRecord>,
    },
    File {
        version: u32,
        repository: String,
        actor: String,
        operation: String,
        commit: String,
        path: String,
        file: RetainedFile,
        offset: u64,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionRequest {
    pub version: u32,
    pub command: RetentionCommand,
}

impl RetentionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported retention request version".into());
        }
        let operation = match &self.command {
            RetentionCommand::Prepare {
                operation,
                commit,
                paths,
            } => {
                crate::binary::oid(commit)?;
                if paths.is_empty() || paths.len() > 128 {
                    return Err("release retention requires 1..=128 paths".into());
                }
                let unique: std::collections::BTreeSet<_> = paths.iter().collect();
                if unique.len() != paths.len() {
                    return Err("duplicate release path".into());
                }
                for path in paths {
                    crate::repository::validate_tree_path(path)?;
                    if path.len() > 1024 {
                        return Err("release path too long".into());
                    }
                }
                operation
            }
            RetentionCommand::Read {
                operation, path, ..
            } => {
                crate::repository::validate_tree_path(path)?;
                if path.len() > 1024 {
                    return Err("release path too long".into());
                }
                operation
            }
            RetentionCommand::Query { operation }
            | RetentionCommand::Promote { operation }
            | RetentionCommand::Close { operation } => operation,
        };
        crate::binary::oid(operation)?;
        Ok(())
    }
}

impl RetentionReply {
    /// Validate a decoded response against the exact request before exposing it
    /// to an integration. Transport errors remain unknown outcomes.
    pub fn validate_for(&self, repository: &str, request: &RetentionCommand) -> Result<(), String> {
        repository_name(repository)?;
        let id = match request {
            RetentionCommand::Prepare { operation, .. }
            | RetentionCommand::Query { operation }
            | RetentionCommand::Promote { operation }
            | RetentionCommand::Close { operation }
            | RetentionCommand::Read { operation, .. } => operation,
        };
        crate::binary::oid(id)?;
        match (request, self) {
            (
                RetentionCommand::Read { path, offset, .. },
                RetentionReply::File {
                    version: RETENTION_VERSION,
                    repository: returned_repository,
                    actor,
                    operation,
                    commit,
                    path: returned,
                    file,
                    offset: got,
                    bytes,
                },
            ) if returned_repository == repository
                && !actor.is_empty()
                && actor.len() <= 128
                && operation == id
                && returned == path
                && got == offset =>
            {
                crate::repository::validate_tree_path(path)?;
                crate::binary::oid(commit)?;
                crate::binary::oid(&file.blob)?;
                crate::binary::oid(&file.sha256)?;
                if *offset >= file.size
                    || bytes.len() as u64 != (file.size - offset).min(TRANSFER_CHUNK as u64)
                {
                    return Err("invalid retained file slice".into());
                }
                if *offset == 0 && bytes.len() as u64 == file.size {
                    use sha2::{Digest, Sha256};
                    if format!("{:x}", Sha256::digest(bytes)) != file.sha256 {
                        return Err("retained file checksum mismatch".into());
                    }
                }
                Ok(())
            }
            (
                RetentionCommand::Query { .. },
                RetentionReply::State {
                    version: RETENTION_VERSION,
                    repository: returned_repository,
                    operation,
                    record: None,
                },
            ) if returned_repository == repository && operation == id => Ok(()),
            (
                _,
                RetentionReply::State {
                    version: RETENTION_VERSION,
                    repository: returned_repository,
                    operation,
                    record: Some(record),
                },
            ) if returned_repository == repository
                && operation == id
                && record.version == RETENTION_VERSION
                && &record.operation == id
                && !record.actor.is_empty() =>
            {
                record.validate()?;
                if record.repository.as_deref() != Some(repository)
                    && !(record.repository.is_none()
                        && matches!(request, RetentionCommand::Query { .. }))
                {
                    return Err("retention repository mismatch".into());
                }
                match request {
                    RetentionCommand::Prepare { commit, paths, .. } => {
                        let selected: std::collections::BTreeSet<_> = paths.iter().collect();
                        if record.state == RetentionState::Closed
                            || record.commit.as_ref() != Some(commit)
                            || selected.len() != paths.len()
                            || !record.files.keys().eq(selected)
                        {
                            return Err("retention prepare response mismatch".into());
                        }
                    }
                    RetentionCommand::Promote { .. }
                        if record.state != RetentionState::Promoted =>
                    {
                        return Err("retention promotion response mismatch".into())
                    }
                    RetentionCommand::Close { .. } if record.state != RetentionState::Closed => {
                        return Err("retention close response mismatch".into())
                    }
                    RetentionCommand::Read { .. } => {
                        return Err("retention response is not a file".into())
                    }
                    _ => (),
                }
                Ok(())
            }
            _ => Err("invalid release retention response".into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationCapabilities {
    pub version: u32,
    pub tool_version: String,
    pub target_os: String,
    pub target_arch: String,
    pub target_env: String,
    pub rpc_version: u32,
    pub retention_version: u32,
    pub scoped_upload_request_version: u32,
    pub scoped_upload_status_version: u32,
    pub scoped_receipt_version: u32,
    pub scoped_chunk_version: u32,
    pub server_policy_version: u32,
    pub node_safe_integer_max: u64,
    pub authorization_natural_expiry: bool,
    pub authorization_revocation_fence: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandResult<T> {
    Data { value: T },
    Empty,
    Text { text: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope<T> {
    pub protocol: String,
    pub version: u32,
    pub command: String,
    pub ok: bool,
    pub exit_code: i32,
    pub result: CommandResult<T>,
    #[serde(deserialize_with = "present_option")]
    pub error: Option<CommandError>,
}

/// Machine result for a staged tree commit. `head` is the identity before
/// publication: a branch name or, for detached HEAD, its previous commit ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeCommitResult {
    pub version: u32,
    pub head: String,
    /// Previous HEAD tip; `--amend` may retain different commit parents.
    #[serde(deserialize_with = "present_option")]
    pub previous: Option<String>,
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignCommitResult {
    pub version: u32,
    pub head: String,
    pub previous: String,
    pub commit: String,
    pub author: String,
}

/// The identity and digest are derived from the same locked snapshot and
/// byte buffer used to publish the bundle, not from a later repository read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleResult {
    pub version: u32,
    pub head: String,
    #[serde(deserialize_with = "present_option")]
    pub tip: Option<String>,
    pub objects: u64,
    pub bytes: u64,
    pub sha256: String,
}

fn validate_head_and_tip(head: &str, tip: Option<&str>) -> Result<(), String> {
    crate::repository::validate_head_identity(head)?;
    if let Some(tip) = tip {
        crate::binary::oid(tip)?;
    }
    if crate::repository::is_detached_identity(head) && tip != Some(head) {
        return Err("detached HEAD does not match its tip".into());
    }
    Ok(())
}

impl TreeCommitResult {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported tree commit result version".into());
        }
        validate_head_and_tip(&self.head, self.previous.as_deref())?;
        crate::binary::oid(&self.commit).map(|_| ())
    }
}

impl SignCommitResult {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported sign commit result version".into());
        }
        validate_head_and_tip(&self.head, Some(&self.previous))?;
        crate::binary::oid(&self.commit)?;
        crate::repository::validate_author_identity(&self.author)
    }
}

impl BundleResult {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.bytes == 0
            || self.bytes > NODE_SAFE_INTEGER_MAX
            || self.objects > NODE_SAFE_INTEGER_MAX
            || (self.tip.is_some() && self.objects == 0)
        {
            return Err("invalid bundle result".into());
        }
        validate_head_and_tip(&self.head, self.tip.as_deref())?;
        crate::binary::oid(&self.sha256).map(|_| ())
    }
}

impl<T> CommandEnvelope<T> {
    pub fn validate_for(&self, command: &str, process_exit: i32) -> Result<Option<&T>, String> {
        if self.protocol != "wasm-git.command"
            || self.version != 1
            || self.command != command
            || self.exit_code != process_exit
            || self.ok != (process_exit == 0)
        {
            return Err("command envelope or process status mismatch".into());
        }
        match (&self.result, &self.error, self.ok) {
            (CommandResult::Data { value }, None, true) => Ok(Some(value)),
            (CommandResult::Empty, Some(error), false)
                if error.code == "command_failed" && !error.message.is_empty() =>
            {
                Ok(None)
            }
            _ => Err("invalid command result".into()),
        }
    }
}

impl IntegrationCapabilities {
    pub fn current() -> Self {
        Self {
            version: 1,
            tool_version: env!("CARGO_PKG_VERSION").into(),
            target_os: std::env::consts::OS.into(),
            target_arch: std::env::consts::ARCH.into(),
            target_env: if cfg!(target_env = "musl") {
                "musl"
            } else if cfg!(target_env = "gnu") {
                "gnu"
            } else if cfg!(target_env = "msvc") {
                "msvc"
            } else {
                "other"
            }
            .into(),
            rpc_version: 1,
            retention_version: RETENTION_VERSION,
            scoped_upload_request_version: 1,
            scoped_upload_status_version: SCOPED_UPLOAD_STATUS_VERSION,
            scoped_receipt_version: 4,
            scoped_chunk_version: 1,
            server_policy_version: SERVER_POLICY_VERSION,
            node_safe_integer_max: NODE_SAFE_INTEGER_MAX,
            authorization_natural_expiry: false,
            authorization_revocation_fence: cfg!(all(
                target_os = "linux",
                target_arch = "x86_64",
                any(target_env = "gnu", target_env = "musl")
            )),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if crate::binary::encode(self)? != crate::binary::encode(&Self::current())? {
            return Err("integration capabilities do not match this native build".into());
        }
        Ok(())
    }
}

/// Check the original JSON before a JavaScript consumer can round numbers or
/// collapse duplicate object members, including members of dynamic maps.
/// This does not restrict u64 values in the native binary protocol.
pub fn validate_node_safe_json(input: &[u8]) -> Result<(), String> {
    use serde::de::{Error, MapAccess, SeqAccess, Visitor};

    struct CheckedValue;
    impl<'de> Deserialize<'de> for CheckedValue {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct CheckedVisitor;
            impl<'de> Visitor<'de> for CheckedVisitor {
                type Value = CheckedValue;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("JSON with unique object keys and exact JavaScript-safe integers")
                }
                fn visit_bool<E: Error>(self, _: bool) -> Result<Self::Value, E> {
                    Ok(CheckedValue)
                }
                fn visit_str<E: Error>(self, _: &str) -> Result<Self::Value, E> {
                    Ok(CheckedValue)
                }
                fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
                    Ok(CheckedValue)
                }
                fn visit_i64<E: Error>(self, value: i64) -> Result<Self::Value, E> {
                    if value.unsigned_abs() <= NODE_SAFE_INTEGER_MAX {
                        Ok(CheckedValue)
                    } else {
                        Err(E::custom(
                            "JSON number is not an exact JavaScript safe integer",
                        ))
                    }
                }
                fn visit_u64<E: Error>(self, value: u64) -> Result<Self::Value, E> {
                    if value <= NODE_SAFE_INTEGER_MAX {
                        Ok(CheckedValue)
                    } else {
                        Err(E::custom(
                            "JSON number is not an exact JavaScript safe integer",
                        ))
                    }
                }
                fn visit_f64<E: Error>(self, _: f64) -> Result<Self::Value, E> {
                    Err(E::custom(
                        "JSON number is not an exact JavaScript safe integer",
                    ))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                    while seq.next_element::<CheckedValue>()?.is_some() {}
                    Ok(CheckedValue)
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut keys = std::collections::HashSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !keys.insert(key) {
                            return Err(A::Error::custom("duplicate JSON object key"));
                        }
                        map.next_value::<CheckedValue>()?;
                    }
                    Ok(CheckedValue)
                }
            }
            deserializer.deserialize_any(CheckedVisitor)
        }
    }
    serde_json::from_slice::<CheckedValue>(input)
        .map(|_| ())
        .map_err(|e| format!("invalid integration JSON: {e}"))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IntegrationValidation {
    ChunkIdentity {
        chunk: ScopedChunkIdentity,
    },
    ChunkStatus {
        chunk: ScopedChunkIdentity,
        status: ScopedChunkStatus,
    },
    OperationCredentialRequest {
        request: OperationCredentialRequest,
    },
    OperationCredentialQuery {
        request: OperationCredentialQuery,
    },
    OperationCredentialIssueCliReply {
        request: OperationCredentialRequest,
        process_exit: i32,
        response: CommandEnvelope<OperationCredentialReceipt>,
    },
    OperationCredentialQueryCliReply {
        request: OperationCredentialRequest,
        process_exit: i32,
        response: CommandEnvelope<OperationCredentialQueryReply>,
    },
    PolicyMigrationRequest {
        request: PolicyMigrationRequest,
    },
    PolicyMigrationCliReply {
        request: PolicyMigrationRequest,
        process_exit: i32,
        response: CommandEnvelope<PolicyMigrationReceipt>,
    },
    PolicyUpdateRequest {
        request: PolicyUpdateRequest,
    },
    PolicyQueryRequest {
        request: PolicyQueryRequest,
    },
    PolicyUpdateCliReply {
        request: PolicyUpdateRequest,
        process_exit: i32,
        response: CommandEnvelope<PolicyUpdateReceipt>,
    },
    PolicyQueryCliReply {
        request: PolicyUpdateRequest,
        process_exit: i32,
        response: CommandEnvelope<PolicyQueryReply>,
    },
    PolicyStatusCliReply {
        process_exit: i32,
        response: CommandEnvelope<PolicyStatus>,
    },
    RetentionRequest {
        request: RetentionRequest,
    },
    RetentionReply {
        repository: String,
        request: RetentionRequest,
        reply: RetentionReply,
    },
    UploadBinding {
        binding: UploadBinding,
    },
    UploadBindingRequest {
        request: UploadBindingRequest,
    },
    UploadBindingCliReply {
        request: UploadBindingRequest,
        process_exit: i32,
        response: CommandEnvelope<UploadBinding>,
    },
    UploadRequest {
        request: ScopedUploadRequest,
    },
    UploadReply {
        repository: String,
        request: ScopedUploadRequest,
        reply: ScopedUploadReply,
    },
    RetentionCliReply {
        repository: String,
        request: RetentionRequest,
        process_exit: i32,
        response: CommandEnvelope<RetentionReply>,
    },
    UploadCliReply {
        repository: String,
        request: ScopedUploadRequest,
        process_exit: i32,
        response: CommandEnvelope<ScopedUploadReply>,
    },
    Capabilities {
        capabilities: IntegrationCapabilities,
    },
    CapabilitiesCliReply {
        process_exit: i32,
        response: CommandEnvelope<IntegrationCapabilities>,
    },
    TreeCommitCliReply {
        head: String,
        #[serde(deserialize_with = "present_option")]
        previous: Option<String>,
        process_exit: i32,
        response: CommandEnvelope<TreeCommitResult>,
    },
    SignCommitCliReply {
        head: String,
        previous: String,
        author: String,
        process_exit: i32,
        response: CommandEnvelope<SignCommitResult>,
    },
    BundleCliReply {
        head: String,
        #[serde(deserialize_with = "present_option")]
        tip: Option<String>,
        process_exit: i32,
        response: CommandEnvelope<BundleResult>,
    },
    BundleUploadBinding {
        bundle: BundleResult,
        binding: UploadBinding,
    },
    OperationRecoveryRequest {
        request: OperationRecoveryRequest,
    },
    OperationRecoveryCliReply {
        request: OperationRecoveryRequest,
        process_exit: i32,
        response: Box<CommandEnvelope<OperationRecoveryReply>>,
    },
    OperationContextCliReply {
        request: OperationCredentialQuery,
        process_exit: i32,
        response: CommandEnvelope<OperationContextReply>,
    },
    AuditQueryRequest {
        request: AuditQueryRequest,
    },
    AuditQueryCliReply {
        request: AuditQueryRequest,
        process_exit: i32,
        response: CommandEnvelope<AuditQueryReply>,
    },
    CapacityQueryRequest {
        request: CapacityQueryRequest,
    },
    CapacityQueryCliReply {
        request: CapacityQueryRequest,
        process_exit: i32,
        response: CommandEnvelope<CapacityQueryReply>,
    },
}

impl IntegrationValidation {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::OperationRecoveryRequest { request } => request.validate(),
            Self::OperationRecoveryCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(reply) = response.validate_for("operation-recover", *process_exit)? {
                    reply.validate_for(request)?;
                }
                Ok(())
            }
            Self::OperationContextCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(reply) = response.validate_for("operation-context", *process_exit)? {
                    reply.validate_for(request)?;
                }
                Ok(())
            }
            Self::AuditQueryRequest { request } => request.validate(),
            Self::AuditQueryCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(reply) = response.validate_for("audit-query", *process_exit)? {
                    reply.validate_for(request)?;
                }
                Ok(())
            }
            Self::CapacityQueryRequest { request } => request.validate(),
            Self::CapacityQueryCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(reply) = response.validate_for("capacity-query", *process_exit)? {
                    reply.validate_for(request)?;
                }
                Ok(())
            }
            Self::ChunkIdentity { chunk } => chunk.validate(),
            Self::ChunkStatus { chunk, status } => status.validate_for(chunk),
            Self::OperationCredentialRequest { request } => request.validate(),
            Self::OperationCredentialQuery { request } => request.validate(),
            Self::OperationCredentialIssueCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(receipt) =
                    response.validate_for("operation-credential-issue", *process_exit)?
                {
                    receipt.validate_request(request)?;
                }
                Ok(())
            }
            Self::OperationCredentialQueryCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(reply) =
                    response.validate_for("operation-credential-query", *process_exit)?
                {
                    reply.validate_request(request)?;
                }
                Ok(())
            }
            Self::PolicyMigrationRequest { request } => request.validate(),
            Self::PolicyMigrationCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(receipt) = response.validate_for("policy-migrate", *process_exit)? {
                    receipt.validate_request(request)?;
                }
                Ok(())
            }
            Self::PolicyUpdateRequest { request } => request.validate(),
            Self::PolicyQueryRequest { request } => request.validate(),
            Self::PolicyUpdateCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(receipt) = response.validate_for("policy-set", *process_exit)? {
                    receipt.validate_update(request)?;
                }
                Ok(())
            }
            Self::PolicyQueryCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(reply) = response.validate_for("policy-query", *process_exit)? {
                    reply.validate_update(request)?;
                }
                Ok(())
            }
            Self::PolicyStatusCliReply {
                process_exit,
                response,
            } => {
                if let Some(status) = response.validate_for("policy-status", *process_exit)? {
                    status.validate()?;
                }
                Ok(())
            }
            Self::RetentionRequest { request } => request.validate(),
            Self::RetentionReply {
                repository,
                request,
                reply,
            } => {
                request.validate()?;
                reply.validate_for(repository, &request.command)
            }
            Self::UploadBinding { binding } => binding.validate(),
            Self::UploadBindingRequest { request } => request.validate(),
            Self::UploadBindingCliReply {
                request,
                process_exit,
                response,
            } => {
                request.validate()?;
                if let Some(binding) = response.validate_for("upload-binding", *process_exit)? {
                    binding.validate()?;
                    if crate::binary::encode(binding.operation.as_ref())?
                        != crate::binary::encode(request.operation.as_ref())?
                    {
                        return Err("upload binding operation mismatch".into());
                    }
                }
                Ok(())
            }
            Self::UploadRequest { request } => request.validate(),
            Self::UploadReply {
                repository,
                request,
                reply,
            } => reply.validate_for(repository, request),
            Self::RetentionCliReply {
                repository,
                request,
                process_exit,
                response,
            } => {
                repository_name(repository)?;
                request.validate()?;
                if let Some(reply) = response.validate_for("retention-rpc", *process_exit)? {
                    reply.validate_for(repository, &request.command)?;
                }
                Ok(())
            }
            Self::UploadCliReply {
                repository,
                request,
                process_exit,
                response,
            } => {
                repository_name(repository)?;
                request.validate()?;
                if let Some(reply) = response.validate_for("scoped-upload-rpc", *process_exit)? {
                    reply.validate_for(repository, request)?;
                }
                Ok(())
            }
            Self::Capabilities { capabilities } => capabilities.validate(),
            Self::CapabilitiesCliReply {
                process_exit,
                response,
            } => {
                if let Some(capabilities) =
                    response.validate_for("integration-capabilities", *process_exit)?
                {
                    capabilities.validate()?;
                }
                Ok(())
            }
            Self::TreeCommitCliReply {
                head,
                previous,
                process_exit,
                response,
            } => {
                validate_head_and_tip(head, previous.as_deref())?;
                if let Some(result) = response.validate_for("commit", *process_exit)? {
                    result.validate()?;
                    if &result.head != head || &result.previous != previous {
                        return Err("tree commit result identity mismatch".into());
                    }
                }
                Ok(())
            }
            Self::SignCommitCliReply {
                head,
                previous,
                author,
                process_exit,
                response,
            } => {
                validate_head_and_tip(head, Some(previous))?;
                crate::repository::validate_author_identity(author)?;
                if let Some(result) = response.validate_for("sign-commit", *process_exit)? {
                    result.validate()?;
                    if &result.head != head
                        || &result.previous != previous
                        || &result.author != author
                    {
                        return Err("sign commit result identity mismatch".into());
                    }
                }
                Ok(())
            }
            Self::BundleCliReply {
                head,
                tip,
                process_exit,
                response,
            } => {
                validate_head_and_tip(head, tip.as_deref())?;
                if let Some(result) = response.validate_for("bundle", *process_exit)? {
                    result.validate()?;
                    if &result.head != head || &result.tip != tip {
                        return Err("bundle result identity mismatch".into());
                    }
                }
                Ok(())
            }
            Self::BundleUploadBinding { bundle, binding } => {
                bundle.validate()?;
                binding.validate()?;
                if bundle.bytes != binding.total || bundle.sha256 != binding.digest {
                    return Err("bundle and upload binding content mismatch".into());
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefUpdate {
    pub namespace: RefNamespace,
    pub name: String,
    #[serde(deserialize_with = "present_option")]
    pub expected: Option<String>,
    #[serde(deserialize_with = "present_option")]
    pub new: Option<String>,
    pub force: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchPublication {
    pub updates: Vec<RefUpdate>,
    pub bundle: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OptionsPublication {
    pub publication: BatchPublication,
    pub options: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PartialReceipt {
    /// None acknowledges this update; Some explains its rejection. Order is
    /// identical to the request, whose digest is in Response::tip.
    pub rejections: Vec<Option<String>>,
}

pub fn validate_push_options(options: &[String]) -> Result<(), String> {
    if options.len() > 256
        || options.iter().any(|s| s.contains(['\0', '\n']))
        || options.iter().map(String::len).sum::<usize>() > 65_536
    {
        return Err("push options require at most 256 values and 64 KiB, without NUL or LF".into());
    }
    Ok(())
}

pub fn publication_payload(
    operation: &Operation,
    payload: &[u8],
) -> Result<(BatchPublication, Vec<String>), String> {
    match operation {
        Operation::PublishBatch {} => Ok((crate::binary::decode(payload)?, Vec::new())),
        Operation::PublishOptions {}
        | Operation::CheckBatch {}
        | Operation::PublishPartial { .. } => {
            let decoded: OptionsPublication = crate::binary::decode(payload)?;
            validate_push_options(&decoded.options)?;
            Ok((decoded.publication, decoded.options))
        }
        _ => Err("expected a batch publication".into()),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RefPage {
    pub generation: String,
    pub state: RepositoryState,
    pub next: Option<String>,
}

// An uploaded bundle can end only in a publication. A separate, nonrecursive
// wire enum prevents untrusted nested ApplyUpload frames exhausting the stack.
mod publication {
    use super::*;
    #[derive(Serialize)]
    enum Borrowed<'a> {
        Push {
            branch: &'a str,
            expected: &'a Option<String>,
            force: bool,
        },
        UpdateRef {
            namespace: &'a RefNamespace,
            name: &'a str,
            expected: &'a Option<String>,
            new: &'a Option<String>,
            force: bool,
        },
        PublishBatch {},
        PublishOptions {},
        CheckBatch {},
        PublishPartial {
            dry_run: bool,
        },
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    enum Owned {
        Push {
            branch: String,
            #[serde(deserialize_with = "present_option")]
            expected: Option<String>,
            force: bool,
        },
        UpdateRef {
            namespace: RefNamespace,
            name: String,
            #[serde(deserialize_with = "present_option")]
            expected: Option<String>,
            #[serde(deserialize_with = "present_option")]
            new: Option<String>,
            force: bool,
        },
        PublishBatch {},
        PublishOptions {},
        CheckBatch {},
        PublishPartial {
            dry_run: bool,
        },
    }
    pub fn serialize<S: serde::Serializer>(value: &Operation, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Operation::PublishBatch {} => Borrowed::PublishBatch {}.serialize(s),
            Operation::PublishOptions {} => Borrowed::PublishOptions {}.serialize(s),
            Operation::CheckBatch {} => Borrowed::CheckBatch {}.serialize(s),
            Operation::PublishPartial { dry_run } => {
                Borrowed::PublishPartial { dry_run: *dry_run }.serialize(s)
            }
            Operation::Push {
                branch,
                expected,
                force,
            } => Borrowed::Push {
                branch,
                expected,
                force: *force,
            }
            .serialize(s),
            Operation::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force,
            } => Borrowed::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force: *force,
            }
            .serialize(s),
            _ => Err(serde::ser::Error::custom(
                "uploaded operation must publish a ref",
            )),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Box<Operation>, D::Error> {
        Ok(Box::new(match Owned::deserialize(d)? {
            Owned::PublishBatch {} => Operation::PublishBatch {},
            Owned::PublishOptions {} => Operation::PublishOptions {},
            Owned::CheckBatch {} => Operation::CheckBatch {},
            Owned::PublishPartial { dry_run } => Operation::PublishPartial { dry_run },
            Owned::Push {
                branch,
                expected,
                force,
            } => Operation::Push {
                branch,
                expected,
                force,
            },
            Owned::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force,
            } => Operation::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force,
            },
        }))
    }
}

// A nullable mutation operand must still be present. Serde's ordinary Option
// default would turn an omitted `new` into a deletion and an omitted lease into
// the explicit absence expectation. deserialize_with also makes absence an error.
fn present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    pub publication_unknown: bool,
    #[serde(deserialize_with = "present_option")]
    pub error: Option<String>,
    #[serde(deserialize_with = "present_option")]
    pub state: Option<RepositoryState>,
    #[serde(deserialize_with = "present_option")]
    pub tip: Option<String>,
}

fn lengths(metadata: usize, payload: usize) -> Result<(), String> {
    if metadata == 0 || metadata > MAX_METADATA_BYTES {
        return Err("RPC metadata length is outside 1..=65536".into());
    }
    if HEADER_BYTES
        .checked_add(metadata)
        .and_then(|n| n.checked_add(payload))
        .is_none_or(|n| n > MAX_PACKET_BYTES)
    {
        return Err("RPC packet exceeds 64 MiB".into());
    }
    Ok(())
}

pub fn write_packet<W: Write, T: Serialize>(
    writer: &mut W,
    metadata: &T,
    payload: &[u8],
) -> Result<(), String> {
    let metadata = crate::binary::encode(metadata).map_err(|e| e.to_string())?;
    lengths(metadata.len(), payload.len())?;
    writer
        .write_all(MAGIC)
        .and_then(|()| writer.write_all(&(metadata.len() as u32).to_be_bytes()))
        .and_then(|()| writer.write_all(&(payload.len() as u64).to_be_bytes()))
        .and_then(|()| writer.write_all(&metadata))
        .and_then(|()| writer.write_all(payload))
        .map_err(|e| format!("write RPC packet: {e}"))
}

/// Read one frame without waiting for EOF. Stream owners check EOF separately.
pub fn read_packet<R: Read, T: DeserializeOwned + Serialize>(
    reader: &mut R,
) -> Result<(T, Vec<u8>), String> {
    let mut header = [0; HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .map_err(|e| format!("read RPC header: {e}"))?;
    if &header[..8] != MAGIC {
        return Err("unrecognized RPC magic/version".into());
    }
    let metadata = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    let payload = usize::try_from(u64::from_be_bytes(header[12..20].try_into().unwrap()))
        .map_err(|_| "RPC payload length does not fit this platform")?;
    lengths(metadata, payload)?;
    let mut encoded_metadata = vec![0; metadata];
    reader
        .read_exact(&mut encoded_metadata)
        .map_err(|e| format!("read RPC metadata: {e}"))?;
    let metadata = crate::binary::decode(&encoded_metadata)
        .map_err(|e| format!("invalid RPC metadata: {e}"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(payload)
        .map_err(|e| format!("allocate RPC payload: {e}"))?;
    bytes.resize(payload, 0);
    reader
        .read_exact(&mut bytes)
        .map_err(|e| format!("read RPC payload: {e}"))?;
    Ok((metadata, bytes))
}

pub fn read_complete_packet<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
) -> Result<(T, Vec<u8>), String> {
    let mut reader = Cursor::new(bytes);
    let packet = read_packet(&mut reader)?;
    if reader.position() != bytes.len() as u64 {
        return Err("trailing bytes after RPC packet".into());
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_migration_receipts_bind_the_exact_source_and_preserve_empty_policy_state() {
        let request = PolicyMigrationRequest {
            version: 1,
            source_sha256: "a".repeat(64),
        };
        let receipt = PolicyMigrationReceipt {
            version: 1,
            source_sha256: request.source_sha256.clone(),
            source_version: 1,
            target_version: 2,
            generation: 0,
            policy_sha256: None,
            receipts: 1,
        };
        receipt.validate_request(&request).unwrap();
        let validation = IntegrationValidation::PolicyMigrationCliReply {
            request: request.clone(),
            process_exit: 0,
            response: CommandEnvelope {
                protocol: "wasm-git.command".into(),
                version: 1,
                command: "policy-migrate".into(),
                ok: true,
                exit_code: 0,
                result: CommandResult::Data {
                    value: receipt.clone(),
                },
                error: None,
            },
        };
        validation.validate().unwrap();
        let mut invalid = receipt.clone();
        invalid.source_sha256 = "b".repeat(64);
        assert!(invalid.validate_request(&request).is_err());
        invalid = receipt.clone();
        invalid.generation = 1;
        assert!(invalid.validate().is_err());
        invalid.policy_sha256 = Some("c".repeat(64));
        invalid.receipts = 0;
        assert!(invalid.validate().is_err());
        invalid.receipts = 1;
        invalid.target_version = 3;
        assert!(invalid.validate().is_err());
        let mut missing = serde_json::to_value(&receipt).unwrap();
        missing.as_object_mut().unwrap().remove("policy_sha256");
        assert!(serde_json::from_value::<PolicyMigrationReceipt>(missing).is_err());
    }

    #[test]
    fn scoped_json_requires_explicit_leases_and_rejects_unknown_fields() {
        let missing =
            serde_json::json!({"version":1,"operation":{"Push":{"branch":"topic","force":false}}});
        assert!(serde_json::from_value::<UploadBindingRequest>(missing).is_err());
        let extra = serde_json::json!({"version":1,"operation":{"Push":{"branch":"topic","expected":null,"force":false,"ignored":true}}});
        assert!(serde_json::from_value::<UploadBindingRequest>(extra).is_err());
        let omitted_new =
            serde_json::json!({"namespace":"branch","name":"topic","expected":null,"force":false});
        assert!(serde_json::from_value::<RefUpdate>(omitted_new).is_err());
    }

    #[test]
    fn node_validation_checks_original_keys_and_numbers() {
        assert!(validate_node_safe_json(
            br#"{"value":9007199254740991,"items":[-9007199254740991,true,null,"text"]}"#
        )
        .is_ok());
        assert!(validate_node_safe_json(br#"{"value":9007199254740992}"#).is_err());
        assert!(validate_node_safe_json(br#"{"value":-9007199254740992}"#).is_err());
        assert!(validate_node_safe_json(br#"{"value":1.0}"#).is_err());
        assert!(validate_node_safe_json(br#"{"items":[{"key":1,"\u006bey":2}]}"#).is_err());
        assert!(validate_node_safe_json(br#"{"value":1} {}"#).is_err());
    }
    fn request(operation: Operation) -> Request {
        Request {
            version: 1,
            repository: "project".into(),
            operation,
        }
    }
    fn packet(operation: Operation) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &request(operation), b"\0\xffpayload").unwrap();
        bytes
    }
    #[test]
    fn every_opcode_roundtrips_with_explicit_option_operands() {
        for operation in [
            Operation::Advertise {},
            Operation::Fetch {},
            Operation::FetchSlice {
                offset: 9,
                expected: Some("a".repeat(64)),
            },
            Operation::UploadSlice {
                transfer: "b".repeat(64),
                total: 32,
                offset: 0,
            },
            Operation::ApplyUpload {
                transfer: "c".repeat(64),
                operation: Box::new(Operation::Push {
                    branch: "main".into(),
                    expected: None,
                    force: false,
                }),
            },
            Operation::Fork {
                destination: "fork".into(),
            },
            Operation::Push {
                branch: "main".into(),
                expected: Some("d".repeat(64)),
                force: true,
            },
            Operation::UpdateRef {
                namespace: RefNamespace::Tag,
                name: "v1".into(),
                expected: None,
                new: None,
                force: false,
            },
        ] {
            let original = crate::binary::encode(&operation).unwrap();
            let bytes = packet(operation);
            let (got, payload): (Request, _) = read_complete_packet(&bytes).unwrap();
            assert_eq!(crate::binary::encode(&got.operation).unwrap(), original);
            assert_eq!(payload, b"\0\xffpayload");
            assert_eq!(got.repository, "project");
        }
        for unknown in [false, true] {
            let response = Response {
                version: 1,
                ok: false,
                publication_unknown: unknown,
                error: Some("inspect ref".into()),
                state: None,
                tip: None,
            };
            let mut bytes = Vec::new();
            write_packet(&mut bytes, &response, &[]).unwrap();
            let (got, _): (Response, _) = read_complete_packet(&bytes).unwrap();
            assert_eq!(got.publication_unknown, unknown);
        }
    }
    #[test]
    fn every_truncation_and_trailing_bytes_are_rejected() {
        let mut bytes = packet(Operation::Push {
            branch: "topic".into(),
            expected: None,
            force: false,
        });
        for end in 0..bytes.len() {
            assert!(
                read_complete_packet::<Request>(&bytes[..end]).is_err(),
                "{end}"
            );
        }
        bytes.push(0);
        assert!(read_complete_packet::<Request>(&bytes)
            .unwrap_err()
            .contains("trailing"));
    }
    #[test]
    fn lengths_opcodes_option_tags_and_nested_uploads_fail_closed() {
        for (metadata, payload) in [
            (0u32, 0u64),
            (65_537, 0),
            (1, u64::MAX),
            (1, MAX_PACKET_BYTES as u64),
        ] {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(metadata.to_be_bytes());
            bytes.extend(payload.to_be_bytes());
            assert!(read_complete_packet::<Request>(&bytes).is_err());
        }
        let mut bytes = packet(Operation::Fetch {});
        // Native schema: header20, record magic8, version1, string length1,
        // repository7, opcode1. An unsupported opcode cannot become a command.
        bytes[37] = 250;
        assert!(read_complete_packet::<Request>(&bytes).is_err());
        let mut bytes = packet(Operation::Push {
            branch: "x".into(),
            expected: None,
            force: false,
        });
        bytes[40] = 2;
        assert!(read_complete_packet::<Request>(&bytes).is_err());
        let invalid = request(Operation::ApplyUpload {
            transfer: "a".repeat(64),
            operation: Box::new(Operation::Fetch {}),
        });
        assert!(write_packet(&mut Vec::new(), &invalid, &[]).is_err());
        assert!(read_complete_packet::<Request>(br#"{"version":1}"#).is_err());
    }
}
