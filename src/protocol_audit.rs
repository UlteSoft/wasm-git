//! Bounded local audit projections. Native receipts remain the source of
//! repository decisions; an observation is only evidence of one scoped read.
use super::*;

pub const AUDIT_PAGE_MAX_ITEMS: u32 = 100;
pub const AUDIT_PAGE_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditOperator {
    pub version: u32,
    pub identity: String,
}

impl AuditOperator {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.identity.is_empty()
            || self.identity.len() > 256
            || self.identity.chars().any(char::is_control)
        {
            return Err("invalid audit operator".into());
        }
        Ok(())
    }
}

/// A legacy import records its observer and observation time, not an invented
/// original issuer or issuance time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AuditAttribution {
    Recorded {
        operator: AuditOperator,
        unix_millis: u64,
    },
    LegacyImported {
        observer: AuditOperator,
        observed_at_unix_millis: u64,
    },
}

impl AuditAttribution {
    pub fn validate(&self) -> Result<(), String> {
        let (operator, time) = match self {
            Self::Recorded {
                operator,
                unix_millis,
            } => (operator, unix_millis),
            Self::LegacyImported {
                observer,
                observed_at_unix_millis,
            } => (observer, observed_at_unix_millis),
        };
        operator.validate()?;
        if *time > NODE_SAFE_INTEGER_MAX {
            return Err("invalid audit time".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryAuditAction {
    Inspect,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditObservationScope {
    Chunk,
    UploadSession,
    Retention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AuditEvidenceObservation {
    /// No native decision was observed at this instant. A delayed original
    /// request may still arrive unless the caller has fenced it separately.
    Unobserved,
    /// Hash of a canonical, validated native state or receipt, not a claim
    /// that this particular operation caused that state.
    Observed {
        sha256: String,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditObservation {
    pub version: u32,
    pub scope: AuditObservationScope,
    pub evidence: AuditEvidenceObservation,
}

impl AuditObservation {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("invalid audit observation version".into());
        }
        if let AuditEvidenceObservation::Observed { sha256 } = &self.evidence {
            crate::binary::oid(sha256)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecordedObservation {
    pub observed_at_unix_millis: u64,
    pub source: AuditObservationSource,
    pub observation: AuditObservation,
}

impl AuditRecordedObservation {
    pub fn validate(&self) -> Result<(), String> {
        if self.observed_at_unix_millis > NODE_SAFE_INTEGER_MAX {
            return Err("invalid audit observation time".into());
        }
        self.source.validate()?;
        self.observation.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AuditObservationSource {
    OriginalAttempt,
    RecoveryAttempt {
        recovery_id: String,
        attempt_id: u64,
    },
}

impl AuditObservationSource {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::RecoveryAttempt {
            recovery_id,
            attempt_id,
        } = self
        {
            crate::binary::oid(recovery_id)?;
            if *attempt_id == 0 || *attempt_id > NODE_SAFE_INTEGER_MAX {
                return Err("invalid recovery audit attempt".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AuditTarget {
    Operation {
        operation_id: String,
        attempt_id: u64,
    },
    Recovery {
        recovery_id: String,
        attempt_id: u64,
    },
}

impl AuditTarget {
    pub fn validate(&self) -> Result<(), String> {
        let (id, attempt_id) = match self {
            Self::Operation {
                operation_id,
                attempt_id,
            } => (operation_id, attempt_id),
            Self::Recovery {
                recovery_id,
                attempt_id,
            } => (recovery_id, attempt_id),
        };
        crate::binary::oid(id)?;
        if *attempt_id == 0 || *attempt_id > NODE_SAFE_INTEGER_MAX {
            return Err("invalid audit attempt".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditOperationScope {
    pub actor: String,
    pub repository: String,
    #[serde(deserialize_with = "present_option")]
    pub session: Option<String>,
    #[serde(deserialize_with = "present_option")]
    pub retention_operation: Option<String>,
}

impl AuditOperationScope {
    pub(crate) fn from_original(original: &OperationCredentialRequest) -> Result<Self, String> {
        original.validate()?;
        let (session, retention_operation) = match &original.request.operation {
            Operation::UploadChunk { chunk } | Operation::QueryChunk { chunk } => {
                (Some(chunk.scope.session.clone()), None)
            }
            Operation::ScopedUploadSlice { session, .. }
            | Operation::ApplyScopedUpload { session, .. }
            | Operation::CancelScopedUpload { session, .. }
            | Operation::QueryScopedUpload { session, .. } => (Some(session.clone()), None),
            Operation::ReleaseRetention { command } => {
                let id = match command {
                    RetentionCommand::Prepare { operation, .. }
                    | RetentionCommand::Query { operation }
                    | RetentionCommand::Promote { operation }
                    | RetentionCommand::Close { operation }
                    | RetentionCommand::Read { operation, .. } => operation,
                };
                (None, Some(id.clone()))
            }
            _ => (None, None),
        };
        let scope = Self {
            actor: original.actor.clone(),
            repository: original.request.repository.clone(),
            session,
            retention_operation,
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn validate(&self) -> Result<(), String> {
        repository_component(&self.actor)?;
        repository_name(&self.repository)?;
        if let Some(session) = &self.session {
            crate::binary::oid(session)?;
        }
        if let Some(operation) = &self.retention_operation {
            crate::binary::oid(operation)?;
        }
        if self.session.is_some() && self.retention_operation.is_some() {
            return Err("invalid audit operation scope".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AuditItem {
    Policy {
        receipt: PolicyUpdateReceipt,
        attribution: AuditAttribution,
    },
    Credential {
        operation_id: String,
        scope: AuditOperationScope,
        request_sha256: String,
        attribution: AuditAttribution,
    },
    Recovery {
        recovery_id: String,
        original_operation_id: String,
        scope: AuditOperationScope,
        action: RecoveryAuditAction,
        operator: AuditOperator,
        recorded_at_unix_millis: u64,
    },
    OperationAttempt {
        operation_id: String,
        scope: AuditOperationScope,
        started_at_unix_millis: u64,
        #[serde(deserialize_with = "present_option")]
        observation: Option<AuditRecordedObservation>,
    },
    RecoveryAttempt {
        recovery_id: String,
        original_operation_id: String,
        scope: AuditOperationScope,
        action: RecoveryAuditAction,
        operator: AuditOperator,
        started_at_unix_millis: u64,
        #[serde(deserialize_with = "present_option")]
        observation: Option<AuditRecordedObservation>,
    },
}

impl AuditItem {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Policy {
                receipt,
                attribution,
            } => {
                receipt.validate()?;
                attribution.validate()
            }
            Self::Credential {
                operation_id,
                scope,
                request_sha256,
                attribution,
            } => {
                crate::binary::oid(operation_id)?;
                scope.validate()?;
                crate::binary::oid(request_sha256)?;
                attribution.validate()
            }
            Self::Recovery {
                recovery_id,
                original_operation_id,
                scope,
                operator,
                recorded_at_unix_millis,
                ..
            } => {
                crate::binary::oid(recovery_id)?;
                crate::binary::oid(original_operation_id)?;
                scope.validate()?;
                operator.validate()?;
                if *recorded_at_unix_millis > NODE_SAFE_INTEGER_MAX {
                    return Err("invalid recovery audit time".into());
                }
                Ok(())
            }
            Self::OperationAttempt {
                operation_id,
                scope,
                started_at_unix_millis,
                observation,
            } => {
                crate::binary::oid(operation_id)?;
                scope.validate()?;
                if *started_at_unix_millis > NODE_SAFE_INTEGER_MAX {
                    return Err("invalid operation attempt time".into());
                }
                if let Some(observation) = observation {
                    observation.validate()?;
                }
                Ok(())
            }
            Self::RecoveryAttempt {
                recovery_id,
                original_operation_id,
                scope,
                operator,
                started_at_unix_millis,
                observation,
                ..
            } => {
                crate::binary::oid(recovery_id)?;
                crate::binary::oid(original_operation_id)?;
                scope.validate()?;
                operator.validate()?;
                if *started_at_unix_millis > NODE_SAFE_INTEGER_MAX {
                    return Err("invalid recovery attempt time".into());
                }
                if let Some(observation) = observation {
                    observation.validate()?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSummary {
    pub version: u32,
    pub sequence: u64,
    pub item: AuditItem,
}

impl AuditSummary {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.sequence == 0 || self.sequence > NODE_SAFE_INTEGER_MAX {
            return Err("invalid audit sequence".into());
        }
        self.item.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQueryRequest {
    pub version: u32,
    pub after: u64,
    #[serde(deserialize_with = "present_option")]
    pub high_water: Option<u64>,
    pub limit: u32,
}

impl AuditQueryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.limit == 0
            || self.limit > AUDIT_PAGE_MAX_ITEMS
            || self.after > NODE_SAFE_INTEGER_MAX
            || self
                .high_water
                .is_some_and(|water| water > NODE_SAFE_INTEGER_MAX || self.after > water)
            || (self.high_water.is_none() && self.after != 0)
        {
            return Err("invalid audit page request".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQueryReply {
    pub version: u32,
    pub high_water: u64,
    /// New work stops at this cumulative index watermark. Recovery and policy
    /// revocation still have access; their records remain in high_water.
    pub max_admission_entries: u64,
    pub items: Vec<AuditSummary>,
    #[serde(deserialize_with = "present_option")]
    pub next_after: Option<u64>,
}

impl AuditQueryReply {
    pub fn validate_for(&self, request: &AuditQueryRequest) -> Result<(), String> {
        request.validate()?;
        if self.version != 1
            || self.high_water > NODE_SAFE_INTEGER_MAX
            || self.max_admission_entries == 0
            || self.max_admission_entries > NODE_SAFE_INTEGER_MAX
            || request
                .high_water
                .is_some_and(|water| water != self.high_water)
            || request.after > self.high_water
            || self.items.len() > request.limit as usize
        {
            return Err("invalid audit page header".into());
        }
        let mut last = request.after;
        for item in &self.items {
            item.validate()?;
            if item.sequence != last + 1 || item.sequence > self.high_water {
                return Err("audit page sequence gap".into());
            }
            last = item.sequence;
        }
        let expected_next = if last < self.high_water {
            Some(last)
        } else {
            None
        };
        if self.next_after != expected_next || (self.items.is_empty() && expected_next.is_some()) {
            return Err("invalid audit page continuation".into());
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > AUDIT_PAGE_MAX_BYTES
        {
            return Err("audit page exceeds byte bound".into());
        }
        Ok(())
    }
}
