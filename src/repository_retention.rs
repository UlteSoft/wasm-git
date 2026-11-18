//! Durable release references. All transitions and maintenance consult this
//! file under the repository writer lock, including close-before-prepare.
use super::record_store::{self, Bucket, RecordStore, Seed};
use super::*;
use crate::protocol::{
    RetainedFile, RetentionCommand, RetentionRecord, RetentionReply, RetentionState,
};
use sha2::{Digest, Sha256};

const MAX_RECORDS: usize = 4096;
const MAX_FILES: usize = 128;

#[derive(Debug)]
pub struct RetentionError {
    pub message: String,
    pub outcome_unknown: bool,
}
impl std::fmt::Display for RetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RetentionError {}
impl From<String> for RetentionError {
    fn from(message: String) -> Self {
        Self {
            message,
            outcome_unknown: false,
        }
    }
}
impl From<&str> for RetentionError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retentions {
    version: u32,
    records: BTreeMap<String, RetentionRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRetentionRecord {
    version: u32,
    operation: String,
    actor: String,
    state: RetentionState,
    commit: Option<String>,
    files: BTreeMap<String, RetainedFile>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRetentions {
    version: u32,
    records: BTreeMap<String, LegacyRetentionRecord>,
}

fn retention_bucket(state: &RetentionState) -> Bucket {
    match state {
        RetentionState::Prepared => Bucket::Active,
        RetentionState::Promoted => Bucket::Pinned,
        RetentionState::Closed => Bucket::Terminal,
    }
}

fn validate_retention_record(id: &str, bucket: Bucket, bytes: &[u8]) -> RepositoryResult<()> {
    let record: RetentionRecord = crate::binary::decode(bytes)?;
    if record.operation != id || retention_bucket(&record.state) != bucket {
        return Err("invalid release retention record".into());
    }
    record.validate()
}

fn compare_retention_records(
    _id: &str,
    active: &[u8],
    bucket: Bucket,
    terminal: &[u8],
) -> RepositoryResult<()> {
    let old: RetentionRecord = crate::binary::decode(active)?;
    let mut new: RetentionRecord = crate::binary::decode(terminal)?;
    if !matches!(
        (bucket, &new.state),
        (Bucket::Pinned, RetentionState::Promoted) | (Bucket::Terminal, RetentionState::Closed)
    ) {
        return Err("invalid release retention transition".into());
    }
    new.state = RetentionState::Prepared;
    if old != new {
        return Err("conflicting release retention transition".into());
    }
    Ok(())
}

impl Repository {
    fn retention_store(&self) -> RepositoryResult<RecordStore<'_>> {
        RecordStore::open(self, "release-retentions", |bytes| {
            let Some(bytes) = bytes else {
                return Ok(Vec::new());
            };
            let state = match crate::binary::decode::<Retentions>(bytes) {
                Ok(state) if state.version == crate::protocol::RETENTION_VERSION => state,
                _ => {
                    let legacy: LegacyRetentions = crate::binary::decode(bytes)?;
                    if legacy.version != 1 {
                        return Err("invalid release retention store version".into());
                    }
                    let mut records = BTreeMap::new();
                    for (id, record) in legacy.records {
                        if record.version != 1 {
                            return Err("invalid legacy release retention record".into());
                        }
                        records.insert(
                            id,
                            RetentionRecord {
                                version: crate::protocol::RETENTION_VERSION,
                                operation: record.operation,
                                actor: record.actor,
                                state: record.state,
                                repository: None,
                                commit: record.commit,
                                files: record.files,
                            },
                        );
                    }
                    Retentions {
                        version: crate::protocol::RETENTION_VERSION,
                        records,
                    }
                }
            };
            if state.records.len() > MAX_RECORDS {
                return Err("invalid release retention store version or size".into());
            }
            let mut seeds = Vec::with_capacity(state.records.len());
            for (id, record) in state.records {
                let bucket = retention_bucket(&record.state);
                let payload = crate::binary::encode(&record)?;
                validate_retention_record(&id, bucket, &payload)?;
                seeds.push(Seed {
                    id,
                    bucket,
                    payload,
                });
            }
            Ok(seeds)
        })
    }

    fn retention_record(&self, id: &str) -> RepositoryResult<Option<RetentionRecord>> {
        self.retention_store()?
            .get(id, validate_retention_record, compare_retention_records)?
            .map(|(_, bytes)| crate::binary::decode(&bytes))
            .transpose()
    }

    fn save_retention_record(
        &self,
        record: &RetentionRecord,
        before_publish: &mut impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), RetentionError> {
        let bytes = crate::binary::encode(record)?;
        self.retention_store()?
            .write(
                &record.operation,
                retention_bucket(&record.state),
                &bytes,
                validate_retention_record,
                compare_retention_records,
                before_publish,
            )
            .map_err(|error| RetentionError {
                message: error.message,
                outcome_unknown: error.outcome_unknown,
            })
    }

    fn fence_retention(&self, state: &RetentionState) -> Result<(), RetentionError> {
        self.retention_store()?
            .fence(retention_bucket(state))
            .map_err(|message| RetentionError {
                message,
                outcome_unknown: true,
            })
    }

    pub(super) fn retention_capacity_locked(
        &self,
    ) -> RepositoryResult<crate::protocol::AdmissionCapacity> {
        self.retention_store()?
            .capacity(validate_retention_record, compare_retention_records)
    }

    // These helpers are called with the writer lock already held.
    pub(super) fn release_retention_roots(&self) -> RepositoryResult<Vec<String>> {
        let store = self.retention_store()?;
        let mut roots = Vec::new();
        for bucket in [Bucket::Active, Bucket::Pinned] {
            store.for_each(
                bucket,
                validate_retention_record,
                compare_retention_records,
                |_, bytes| {
                    let record: RetentionRecord = crate::binary::decode(bytes)?;
                    if let Some(commit) = record.commit {
                        roots.push(commit);
                    }
                    Ok(())
                },
            )?;
        }
        Ok(roots)
    }
    pub(super) fn release_retention_blobs(&self) -> RepositoryResult<BTreeSet<String>> {
        let store = self.retention_store()?;
        let mut blobs = BTreeSet::new();
        for bucket in [Bucket::Active, Bucket::Pinned] {
            store.for_each(
                bucket,
                validate_retention_record,
                compare_retention_records,
                |_, bytes| {
                    let record: RetentionRecord = crate::binary::decode(bytes)?;
                    blobs.extend(record.files.into_values().map(|f| f.blob));
                    Ok(())
                },
            )?;
        }
        Ok(blobs)
    }

    #[cfg(test)]
    pub fn release_retention(
        &self,
        repository: &str,
        actor: &str,
        command: RetentionCommand,
    ) -> Result<RetentionReply, RetentionError> {
        self.release_retention_checked(repository, actor, command, || Ok(()))
    }

    pub fn release_retention_checked(
        &self,
        repository: &str,
        actor: &str,
        command: RetentionCommand,
        mut before_publish: impl FnMut() -> RepositoryResult<()>,
    ) -> Result<RetentionReply, RetentionError> {
        crate::protocol::RetentionRequest {
            version: 1,
            command: command.clone(),
        }
        .validate()?;
        crate::protocol::repository_name(repository)?;
        if actor.is_empty() || actor.len() > 128 {
            return Err("invalid authenticated actor".into());
        }
        let _lock = self.lock()?;
        let operation = match &command {
            RetentionCommand::Prepare { operation, .. }
            | RetentionCommand::Query { operation }
            | RetentionCommand::Promote { operation }
            | RetentionCommand::Close { operation }
            | RetentionCommand::Read { operation, .. } => operation,
        };
        require_id(operation)?;
        let mut stored = self
            .retention_record(operation)
            .map_err(|message| RetentionError {
                message,
                outcome_unknown: true,
            })?;
        if let Some(record) = &stored {
            let legacy_query =
                record.repository.is_none() && matches!(&command, RetentionCommand::Query { .. });
            if record.actor != actor
                || (!legacy_query && record.repository.as_deref() != Some(repository))
            {
                return Err("release retention unavailable to this actor or repository".into());
            }
        }
        match command {
            RetentionCommand::Query { operation } => {
                if let Some(record) = &stored {
                    self.fence_retention(&record.state)?;
                }
                Ok(RetentionReply::State {
                    version: crate::protocol::RETENTION_VERSION,
                    repository: repository.into(),
                    operation: operation.clone(),
                    record: stored,
                })
            }
            RetentionCommand::Prepare {
                operation,
                commit,
                mut paths,
            } => {
                require_id(&commit)?;
                if paths.is_empty() || paths.len() > MAX_FILES {
                    return Err("release retention requires 1..=128 paths".into());
                }
                paths.sort();
                if paths.windows(2).any(|p| p[0] == p[1]) {
                    return Err("duplicate release path".into());
                }
                for path in &paths {
                    validate_tree_path(path)?;
                    if path.len() > 1024 {
                        return Err("release path too long".into());
                    }
                }
                if let Some(record) = &stored {
                    if record.state == RetentionState::Closed {
                        return Err("release retention is closed".into());
                    }
                    if record.commit.as_deref() != Some(&commit)
                        || !record.files.keys().eq(paths.iter())
                    {
                        return Err("release operation identity conflict".into());
                    }
                    self.fence_retention(&record.state)?;
                    return Ok(RetentionReply::State {
                        version: crate::protocol::RETENTION_VERSION,
                        repository: repository.into(),
                        operation: operation.clone(),
                        record: Some(record.clone()),
                    });
                }
                let snapshot = self.snapshot()?;
                let Some(Object::Commit(found)) = snapshot.objects.get(&commit) else {
                    return Err("release commit is not stored".into());
                };
                let Some(Object::Tree(tree)) =
                    found.tree.as_ref().and_then(|id| snapshot.objects.get(id))
                else {
                    return Err("release requires a named commit tree".into());
                };
                let mut files = BTreeMap::new();
                for path in paths {
                    let blob = tree
                        .entries
                        .get(&path)
                        .ok_or("release path absent from commit")?;
                    let Some(Object::Wasm(bytes)) = snapshot.objects.get(blob) else {
                        return Err(
                            "release file must be available locally before preparation".into()
                        );
                    };
                    files.insert(
                        path,
                        RetainedFile {
                            blob: blob.clone(),
                            sha256: format!("{:x}", Sha256::digest(bytes)),
                            size: bytes.len() as u64,
                        },
                    );
                }
                let record = RetentionRecord {
                    version: crate::protocol::RETENTION_VERSION,
                    operation: operation.clone(),
                    actor: actor.into(),
                    state: RetentionState::Prepared,
                    repository: Some(repository.into()),
                    commit: Some(commit),
                    files,
                };
                let store = self.retention_store()?;
                let (count, bytes) =
                    store.active_stats(validate_retention_record, compare_retention_records)?;
                let size = store.encoded_len(&operation, &crate::binary::encode(&record)?)?;
                if count >= record_store::MAX_ACTIVE_RECORDS
                    || bytes
                        .checked_add(size)
                        .is_none_or(|total| total > record_store::MAX_ACTIVE_BYTES)
                {
                    return Err("release retention admission capacity exceeded".into());
                }
                self.save_retention_record(&record, &mut before_publish)?;
                Ok(RetentionReply::State {
                    version: crate::protocol::RETENTION_VERSION,
                    repository: repository.into(),
                    operation,
                    record: Some(record),
                })
            }
            RetentionCommand::Promote { operation } => {
                let record = stored.as_mut().ok_or("release retention not prepared")?;
                if record.state == RetentionState::Closed {
                    return Err("release retention is closed".into());
                }
                if record.state == RetentionState::Prepared {
                    record.state = RetentionState::Promoted;
                    self.save_retention_record(record, &mut before_publish)?;
                } else {
                    self.fence_retention(&record.state)?;
                }
                Ok(RetentionReply::State {
                    version: crate::protocol::RETENTION_VERSION,
                    repository: repository.into(),
                    operation: operation.clone(),
                    record: stored,
                })
            }
            RetentionCommand::Close { operation } => {
                if let Some(record) = stored.as_mut() {
                    if record.state == RetentionState::Promoted {
                        return Err("promoted release retention cannot be closed".into());
                    }
                    if record.state == RetentionState::Closed {
                        self.fence_retention(&record.state)?;
                        return Ok(RetentionReply::State {
                            version: crate::protocol::RETENTION_VERSION,
                            repository: repository.into(),
                            operation: operation.clone(),
                            record: Some(record.clone()),
                        });
                    }
                    record.state = RetentionState::Closed;
                } else {
                    stored = Some(RetentionRecord {
                        version: crate::protocol::RETENTION_VERSION,
                        operation: operation.clone(),
                        actor: actor.into(),
                        state: RetentionState::Closed,
                        repository: Some(repository.into()),
                        commit: None,
                        files: BTreeMap::new(),
                    });
                }
                self.save_retention_record(
                    stored.as_ref().ok_or("missing retention record")?,
                    &mut before_publish,
                )?;
                Ok(RetentionReply::State {
                    version: crate::protocol::RETENTION_VERSION,
                    repository: repository.into(),
                    operation: operation.clone(),
                    record: stored,
                })
            }
            RetentionCommand::Read {
                operation,
                path,
                offset,
            } => {
                let record = stored.as_ref().ok_or("release retention not found")?;
                if record.state == RetentionState::Closed {
                    return Err("release retention is closed".into());
                }
                let file = record
                    .files
                    .get(&path)
                    .ok_or("path not protected by release retention")?
                    .clone();
                let commit = record.commit.clone().ok_or("invalid release commit")?;
                let snapshot = self.snapshot()?;
                let Some(Object::Wasm(bytes)) = snapshot.objects.get(&file.blob) else {
                    return Err("protected file unavailable locally".into());
                };
                if bytes.len() as u64 != file.size
                    || format!("{:x}", Sha256::digest(bytes)) != file.sha256
                {
                    return Err("protected file checksum mismatch".into());
                }
                let start = usize::try_from(offset).map_err(|_| "invalid release file offset")?;
                if start >= bytes.len() {
                    return Err("invalid release file offset".into());
                }
                self.fence_retention(&record.state)?;
                Ok(RetentionReply::File {
                    version: crate::protocol::RETENTION_VERSION,
                    repository: repository.into(),
                    actor: actor.into(),
                    operation,
                    commit,
                    path,
                    file,
                    offset,
                    bytes: bytes[start
                        ..bytes
                            .len()
                            .min(start.saturating_add(crate::protocol::TRANSFER_CHUNK))]
                        .to_vec(),
                })
            }
        }
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
                "wasm-git-retention-{}-{}",
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

    fn setup() -> (Temp, Repository, String, Vec<u8>) {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        let bytes = b"\0asm\x01\0\0\0".to_vec();
        let tree = repo
            .create_files_tree(&BTreeMap::from([
                ("a.wasm".into(), bytes.clone()),
                ("b.wasm".into(), b"\0asm\x01\0\0\0\0\x02\x01x".to_vec()),
            ]))
            .unwrap();
        let commit = repo.create_commit_object(&tree, &[], "release").unwrap();
        (temp, repo, commit, bytes)
    }
    fn prepare(id: &str, commit: &str) -> RetentionCommand {
        RetentionCommand::Prepare {
            operation: id.into(),
            commit: commit.into(),
            paths: vec!["a.wasm".into()],
        }
    }

    #[test]
    fn publication_checks_run_after_staging_and_only_for_new_decisions() {
        let (_temp, repo, commit, bytes) = setup();
        let id = "a".repeat(64);
        let error = repo
            .release_retention_checked("project", "coordinator", prepare(&id, &commit), || {
                assert!(repo.retention_record(&id).unwrap().is_none());
                assert!(fs::read_dir(repo.common_metadata.join("tmp"))
                    .unwrap()
                    .next()
                    .is_some());
                Err("authorization expired".into())
            })
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(error.message, "authorization expired");
        assert!(repo.retention_record(&id).unwrap().is_none());

        repo.release_retention("project", "coordinator", prepare(&id, &commit))
            .unwrap();
        let prepared = repo.retention_record(&id).unwrap();
        repo.release_retention_checked("project", "coordinator", prepare(&id, &commit), || {
            panic!("exact prepare replay must not reauthorize an existing decision")
        })
        .unwrap();
        repo.release_retention_checked(
            "project",
            "coordinator",
            RetentionCommand::Query {
                operation: id.clone(),
            },
            || panic!("query does not publish"),
        )
        .unwrap();
        let read = repo
            .release_retention_checked(
                "project",
                "coordinator",
                RetentionCommand::Read {
                    operation: id.clone(),
                    path: "a.wasm".into(),
                    offset: 0,
                },
                || panic!("read does not publish"),
            )
            .unwrap();
        assert!(matches!(read, RetentionReply::File { bytes: actual, .. } if actual == bytes));
        let error = repo
            .release_retention_checked(
                "project",
                "coordinator",
                RetentionCommand::Promote {
                    operation: id.clone(),
                },
                || {
                    assert_eq!(repo.retention_record(&id).unwrap(), prepared);
                    Err("authorization expired".into())
                },
            )
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(repo.retention_record(&id).unwrap(), prepared);

        repo.release_retention_checked(
            "project",
            "coordinator",
            RetentionCommand::Promote {
                operation: id.clone(),
            },
            || Ok(()),
        )
        .unwrap();
        repo.release_retention_checked(
            "project",
            "coordinator",
            RetentionCommand::Promote { operation: id },
            || panic!("promote replay must not publish"),
        )
        .unwrap();

        let closed = "b".repeat(64);
        let before = repo.retention_record(&closed).unwrap();
        let error = repo
            .release_retention_checked(
                "project",
                "coordinator",
                RetentionCommand::Close { operation: closed },
                || Err("authorization expired".into()),
            )
            .unwrap_err();
        assert!(!error.outcome_unknown);
        assert_eq!(repo.retention_record(&"b".repeat(64)).unwrap(), before);
    }

    #[test]
    fn tombstones_and_promotion_survive_reopen_and_replays() {
        let (temp, repo, commit, _) = setup();
        let closed = "1".repeat(64);
        repo.release_retention(
            "project",
            "coordinator",
            RetentionCommand::Close {
                operation: closed.clone(),
            },
        )
        .unwrap();
        let reopened = Repository::open(&temp.0).unwrap();
        assert!(reopened
            .release_retention("project", "coordinator", prepare(&closed, &commit))
            .is_err());
        assert!(reopened
            .release_retention(
                "project",
                "other",
                RetentionCommand::Query { operation: closed }
            )
            .is_err());
        let id = "2".repeat(64);
        reopened
            .release_retention("project", "coordinator", prepare(&id, &commit))
            .unwrap();
        assert!(reopened
            .release_retention(
                "other",
                "coordinator",
                RetentionCommand::Query {
                    operation: id.clone()
                }
            )
            .is_err());
        assert!(reopened
            .release_retention(
                "other",
                "coordinator",
                RetentionCommand::Close {
                    operation: id.clone()
                }
            )
            .is_err());
        reopened
            .release_retention("project", "coordinator", prepare(&id, &commit))
            .unwrap();
        assert!(reopened
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Prepare {
                    operation: id.clone(),
                    commit: commit.clone(),
                    paths: vec!["b.wasm".into()]
                }
            )
            .is_err());
        reopened
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Promote {
                    operation: id.clone(),
                },
            )
            .unwrap();
        assert!(Repository::open(&temp.0)
            .unwrap()
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Close {
                    operation: id.clone()
                }
            )
            .is_err());
        assert!(matches!(
            reopened
                .release_retention(
                    "project",
                    "coordinator",
                    RetentionCommand::Query { operation: id }
                )
                .unwrap(),
            RetentionReply::State {
                record: Some(RetentionRecord {
                    state: RetentionState::Promoted,
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn writer_lock_orders_gc_and_pool_eviction_with_protected_bytes() {
        let (_temp, repo, commit, bytes) = setup();
        let id = "3".repeat(64);
        let guard = repo.lock().unwrap();
        assert!(repo
            .release_retention("project", "coordinator", prepare(&id, &commit))
            .is_err());
        drop(guard);
        repo.release_retention("project", "coordinator", prepare(&id, &commit))
            .unwrap();
        repo.garbage_collect_mode(&[], true, true).unwrap();
        let config = repo
            .attach_pool(vec![PromiseSource {
                location: "http://127.0.0.1:9419/pool".into(),
                allow_insecure_http: false,
            }])
            .unwrap();
        let _pool = repo.pool_lock().unwrap();
        assert_eq!(
            repo.evict_pool_blobs(&config, &BTreeSet::from([commit.clone()]))
                .unwrap(),
            1
        );
        match repo
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Read {
                    operation: id.clone(),
                    path: "a.wasm".into(),
                    offset: 0,
                },
            )
            .unwrap()
        {
            RetentionReply::File { bytes: actual, .. } => assert_eq!(actual, bytes),
            _ => panic!("expected retained file"),
        }
        assert!(repo
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Read {
                    operation: id.clone(),
                    path: "b.wasm".into(),
                    offset: 0
                }
            )
            .is_err());
        repo.release_retention(
            "project",
            "coordinator",
            RetentionCommand::Close { operation: id },
        )
        .unwrap();
        repo.garbage_collect_mode(&[], true, false).unwrap();
        assert!(!repo.snapshot().unwrap().objects.contains_key(&commit));
    }

    #[test]
    fn legacy_unbound_retention_remains_protected_and_cannot_be_mutated() {
        let (_temp, repo, commit, bytes) = setup();
        let id = "9".repeat(64);
        // Start with a real old-format store, not a rollback of an already
        // published new-format marker over unrelated authoritative records.
        let snapshot = repo.snapshot().unwrap();
        let Object::Commit(found) = snapshot.objects.get(&commit).unwrap() else {
            panic!("missing commit")
        };
        let Object::Tree(tree) = snapshot.objects.get(found.tree.as_ref().unwrap()).unwrap() else {
            panic!("missing tree")
        };
        let files = BTreeMap::from([(
            "a.wasm".into(),
            RetainedFile {
                blob: tree.entries["a.wasm"].clone(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                size: bytes.len() as u64,
            },
        )]);
        let legacy = LegacyRetentions {
            version: 1,
            records: BTreeMap::from([(
                id.clone(),
                LegacyRetentionRecord {
                    version: 1,
                    operation: id.clone(),
                    actor: "coordinator".into(),
                    state: RetentionState::Prepared,
                    commit: Some(commit.clone()),
                    files,
                },
            )]),
        };
        repo.atomic_write(
            &repo.common_metadata.join("release-retentions.bin"),
            &crate::binary::encode(&legacy).unwrap(),
        )
        .unwrap();
        let query = repo
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Query {
                    operation: id.clone(),
                },
            )
            .unwrap();
        assert!(matches!(
            query,
            RetentionReply::State {
                record: Some(RetentionRecord {
                    repository: None,
                    ..
                }),
                ..
            }
        ));
        assert!(repo
            .release_retention(
                "project",
                "coordinator",
                RetentionCommand::Close { operation: id }
            )
            .is_err());
        repo.garbage_collect_mode(&[], true, true).unwrap();
        assert!(repo.snapshot().unwrap().objects.contains_key(&commit));
    }

    #[test]
    fn concurrent_promote_and_close_have_one_durable_winner() {
        let (_temp, repo, commit, _) = setup();
        let id = "4".repeat(64);
        repo.release_retention("project", "coordinator", prepare(&id, &commit))
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for promote in [true, false] {
            let barrier = barrier.clone();
            let repo = repo.clone();
            let id = id.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..1000 {
                    let command = if promote {
                        RetentionCommand::Promote {
                            operation: id.clone(),
                        }
                    } else {
                        RetentionCommand::Close {
                            operation: id.clone(),
                        }
                    };
                    match repo.release_retention("project", "coordinator", command) {
                        Err(e) if e.message.contains("resource is locked") => {
                            std::thread::sleep(std::time::Duration::from_millis(1))
                        }
                        result => return result.is_ok(),
                    }
                }
                panic!("writer lock did not become available");
            }));
        }
        barrier.wait();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|success| *success)
            .count();
        assert_eq!(successes, 1);
        assert!(matches!(
            repo.release_retention(
                "project",
                "coordinator",
                RetentionCommand::Query { operation: id }
            )
            .unwrap(),
            RetentionReply::State {
                record: Some(RetentionRecord {
                    state: RetentionState::Promoted | RetentionState::Closed,
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn active_watermark_allows_unknown_close_and_keeps_promoted_roots() {
        let (temp, repo, commit, _) = setup();
        let initial = "1".repeat(64);
        let RetentionReply::State {
            record: Some(mut record),
            ..
        } = repo
            .release_retention("project", "coordinator", prepare(&initial, &commit))
            .unwrap()
        else {
            panic!("missing prepared record")
        };
        let store = repo.retention_store().unwrap();
        for index in 0..record_store::MAX_ACTIVE_RECORDS - 1 {
            record.operation = format!("{index:064x}");
            store
                .seed_fixture(
                    &record.operation,
                    Bucket::Active,
                    &crate::binary::encode(&record).unwrap(),
                )
                .unwrap();
        }
        let full = repo.capacity_status().unwrap();
        assert_eq!(
            full.retentions.active_records,
            record_store::MAX_ACTIVE_RECORDS
        );
        assert_eq!(
            full.retentions.total_records,
            record_store::MAX_ACTIVE_RECORDS
        );
        let next = "a".repeat(64);
        assert!(repo
            .release_retention("project", "coordinator", prepare(&next, &commit))
            .unwrap_err()
            .message
            .contains("admission capacity"));
        let closed = "b".repeat(64);
        repo.release_retention(
            "project",
            "coordinator",
            RetentionCommand::Close {
                operation: closed.clone(),
            },
        )
        .unwrap();
        assert!(repo
            .release_retention("project", "coordinator", prepare(&closed, &commit))
            .is_err());
        let promoted = format!("{:064x}", 0);
        repo.release_retention(
            "project",
            "coordinator",
            RetentionCommand::Promote {
                operation: promoted.clone(),
            },
        )
        .unwrap();
        repo.release_retention("project", "coordinator", prepare(&next, &commit))
            .unwrap();
        let after = repo.capacity_status().unwrap();
        assert_eq!(
            after.retentions.active_records,
            record_store::MAX_ACTIVE_RECORDS
        );
        assert_eq!(
            after.retentions.total_records,
            record_store::MAX_ACTIVE_RECORDS + 2
        );
        assert!(after.retentions.total_bytes > after.retentions.active_bytes);
        let reopened = Repository::open(&temp.0).unwrap();
        let _writer = reopened.lock().unwrap();
        assert!(reopened
            .release_retention_roots()
            .unwrap()
            .contains(&commit));
        assert_eq!(
            reopened.retention_record(&promoted).unwrap().unwrap().state,
            RetentionState::Promoted
        );
        assert_eq!(
            reopened.retention_record(&closed).unwrap().unwrap().state,
            RetentionState::Closed
        );
    }
}
