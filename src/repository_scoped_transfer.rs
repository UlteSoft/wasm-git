//! Persistent ownership and outcome for server upload sessions. The service
//! gate serializes the complete RPC; this ledger also survives process exits.
use super::record_store::{self, Bucket, RecordStore, Seed};
use super::*;
use crate::protocol::{Response, ScopedUploadState, ScopedUploadStatus, UploadBinding};

#[derive(Debug)]
pub(crate) struct ScopedUploadError {
    pub(crate) message: String,
    pub(crate) outcome_unknown: bool,
}

impl From<String> for ScopedUploadError {
    fn from(message: String) -> Self {
        Self {
            message,
            outcome_unknown: false,
        }
    }
}

impl From<&str> for ScopedUploadError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl ScopedUploadError {
    pub(super) fn unknown(message: String) -> Self {
        Self {
            message,
            outcome_unknown: true,
        }
    }
}

impl ScopedReceipt {
    pub(crate) fn success(
        scope: &ScopedExecution,
        updates: &[crate::protocol::RefUpdate],
        rejections: &[Option<String>],
        dry_run: bool,
    ) -> RepositoryResult<Self> {
        use crate::protocol::Operation;
        if updates.len() != rejections.len() || updates.len() != scope.binding.targets.len() {
            return Err("scoped receipt has an invalid update count".into());
        }
        let expected_dry = matches!(
            *scope.binding.operation,
            Operation::CheckBatch {} | Operation::PublishPartial { dry_run: true }
        );
        if dry_run != expected_dry {
            return Err("scoped receipt dry-run mismatch".into());
        }
        for (update, target) in updates.iter().zip(&scope.binding.targets) {
            let reference = format!(
                "refs/{}/{}",
                if matches!(update.namespace, crate::protocol::RefNamespace::Branch) {
                    "heads"
                } else {
                    "tags"
                },
                update.name
            );
            if reference != target.reference || update.force != target.force {
                return Err("scoped receipt target mismatch".into());
            }
            for id in update.expected.iter().chain(&update.new) {
                require_id(id)?;
            }
        }
        if rejections
            .iter()
            .flatten()
            .any(|reason| reason.len() > 65_536)
        {
            return Err("scoped rejection exceeds bounds".into());
        }
        let mut response = Response {
            version: 1,
            ok: true,
            publication_unknown: false,
            error: None,
            state: None,
            tip: None,
        };
        let payload = match scope.binding.operation.as_ref() {
            Operation::Push { expected, .. }
                if updates.len() == 1
                    && rejections[0].is_none()
                    && &updates[0].expected == expected
                    && updates[0].new.is_some() =>
            {
                response.tip = updates[0].new.clone();
                Vec::new()
            }
            Operation::UpdateRef { expected, new, .. }
                if updates.len() == 1
                    && rejections[0].is_none()
                    && &updates[0].expected == expected
                    && &updates[0].new == new =>
            {
                response.tip = updates[0].new.clone();
                Vec::new()
            }
            Operation::PublishBatch {}
            | Operation::PublishOptions {}
            | Operation::CheckBatch {}
                if rejections.iter().all(Option::is_none) =>
            {
                response.tip = Some(scope.binding.digest.clone());
                Vec::new()
            }
            Operation::PublishPartial { .. } => {
                response.tip = Some(scope.binding.digest.clone());
                crate::binary::encode(&crate::protocol::PartialReceipt {
                    rejections: rejections.to_vec(),
                })?
            }
            _ => return Err("invalid scoped success receipt".into()),
        };
        Ok(Self {
            version: 4,
            scope: scope.clone(),
            updates: updates.to_vec(),
            rejections: rejections.to_vec(),
            dry_run,
            aborted: false,
            response,
            payload,
        })
    }

    pub(crate) fn failure(
        scope: &ScopedExecution,
        mut response: Response,
    ) -> RepositoryResult<Self> {
        if response.ok || response.error.is_none() {
            return Err("invalid scoped failure receipt".into());
        }
        response.publication_unknown = false;
        Ok(Self {
            version: 4,
            scope: scope.clone(),
            updates: Vec::new(),
            rejections: Vec::new(),
            dry_run: false,
            aborted: false,
            response,
            payload: Vec::new(),
        })
    }

    pub(crate) fn aborted(scope: &ScopedExecution) -> Self {
        Self {
            version: 4,
            scope: scope.clone(),
            updates: Vec::new(),
            rejections: Vec::new(),
            dry_run: false,
            aborted: true,
            response: Response {
                version: 1,
                ok: false,
                publication_unknown: false,
                error: Some("operation aborted before native decision".into()),
                state: None,
                tip: None,
            },
            payload: Vec::new(),
        }
    }

    pub fn validate(&self) -> RepositoryResult<()> {
        require_id(&self.scope.session)?;
        crate::protocol::repository_name(&self.scope.repository)?;
        self.scope.binding.validate()?;
        if self.version != 4
            || self.scope.actor.is_empty()
            || self.scope.actor.len() > 128
            || self.response.version != 1
            || self.response.publication_unknown
            || self.response.state.is_some()
            || (!self.response.ok && self.response.tip.is_some())
            || self.payload.len() > crate::protocol::MAX_PACKET_BYTES
        {
            return Err("invalid scoped receipt identity or response".into());
        }
        let expected = if self.aborted {
            Self::aborted(&self.scope)
        } else if self.response.ok {
            Self::success(&self.scope, &self.updates, &self.rejections, self.dry_run)?
        } else {
            Self::failure(&self.scope, self.response.clone())?
        };
        if crate::binary::encode(&expected)? != crate::binary::encode(self)? {
            return Err("noncanonical scoped receipt".into());
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScopedRecord {
    pub(super) actor: String,
    pub(super) binding: UploadBinding,
    pub(super) state: ScopedUploadState,
    pub(super) repository: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScopedLedger {
    pub(super) version: u32,
    pub(super) records: BTreeMap<String, ScopedRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyScopedRecord {
    actor: String,
    binding: UploadBinding,
    state: ScopedUploadState,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyScopedLedger {
    version: u32,
    records: BTreeMap<String, LegacyScopedRecord>,
}

fn scoped_bucket(state: &ScopedUploadState) -> Bucket {
    match state {
        ScopedUploadState::Open
        | ScopedUploadState::Applying
        | ScopedUploadState::ApplyingV4 { .. } => Bucket::Active,
        _ => Bucket::Terminal,
    }
}

fn validate_scoped_record(id: &str, bucket: Bucket, bytes: &[u8]) -> RepositoryResult<()> {
    let record: ScopedRecord = crate::binary::decode(bytes)?;
    require_id(id)?;
    if scoped_bucket(&record.state) != bucket || record.actor.is_empty() || record.actor.len() > 128
    {
        return Err("invalid scoped upload record".into());
    }
    record.binding.validate()?;
    if let Some(repository) = &record.repository {
        crate::protocol::repository_name(repository)?;
    }
    if let ScopedUploadState::ApplyingV4 { repository }
    | ScopedUploadState::Aborted { repository } = &record.state
    {
        crate::protocol::repository_name(repository)?;
        if record.repository.as_ref() != Some(repository) {
            return Err("scoped upload repository does not match its state".into());
        }
    }
    if let ScopedUploadState::FinishedV4 { receipt } = &record.state {
        receipt.validate()?;
        if receipt.aborted
            || receipt.scope.session != id
            || receipt.scope.actor != record.actor
            || record.repository.as_ref() != Some(&receipt.scope.repository)
            || crate::binary::encode(&receipt.scope.binding)?
                != crate::binary::encode(&record.binding)?
        {
            return Err("scoped receipt does not match ledger identity".into());
        }
    }
    Ok(())
}

fn compare_scoped_records(
    _id: &str,
    active: &[u8],
    bucket: Bucket,
    terminal: &[u8],
) -> RepositoryResult<()> {
    let old: ScopedRecord = crate::binary::decode(active)?;
    let new: ScopedRecord = crate::binary::decode(terminal)?;
    if bucket != Bucket::Terminal
        || old.actor != new.actor
        || old.repository != new.repository
        || crate::binary::encode(&old.binding)? != crate::binary::encode(&new.binding)?
        || !matches!(
            (&old.state, &new.state),
            (ScopedUploadState::Open, ScopedUploadState::Canceled)
                | (
                    ScopedUploadState::ApplyingV4 { .. },
                    ScopedUploadState::Aborted { .. } | ScopedUploadState::FinishedV4 { .. }
                )
        )
    {
        return Err("conflicting scoped upload transition".into());
    }
    Ok(())
}

impl Repository {
    fn scoped_store(&self) -> RepositoryResult<RecordStore<'_>> {
        RecordStore::open(self, "scoped-uploads", |bytes| {
            let Some(bytes) = bytes else {
                return Ok(Vec::new());
            };
            let ledger = match crate::binary::decode::<ScopedLedger>(bytes) {
                Ok(ledger) if ledger.version == 3 => ledger,
                _ => {
                    let legacy: LegacyScopedLedger = crate::binary::decode(bytes)?;
                    if !matches!(legacy.version, 1 | 2) {
                        return Err("invalid scoped upload ledger version".into());
                    }
                    let mut records = BTreeMap::new();
                    for (id, record) in legacy.records {
                        if legacy.version == 1
                            && matches!(
                                record.state,
                                ScopedUploadState::ApplyingV4 { .. }
                                    | ScopedUploadState::Aborted { .. }
                                    | ScopedUploadState::FinishedV4 { .. }
                            )
                        {
                            return Err("v4 scoped state requires ledger version 2".into());
                        }
                        let repository = match &record.state {
                            ScopedUploadState::ApplyingV4 { repository }
                            | ScopedUploadState::Aborted { repository } => Some(repository.clone()),
                            ScopedUploadState::FinishedV4 { receipt } => {
                                Some(receipt.scope.repository.clone())
                            }
                            _ => None,
                        };
                        records.insert(
                            id,
                            ScopedRecord {
                                actor: record.actor,
                                binding: record.binding,
                                state: record.state,
                                repository,
                            },
                        );
                    }
                    ScopedLedger {
                        version: 3,
                        records,
                    }
                }
            };
            if ledger.records.len() > 4096 {
                return Err("invalid scoped upload ledger".into());
            }
            let mut seeds = Vec::with_capacity(ledger.records.len());
            for (id, record) in ledger.records {
                let bucket = scoped_bucket(&record.state);
                let payload = crate::binary::encode(&record)?;
                validate_scoped_record(&id, bucket, &payload)?;
                seeds.push(Seed {
                    id,
                    bucket,
                    payload,
                });
            }
            Ok(seeds)
        })
    }

    pub(super) fn scoped_record(&self, id: &str) -> RepositoryResult<Option<ScopedRecord>> {
        self.scoped_store()?
            .get(id, validate_scoped_record, compare_scoped_records)?
            .map(|(_, bytes)| crate::binary::decode(&bytes))
            .transpose()
    }

    #[cfg(test)]
    fn scoped_ledger(&self) -> RepositoryResult<ScopedLedger> {
        let store = self.scoped_store()?;
        let mut records = BTreeMap::new();
        for bucket in [Bucket::Active, Bucket::Terminal] {
            store.for_each(
                bucket,
                validate_scoped_record,
                compare_scoped_records,
                |id, bytes| {
                    records.insert(id.into(), crate::binary::decode(bytes)?);
                    Ok(())
                },
            )?;
        }
        Ok(ScopedLedger {
            version: 3,
            records,
        })
    }

    #[cfg(test)]
    fn save_scoped_ledger(&self, ledger: &ScopedLedger) -> RepositoryResult<()> {
        for (id, record) in &ledger.records {
            self.save_scoped_record_checked(id, record, &mut || Ok(()))
                .map_err(|error| error.message)?;
        }
        Ok(())
    }

    /// Snapshot admission usage and retained record totals under the writer and transfer locks.
    pub fn capacity_status(&self) -> RepositoryResult<crate::protocol::RepositoryCapacity> {
        let _writer = self.lock()?;
        let _state = self.lock_named("transfer-state.lock")?;
        let scoped = self
            .scoped_store()?
            .capacity(validate_scoped_record, compare_scoped_records)?;
        let retentions = self.retention_capacity_locked()?;
        let status = crate::protocol::RepositoryCapacity {
            version: 1,
            scoped,
            retentions,
        };
        status.validate()?;
        Ok(status)
    }

    fn save_scoped_record_checked(
        &self,
        id: &str,
        record: &ScopedRecord,
        before_publish: &mut impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), ScopedUploadError> {
        let bytes = crate::binary::encode(record)?;
        self.scoped_store()?
            .write(
                id,
                scoped_bucket(&record.state),
                &bytes,
                validate_scoped_record,
                compare_scoped_records,
                before_publish,
            )
            .map_err(|error| ScopedUploadError {
                message: error.message,
                outcome_unknown: error.outcome_unknown,
            })
    }
    pub(super) fn check_scope(
        record: &ScopedRecord,
        repository: &str,
        actor: &str,
        binding: &UploadBinding,
        allow_legacy_terminal: bool,
    ) -> RepositoryResult<()> {
        let legacy_terminal = allow_legacy_terminal
            && record.repository.is_none()
            && !matches!(record.state, ScopedUploadState::Open);
        if (!legacy_terminal && record.repository.as_deref() != Some(repository))
            || record.actor != actor
            || crate::binary::encode(&record.binding)? != crate::binary::encode(binding)?
        {
            return Err("scoped upload identity conflict".into());
        }
        Ok(())
    }

    // The caller holds transfer-state.lock and has checked any cleanup or
    // existing per-chunk decision relevant to its entry point.
    pub(super) fn ensure_scoped_open_locked(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
        before_publish: &mut impl FnMut() -> RepositoryResult<()>,
        existing_fence_unknown: bool,
    ) -> Result<bool, ScopedUploadError> {
        if let Some(record) = self.scoped_record(session)? {
            Self::check_scope(&record, repository, actor, binding, false)?;
            if !matches!(record.state, ScopedUploadState::Open) {
                return Err("scoped upload is not open".into());
            }
            // A prior Open rename may have succeeded before its directory
            // sync failed; fence it before confirming or extending the state.
            self.scoped_store()?
                .fence(Bucket::Active)
                .map_err(|message| ScopedUploadError {
                    message,
                    outcome_unknown: existing_fence_unknown,
                })?;
            return Ok(false);
        }
        if self.scoped_upload_directory_exists(session)? {
            return Err("upload session already exists outside this scope".into());
        }
        let record = ScopedRecord {
            actor: actor.into(),
            binding: binding.clone(),
            state: ScopedUploadState::Open,
            repository: Some(repository.into()),
        };
        let store = self.scoped_store()?;
        let (count, bytes) = store.active_stats(validate_scoped_record, compare_scoped_records)?;
        let size = store.encoded_len(session, &crate::binary::encode(&record)?)?;
        if count >= record_store::MAX_ACTIVE_RECORDS
            || bytes
                .checked_add(size)
                .is_none_or(|total| total > record_store::MAX_ACTIVE_BYTES)
        {
            return Err("scoped upload admission capacity exceeded".into());
        }
        self.save_scoped_record_checked(session, &record, before_publish)?;
        Ok(true)
    }

    fn check_receipt_state(
        record: &ScopedRecord,
        receipt: &ScopedReceipt,
    ) -> RepositoryResult<bool> {
        Self::check_scope(
            record,
            &receipt.scope.repository,
            &receipt.scope.actor,
            &receipt.scope.binding,
            false,
        )?;
        match &record.state {
            ScopedUploadState::ApplyingV4 { repository }
                if repository == &receipt.scope.repository =>
            {
                Ok(false)
            }
            ScopedUploadState::Aborted { repository }
                if receipt.aborted && repository == &receipt.scope.repository =>
            {
                Ok(true)
            }
            ScopedUploadState::FinishedV4 { receipt: existing }
                if crate::binary::encode(existing.as_ref())? == crate::binary::encode(receipt)? =>
            {
                Ok(true)
            }
            _ => Err("scoped upload state conflicts with durable journal".into()),
        }
    }

    // Validate the identity and state before recovery changes any reference.
    pub(super) fn scoped_check_journal(&self, receipt: &ScopedReceipt) -> RepositoryResult<()> {
        receipt.validate()?;
        let _lock = self.lock_named("transfer-state.lock")?;
        let mut record = self
            .scoped_record(&receipt.scope.session)?
            .ok_or("scoped upload not found")?;
        if !Self::check_receipt_state(&record, receipt)? {
            record.state = if receipt.aborted {
                ScopedUploadState::Aborted {
                    repository: receipt.scope.repository.clone(),
                }
            } else {
                ScopedUploadState::FinishedV4 {
                    receipt: Box::new(receipt.clone()),
                }
            };
            self.scoped_store()?
                .encoded_len(&receipt.scope.session, &crate::binary::encode(&record)?)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn scoped_upload_slice(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
        offset: u64,
        bytes: &[u8],
    ) -> RepositoryResult<()> {
        self.scoped_upload_slice_checked(repository, actor, session, binding, offset, bytes, || {
            Ok(())
        })
        .map_err(|error| error.message)
    }

    #[allow(clippy::too_many_arguments)] // Explicit legacy slice identity and decision callback.
    pub(crate) fn scoped_upload_slice_checked(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
        offset: u64,
        bytes: &[u8],
        mut before_publish: impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), ScopedUploadError> {
        require_id(session)?;
        crate::protocol::repository_name(repository)?;
        binding.validate_slice(offset, bytes.len())?;
        let _lock = self.lock_named("transfer-state.lock")?;
        self.require_live_scoped_chunks_locked(session)?;
        let newly_opened = self.ensure_scoped_open_locked(
            repository,
            actor,
            session,
            binding,
            &mut before_publish,
            true,
        )?;
        self.repair_scoped_upload_directory(session, binding.total)
            .map_err(|message| ScopedUploadError {
                message,
                outcome_unknown: newly_opened,
            })?;
        self.upload_slice_checked(session, binding.total, offset, bytes, &mut before_publish)
            .map_err(|mut error| {
                error.outcome_unknown |= newly_opened;
                error
            })
    }

    pub(crate) fn scoped_upload_read(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
    ) -> RepositoryResult<Vec<u8>> {
        require_id(session)?;
        crate::protocol::repository_name(repository)?;
        binding.validate()?;
        let _lock = self.lock_named("transfer-state.lock")?;
        let record = self
            .scoped_record(session)?
            .ok_or("scoped upload not found")?;
        Self::check_scope(&record, repository, actor, binding, false)?;
        if !matches!(record.state, ScopedUploadState::Open) {
            return Err("scoped upload is not open".into());
        }
        self.scoped_store()?.fence(Bucket::Active)?;
        {
            let _transfer = self.lock_named("transfer.lock")?;
            self.recover_scoped_chunks_locked(repository, actor, session, binding)?;
        }
        let bytes = self.read_upload_staged(session)?;
        binding.validate_payload(&bytes)?;
        Ok(bytes)
    }

    #[cfg(test)]
    pub(crate) fn scoped_upload_applying(
        &self,
        gate: &ServiceGuard,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
    ) -> RepositoryResult<()> {
        self.scoped_upload_applying_checked(gate, repository, actor, session, binding, || Ok(()))
            .map_err(|error| error.message)
    }

    pub(crate) fn scoped_upload_applying_checked(
        &self,
        gate: &ServiceGuard,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
        mut before_publish: impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), ScopedUploadError> {
        crate::protocol::repository_name(repository)?;
        if gate.common_metadata != self.common_metadata {
            return Err("service guard belongs to another repository".into());
        }
        let _lock = self.lock_named("transfer-state.lock")?;
        self.require_live_scoped_chunks_locked(session)?;
        let mut record = self
            .scoped_record(session)?
            .ok_or("scoped upload not found")?;
        Self::check_scope(&record, repository, actor, binding, false)?;
        if !matches!(record.state, ScopedUploadState::Open) {
            return Err("scoped upload is not open".into());
        }
        record.state = ScopedUploadState::ApplyingV4 {
            repository: repository.into(),
        };
        self.save_scoped_record_checked(session, &record, &mut before_publish)
    }

    // Called only while the repository writer lock and its v4 journal are held.
    pub(super) fn scoped_finish_from_journal(
        &self,
        receipt: &ScopedReceipt,
    ) -> RepositoryResult<()> {
        receipt.validate()?;
        let _lock = self.lock_named("transfer-state.lock")?;
        let mut record = self
            .scoped_record(&receipt.scope.session)?
            .ok_or("scoped upload not found")?;
        if Self::check_receipt_state(&record, receipt)? {
            self.scoped_store()?.fence(scoped_bucket(&record.state))?;
            return Ok(());
        }
        record.state = if receipt.aborted {
            ScopedUploadState::Aborted {
                repository: receipt.scope.repository.clone(),
            }
        } else {
            ScopedUploadState::FinishedV4 {
                receipt: Box::new(receipt.clone()),
            }
        };
        self.save_scoped_record_checked(&receipt.scope.session, &record, &mut || Ok(()))
            .map_err(|error| error.message)
    }

    // The caller holds writer lock, after recovering any pending journal.
    pub(super) fn scoped_finished_under_writer(
        &self,
        scope: &ScopedExecution,
    ) -> RepositoryResult<Option<(Response, Vec<u8>)>> {
        let _lock = self.lock_named("transfer-state.lock")?;
        let record = self
            .scoped_record(&scope.session)?
            .ok_or("scoped upload not found")?;
        Self::check_scope(
            &record,
            &scope.repository,
            &scope.actor,
            &scope.binding,
            false,
        )?;
        match &record.state {
            ScopedUploadState::ApplyingV4 { repository } if repository == &scope.repository => {
                Ok(None)
            }
            ScopedUploadState::Finished { response, payload } => {
                Ok(Some((response.clone(), payload.clone())))
            }
            ScopedUploadState::FinishedV4 { receipt }
                if receipt.scope.repository == scope.repository =>
            {
                Ok(Some((receipt.response.clone(), receipt.payload.clone())))
            }
            _ => Err("scoped upload has no v4 operation in progress".into()),
        }
    }

    pub(super) fn scoped_upload_directory_exists(&self, session: &str) -> RepositoryResult<bool> {
        let path = self
            .common_metadata
            .join("tmp")
            .join(format!("upload-{session}"));
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("inspect scoped upload directory", &path, error)),
        }
    }

    pub(crate) fn scoped_upload_cleanup(&self, session: &str) -> RepositoryResult<()> {
        require_id(session)?;
        let _state = self.lock_named("transfer-state.lock")?;
        let record = self.scoped_record(session)?;
        let reason = match record.as_ref().map(|record| &record.state) {
            Some(
                ScopedUploadState::Finished { .. }
                | ScopedUploadState::FinishedV4 { .. }
                | ScopedUploadState::Aborted { .. },
            ) => crate::protocol::ScopedChunkCleanupReason::Completed,
            Some(ScopedUploadState::Canceled) => {
                crate::protocol::ScopedChunkCleanupReason::Canceled
            }
            _ => return Err("scoped upload is not terminal; cleanup is not authorized".into()),
        };
        self.scoped_store()?.fence(Bucket::Terminal)?;
        let _transfer = self.lock_named("transfer.lock")?;
        self.cleanup_scoped_chunks_locked(session, reason)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn scoped_upload_cancel(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
    ) -> RepositoryResult<()> {
        self.scoped_upload_cancel_checked(repository, actor, session, binding, || Ok(()))
            .map_err(|error| error.message)
    }

    pub(crate) fn scoped_upload_cancel_checked(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
        mut before_publish: impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), ScopedUploadError> {
        require_id(session)?;
        crate::protocol::repository_name(repository)?;
        binding.validate()?;
        let _lock = self.lock_named("transfer-state.lock")?;
        let mut record = self
            .scoped_record(session)
            .map_err(ScopedUploadError::unknown)?;
        let needs_publish = if let Some(record) = record.as_mut() {
            Self::check_scope(record, repository, actor, binding, false)?;
            match record.state {
                ScopedUploadState::Open => {
                    record.state = ScopedUploadState::Canceled;
                    true
                }
                ScopedUploadState::Canceled => false,
                _ => {
                    return Err(
                        "scoped upload outcome must be recovered before cancellation".into(),
                    )
                }
            }
        } else {
            if self.scoped_upload_directory_exists(session)? {
                return Err("upload session already exists outside this scope".into());
            }
            record = Some(ScopedRecord {
                actor: actor.into(),
                binding: binding.clone(),
                state: ScopedUploadState::Canceled,
                repository: Some(repository.into()),
            });
            true
        };
        if needs_publish {
            self.save_scoped_record_checked(
                session,
                record.as_ref().ok_or("missing cancellation record")?,
                &mut before_publish,
            )?;
        } else {
            // Never remove chunks until a previously published cancellation
            // tombstone has a completed directory durability fence.
            self.scoped_store()?
                .fence(Bucket::Terminal)
                .map_err(ScopedUploadError::unknown)?;
        }
        let _transfer = self
            .lock_named("transfer.lock")
            .map_err(ScopedUploadError::unknown)?;
        self.cleanup_scoped_chunks_locked(
            session,
            crate::protocol::ScopedChunkCleanupReason::Canceled,
        )
        .map_err(ScopedUploadError::unknown)?;
        Ok(())
    }

    pub(crate) fn scoped_upload_status(
        &self,
        gate: &ServiceGuard,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
    ) -> RepositoryResult<ScopedUploadStatus> {
        self.scoped_upload_status_inner(gate, repository, actor, session, binding, true)
    }

    /// Recover a permitted durable journal, but never invent a terminal decision.
    pub(crate) fn scoped_upload_observe(
        &self,
        gate: &ServiceGuard,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
    ) -> RepositoryResult<ScopedUploadStatus> {
        self.scoped_upload_status_inner(gate, repository, actor, session, binding, false)
    }

    fn scoped_upload_status_inner(
        &self,
        gate: &ServiceGuard,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &UploadBinding,
        abort_interrupted: bool,
    ) -> RepositoryResult<ScopedUploadStatus> {
        require_id(session)?;
        crate::protocol::repository_name(repository)?;
        binding.validate()?;
        if gate.common_metadata != self.common_metadata {
            return Err("service guard belongs to another repository".into());
        }
        // The guard excludes every in-flight service worker. Acquiring the
        // writer lock first rolls forward a pending v4 decision before reading
        // the ledger. Only a v4 Applying with no journal can then be aborted.
        let writer = self.lock()?;
        let _lock = self.lock_named("transfer-state.lock")?;
        let record = self.scoped_record(session)?;
        let state = match record.as_ref() {
            Some(record) => {
                // Legacy records without repository identity are readable by
                // the old status API, but cannot authorize restricted recovery.
                Self::check_scope(record, repository, actor, binding, abort_interrupted)?;
                match &record.state {
                    ScopedUploadState::ApplyingV4 { repository: actual }
                    | ScopedUploadState::Aborted { repository: actual }
                        if actual != repository =>
                    {
                        return Err("scoped upload repository conflict".into())
                    }
                    ScopedUploadState::FinishedV4 { receipt }
                        if receipt.scope.repository != repository =>
                    {
                        return Err("scoped upload repository conflict".into())
                    }
                    _ => (),
                }
                Some(record.state.clone())
            }
            None => None,
        };
        // State reads must not turn an uncertain earlier rename into a
        // confirmed status until the ledger's directory is synchronized.
        if let Some(record) = &record {
            self.scoped_store()?.fence(scoped_bucket(&record.state))?;
        }
        drop(_lock);
        if abort_interrupted && matches!(state, Some(ScopedUploadState::ApplyingV4 { .. })) {
            let scope = ScopedExecution {
                actor: actor.into(),
                repository: repository.into(),
                session: session.into(),
                binding: binding.clone(),
            };
            self.scoped_record_receipt_locked(writer, ScopedReceipt::aborted(&scope))?;
            return Ok(ScopedUploadStatus {
                version: crate::protocol::SCOPED_UPLOAD_STATUS_VERSION,
                state: Some(ScopedUploadState::Aborted {
                    repository: repository.into(),
                }),
            });
        }
        Ok(ScopedUploadStatus {
            version: crate::protocol::SCOPED_UPLOAD_STATUS_VERSION,
            state,
        })
    }

    /// Close an interrupted v4 apply only after the writer lock establishes
    /// there is no committed journal, while the service gate excludes workers.
    pub(crate) fn scoped_upload_abort_checked(
        &self,
        gate: &ServiceGuard,
        scope: &ScopedExecution,
        mut before_publish: impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), ScopedUploadError> {
        require_id(&scope.session)?;
        crate::protocol::repository_name(&scope.repository)?;
        scope.binding.validate()?;
        if gate.common_metadata != self.common_metadata {
            return Err("service guard belongs to another repository".into());
        }
        let _writer = self.lock().map_err(ScopedUploadError::unknown)?;
        let _state = self.lock_named("transfer-state.lock")?;
        let mut record = self
            .scoped_record(&scope.session)
            .map_err(ScopedUploadError::unknown)?
            .ok_or("scoped upload not found")?;
        Self::check_scope(
            &record,
            &scope.repository,
            &scope.actor,
            &scope.binding,
            false,
        )?;
        if !matches!(&record.state, ScopedUploadState::ApplyingV4 { repository } if repository == &scope.repository)
        {
            return Err("scoped upload is not an interrupted v4 apply".into());
        }
        record.state = ScopedUploadState::Aborted {
            repository: scope.repository.clone(),
        };
        self.save_scoped_record_checked(&scope.session, &record, &mut before_publish)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Operation, ScopedUploadState, UploadTarget};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "wasm-git-scoped-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn observing_interrupted_apply_does_not_abort_until_authorized_close() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let payload = b"frozen intent";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let scope = ScopedExecution {
            actor: "alice".into(),
            repository: "project".into(),
            session: "d".repeat(64),
            binding,
        };
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_upload_slice(
            &scope.repository,
            &scope.actor,
            &scope.session,
            &scope.binding,
            0,
            payload,
        )
        .unwrap();
        repo.scoped_upload_applying(
            &gate,
            &scope.repository,
            &scope.actor,
            &scope.session,
            &scope.binding,
        )
        .unwrap();
        let observe = || {
            repo.scoped_upload_observe(
                &gate,
                &scope.repository,
                &scope.actor,
                &scope.session,
                &scope.binding,
            )
            .unwrap()
        };
        assert!(matches!(
            observe().state,
            Some(ScopedUploadState::ApplyingV4 { .. })
        ));
        let failure = repo
            .scoped_upload_abort_checked(&gate, &scope, || Err("authorization expired".into()))
            .unwrap_err();
        assert!(!failure.outcome_unknown);
        assert!(matches!(
            observe().state,
            Some(ScopedUploadState::ApplyingV4 { .. })
        ));
        repo.scoped_upload_abort_checked(&gate, &scope, || Ok(()))
            .unwrap();
        assert!(matches!(
            observe().state,
            Some(ScopedUploadState::Aborted { .. })
        ));
    }

    #[test]
    fn applying_intent_checks_authority_at_publication_and_recovers_without_it() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        let payload = b"frozen intent";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let session = "c".repeat(64);
        repo.scoped_upload_slice("project", "alice", &session, &binding, 0, payload)
            .unwrap();
        let path = repo
            .common_metadata
            .join("scoped-uploads-records/active")
            .join(&session);
        let before = fs::read(&path).unwrap();
        let error = repo
            .scoped_upload_applying_checked(&gate, "project", "alice", &session, &binding, || {
                assert_eq!(fs::read(&path).unwrap(), before);
                assert!(fs::read_dir(repo.common_metadata.join("tmp"))
                    .unwrap()
                    .any(|entry| {
                        entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with("ref-")
                    }));
                Err("authorization expired".into())
            })
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(matches!(
            &repo.scoped_ledger().unwrap().records[&session].state,
            ScopedUploadState::Open
        ));

        repo.scoped_upload_applying_checked(&gate, "project", "alice", &session, &binding, || {
            Ok(())
        })
        .unwrap();
        drop(gate);
        let reopened = Repository::open(&temp.0).unwrap();
        let gate = reopened.service_lock(std::time::Duration::ZERO).unwrap();
        assert!(matches!(
            reopened
                .scoped_upload_status(&gate, "project", "alice", &session, &binding)
                .unwrap()
                .state,
            Some(ScopedUploadState::Aborted { .. })
        ));
    }

    #[test]
    fn authorization_fences_new_upload_and_cancel_decisions_but_not_replays() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let payload = b"frozen chunk";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let session = "a".repeat(64);
        let ledger_path = repo
            .common_metadata
            .join("scoped-uploads-records/active")
            .join(&session);
        let directory = repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{session}"));
        let error = repo
            .scoped_upload_slice_checked("project", "alice", &session, &binding, 0, payload, || {
                assert!(!ledger_path.exists());
                assert!(fs::read_dir(repo.common_metadata.join("tmp"))
                    .unwrap()
                    .next()
                    .is_some());
                Err("authorization expired".into())
            })
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(error.message, "authorization expired");
        assert!(!ledger_path.exists());
        assert!(!directory.exists());

        let mut checks = 0;
        let error = repo
            .scoped_upload_slice_checked("project", "alice", &session, &binding, 0, payload, || {
                checks += 1;
                if checks == 1 {
                    Ok(())
                } else {
                    Err("authorization expired".into())
                }
            })
            .unwrap_err();
        assert_eq!(checks, 2);
        assert!(error.outcome_unknown);
        assert!(matches!(
            &repo.scoped_ledger().unwrap().records[&session].state,
            ScopedUploadState::Open
        ));
        assert!(!directory.join("0").exists());

        let mut checks = 0;
        repo.scoped_upload_slice_checked(
            "project",
            "alice",
            &session,
            &binding,
            0,
            payload,
            || {
                checks += 1;
                assert!(!directory.join("0").exists());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(checks, 1);
        assert_eq!(fs::read(directory.join("0")).unwrap(), payload);
        repo.scoped_upload_slice_checked(
            "project",
            "alice",
            &session,
            &binding,
            0,
            payload,
            || panic!("an exact slice replay must not publish again"),
        )
        .unwrap();

        let before = fs::read(&ledger_path).unwrap();
        let absent = "b".repeat(64);
        let error = repo
            .scoped_upload_cancel_checked("project", "alice", &absent, &binding, || {
                Err("authorization expired".into())
            })
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(fs::read(&ledger_path).unwrap(), before);
        let error = repo
            .scoped_upload_cancel_checked("project", "alice", &session, &binding, || {
                assert_eq!(fs::read(&ledger_path).unwrap(), before);
                Err("authorization expired".into())
            })
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert!(directory.join("0").exists());
        assert_eq!(fs::read(&ledger_path).unwrap(), before);

        fs::write(directory.join("foreign"), b"prevent cleanup").unwrap();
        let error = repo
            .scoped_upload_cancel_checked("project", "alice", &session, &binding, || Ok(()))
            .unwrap_err();
        assert!(error.outcome_unknown);
        assert!(matches!(
            &repo.scoped_ledger().unwrap().records[&session].state,
            ScopedUploadState::Canceled
        ));
        let reopened = Repository::open(&temp.0).unwrap();
        let error = reopened
            .scoped_upload_cancel_checked("project", "alice", &session, &binding, || {
                panic!("canceled replay is cleanup of an existing decision")
            })
            .unwrap_err();
        assert!(error.outcome_unknown);
        fs::remove_file(directory.join("foreign")).unwrap();
        reopened
            .scoped_upload_cancel_checked("project", "alice", &session, &binding, || {
                panic!("canceled replay must not reauthorize cleanup")
            })
            .unwrap();
        assert!(!directory.exists());
        assert!(matches!(
            &reopened.scoped_ledger().unwrap().records[&session].state,
            ScopedUploadState::Canceled
        ));
    }

    #[test]
    fn session_owner_targets_cancel_and_unknown_outcome_survive_restart() {
        let t = Temp::new();
        let repo = Repository::init(&t.0).unwrap();
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        let id = "1".repeat(64);
        let payload = b"a frozen bundle";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "a".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let other_binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "b".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        repo.scoped_upload_slice("project", "alice", &id, &binding, 0, payload)
            .unwrap();
        assert!(repo
            .scoped_upload_slice("other", "alice", &id, &binding, 0, payload)
            .is_err());
        assert!(repo
            .scoped_upload_read("other", "alice", &id, &binding)
            .is_err());
        assert!(repo
            .scoped_upload_cancel("other", "alice", &id, &binding)
            .is_err());
        assert!(repo
            .scoped_upload_status(&gate, "other", "alice", &id, &binding)
            .is_err());
        assert!(repo
            .scoped_upload_slice("project", "bob", &id, &binding, 0, payload)
            .is_err());
        assert!(repo
            .scoped_upload_read("project", "bob", &id, &binding)
            .is_err());
        assert!(repo
            .scoped_upload_cancel("project", "bob", &id, &binding)
            .is_err());
        assert!(repo
            .scoped_upload_status(&gate, "project", "alice", &id, &other_binding)
            .is_err());
        assert_eq!(
            repo.scoped_upload_read("project", "alice", &id, &binding)
                .unwrap(),
            payload
        );
        repo.scoped_upload_applying(&gate, "project", "alice", &id, &binding)
            .unwrap();
        let reopened = Repository::open(&t.0).unwrap();
        assert!(reopened
            .scoped_upload_status(&gate, "other", "alice", &id, &binding)
            .is_err());
        assert!(matches!(
            reopened
                .scoped_upload_status(&gate, "project", "alice", &id, &binding)
                .unwrap()
                .state,
            Some(ScopedUploadState::Aborted { .. })
        ));
        assert!(reopened
            .scoped_upload_read("project", "alice", &id, &binding)
            .is_err());
        assert!(reopened
            .scoped_upload_cancel("project", "alice", &id, &binding)
            .is_err());
        let closed = "2".repeat(64);
        reopened
            .scoped_upload_cancel("project", "alice", &closed, &binding)
            .unwrap();
        assert!(reopened
            .scoped_upload_slice("project", "alice", &closed, &binding, 0, payload)
            .is_err());
        let interrupted = "3".repeat(64);
        let mut ledger = reopened.scoped_ledger().unwrap();
        ledger.records.insert(
            interrupted.clone(),
            ScopedRecord {
                actor: "alice".into(),
                binding: binding.clone(),
                state: ScopedUploadState::Open,
                repository: Some("project".into()),
            },
        );
        reopened.save_scoped_ledger(&ledger).unwrap();
        fs::create_dir(
            reopened
                .common_metadata
                .join("tmp")
                .join(format!("upload-{interrupted}")),
        )
        .unwrap();
        Repository::open(&t.0)
            .unwrap()
            .scoped_upload_slice("project", "alice", &interrupted, &binding, 0, payload)
            .unwrap();
        assert_eq!(
            reopened
                .scoped_upload_read("project", "alice", &interrupted, &binding)
                .unwrap(),
            payload
        );
        let legacy = "4".repeat(64);
        reopened
            .scoped_upload_slice("project", "alice", &legacy, &binding, 0, payload)
            .unwrap();
        let mut ledger = reopened.scoped_ledger().unwrap();
        ledger.records.get_mut(&legacy).unwrap().state = ScopedUploadState::Applying;
        reopened.save_scoped_ledger(&ledger).unwrap();
        assert!(matches!(
            reopened
                .scoped_upload_status(&gate, "project", "alice", &legacy, &binding)
                .unwrap()
                .state,
            Some(ScopedUploadState::Applying)
        ));
    }

    #[test]
    fn legacy_applying_is_not_reinterpreted_when_ledger_upgrades() {
        let t = Temp::new();
        let repo = Repository::init(&t.0).unwrap();
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        let payload = b"legacy";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "a".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let old = "a".repeat(64);
        let old_open = "c".repeat(64);
        let ledger = LegacyScopedLedger {
            version: 1,
            records: BTreeMap::from([
                (
                    old.clone(),
                    LegacyScopedRecord {
                        actor: "alice".into(),
                        binding: binding.clone(),
                        state: ScopedUploadState::Applying,
                    },
                ),
                (
                    old_open.clone(),
                    LegacyScopedRecord {
                        actor: "alice".into(),
                        binding: binding.clone(),
                        state: ScopedUploadState::Open,
                    },
                ),
            ]),
        };
        repo.atomic_write(
            &repo.common_metadata.join("scoped-uploads.bin"),
            &crate::binary::encode(&ledger).unwrap(),
        )
        .unwrap();
        let state = repo
            .scoped_upload_status(&gate, "project", "alice", &old, &binding)
            .unwrap();
        assert_eq!(state.version, crate::protocol::SCOPED_UPLOAD_STATUS_VERSION);
        assert!(matches!(state.state, Some(ScopedUploadState::Applying)));
        assert!(repo
            .scoped_upload_slice("project", "alice", &old_open, &binding, 0, payload)
            .is_err());
        assert!(repo
            .scoped_upload_cancel("project", "alice", &old_open, &binding)
            .is_err());
        let new = "b".repeat(64);
        repo.scoped_upload_slice("project", "alice", &new, &binding, 0, payload)
            .unwrap();
        assert_eq!(repo.scoped_ledger().unwrap().version, 3);
        assert!(matches!(
            repo.scoped_upload_status(&gate, "project", "alice", &old, &binding)
                .unwrap()
                .state,
            Some(ScopedUploadState::Applying)
        ));
    }

    #[test]
    fn active_watermark_never_blocks_tombstones_or_reuses_terminal_ids() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let payload = b"watermark";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let record = ScopedRecord {
            actor: "alice".into(),
            binding: binding.clone(),
            state: ScopedUploadState::Open,
            repository: Some("project".into()),
        };
        let store = repo.scoped_store().unwrap();
        let encoded = crate::binary::encode(&record).unwrap();
        for index in 0..record_store::MAX_ACTIVE_RECORDS {
            store
                .seed_fixture(&format!("{index:064x}"), Bucket::Active, &encoded)
                .unwrap();
        }
        let full = repo.capacity_status().unwrap();
        assert_eq!(full.scoped.active_records, record_store::MAX_ACTIVE_RECORDS);
        assert_eq!(full.scoped.total_records, record_store::MAX_ACTIVE_RECORDS);
        assert!(full.scoped.active_bytes > 0);
        let next = "a".repeat(64);
        assert!(repo
            .scoped_upload_slice("project", "alice", &next, &binding, 0, payload)
            .unwrap_err()
            .contains("admission capacity"));
        let canceled = "b".repeat(64);
        repo.scoped_upload_cancel("project", "alice", &canceled, &binding)
            .unwrap();
        assert!(repo
            .scoped_upload_slice("project", "alice", &canceled, &binding, 0, payload)
            .is_err());
        let first = format!("{:064x}", 0);
        repo.scoped_upload_cancel("project", "alice", &first, &binding)
            .unwrap();
        repo.scoped_upload_slice("project", "alice", &next, &binding, 0, payload)
            .unwrap();
        let after = repo.capacity_status().unwrap();
        assert_eq!(
            after.scoped.active_records,
            record_store::MAX_ACTIVE_RECORDS
        );
        assert_eq!(
            after.scoped.total_records,
            record_store::MAX_ACTIVE_RECORDS + 2
        );
        assert!(after.scoped.total_bytes > after.scoped.active_bytes);
        let reopened = Repository::open(&temp.0).unwrap();
        assert!(matches!(
            reopened.scoped_record(&canceled).unwrap().unwrap().state,
            ScopedUploadState::Canceled
        ));
        assert!(matches!(
            reopened.scoped_record(&first).unwrap().unwrap().state,
            ScopedUploadState::Canceled
        ));
    }

    #[test]
    fn active_byte_watermark_rejects_admission_but_not_cancellation() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let payload = b"x";
        let binding = UploadBinding {
            version: 1,
            digest: format!("{:x}", Sha256::digest(payload)),
            total: 1,
            operation: Box::new(Operation::PublishBatch {}),
            targets: vec![
                UploadTarget {
                    reference: format!("refs/heads/{}", "a".repeat(128)),
                    force: false,
                };
                4096
            ],
        };
        binding.validate().unwrap();
        let record = ScopedRecord {
            actor: "alice".into(),
            binding: binding.clone(),
            state: ScopedUploadState::Open,
            repository: Some("project".into()),
        };
        let encoded = crate::binary::encode(&record).unwrap();
        let store = repo.scoped_store().unwrap();
        let size = store.encoded_len(&format!("{:064x}", 0), &encoded).unwrap();
        let count = record_store::MAX_ACTIVE_BYTES / size;
        assert!(count > 0 && count < record_store::MAX_ACTIVE_RECORDS);
        for index in 0..count {
            store
                .seed_fixture(&format!("{index:064x}"), Bucket::Active, &encoded)
                .unwrap();
        }
        let next = "a".repeat(64);
        let full = repo.capacity_status().unwrap();
        assert!(full.scoped.active_bytes + size > full.scoped.max_active_bytes);
        assert!(repo
            .scoped_upload_slice("project", "alice", &next, &binding, 0, payload)
            .unwrap_err()
            .contains("admission capacity"));
        let closed = "b".repeat(64);
        repo.scoped_upload_cancel("project", "alice", &closed, &binding)
            .unwrap();
        assert!(
            repo.capacity_status().unwrap().scoped.total_bytes > record_store::MAX_ACTIVE_BYTES
        );
        repo.scoped_upload_cancel("project", "alice", &format!("{:064x}", 0), &binding)
            .unwrap();
        repo.scoped_upload_slice("project", "alice", &next, &binding, 0, payload)
            .unwrap();
        assert!(matches!(
            repo.scoped_record(&closed).unwrap().unwrap().state,
            ScopedUploadState::Canceled
        ));
    }

    #[test]
    fn interrupted_terminal_publication_reconciles_only_matching_identity() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            b"identity",
        )
        .unwrap();
        let store = repo.scoped_store().unwrap();
        let id = "d".repeat(64);
        let active = ScopedRecord {
            actor: "alice".into(),
            binding: binding.clone(),
            state: ScopedUploadState::Open,
            repository: Some("project".into()),
        };
        let mut terminal = active.clone();
        terminal.state = ScopedUploadState::Canceled;
        store
            .seed_fixture(
                &id,
                Bucket::Active,
                &crate::binary::encode(&active).unwrap(),
            )
            .unwrap();
        store
            .seed_fixture(
                &id,
                Bucket::Terminal,
                &crate::binary::encode(&terminal).unwrap(),
            )
            .unwrap();
        assert!(matches!(
            repo.scoped_record(&id).unwrap().unwrap().state,
            ScopedUploadState::Canceled
        ));
        assert!(!repo
            .common_metadata
            .join("scoped-uploads-records/active")
            .join(&id)
            .exists());

        let conflict = "e".repeat(64);
        terminal.actor = "mallory".into();
        store
            .seed_fixture(
                &conflict,
                Bucket::Active,
                &crate::binary::encode(&active).unwrap(),
            )
            .unwrap();
        store
            .seed_fixture(
                &conflict,
                Bucket::Terminal,
                &crate::binary::encode(&terminal).unwrap(),
            )
            .unwrap();
        assert!(repo.scoped_record(&conflict).is_err());
        assert!(repo
            .common_metadata
            .join("scoped-uploads-records/active")
            .join(&conflict)
            .exists());
    }

    #[test]
    fn missing_marker_preserves_existing_record_directory() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            b"frozen",
        )
        .unwrap();
        let id = "f".repeat(64);
        repo.scoped_upload_cancel("project", "alice", &id, &binding)
            .unwrap();
        let terminal = repo
            .common_metadata
            .join("scoped-uploads-records/terminal")
            .join(&id);
        fs::remove_file(repo.common_metadata.join("scoped-uploads.bin")).unwrap();
        assert!(repo.scoped_record(&id).is_err());
        assert!(terminal.exists());
    }

    #[test]
    fn oversized_failure_finishes_without_reference_journal() {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let payload = b"frozen";
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let session = "c".repeat(64);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_upload_slice("project", "alice", &session, &binding, 0, payload)
            .unwrap();
        repo.scoped_upload_applying(&gate, "project", "alice", &session, &binding)
            .unwrap();
        let scope = ScopedExecution {
            actor: "alice".into(),
            repository: "project".into(),
            session: session.clone(),
            binding: binding.clone(),
        };
        let response = Response {
            version: 1,
            ok: false,
            publication_unknown: true,
            error: Some("错".repeat(12_000)),
            state: None,
            tip: None,
        };
        let (reply, _) = repo.scoped_record_failure(&gate, &scope, response).unwrap();
        assert!(!reply.publication_unknown);
        assert!(reply.error.as_ref().unwrap().len() <= 32 * 1024 + 3);
        crate::protocol::write_packet(&mut Vec::new(), &reply, &[]).unwrap();
        assert!(!repo.common_metadata.join("ref-transaction.bin").exists());
        assert!(matches!(
            repo.scoped_upload_status(&gate, "project", "alice", &session, &binding)
                .unwrap()
                .state,
            Some(ScopedUploadState::FinishedV4 { .. })
        ));
    }
}
