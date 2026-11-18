//! Original-operation chunk facts owned by the native repository holder.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedChunkIdentity {
    pub version: u32,
    pub operation_id: String,
    pub scope: ScopedExecution,
    pub offset: u64,
    pub payload_sha256: String,
}

impl ScopedChunkIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported chunk identity version".into());
        }
        crate::binary::oid(&self.operation_id)?;
        crate::binary::oid(&self.scope.session)?;
        crate::binary::oid(&self.payload_sha256)?;
        repository_component(&self.scope.actor)?;
        repository_name(&self.scope.repository)?;
        self.scope.binding.validate()?;
        let length = self
            .scope
            .binding
            .total
            .saturating_sub(self.offset)
            .min(TRANSFER_CHUNK as u64);
        self.scope
            .binding
            .validate_slice(self.offset, length as usize)
    }

    pub fn validate_scope(&self, repository: &str, actor: &str) -> Result<(), String> {
        self.validate()?;
        if self.scope.repository != repository || self.scope.actor != actor {
            return Err("chunk scope does not match the authenticated request".into());
        }
        Ok(())
    }

    pub fn validate_payload(&self, bytes: &[u8]) -> Result<(), String> {
        self.validate()?;
        self.scope
            .binding
            .validate_slice(self.offset, bytes.len())?;
        if self.payload_sha256 != format!("{:x}", Sha256::digest(bytes)) {
            return Err("chunk bytes do not match the original operation".into());
        }
        Ok(())
    }

    pub fn upload_request(&self) -> Result<Request, String> {
        self.validate()?;
        Ok(Request {
            version: 1,
            repository: self.scope.repository.clone(),
            operation: Operation::UploadChunk {
                chunk: self.clone(),
            },
        })
    }

    pub fn validate_credential(
        &self,
        credential: &OperationCredentialReceipt,
    ) -> Result<(), String> {
        self.validate()?;
        credential.validate()?;
        let request = self.upload_request()?;
        if credential.operation_id != self.operation_id
            || credential.actor != self.scope.actor
            || credential.request_sha256 != request.credential_digest(&self.payload_sha256)?
            || credential.effect_key != request.credential_effect_key(&self.scope.actor)?
        {
            return Err("chunk does not match the original issued operation".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopedChunkCleanupReason {
    Completed,
    Canceled,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ScopedChunkData {
    Available,
    Cleaned { reason: ScopedChunkCleanupReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ScopedChunkState {
    /// No decision was observed under the holder's lock. This is not a
    /// permanent non-acceptance or a tombstone against a delayed original call.
    Unobserved,
    Accepted {
        data: ScopedChunkData,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedChunkStatus {
    pub version: u32,
    pub chunk: ScopedChunkIdentity,
    pub state: ScopedChunkState,
}

impl ScopedChunkStatus {
    pub fn validate_for(&self, chunk: &ScopedChunkIdentity) -> Result<(), String> {
        self.chunk.validate()?;
        chunk.validate()?;
        if self.version != 1 || crate::binary::encode(&self.chunk)? != crate::binary::encode(chunk)?
        {
            return Err("chunk result does not match the original operation".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk() -> ScopedChunkIdentity {
        ScopedChunkIdentity {
            version: 1,
            operation_id: "a".repeat(64),
            scope: ScopedExecution {
                actor: "alice".into(),
                repository: "project".into(),
                session: "b".repeat(64),
                binding: UploadBinding::for_payload(
                    Operation::UpdateRef {
                        namespace: RefNamespace::Branch,
                        name: "topic".into(),
                        expected: None,
                        new: None,
                        force: false,
                    },
                    &[1, 2],
                )
                .unwrap(),
            },
            offset: 0,
            payload_sha256: format!("{:x}", Sha256::digest([1, 2])),
        }
    }
    #[test]
    fn chunk_results_bind_original_scope_bytes_offset_and_operation() {
        let chunk = chunk();
        chunk.validate_payload(&[1, 2]).unwrap();
        assert!(chunk.validate_payload(&[2, 1]).is_err());
        let mut changed = chunk.clone();
        changed.scope.actor = "bob".into();
        for state in [
            ScopedChunkState::Unobserved,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Available,
            },
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Cleaned {
                    reason: ScopedChunkCleanupReason::Expired,
                },
            },
        ] {
            let status = ScopedChunkStatus {
                version: 1,
                chunk: chunk.clone(),
                state,
            };
            status.validate_for(&chunk).unwrap();
            assert!(status.validate_for(&changed).is_err());
        }
        let mut changed = chunk.clone();
        changed.offset = 1;
        assert!(changed.validate().is_err());
        changed = chunk.clone();
        changed.operation_id = "c".repeat(64);
        let credential = OperationCredentialRequest {
            version: 1,
            operation_id: chunk.operation_id.clone(),
            policy_generation: 1,
            actor: chunk.scope.actor.clone(),
            token_sha256: "d".repeat(64),
            request: chunk.upload_request().unwrap(),
            payload_sha256: chunk.payload_sha256.clone(),
            expires_at: 100,
        }
        .receipt()
        .unwrap();
        chunk.validate_credential(&credential).unwrap();
        assert!(changed.validate_credential(&credential).is_err());
        let query = ScopedUploadRequest {
            version: 1,
            command: ScopedUploadCommand::QueryChunk {
                chunk: chunk.clone(),
            },
        };
        let reply = ScopedUploadReply::Chunk {
            version: 1,
            status: ScopedChunkStatus {
                version: 1,
                chunk,
                state: ScopedChunkState::Unobserved,
            },
        };
        reply.validate_for("project", &query).unwrap();
        assert!(reply.validate_for("other", &query).is_err());
    }
}
