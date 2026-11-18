//! The holder binds an issued credential to one exact native request. A
//! mutation's identity is independent of its input and of its bearer token.
use super::*;
use sha2::{Digest, Sha256};

fn hash(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain);
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}

impl Request {
    /// The strict entry point excludes operations without native per-operation
    /// replay and terminal state. Legacy clients use their existing entry point.
    pub fn credential_effect_key(&self, actor: &str) -> Result<Option<String>, String> {
        if self.version != 1 { return Err("unsupported credential RPC version".into()); }
        repository_name(&self.repository)?;
        repository_component(actor)?;
        let identity = match &self.operation {
            Operation::Advertise {} | Operation::Fetch {} | Operation::FetchSlice { .. }
                | Operation::ObjectSlice { .. } | Operation::RefsPage { .. }
                | Operation::QueryScopedUpload { .. } => return Ok(None),
            Operation::QueryChunk { chunk } => {
                chunk.validate_scope(&self.repository, actor)?;
                return Ok(None);
            }
            Operation::UploadChunk { chunk } => {
                chunk.validate_scope(&self.repository, actor)?;
                format!("upload:{}:{}", chunk.scope.session, chunk.offset)
            }
            Operation::ScopedUploadSlice { session, binding, offset } => {
                crate::binary::oid(session)?; binding.validate()?;
                format!("upload:{session}:{offset}")
            }
            Operation::ApplyScopedUpload { session, binding } => {
                crate::binary::oid(session)?; binding.validate()?;
                format!("apply:{session}")
            }
            Operation::CancelScopedUpload { session, binding } => {
                crate::binary::oid(session)?; binding.validate()?;
                format!("cancel:{session}")
            }
            Operation::ReleaseRetention { command } => {
                RetentionRequest { version: 1, command: command.clone() }.validate()?;
                match command {
                    RetentionCommand::Query { .. } | RetentionCommand::Read { .. } => return Ok(None),
                    RetentionCommand::Prepare { operation, .. } => format!("retain:{operation}"),
                    RetentionCommand::Promote { operation } => format!("promote:{operation}"),
                    RetentionCommand::Close { operation } => format!("close:{operation}"),
                }
            }
            _ => return Err("operation is not supported by the strict credential entry point".into()),
        };
        Ok(Some(hash(b"wasm-git.mutation-identity.v1\0", &[actor.as_bytes(), self.repository.as_bytes(), identity.as_bytes()])))
    }

    pub fn credential_digest(&self, payload_sha256: &str) -> Result<String, String> {
        if self.version != 1 { return Err("unsupported credential RPC version".into()); }
        repository_name(&self.repository)?;
        crate::binary::oid(payload_sha256)?;
        let metadata = crate::binary::encode(self)?;
        if metadata.len() > MAX_METADATA_BYTES { return Err("credential request metadata exceeds bound".into()); }
        Ok(hash(b"wasm-git.operation-request.v1\0", &[&metadata, payload_sha256.as_bytes()]))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationCredentialRequest {
    pub version: u32,
    pub operation_id: String,
    pub policy_generation: u64,
    pub actor: String,
    pub token_sha256: String,
    pub request: Request,
    pub payload_sha256: String,
    /// Exclusive UTC Unix-second bound chosen by the trusted issuer from all
    /// original identity, admission, grant and operation deadline bounds.
    pub expires_at: u64,
}

impl OperationCredentialRequest {
    pub fn receipt(&self) -> Result<OperationCredentialReceipt, String> {
        if let Operation::UploadChunk { chunk } = &self.request.operation {
            if self.operation_id != chunk.operation_id || self.payload_sha256 != chunk.payload_sha256 {
                return Err("chunk issuance must bind its original operation and bytes".into());
            }
        }
        let receipt = OperationCredentialReceipt {
            version: self.version, operation_id: self.operation_id.clone(), policy_generation: self.policy_generation, actor: self.actor.clone(),
            token_sha256: self.token_sha256.clone(),
            request_sha256: self.request.credential_digest(&self.payload_sha256)?,
            effect_key: self.request.credential_effect_key(&self.actor)?, expires_at: self.expires_at,
        };
        receipt.validate()?;
        Ok(receipt)
    }
    pub fn validate(&self) -> Result<(), String> { self.receipt().map(|_| ()) }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationCredentialReceipt {
    pub version: u32,
    pub operation_id: String,
    pub policy_generation: u64,
    pub actor: String,
    pub token_sha256: String,
    pub request_sha256: String,
    #[serde(deserialize_with = "present_option")]
    pub effect_key: Option<String>,
    pub expires_at: u64,
}

impl OperationCredentialReceipt {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.expires_at == 0 || self.expires_at > NODE_SAFE_INTEGER_MAX
            || self.policy_generation == 0 || self.policy_generation > NODE_SAFE_INTEGER_MAX {
            return Err("invalid operation credential version, generation or expiry".into());
        }
        repository_component(&self.actor)?;
        for id in [&self.operation_id, &self.token_sha256, &self.request_sha256] { crate::binary::oid(id)?; }
        if let Some(key) = &self.effect_key { crate::binary::oid(key)?; }
        Ok(())
    }
    pub fn validate_request(&self, request: &OperationCredentialRequest) -> Result<(), String> {
        self.validate()?;
        if self != &request.receipt()? { return Err("operation credential does not match issuance".into()); }
        Ok(())
    }
    pub fn validate_rpc(&self, request: &Request, payload: &[u8]) -> Result<(), String> {
        self.validate()?;
        let digest = format!("{:x}", Sha256::digest(payload));
        if self.request_sha256 != request.credential_digest(&digest)?
            || self.effect_key != request.credential_effect_key(&self.actor)? {
            return Err("operation credential does not match the request".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationCredentialQuery { pub version: u32, pub operation_id: String }
impl OperationCredentialQuery {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 { return Err("invalid operation credential query version".into()); }
        crate::binary::oid(&self.operation_id).map(|_| ())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationCredentialQueryReply {
    pub version: u32, pub operation_id: String,
    #[serde(deserialize_with = "present_option")]
    pub receipt: Option<OperationCredentialReceipt>,
}
impl OperationCredentialQueryReply {
    pub fn validate_request(&self, request: &OperationCredentialRequest) -> Result<(), String> {
        request.validate()?;
        if self.version != 1 || self.operation_id != request.operation_id { return Err("operation credential query mismatch".into()); }
        if let Some(receipt) = &self.receipt { receipt.validate_request(request)?; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn issuance() -> OperationCredentialRequest {
        OperationCredentialRequest { version: 1, operation_id: "a".repeat(64), policy_generation: 1, actor: "alice".into(),
            token_sha256: "b".repeat(64), expires_at: 100,
            request: Request { version: 1, repository: "team/repo".into(), operation: Operation::ReleaseRetention {
                command: RetentionCommand::Prepare { operation: "c".repeat(64), commit: "d".repeat(64), paths: vec!["main.wasm".into()] } } },
            payload_sha256: format!("{:x}", Sha256::digest([])) }
    }
    #[test]
    fn credential_binds_exact_request_and_mutation_identity_cannot_be_renewed_by_new_input() {
        let request = issuance(); let receipt = request.receipt().unwrap();
        receipt.validate_rpc(&request.request, &[]).unwrap();
        assert!(receipt.validate_rpc(&request.request, &[1]).is_err());
        let mut next = request.clone(); next.operation_id = "e".repeat(64); next.expires_at += 1;
        next.request.operation = Operation::ReleaseRetention { command: RetentionCommand::Prepare {
            operation: "c".repeat(64), commit: "f".repeat(64), paths: vec!["other.wasm".into()] } };
        let renewed = next.receipt().unwrap();
        assert_eq!(receipt.effect_key, renewed.effect_key);
        assert_ne!(receipt.request_sha256, renewed.request_sha256);
        assert!(receipt.validate_request(&next).is_err());
        next.actor = "bob".into(); assert_ne!(receipt.effect_key, next.receipt().unwrap().effect_key);
        next.request.operation = Operation::ReleaseRetention { command: RetentionCommand::Query { operation: "c".repeat(64) } };
        assert_eq!(next.receipt().unwrap().effect_key, None);
        next.request.operation = Operation::Push { branch: "main".into(), expected: None, force: false };
        assert!(next.receipt().is_err());
    }
    #[test]
    fn credential_rejects_unsafe_expiry_and_missing_nullable_identity() {
        let mut request = issuance(); request.expires_at = NODE_SAFE_INTEGER_MAX + 1;
        assert!(request.receipt().is_err()); request.expires_at = 100;
        let mut value = serde_json::to_value(request.receipt().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("effect_key");
        assert!(serde_json::from_value::<OperationCredentialReceipt>(value).is_err());
    }
}
