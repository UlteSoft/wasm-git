use serde::{Deserialize, Serialize};

/// Encoded record bytes and counts; terminal records remain in the totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionCapacity {
    pub active_records: u64,
    pub active_bytes: u64,
    pub total_records: u64,
    pub total_bytes: u64,
    pub max_active_records: u64,
    pub max_active_bytes: u64,
}

impl AdmissionCapacity {
    pub fn validate(&self) -> Result<(), String> {
        let values = [
            self.active_records,
            self.active_bytes,
            self.total_records,
            self.total_bytes,
            self.max_active_records,
            self.max_active_bytes,
        ];
        if values
            .iter()
            .any(|value| *value > super::NODE_SAFE_INTEGER_MAX)
            || self.max_active_records == 0
            || self.max_active_bytes == 0
            || self.active_records > self.total_records
            || self.active_bytes > self.total_bytes
        {
            return Err("invalid repository admission capacity".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryCapacity {
    pub version: u32,
    pub scoped: AdmissionCapacity,
    pub retentions: AdmissionCapacity,
}

impl RepositoryCapacity {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported repository capacity version".into());
        }
        self.scoped.validate()?;
        self.retentions.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_integers_without_hiding_over_watermark_usage() {
        let mut usage = AdmissionCapacity {
            active_records: 2,
            active_bytes: 3,
            total_records: 2,
            total_bytes: 3,
            max_active_records: 1,
            max_active_bytes: 1,
        };
        let mut status = RepositoryCapacity {
            version: 1,
            scoped: usage.clone(),
            retentions: usage.clone(),
        };
        assert!(status.validate().is_ok());
        usage.active_records = 3;
        assert!(usage.validate().is_err());
        usage.active_records = 2;
        usage.total_bytes = super::super::NODE_SAFE_INTEGER_MAX + 1;
        assert!(usage.validate().is_err());
        status.version = 2;
        assert!(status.validate().is_err());
        status.version = 1;
        status.retentions.max_active_bytes = 0;
        assert!(status.validate().is_err());
    }
}
