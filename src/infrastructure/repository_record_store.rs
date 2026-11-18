//! Per-identity repository records. Callers hold their domain lock throughout.
use super::*;

pub(super) const MAX_ACTIVE_RECORDS: u64 = 4096;
pub(super) const MAX_ACTIVE_BYTES: u64 = 64 * 1024 * 1024;
// An old 64 MiB ledger record can gain a receipt from an old 64 MiB journal;
// direct failure receipts also need room for their bounded response metadata.
pub(super) const MAX_RECORD_BYTES: u64 = 2 * 64 * 1024 * 1024 + 1024 * 1024;
const MAX_LEGACY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CAPACITY_SCAN: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Bucket {
    Active,
    Terminal,
    Pinned,
}
impl Bucket {
    fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Terminal => "terminal",
            Self::Pinned => "pinned",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    id: String,
    payload: Vec<u8>,
}

pub(super) struct Seed {
    pub id: String,
    pub bucket: Bucket,
    pub payload: Vec<u8>,
}

pub(super) struct StoreError {
    pub message: String,
    pub outcome_unknown: bool,
}
impl From<String> for StoreError {
    fn from(message: String) -> Self {
        Self {
            message,
            outcome_unknown: false,
        }
    }
}

pub(super) type Validate = fn(&str, Bucket, &[u8]) -> RepositoryResult<()>;
pub(super) type Compare = fn(&str, &[u8], Bucket, &[u8]) -> RepositoryResult<()>;

pub(super) struct RecordStore<'a> {
    repo: &'a Repository,
    root: PathBuf,
}

fn owner_matches(path: &Path, marker: &[u8]) -> RepositoryResult<bool> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() != marker.len() as u64
    {
        return Ok(false);
    }
    Ok(read_regular(path)? == marker)
}

struct MigrationScan {
    owner_present: bool,
    records: BTreeSet<String>,
    owner_temps: Vec<PathBuf>,
}

// A renamed root was complete before publication; a staging directory may
// contain any matching prefix of the migration. Neither may hide later data.
fn inspect_migration(
    path: &Path,
    marker: &[u8],
    seeds: &BTreeMap<&str, &Seed>,
    staging: bool,
) -> RepositoryResult<MigrationScan> {
    require_directory(path)?;
    let owner = path.join("owner");
    let owner_present = match fs::symlink_metadata(&owner) {
        Ok(_) if owner_matches(&owner, marker)? => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && staging => false,
        _ => return Err("unrecognized repository record migration owner".into()),
    };
    let mut scan = MigrationScan {
        owner_present,
        records: BTreeSet::new(),
        owner_temps: Vec::new(),
    };
    let mut directories = BTreeSet::new();
    for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid migration entry")?;
        if name == "owner" {
            continue;
        }
        if staging && name.starts_with(".wasm-git-output-") {
            // Only the old owner publication used sibling output files here.
            // Its interrupted write can contain a prefix of the owner marker.
            let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > marker.len() as u64
                || !marker.starts_with(&read_regular(&entry.path())?)
            {
                return Err("unrecognized repository record migration temporary file".into());
            }
            scan.owner_temps.push(entry.path());
            continue;
        }
        if !owner_present {
            return Err("unrecognized repository record migration directory".into());
        }
        let bucket = match name.as_str() {
            "active" => Bucket::Active,
            "terminal" => Bucket::Terminal,
            "pinned" => Bucket::Pinned,
            _ => return Err("unrecognized repository record migration entry".into()),
        };
        require_directory(&entry.path())?;
        directories.insert(name);
        for record in fs::read_dir(entry.path()).map_err(|e| e.to_string())? {
            let record = record.map_err(|e| e.to_string())?;
            let id = record
                .file_name()
                .into_string()
                .map_err(|_| "invalid migration record filename")?;
            require_id(&id)?;
            let seed = seeds
                .get(id.as_str())
                .ok_or("repository record migration contains an additional record")?;
            let expected = RecordStore::encode(&seed.id, &seed.payload)?;
            let metadata = fs::symlink_metadata(record.path()).map_err(|e| e.to_string())?;
            if seed.bucket != bucket
                || !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != expected.len() as u64
                || read_regular(&record.path())? != expected
                || !scan.records.insert(id)
            {
                return Err("repository record migration conflicts with the legacy ledger".into());
            }
        }
    }
    if !staging && (directories.len() != 3 || scan.records.len() != seeds.len()) {
        return Err("repository record migration root does not match the legacy ledger".into());
    }
    Ok(scan)
}

impl<'a> RecordStore<'a> {
    /// The old file is authoritative until the marker is durably published.
    pub(super) fn open(
        repo: &'a Repository,
        name: &str,
        legacy: impl FnOnce(Option<&[u8]>) -> RepositoryResult<Vec<Seed>>,
    ) -> RepositoryResult<Self> {
        let marker = format!("wasm-git-record-store:{name}:1\n").into_bytes();
        let old = repo.common_metadata.join(format!("{name}.bin"));
        let root = repo.common_metadata.join(format!("{name}-records"));
        let staging = repo
            .common_metadata
            .join("tmp")
            .join(format!("{name}-records-migration"));
        let bytes = match fs::symlink_metadata(&old) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
            Ok(m) if !m.is_file() || m.file_type().is_symlink() || m.len() > MAX_LEGACY_BYTES => {
                return Err("invalid repository record store marker or legacy file".into())
            }
            Ok(_) => Some(read_regular(&old)?),
        };
        if bytes.as_deref() == Some(marker.as_slice()) {
            let store = Self { repo, root };
            store.verify_layout(&marker)?;
            // A previous marker rename might have returned an uncertain fence.
            sync_directory(&repo.common_metadata)?;
            return Ok(store);
        }
        let seeds = legacy(bytes.as_deref())?;
        let mut expected = BTreeMap::new();
        for seed in &seeds {
            require_id(&seed.id)?;
            if expected.insert(seed.id.as_str(), seed).is_some() {
                return Err("duplicate repository record migration identity".into());
            }
        }
        let exists = |path: &Path| -> RepositoryResult<bool> {
            match fs::symlink_metadata(path) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.to_string()),
            }
        };
        let root_exists = exists(&root)?;
        let staging_exists = exists(&staging)?;
        if root_exists && staging_exists {
            return Err("conflicting repository record migration directories".into());
        }
        let (directory, scan) = if root_exists {
            (&root, inspect_migration(&root, &marker, &expected, false)?)
        } else if staging_exists {
            (
                &staging,
                inspect_migration(&staging, &marker, &expected, true)?,
            )
        } else {
            fs::create_dir(&staging).map_err(|e| e.to_string())?;
            (
                &staging,
                MigrationScan {
                    owner_present: false,
                    records: BTreeSet::new(),
                    owner_temps: Vec::new(),
                },
            )
        };
        for path in &scan.owner_temps {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        if !scan.owner_present {
            write_new_file(&directory.join("owner"), &marker)?;
        }
        for bucket in [Bucket::Active, Bucket::Terminal, Bucket::Pinned] {
            let path = directory.join(bucket.name());
            match fs::create_dir(&path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    require_directory(&path)?
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        for seed in &seeds {
            let path = directory.join(seed.bucket.name()).join(&seed.id);
            if scan.records.contains(&seed.id) {
                // An interrupted old migration may have written complete bytes
                // but not reached its file sync yet.
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|file| file.sync_all())
                    .map_err(|e| e.to_string())?;
            } else {
                // Stage outside the migration directory, so a stopped write
                // cannot leave a partial file under an authoritative ID.
                repo.atomic_write(&path, &Self::encode(&seed.id, &seed.payload)?)?;
            }
        }
        for bucket in [Bucket::Active, Bucket::Terminal, Bucket::Pinned] {
            sync_directory(&directory.join(bucket.name()))?;
        }
        sync_directory(directory)?;
        if !root_exists {
            fs::rename(&staging, &root).map_err(|e| e.to_string())?;
        }
        sync_directory(&repo.common_metadata.join("tmp"))?;
        sync_directory(&repo.common_metadata)?;
        repo.atomic_write(&old, &marker)?;
        let store = Self { repo, root };
        store.verify_layout(&marker)?;
        Ok(store)
    }

    fn verify_layout(&self, marker: &[u8]) -> RepositoryResult<()> {
        require_directory(&self.root)?;
        if !owner_matches(&self.root.join("owner"), marker)? {
            return Err("invalid repository record store owner".into());
        }
        for bucket in [Bucket::Active, Bucket::Terminal, Bucket::Pinned] {
            require_directory(&self.directory(bucket))?;
        }
        Ok(())
    }

    fn directory(&self, bucket: Bucket) -> PathBuf {
        self.root.join(bucket.name())
    }
    fn path(&self, bucket: Bucket, id: &str) -> PathBuf {
        self.directory(bucket).join(id)
    }

    fn encode(id: &str, payload: &[u8]) -> RepositoryResult<Vec<u8>> {
        require_id(id)?;
        let bytes = crate::binary::encode(&Envelope {
            version: 1,
            id: id.into(),
            payload: payload.to_vec(),
        })?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err("repository record exceeds its size bound".into());
        }
        Ok(bytes)
    }
    pub(super) fn encoded_len(&self, id: &str, payload: &[u8]) -> RepositoryResult<u64> {
        Ok(Self::encode(id, payload)?.len() as u64)
    }
    fn read(&self, bucket: Bucket, id: &str) -> RepositoryResult<Option<Vec<u8>>> {
        require_id(id)?;
        let path = self.path(bucket, id);
        let meta = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
            Ok(m) => m,
        };
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_RECORD_BYTES {
            return Err("invalid repository record file".into());
        }
        let envelope: Envelope = crate::binary::decode(&read_regular(&path)?)?;
        if envelope.version != 1 || envelope.id != id {
            return Err("repository record identity mismatch".into());
        }
        Ok(Some(envelope.payload))
    }
    pub(super) fn fence(&self, bucket: Bucket) -> RepositoryResult<()> {
        sync_directory(&self.directory(bucket))
    }

    pub(super) fn get(
        &self,
        id: &str,
        validate: Validate,
        compare: Compare,
    ) -> RepositoryResult<Option<(Bucket, Vec<u8>)>> {
        self.get_inner(id, validate, compare, true)
    }

    fn get_inner(
        &self,
        id: &str,
        validate: Validate,
        compare: Compare,
        fence_active: bool,
    ) -> RepositoryResult<Option<(Bucket, Vec<u8>)>> {
        let active = self.read(Bucket::Active, id)?;
        let terminal = self.read(Bucket::Terminal, id)?;
        let pinned = self.read(Bucket::Pinned, id)?;
        if terminal.is_some() && pinned.is_some() {
            return Err("conflicting terminal repository records".into());
        }
        let finished = terminal
            .map(|v| (Bucket::Terminal, v))
            .or_else(|| pinned.map(|v| (Bucket::Pinned, v)));
        if let Some((bucket, payload)) = finished {
            validate(id, bucket, &payload)?;
            if let Some(active) = active {
                validate(id, Bucket::Active, &active)?;
                compare(id, &active, bucket, &payload)?;
                self.fence(bucket)?;
                fs::remove_file(self.path(Bucket::Active, id)).map_err(|e| e.to_string())?;
                self.fence(Bucket::Active)?;
            } else {
                self.fence(bucket)?;
            }
            return Ok(Some((bucket, payload)));
        }
        if let Some(payload) = active {
            validate(id, Bucket::Active, &payload)?;
            if fence_active {
                self.fence(Bucket::Active)?;
            }
            Ok(Some((Bucket::Active, payload)))
        } else {
            for bucket in [Bucket::Active, Bucket::Terminal, Bucket::Pinned] {
                self.fence(bucket)?;
            }
            Ok(None)
        }
    }

    pub(super) fn write(
        &self,
        id: &str,
        bucket: Bucket,
        payload: &[u8],
        validate: Validate,
        compare: Compare,
        before_publish: &mut impl FnMut() -> RepositoryResult<()>,
    ) -> Result<(), StoreError> {
        validate(id, bucket, payload)?;
        let bytes = Self::encode(id, payload)?;
        for final_bucket in [Bucket::Terminal, Bucket::Pinned] {
            if let Some(existing) = self.read(final_bucket, id)? {
                validate(id, final_bucket, &existing)?;
                if bucket == final_bucket && existing == payload {
                    self.get(id, validate, compare)?;
                    return Ok(());
                }
                return Err(
                    "immutable terminal repository record conflicts with this transition"
                        .to_owned()
                        .into(),
                );
            }
        }
        if bucket != Bucket::Active {
            if let Some(active) = self.read(Bucket::Active, id)? {
                validate(id, Bucket::Active, &active)?;
                compare(id, &active, bucket, payload)?;
            }
        }
        let mut passed = false;
        self.repo
            .atomic_write_checked(&self.path(bucket, id), &bytes, || {
                before_publish()?;
                passed = true;
                Ok(())
            })
            .map_err(|message| StoreError {
                message,
                outcome_unknown: passed,
            })?;
        if bucket != Bucket::Active {
            let active = self.path(Bucket::Active, id);
            match fs::remove_file(active) {
                Ok(()) => self.fence(Bucket::Active).map_err(|message| StoreError {
                    message,
                    outcome_unknown: true,
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => {
                    return Err(StoreError {
                        message: e.to_string(),
                        outcome_unknown: true,
                    })
                }
            }
        }
        Ok(())
    }

    fn ids(
        &self,
        bucket: Bucket,
    ) -> RepositoryResult<impl Iterator<Item = RepositoryResult<String>>> {
        let entries = fs::read_dir(self.directory(bucket)).map_err(|e| e.to_string())?;
        Ok(entries.map(|entry| {
            let entry = entry.map_err(|e| e.to_string())?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid repository record filename")?;
            require_id(&id)?;
            Ok(id)
        }))
    }

    pub(super) fn active_stats(
        &self,
        validate: Validate,
        compare: Compare,
    ) -> RepositoryResult<(u64, u64)> {
        let mut count = 0u64;
        let mut bytes = 0u64;
        let mut visited = 0u64;
        self.fence(Bucket::Active)?;
        for id in self.ids(Bucket::Active)? {
            visited += 1;
            if visited > MAX_ACTIVE_RECORDS {
                return Err("active repository record index exceeds its bound".into());
            }
            let id = id?;
            if self
                .get_inner(&id, validate, compare, false)?
                .is_some_and(|(bucket, _)| bucket == Bucket::Active)
            {
                count += 1;
                bytes = bytes
                    .checked_add(
                        fs::metadata(self.path(Bucket::Active, &id))
                            .map_err(|e| e.to_string())?
                            .len(),
                    )
                    .ok_or("repository capacity overflow")?;
            }
        }
        Ok((count, bytes))
    }

    pub(super) fn capacity(
        &self,
        validate: Validate,
        compare: Compare,
    ) -> RepositoryResult<crate::protocol::AdmissionCapacity> {
        let (active_records, active_bytes) = self.active_stats(validate, compare)?;
        let mut total_records = 0u64;
        let mut total_bytes = 0u64;
        for bucket in [Bucket::Active, Bucket::Terminal, Bucket::Pinned] {
            for id in self.ids(bucket)? {
                let id = id?;
                if bucket == Bucket::Terminal {
                    match fs::symlink_metadata(self.path(Bucket::Pinned, &id)) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e.to_string()),
                        Ok(_) => return Err("conflicting terminal repository records".into()),
                    }
                }
                total_records += 1;
                if total_records > MAX_CAPACITY_SCAN {
                    return Err(
                        "repository capacity cannot be determined: scan bound exceeded".into(),
                    );
                }
                let metadata =
                    fs::symlink_metadata(self.path(bucket, &id)).map_err(|e| e.to_string())?;
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() > MAX_RECORD_BYTES
                {
                    return Err("invalid repository record file".into());
                }
                total_bytes = total_bytes
                    .checked_add(metadata.len())
                    .ok_or("repository capacity overflow")?;
            }
        }
        Ok(crate::protocol::AdmissionCapacity {
            active_records,
            active_bytes,
            total_records,
            total_bytes,
            max_active_records: MAX_ACTIVE_RECORDS,
            max_active_bytes: MAX_ACTIVE_BYTES,
        })
    }

    pub(super) fn for_each(
        &self,
        bucket: Bucket,
        validate: Validate,
        compare: Compare,
        mut visit: impl FnMut(&str, &[u8]) -> RepositoryResult<()>,
    ) -> RepositoryResult<()> {
        let mut visited = 0u64;
        for id in self.ids(bucket)? {
            visited += 1;
            if bucket == Bucket::Active && visited > MAX_ACTIVE_RECORDS {
                return Err("active repository record index exceeds its bound".into());
            }
            let id = id?;
            if let Some((actual, payload)) = self.get(&id, validate, compare)? {
                if actual == bucket {
                    visit(&id, &payload)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn seed_fixture(
        &self,
        id: &str,
        bucket: Bucket,
        payload: &[u8],
    ) -> RepositoryResult<()> {
        fs::write(self.path(bucket, id), Self::encode(id, payload)?).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const NAME: &str = "migration-test";
    const LEGACY: &[u8] = b"legacy migration fixture";

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "wasm-git-record-migration-{}-{}",
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

    fn seeds() -> Vec<Seed> {
        vec![
            Seed {
                id: "a".repeat(64),
                bucket: Bucket::Active,
                payload: b"active".to_vec(),
            },
            Seed {
                id: "b".repeat(64),
                bucket: Bucket::Terminal,
                payload: b"terminal".to_vec(),
            },
        ]
    }

    fn fixture() -> (Temp, Repository) {
        let temp = Temp::new();
        let repo = Repository::init(&temp.0).unwrap();
        fs::write(repo.common_metadata.join(format!("{NAME}.bin")), LEGACY).unwrap();
        (temp, repo)
    }

    fn open(repo: &Repository) -> RepositoryResult<RecordStore<'_>> {
        RecordStore::open(repo, NAME, |bytes| {
            if bytes != Some(LEGACY) {
                return Err("unexpected legacy fixture".into());
            }
            Ok(seeds())
        })
    }

    fn marker() -> Vec<u8> {
        format!("wasm-git-record-store:{NAME}:1\n").into_bytes()
    }
    fn legacy_path(repo: &Repository) -> PathBuf {
        repo.common_metadata.join(format!("{NAME}.bin"))
    }
    fn staging_path(repo: &Repository) -> PathBuf {
        repo.common_metadata
            .join("tmp")
            .join(format!("{NAME}-records-migration"))
    }

    #[test]
    fn migration_preserves_additional_or_changed_records_after_legacy_reappears() {
        for case in 0..4 {
            let (_temp, repo) = fixture();
            let store = open(&repo).unwrap();
            let path = match case {
                0 => {
                    let id = "a".repeat(64);
                    store.seed_fixture(&id, Bucket::Active, b"changed").unwrap();
                    store.path(Bucket::Active, &id)
                }
                1 => {
                    let id = "c".repeat(64);
                    store
                        .seed_fixture(&id, Bucket::Terminal, b"new tombstone")
                        .unwrap();
                    store.path(Bucket::Terminal, &id)
                }
                2 => {
                    let id = "a".repeat(64);
                    store
                        .seed_fixture(&id, Bucket::Terminal, b"active")
                        .unwrap();
                    store.path(Bucket::Terminal, &id)
                }
                _ => {
                    let path = store.root.join("unexpected");
                    fs::write(&path, b"preserve").unwrap();
                    path
                }
            };
            let before = fs::read(&path).unwrap();
            fs::write(legacy_path(&repo), LEGACY).unwrap();
            assert!(open(&repo).is_err());
            assert_eq!(fs::read(path).unwrap(), before);
            assert_eq!(fs::read(legacy_path(&repo)).unwrap().as_slice(), LEGACY);
        }
    }

    #[test]
    fn migration_resumes_matching_staging_and_reuses_complete_root() {
        let (_temp, repo) = fixture();
        let staging = staging_path(&repo);
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("owner"), marker()).unwrap();
        fs::write(
            staging.join(".wasm-git-output-interrupted.tmp"),
            &marker()[..8],
        )
        .unwrap();
        fs::create_dir(staging.join("active")).unwrap();
        let records = seeds();
        let first = &records[0];
        fs::write(
            staging.join("active").join(&first.id),
            RecordStore::encode(&first.id, &first.payload).unwrap(),
        )
        .unwrap();
        let store = open(&repo).unwrap();
        assert!(!staging.exists());
        assert_eq!(
            store.read(Bucket::Active, &"a".repeat(64)).unwrap(),
            Some(b"active".to_vec())
        );
        assert_eq!(
            store.read(Bucket::Terminal, &"b".repeat(64)).unwrap(),
            Some(b"terminal".to_vec())
        );
        fs::write(legacy_path(&repo), LEGACY).unwrap();
        open(&repo).unwrap();
        assert_eq!(fs::read(legacy_path(&repo)).unwrap(), marker());
    }

    #[test]
    fn migration_rejects_incomplete_or_unrecognized_existing_records() {
        for unexpected in [false, true] {
            let (_temp, repo) = fixture();
            let staging = staging_path(&repo);
            fs::create_dir(&staging).unwrap();
            fs::write(staging.join("owner"), marker()).unwrap();
            fs::create_dir(staging.join("active")).unwrap();
            let id = if unexpected {
                "c".repeat(64)
            } else {
                "a".repeat(64)
            };
            let path = staging.join("active").join(id);
            let bytes = b"unrecognized bytes";
            fs::write(&path, bytes).unwrap();
            assert!(open(&repo).is_err());
            assert_eq!(fs::read(path).unwrap().as_slice(), bytes.as_slice());
            assert_eq!(fs::read(legacy_path(&repo)).unwrap().as_slice(), LEGACY);
            assert!(staging.exists());
        }
    }
}
