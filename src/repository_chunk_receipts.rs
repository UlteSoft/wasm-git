//! Durable per-slot decisions for scoped upload chunks.
use super::scoped_transfer_storage::{ScopedRecord, ScopedUploadError};
use super::*;
use crate::protocol::{
    ScopedChunkCleanupReason, ScopedChunkData, ScopedChunkIdentity, ScopedChunkState,
    ScopedChunkStatus, ScopedUploadState, TRANSFER_CHUNK,
};

const MAX_RECORD: u64 = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    version: u32,
    session: String,
    offset: u64,
    operation_id: String,
    payload_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cleanup {
    version: u32,
    session: String,
    reason: ScopedChunkCleanupReason,
    cleaned: bool,
}

fn read_record<T: serde::de::DeserializeOwned + Serialize>(
    path: &Path,
) -> RepositoryResult<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect chunk record", path, error)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_RECORD {
        return Err(format!(
            "chunk record must be a bounded regular file: {}",
            path.display()
        ));
    }
    let bytes = read_regular(path)?;
    let record: T = crate::binary::decode(&bytes)?;
    if crate::binary::encode(&record)? != bytes {
        return Err("noncanonical chunk record".into());
    }
    Ok(Some(record))
}

impl Repository {
    fn existing_chunk_directory(&self, session: &str) -> RepositoryResult<Option<PathBuf>> {
        require_id(session)?;
        let root = self.common_metadata.join("scoped-chunks");
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("inspect chunk directory", &root, error)),
            Ok(_) => require_directory(&root)?,
        }
        let directory = root.join(session);
        match fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("inspect chunk directory", &directory, error)),
            Ok(_) => {
                require_directory(&directory)?;
                Ok(Some(directory))
            }
        }
    }

    fn ensure_chunk_directory(&self, session: &str) -> RepositoryResult<PathBuf> {
        require_id(session)?;
        let root = self.common_metadata.join("scoped-chunks");
        match fs::create_dir(&root) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                require_directory(&root)?
            }
            Err(error) => return Err(io_error("create chunk directory", &root, error)),
        }
        // Creation may have succeeded on an earlier attempt whose parent sync
        // failed; fence even when the directory already exists.
        sync_directory(&self.common_metadata)?;
        let directory = root.join(session);
        match fs::create_dir(&directory) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                require_directory(&directory)?
            }
            Err(error) => {
                return Err(io_error(
                    "create chunk session directory",
                    &directory,
                    error,
                ))
            }
        }
        sync_directory(&root)?;
        Ok(directory)
    }

    fn chunk_cleanup_record(&self, session: &str) -> RepositoryResult<Option<Cleanup>> {
        let Some(directory) = self.existing_chunk_directory(session)? else {
            return Ok(None);
        };
        let Some(cleanup): Option<Cleanup> = read_record(&directory.join("cleanup"))? else {
            return Ok(None);
        };
        if cleanup.version != 1 || cleanup.session != session {
            return Err("invalid chunk cleanup record".into());
        }
        Ok(Some(cleanup))
    }

    fn chunk_decision_at(
        &self,
        directory: &Path,
        session: &str,
        offset: u64,
    ) -> RepositoryResult<Option<Decision>> {
        let path = directory.join(format!("decision-{offset}"));
        let Some(decision): Option<Decision> = read_record(&path)? else {
            return Ok(None);
        };
        if decision.version != 1
            || decision.session != session
            || decision.offset != offset
            || !offset.is_multiple_of(TRANSFER_CHUNK as u64)
            || offset >= crate::protocol::MAX_BUNDLE_BYTES
        {
            return Err("invalid chunk decision".into());
        }
        require_id(&decision.operation_id)?;
        require_id(&decision.payload_sha256)?;
        Ok(Some(decision))
    }

    fn chunk_decision(
        &self,
        directory: &Path,
        chunk: &ScopedChunkIdentity,
    ) -> Result<Option<Decision>, ScopedUploadError> {
        let Some(decision) = self
            .chunk_decision_at(directory, &chunk.scope.session, chunk.offset)
            .map_err(ScopedUploadError::unknown)?
        else {
            return Ok(None);
        };
        if decision.operation_id != chunk.operation_id
            || decision.payload_sha256 != chunk.payload_sha256
        {
            return Err("scoped chunk identity conflict".into());
        }
        Ok(Some(decision))
    }

    fn chunk_status(chunk: &ScopedChunkIdentity, state: ScopedChunkState) -> ScopedChunkStatus {
        ScopedChunkStatus {
            version: 1,
            chunk: chunk.clone(),
            state,
        }
    }

    fn terminal_cleanup_reason(state: &ScopedUploadState) -> Option<ScopedChunkCleanupReason> {
        match state {
            ScopedUploadState::Canceled => Some(ScopedChunkCleanupReason::Canceled),
            ScopedUploadState::Finished { .. }
            | ScopedUploadState::FinishedV4 { .. }
            | ScopedUploadState::Aborted { .. } => Some(ScopedChunkCleanupReason::Completed),
            _ => None,
        }
    }

    pub(super) fn require_live_scoped_chunks_locked(&self, session: &str) -> RepositoryResult<()> {
        if let Some(cleanup) = self.chunk_cleanup_record(session)? {
            return Err(format!(
                "scoped upload data is being cleaned or was cleaned: {:?}",
                cleanup.reason
            ));
        }
        Ok(())
    }

    pub(super) fn resume_scoped_cleanup_locked(&self, session: &str) -> RepositoryResult<bool> {
        if let Some(cleanup) = self.chunk_cleanup_record(session)? {
            self.cleanup_scoped_chunks_locked(session, cleanup.reason)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(super) fn resume_all_scoped_cleanups_locked(&self) -> RepositoryResult<()> {
        let root = self.common_metadata.join("scoped-chunks");
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error("inspect chunk directory", &root, error)),
            Ok(_) => require_directory(&root)?,
        }
        for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let session = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid chunk session name")?;
            require_id(&session)?;
            require_directory(&entry.path())?;
            self.resume_scoped_cleanup_locked(&session)?;
        }
        Ok(())
    }

    /// Both transfer-state.lock and transfer.lock are held by the caller.
    pub(super) fn cleanup_scoped_chunks_locked(
        &self,
        session: &str,
        reason: ScopedChunkCleanupReason,
    ) -> RepositoryResult<ScopedChunkCleanupReason> {
        let directory = self.ensure_chunk_directory(session)?;
        let path = directory.join("cleanup");
        let mut cleanup = match self.chunk_cleanup_record(session)? {
            Some(cleanup) => {
                // Finish a possibly uncertain tombstone publication before any unlink.
                sync_directory(&directory)?;
                cleanup
            }
            None => {
                let cleanup = Cleanup {
                    version: 1,
                    session: session.into(),
                    reason,
                    cleaned: false,
                };
                self.atomic_write(&path, &crate::binary::encode(&cleanup)?)?;
                cleanup
            }
        };
        if cleanup.cleaned {
            return Ok(cleanup.reason);
        }
        let mut payloads = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid chunk entry")?;
            if name == "cleanup" {
                continue;
            }
            if let Some(offset) = name.strip_prefix("decision-") {
                let offset = offset
                    .parse::<u64>()
                    .map_err(|_| "invalid chunk decision filename")?;
                if name != format!("decision-{offset}")
                    || self
                        .chunk_decision_at(&directory, session, offset)?
                        .is_none()
                {
                    return Err("invalid chunk decision filename".into());
                }
                continue;
            }
            if let Some(offset) = name.strip_prefix("payload-") {
                let offset = offset
                    .parse::<u64>()
                    .map_err(|_| "invalid chunk payload filename")?;
                if name != format!("payload-{offset}")
                    || !offset.is_multiple_of(TRANSFER_CHUNK as u64)
                    || offset >= crate::protocol::MAX_BUNDLE_BYTES
                {
                    return Err("invalid chunk payload filename".into());
                }
            } else {
                return Err("unexpected chunk session entry".into());
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > TRANSFER_CHUNK as u64
            {
                return Err("invalid chunk payload file".into());
            }
            payloads.push(entry.path());
            if payloads.len() > 4096 {
                return Err("chunk session contains too many payload files".into());
            }
        }
        for path in payloads {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        sync_directory(&directory)?;
        self.remove_upload_cache_locked(session)?;
        cleanup.cleaned = true;
        self.atomic_write(&path, &crate::binary::encode(&cleanup)?)?;
        Ok(cleanup.reason)
    }

    /// Called under transfer-state.lock then transfer.lock, including GC/TTL.
    pub(super) fn expire_scoped_upload_locked(&self, session: &str) -> RepositoryResult<bool> {
        let record = self.scoped_record(session)?;
        let reason = match record.as_ref().map(|record| &record.state) {
            Some(ScopedUploadState::Applying | ScopedUploadState::ApplyingV4 { .. }) => {
                return Ok(false)
            }
            Some(ScopedUploadState::Canceled) => ScopedChunkCleanupReason::Canceled,
            Some(
                ScopedUploadState::Finished { .. }
                | ScopedUploadState::FinishedV4 { .. }
                | ScopedUploadState::Aborted { .. },
            ) => ScopedChunkCleanupReason::Completed,
            Some(ScopedUploadState::Open) | None => ScopedChunkCleanupReason::Expired,
        };
        self.cleanup_scoped_chunks_locked(session, reason)?;
        Ok(true)
    }

    fn validate_chunk_record_scope(
        &self,
        record: &ScopedRecord,
        chunk: &ScopedChunkIdentity,
    ) -> RepositoryResult<()> {
        Self::check_scope(
            record,
            &chunk.scope.repository,
            &chunk.scope.actor,
            &chunk.scope.binding,
            false,
        )
    }

    fn materialize_chunk_locked(
        &self,
        directory: &Path,
        chunk: &ScopedChunkIdentity,
    ) -> RepositoryResult<()> {
        let raw = directory.join(format!("payload-{}", chunk.offset));
        let metadata = fs::symlink_metadata(&raw)
            .map_err(|error| io_error("inspect accepted chunk data", &raw, error))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > TRANSFER_CHUNK as u64
        {
            return Err("invalid accepted chunk data".into());
        }
        let bytes = read_regular(&raw)?;
        chunk.validate_payload(&bytes)?;
        let upload = match self
            .existing_upload_directory_locked(&chunk.scope.session, chunk.scope.binding.total)?
        {
            Some(directory) => directory,
            None => self
                .prepare_upload_directory_locked(&chunk.scope.session, chunk.scope.binding.total)?,
        };
        let target = upload.join(chunk.offset.to_string());
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                if read_regular(&target)? != bytes {
                    return Err("accepted chunk conflicts with cached upload bytes".into());
                }
                sync_directory(&upload)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::hard_link(&raw, &target)
                    .map_err(|error| io_error("materialize accepted chunk", &target, error))?;
                self.finish_upload_link_locked(&upload)
            }
            Err(error) => Err(io_error("inspect upload chunk", &target, error)),
        }
    }

    pub(super) fn recover_scoped_chunks_locked(
        &self,
        repository: &str,
        actor: &str,
        session: &str,
        binding: &crate::protocol::UploadBinding,
    ) -> RepositoryResult<()> {
        self.require_live_scoped_chunks_locked(session)?;
        let Some(directory) = self.existing_chunk_directory(session)? else {
            return Ok(());
        };
        let mut offset = 0;
        while offset < binding.total {
            if let Some(decision) = self.chunk_decision_at(&directory, session, offset)? {
                sync_directory(&directory)?;
                let chunk = ScopedChunkIdentity {
                    version: 1,
                    operation_id: decision.operation_id,
                    scope: ScopedExecution {
                        actor: actor.into(),
                        repository: repository.into(),
                        session: session.into(),
                        binding: binding.clone(),
                    },
                    offset,
                    payload_sha256: decision.payload_sha256,
                };
                chunk.validate()?;
                self.materialize_chunk_locked(&directory, &chunk)?;
            }
            offset += TRANSFER_CHUNK as u64;
        }
        Ok(())
    }

    fn accepted_chunk_locked(
        &self,
        directory: &Path,
        chunk: &ScopedChunkIdentity,
        state: &ScopedUploadState,
    ) -> Result<ScopedChunkStatus, ScopedUploadError> {
        sync_directory(directory).map_err(ScopedUploadError::unknown)?;
        let terminal = Self::terminal_cleanup_reason(state);
        if let Some(cleanup) = self
            .chunk_cleanup_record(&chunk.scope.session)
            .map_err(ScopedUploadError::unknown)?
        {
            let reason = self
                .cleanup_scoped_chunks_locked(&chunk.scope.session, cleanup.reason)
                .map_err(ScopedUploadError::unknown)?;
            return Ok(Self::chunk_status(
                chunk,
                ScopedChunkState::Accepted {
                    data: ScopedChunkData::Cleaned { reason },
                },
            ));
        }
        if let Some(reason) = terminal {
            let reason = self
                .cleanup_scoped_chunks_locked(&chunk.scope.session, reason)
                .map_err(ScopedUploadError::unknown)?;
            return Ok(Self::chunk_status(
                chunk,
                ScopedChunkState::Accepted {
                    data: ScopedChunkData::Cleaned { reason },
                },
            ));
        }
        if let Err(error) = self.materialize_chunk_locked(directory, chunk) {
            if let Some(cleanup) = self
                .chunk_cleanup_record(&chunk.scope.session)
                .map_err(ScopedUploadError::unknown)?
            {
                let reason = self
                    .cleanup_scoped_chunks_locked(&chunk.scope.session, cleanup.reason)
                    .map_err(ScopedUploadError::unknown)?;
                return Ok(Self::chunk_status(
                    chunk,
                    ScopedChunkState::Accepted {
                        data: ScopedChunkData::Cleaned { reason },
                    },
                ));
            }
            return Err(ScopedUploadError::unknown(format!(
                "accepted chunk requires recovery: {error}"
            )));
        }
        Ok(Self::chunk_status(
            chunk,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Available,
            },
        ))
    }

    pub(crate) fn scoped_chunk_query(
        &self,
        gate: &ServiceGuard,
        chunk: &ScopedChunkIdentity,
    ) -> Result<ScopedChunkStatus, ScopedUploadError> {
        chunk.validate()?;
        if gate.common_metadata != self.common_metadata {
            return Err("service guard belongs to another repository".into());
        }
        let _state = self.lock_named("transfer-state.lock")?;
        let _transfer = self.lock_named("transfer.lock")?;
        let record = self
            .scoped_record(&chunk.scope.session)
            .map_err(ScopedUploadError::unknown)?;
        let record = record.as_ref();
        if let Some(record) = record {
            self.validate_chunk_record_scope(record, chunk)?;
        }
        let Some(directory) = self
            .existing_chunk_directory(&chunk.scope.session)
            .map_err(ScopedUploadError::unknown)?
        else {
            if let Some(reason) =
                record.and_then(|record| Self::terminal_cleanup_reason(&record.state))
            {
                self.cleanup_scoped_chunks_locked(&chunk.scope.session, reason)
                    .map_err(ScopedUploadError::unknown)?;
            }
            return Ok(Self::chunk_status(chunk, ScopedChunkState::Unobserved));
        };
        let Some(_decision) = self.chunk_decision(&directory, chunk)? else {
            if !self
                .resume_scoped_cleanup_locked(&chunk.scope.session)
                .map_err(ScopedUploadError::unknown)?
            {
                if let Some(reason) =
                    record.and_then(|record| Self::terminal_cleanup_reason(&record.state))
                {
                    self.cleanup_scoped_chunks_locked(&chunk.scope.session, reason)
                        .map_err(ScopedUploadError::unknown)?;
                }
            }
            return Ok(Self::chunk_status(chunk, ScopedChunkState::Unobserved));
        };
        let record = record.ok_or_else(|| {
            ScopedUploadError::unknown("chunk decision has no authoritative scoped session".into())
        })?;
        self.accepted_chunk_locked(&directory, chunk, &record.state)
    }

    pub(crate) fn scoped_chunk_write(
        &self,
        gate: &ServiceGuard,
        chunk: &ScopedChunkIdentity,
        bytes: &[u8],
        before_publish: impl FnMut() -> RepositoryResult<()>,
    ) -> Result<ScopedChunkStatus, ScopedUploadError> {
        self.scoped_chunk_write_with_checkpoint(gate, chunk, bytes, before_publish, || Ok(()))
    }

    fn scoped_chunk_write_with_checkpoint(
        &self,
        gate: &ServiceGuard,
        chunk: &ScopedChunkIdentity,
        bytes: &[u8],
        mut before_publish: impl FnMut() -> RepositoryResult<()>,
        after_decision: impl FnOnce() -> RepositoryResult<()>,
    ) -> Result<ScopedChunkStatus, ScopedUploadError> {
        chunk.validate_payload(bytes)?;
        if gate.common_metadata != self.common_metadata {
            return Err("service guard belongs to another repository".into());
        }
        let _state = self.lock_named("transfer-state.lock")?;
        let _transfer = self.lock_named("transfer.lock")?;
        let record = self
            .scoped_record(&chunk.scope.session)
            .map_err(ScopedUploadError::unknown)?;
        if let Some(record) = &record {
            self.validate_chunk_record_scope(record, chunk)?;
        }
        if let Some(directory) = self
            .existing_chunk_directory(&chunk.scope.session)
            .map_err(ScopedUploadError::unknown)?
        {
            if self.chunk_decision(&directory, chunk)?.is_some() {
                let record = record.as_ref().ok_or_else(|| {
                    ScopedUploadError::unknown(
                        "chunk decision has no authoritative scoped session".into(),
                    )
                })?;
                return self.accepted_chunk_locked(&directory, chunk, &record.state);
            }
        }
        self.require_live_scoped_chunks_locked(&chunk.scope.session)?;
        let newly_opened = self.ensure_scoped_open_locked(
            &chunk.scope.repository, &chunk.scope.actor, &chunk.scope.session,
            &chunk.scope.binding, &mut before_publish, false,
        ).map_err(|mut error| {
            if error.outcome_unknown { error.message = format!("scoped session admission may have been published; chunk decision was not attempted: {}", error.message); }
            error
        })?;
        let record = self
            .scoped_record(&chunk.scope.session)
            .map_err(ScopedUploadError::unknown)?
            .ok_or("missing scoped upload")?;
        self.stage_and_decide_chunk_locked(
            chunk,
            bytes,
            &record,
            &mut before_publish,
            after_decision,
        )
        .map_err(|mut error| {
            if newly_opened && !error.outcome_unknown {
                error.message = format!(
                    "{}; scoped session remains open, and this chunk decision was not published",
                    error.message
                );
            }
            error
        })
    }

    fn stage_and_decide_chunk_locked(
        &self,
        chunk: &ScopedChunkIdentity,
        bytes: &[u8],
        record: &ScopedRecord,
        before_publish: &mut impl FnMut() -> RepositoryResult<()>,
        after_decision: impl FnOnce() -> RepositoryResult<()>,
    ) -> Result<ScopedChunkStatus, ScopedUploadError> {
        let upload =
            self.prepare_upload_directory_locked(&chunk.scope.session, chunk.scope.binding.total)?;
        let cached = upload.join(chunk.offset.to_string());
        match fs::symlink_metadata(&cached) {
            Ok(_) if read_regular(&cached)? != bytes => {
                return Err("upload slot contains different bytes".into())
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.to_string().into()),
        }
        let directory = self.ensure_chunk_directory(&chunk.scope.session)?;
        let raw = directory.join(format!("payload-{}", chunk.offset));
        let raw_exists = match fs::symlink_metadata(&raw) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                if metadata.len() > TRANSFER_CHUNK as u64 {
                    return Err("invalid staged chunk size".into());
                }
                true
            }
            Ok(_) => return Err("invalid staged chunk file".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string().into()),
        };
        // This one bounded staging inode belongs to an undecided slot. A
        // stopped writer may leave partial bytes, which can be rewritten only
        // while no decision exists and both resource locks are held.
        let mut options = OpenOptions::new();
        options.write(true).create_new(!raw_exists);
        crate::platform_fs::protect_lock_open(&mut options);
        let mut file = options
            .open(&raw)
            .map_err(|error| io_error("open staged chunk", &raw, error))?;
        if !file
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("invalid staged chunk file".into());
        }
        if !raw_exists || read_regular(&raw)? != bytes {
            file.set_len(0).map_err(|error| error.to_string())?;
            file.write_all(bytes).map_err(|error| error.to_string())?;
        }
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        sync_directory(&directory)?;
        let decision = Decision {
            version: 1,
            session: chunk.scope.session.clone(),
            offset: chunk.offset,
            operation_id: chunk.operation_id.clone(),
            payload_sha256: chunk.payload_sha256.clone(),
        };
        let path = directory.join(format!("decision-{}", chunk.offset));
        let mut passed = false;
        self.atomic_write_checked(&path, &crate::binary::encode(&decision)?, || {
            before_publish()?;
            passed = true;
            Ok(())
        })
        .map_err(|message| ScopedUploadError {
            message,
            outcome_unknown: passed,
        })?;
        after_decision().map_err(ScopedUploadError::unknown)?;
        self.accepted_chunk_locked(&directory, chunk, &record.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Operation, UploadBinding};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "wasm-git-chunks-{}-{}",
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

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn fixture(payload: &[u8]) -> (Temp, Repository, ScopedChunkIdentity) {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let binding = UploadBinding::for_payload(
            Operation::Push {
                branch: "main".into(),
                expected: None,
                force: false,
            },
            payload,
        )
        .unwrap();
        let slice = &payload[..payload.len().min(TRANSFER_CHUNK)];
        let chunk = ScopedChunkIdentity {
            version: 1,
            operation_id: "a".repeat(64),
            scope: ScopedExecution {
                actor: "alice".into(),
                repository: "project".into(),
                session: "b".repeat(64),
                binding,
            },
            offset: 0,
            payload_sha256: format!("{:x}", Sha256::digest(slice)),
        };
        (temp, repo, chunk)
    }

    #[test]
    fn undecided_staging_is_unobserved_and_original_can_retry() {
        let payload = b"frozen chunk";
        let (_temp, repo, chunk) = fixture(payload);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        assert_eq!(
            repo.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Unobserved
        );
        let mut checks = 0;
        let error = repo
            .scoped_chunk_write(&gate, &chunk, payload, || {
                checks += 1;
                if checks == 1 {
                    Ok(())
                } else {
                    Err("authorization expired".into())
                }
            })
            .unwrap_err();
        assert_eq!(checks, 2);
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("session remains open"));
        let directory = repo
            .common_metadata
            .join("scoped-chunks")
            .join(&chunk.scope.session);
        assert!(directory.join("payload-0").is_file());
        assert!(!directory.join("decision-0").exists());
        assert_eq!(
            repo.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Unobserved
        );
        let status = repo
            .scoped_chunk_write(&gate, &chunk, payload, || Ok(()))
            .unwrap();
        assert_eq!(
            status.state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Available
            }
        );
    }

    #[test]
    fn legacy_cache_is_not_a_chunk_decision_without_new_authorization() {
        let payload = b"legacy chunk";
        let (_temp, repo, chunk) = fixture(payload);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_upload_slice(
            "project",
            "alice",
            &chunk.scope.session,
            &chunk.scope.binding,
            0,
            payload,
        )
        .unwrap();
        assert_eq!(
            repo.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Unobserved
        );
        let error = repo
            .scoped_chunk_write(&gate, &chunk, payload, || {
                Err("authorization expired".into())
            })
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(
            repo.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Unobserved
        );
        let mut checks = 0;
        let status = repo
            .scoped_chunk_write(&gate, &chunk, payload, || {
                checks += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(checks, 1);
        assert_eq!(
            status.state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Available
            }
        );
    }

    #[test]
    fn committed_chunk_recovers_after_materialization_failure_without_new_authority() {
        let payload = b"frozen chunk";
        let (temp, repo, chunk) = fixture(payload);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        let cache = repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{}/0", chunk.scope.session));
        let mut checks = 0;
        let error = repo
            .scoped_chunk_write(&gate, &chunk, payload, || {
                checks += 1;
                if checks == 2 {
                    fs::write(&cache, b"conflicting cache").unwrap();
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(checks, 2);
        assert!(error.outcome_unknown);
        let directory = repo
            .common_metadata
            .join("scoped-chunks")
            .join(&chunk.scope.session);
        assert!(directory.join("decision-0").is_file());
        assert_eq!(fs::read(directory.join("payload-0")).unwrap(), payload);
        fs::remove_file(&cache).unwrap();
        drop(gate);
        let reopened = Repository::open(&temp.0).unwrap();
        let gate = reopened.service_lock(std::time::Duration::ZERO).unwrap();
        assert_eq!(
            reopened.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Available
            }
        );
        assert_eq!(fs::read(&cache).unwrap(), payload);
        reopened
            .scoped_chunk_write(&gate, &chunk, payload, || {
                panic!("accepted replay must not reauthorize")
            })
            .unwrap();
        let mut conflict = chunk.clone();
        conflict.operation_id = "d".repeat(64);
        assert!(reopened.scoped_chunk_query(&gate, &conflict).is_err());
        assert!(reopened
            .scoped_chunk_write(&gate, &conflict, payload, || Ok(()))
            .is_err());
    }

    #[test]
    fn decision_checkpoint_child() {
        let Some(root) = std::env::var_os("WASMGIT_CHUNK_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let chunk: ScopedChunkIdentity =
            crate::binary::decode(&fs::read(root.join("checkpoint-identity")).unwrap()).unwrap();
        let repo = Repository::open(&root).unwrap();
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        let result = repo.scoped_chunk_write_with_checkpoint(
            &gate,
            &chunk,
            b"process crash chunk",
            || Ok(()),
            || {
                fs::write(root.join("checkpoint-ready"), b"ready")
                    .map_err(|error| error.to_string())?;
                let mut byte = [0u8; 1];
                std::io::stdin()
                    .read_exact(&mut byte)
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
        );
        panic!("test child unexpectedly left its checkpoint: {result:?}");
    }

    #[test]
    fn killed_process_after_durable_decision_recovers_original_chunk() {
        let payload = b"process crash chunk";
        let (temp, repo, chunk) = fixture(payload);
        fs::write(
            temp.0.join("checkpoint-identity"),
            crate::binary::encode(&chunk).unwrap(),
        )
        .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "repository::chunk_receipts::tests::decision_checkpoint_child",
                "--nocapture",
            ])
            .env("WASMGIT_CHUNK_TEST_ROOT", &temp.0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let ready = temp.0.join("checkpoint-ready");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready.is_file() {
            if let Some(status) = child.0.try_wait().unwrap() {
                let mut stderr = String::new();
                child
                    .0
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("checkpoint child exited before decision ({status}): {stderr}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "checkpoint child did not publish a decision"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let directory = repo
            .common_metadata
            .join("scoped-chunks")
            .join(&chunk.scope.session);
        let cache = repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{}/0", chunk.scope.session));
        assert!(directory.join("decision-0").is_file());
        assert_eq!(fs::read(directory.join("payload-0")).unwrap(), payload);
        assert!(!cache.exists());
        assert!(repo.service_lock(std::time::Duration::ZERO).is_err());
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());

        let reopened = Repository::open(&temp.0).unwrap();
        let gate = reopened.service_lock(std::time::Duration::ZERO).unwrap();
        assert_eq!(
            reopened.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Available
            }
        );
        assert_eq!(fs::read(cache).unwrap(), payload);
    }

    #[test]
    fn cancellation_cleanup_recovers_and_never_recreates_accepted_or_late_chunks() {
        let payload = vec![7; TRANSFER_CHUNK + 1];
        let (temp, repo, chunk) = fixture(&payload);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_chunk_write(&gate, &chunk, &payload[..TRANSFER_CHUNK], || Ok(()))
            .unwrap();
        let directory = repo
            .common_metadata
            .join("scoped-chunks")
            .join(&chunk.scope.session);
        fs::write(directory.join("unexpected"), b"retain").unwrap();
        let error = repo
            .scoped_upload_cancel_checked(
                "project",
                "alice",
                &chunk.scope.session,
                &chunk.scope.binding,
                || Ok(()),
            )
            .unwrap_err();
        assert!(error.outcome_unknown);
        let cleanup = repo
            .chunk_cleanup_record(&chunk.scope.session)
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.reason, ScopedChunkCleanupReason::Canceled);
        assert!(!cleanup.cleaned);
        assert!(directory.join("payload-0").is_file());
        fs::remove_file(directory.join("unexpected")).unwrap();
        drop(gate);
        let reopened = Repository::open(&temp.0).unwrap();
        let gate = reopened.service_lock(std::time::Duration::ZERO).unwrap();
        let cleaned = ScopedChunkState::Accepted {
            data: ScopedChunkData::Cleaned {
                reason: ScopedChunkCleanupReason::Canceled,
            },
        };
        assert_eq!(
            reopened.scoped_chunk_query(&gate, &chunk).unwrap().state,
            cleaned
        );
        assert!(!directory.join("payload-0").exists());
        assert!(directory.join("decision-0").is_file());
        assert!(!reopened
            .common_metadata
            .join("tmp")
            .join(format!("upload-{}", chunk.scope.session))
            .exists());
        assert_eq!(
            reopened
                .scoped_chunk_write(&gate, &chunk, &payload[..TRANSFER_CHUNK], || panic!(
                    "cleaned replay must not republish"
                ))
                .unwrap()
                .state,
            cleaned
        );
        let mut late = chunk.clone();
        late.operation_id = "e".repeat(64);
        late.offset = TRANSFER_CHUNK as u64;
        late.payload_sha256 = format!("{:x}", Sha256::digest(&payload[TRANSFER_CHUNK..]));
        assert_eq!(
            reopened.scoped_chunk_query(&gate, &late).unwrap().state,
            ScopedChunkState::Unobserved
        );
        assert!(reopened
            .scoped_chunk_write(&gate, &late, &payload[TRANSFER_CHUNK..], || Ok(()))
            .is_err());
        assert!(!directory.join(format!("payload-{}", late.offset)).exists());
    }

    #[test]
    fn expiry_preserves_decision_and_records_cleaned_reason() {
        let payload = b"frozen chunk";
        let (_temp, repo, chunk) = fixture(payload);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_chunk_write(&gate, &chunk, payload, || Ok(()))
            .unwrap();
        let meta = repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{}/meta", chunk.scope.session));
        File::options()
            .write(true)
            .open(meta)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600 + 1),
            )
            .unwrap();
        repo.expire_transfers().unwrap();
        assert_eq!(
            repo.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Cleaned {
                    reason: ScopedChunkCleanupReason::Expired
                }
            }
        );
        assert_eq!(
            repo.scoped_chunk_write(&gate, &chunk, payload, || panic!(
                "expired replay must not republish"
            ))
            .unwrap()
            .state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Cleaned {
                    reason: ScopedChunkCleanupReason::Expired
                }
            }
        );
    }

    #[test]
    fn terminal_operation_cleanup_preserves_completed_receipt() {
        let payload = vec![9; TRANSFER_CHUNK + 1];
        let (_temp, repo, chunk) = fixture(&payload);
        let gate = repo.service_lock(std::time::Duration::ZERO).unwrap();
        repo.scoped_chunk_write(&gate, &chunk, &payload[..TRANSFER_CHUNK], || Ok(()))
            .unwrap();
        repo.scoped_upload_applying_checked(
            &gate,
            "project",
            "alice",
            &chunk.scope.session,
            &chunk.scope.binding,
            || Ok(()),
        )
        .unwrap();
        assert!(matches!(
            repo.scoped_upload_status(
                &gate,
                "project",
                "alice",
                &chunk.scope.session,
                &chunk.scope.binding
            )
            .unwrap()
            .state,
            Some(ScopedUploadState::Aborted { .. })
        ));
        let mut unobserved = chunk.clone();
        unobserved.offset = TRANSFER_CHUNK as u64;
        unobserved.operation_id = "f".repeat(64);
        unobserved.payload_sha256 = format!("{:x}", Sha256::digest(&payload[TRANSFER_CHUNK..]));
        assert_eq!(
            repo.scoped_chunk_query(&gate, &unobserved).unwrap().state,
            ScopedChunkState::Unobserved
        );
        assert!(!repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{}", chunk.scope.session))
            .exists());
        assert_eq!(
            repo.scoped_chunk_query(&gate, &chunk).unwrap().state,
            ScopedChunkState::Accepted {
                data: ScopedChunkData::Cleaned {
                    reason: ScopedChunkCleanupReason::Completed
                }
            }
        );
    }
}
