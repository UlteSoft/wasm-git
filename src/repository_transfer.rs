//! Bounded-frame transfer of a frozen, checksummed bundle. Chunks are immutable;
//! no uploaded byte becomes an object or a reference until final verification.
use super::*;
use crate::protocol::{MAX_BUNDLE_BYTES, TRANSFER_CHUNK};
use std::io::{Seek, SeekFrom};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    version: u32,
    total: u64,
}

const MAX_UPLOADS: usize = 32;
const MAX_STAGED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const TRANSFER_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

impl Repository {
    pub(crate) fn service_lock(&self, timeout: std::time::Duration) -> RepositoryResult<File> {
        // Lock the immutable format inode: read-only service requests must not
        // create metadata or require a writable repository filesystem.
        let path = self.common_metadata.join("format");
        let file = File::open(&path).map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(2))
                }
                Err(error) => return Err(format!("repository service busy: {error}")),
            }
        }
    }

    // Called under transfer.lock: reservation uses declared totals, so sparse
    // uploads cannot overcommit the quota. Last successful chunk refreshes TTL.
    fn upload_reservations(&self) -> RepositoryResult<(usize, u64)> {
        let mut count = 0;
        let mut reserved = 0u64;
        for entry in fs::read_dir(self.common_metadata.join("tmp")).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_prefix("upload-")) else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            require_directory(&entry.path())?;
            let meta = entry.path().join("meta");
            let modified = fs::symlink_metadata(&meta)
                .map_err(|e| e.to_string())?
                .modified()
                .map_err(|e| e.to_string())?;
            if modified.elapsed().is_ok_and(|age| age > TRANSFER_TTL) {
                self.remove_upload_locked(id)?;
                continue;
            }
            let upload: Upload = crate::binary::decode(&read_regular(&meta)?)?;
            if upload.version != 1 || upload.total > MAX_BUNDLE_BYTES {
                return Err("invalid upload reservation".into());
            }
            count += 1;
            reserved = reserved
                .checked_add(upload.total)
                .ok_or("upload reservation overflow")?;
        }
        Ok((count, reserved))
    }

    pub(crate) fn expire_transfers(&self) -> RepositoryResult<()> {
        let _lock = self.lock_named("transfer.lock")?;
        self.upload_reservations()?;
        for entry in fs::read_dir(self.common_metadata.join("tmp")).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix("download-"))
                .is_some_and(valid_id)
            {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata
                    .modified()
                    .map_err(|e| e.to_string())?
                    .elapsed()
                    .is_ok_and(|age| age > TRANSFER_TTL)
            {
                fs::remove_file(entry.path()).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    pub fn download_slice(
        &self,
        expected: Option<&str>,
        offset: u64,
    ) -> RepositoryResult<(Option<RepositoryState>, Vec<u8>)> {
        self.download_slice_options(expected, offset, None)
    }

    pub fn download_slice_options(
        &self,
        expected: Option<&str>,
        offset: u64,
        options: Option<&FetchOptions>,
    ) -> RepositoryResult<(Option<RepositoryState>, Vec<u8>)> {
        let (id, state) = if let Some(expected) = expected {
            require_id(expected)?;
            (expected.to_owned(), None)
        } else {
            if offset != 0 {
                return Err("initial download must start at zero".into());
            }
            let (state, network) = self.network_fetch(options)?;
            let bytes = if options.is_some() {
                network
            } else {
                Self::empty_network_bundle(&network)?
            };
            if bytes.len() as u64 > MAX_BUNDLE_BYTES {
                return Err("bundle exceeds the 4 GiB transfer limit".into());
            }
            let id = object_id(&bytes);
            let path = self
                .common_metadata
                .join("tmp")
                .join(format!("download-{id}"));
            let _lock = self.lock_named("transfer.lock")?;
            match fs::symlink_metadata(&path) {
                Ok(m) if m.is_file() && !m.file_type().is_symlink() => (),
                Ok(_) => return Err("invalid download cache file".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let temp = self
                        .common_metadata
                        .join("tmp")
                        .join(unique_name("download"));
                    write_new_file(&temp, &bytes)?;
                    let result = fs::hard_link(&temp, &path).map_err(|e| e.to_string());
                    let _ = fs::remove_file(temp);
                    result?;
                    sync_directory(&self.common_metadata.join("tmp"))?;
                }
                Err(e) => return Err(e.to_string()),
            }
            // Cache eviction may require a reader to retry, but never changes
            // repository objects or silently switches a frozen download.
            let mut cached = Vec::new();
            for entry in
                fs::read_dir(self.common_metadata.join("tmp")).map_err(|e| e.to_string())?
            {
                let entry = entry.map_err(|e| e.to_string())?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.strip_prefix("download-").is_some_and(valid_id) {
                    let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
                    if metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && entry.path() != path
                    {
                        cached.push((
                            metadata.modified().map_err(|e| e.to_string())?,
                            entry.path(),
                            metadata.len(),
                        ));
                    }
                }
            }
            cached.sort();
            let mut count = cached.len() + 1;
            let mut size = cached.iter().map(|(_, _, n)| *n).sum::<u64>() + bytes.len() as u64;
            for (_, path, length) in cached {
                if count <= 8 && size <= MAX_STAGED_BYTES {
                    break;
                }
                fs::remove_file(path).map_err(|e| e.to_string())?;
                count -= 1;
                size -= length;
            }
            (id, options.is_none().then_some(state))
        };
        let path = self
            .common_metadata
            .join("tmp")
            .join(format!("download-{id}"));
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("frozen download unavailable; restart fetch: {e}"))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_BUNDLE_BYTES
            || offset >= metadata.len()
        {
            return Err("invalid frozen download or offset".into());
        }
        let mut file = File::open(&path).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let length = (metadata.len() - offset).min(TRANSFER_CHUNK as u64) as usize;
        let mut payload = vec![0; 40 + length];
        payload[..8].copy_from_slice(&metadata.len().to_be_bytes());
        payload[8..40].copy_from_slice(&crate::binary::oid(&id)?);
        file.read_exact(&mut payload[40..])
            .map_err(|e| e.to_string())?;
        Ok((state, payload))
    }

    pub fn upload_slice(
        &self,
        id: &str,
        total: u64,
        offset: u64,
        bytes: &[u8],
    ) -> RepositoryResult<()> {
        require_id(id)?;
        if total == 0
            || total > MAX_BUNDLE_BYTES
            || !offset.is_multiple_of(TRANSFER_CHUNK as u64)
            || offset >= total
            || bytes.len() as u64 != (total - offset).min(TRANSFER_CHUNK as u64)
        {
            return Err("invalid upload slice bounds".into());
        }
        let _lock = self.lock_named("transfer.lock")?;
        let (count, reserved) = self.upload_reservations()?;
        let directory = self
            .common_metadata
            .join("tmp")
            .join(format!("upload-{id}"));
        match fs::symlink_metadata(&directory) {
            Ok(_) => require_directory(&directory)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if count >= MAX_UPLOADS || reserved.saturating_add(total) > MAX_STAGED_BYTES {
                    return Err("staged upload quota exceeded; complete or cancel an upload".into());
                }
                fs::create_dir(&directory).map_err(|e| e.to_string())?;
                write_new_file(
                    &directory.join("meta"),
                    &crate::binary::encode(&Upload { version: 1, total })
                        .map_err(|e| e.to_string())?,
                )?;
                sync_directory(&directory)?;
            }
            Err(e) => return Err(e.to_string()),
        }
        let meta: Upload = crate::binary::decode(&read_regular(&directory.join("meta"))?)
            .map_err(|e| e.to_string())?;
        if meta.version != 1 || meta.total != total {
            return Err("upload metadata changed".into());
        }
        let path = directory.join(offset.to_string());
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                if read_regular(&path)? == bytes {
                    return Ok(());
                } else {
                    return Err("upload retry contains different bytes".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
        let temp = self.common_metadata.join("tmp").join(unique_name("upload"));
        write_new_file(&temp, bytes)?;
        let result = fs::hard_link(&temp, &path).map_err(|e| e.to_string());
        let _ = fs::remove_file(temp);
        result?;
        File::options()
            .write(true)
            .open(directory.join("meta"))
            .map_err(|e| e.to_string())?
            .set_modified(std::time::SystemTime::now())
            .map_err(|e| e.to_string())?;
        sync_directory(&directory)
    }

    pub fn read_upload(&self, id: &str) -> RepositoryResult<Vec<u8>> {
        require_id(id)?;
        let _lock = self.lock_named("transfer.lock")?;
        let directory = self
            .common_metadata
            .join("tmp")
            .join(format!("upload-{id}"));
        require_directory(&directory)?;
        let meta: Upload = crate::binary::decode(&read_regular(&directory.join("meta"))?)
            .map_err(|e| e.to_string())?;
        if meta.version != 1 || meta.total == 0 || meta.total > MAX_BUNDLE_BYTES {
            return Err("invalid upload metadata".into());
        }
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < meta.total {
            let bytes = read_regular(&directory.join(offset.to_string()))?;
            if bytes.len() as u64 != (meta.total - offset).min(TRANSFER_CHUNK as u64) {
                return Err("upload has an incomplete chunk".into());
            }
            result.try_reserve(bytes.len()).map_err(|e| e.to_string())?;
            result.extend_from_slice(&bytes);
            offset += bytes.len() as u64;
        }
        if object_id(&result) != id {
            return Err("uploaded bundle checksum mismatch".into());
        }
        Ok(result)
    }

    pub fn remove_upload(&self, id: &str) -> RepositoryResult<()> {
        require_id(id)?;
        let _lock = self.lock_named("transfer.lock")?;
        self.remove_upload_locked(id)
    }

    fn remove_upload_locked(&self, id: &str) -> RepositoryResult<()> {
        let directory = self
            .common_metadata
            .join("tmp")
            .join(format!("upload-{id}"));
        require_directory(&directory)?;
        // Delete only the metadata and precisely named regular chunks owned by
        // this transfer; an unexpected entry prevents recursive deletion.
        let mut paths = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("invalid upload entry")?;
            if name != "meta"
                && name
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|n| n.to_string() != name)
            {
                return Err("unexpected upload entry".into());
            }
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err("unexpected upload file type".into());
            }
            paths.push(entry.path());
        }
        for path in paths {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        fs::remove_dir(directory).map_err(|e| e.to_string())?;
        sync_directory(&self.common_metadata.join("tmp"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "uwgit-transfer-{}-{}",
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
    fn gc_expires_only_abandoned_transfers_and_a_dry_run_preserves_them() {
        let t = Temp::new();
        let repo = Repository::init(&t.0).unwrap();
        let old = "a".repeat(64);
        let live = "b".repeat(64);
        for id in [&old, &live] {
            repo.upload_slice(id, 1, 0, &[1]).unwrap();
        }
        let meta = repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{old}/meta"));
        File::options()
            .write(true)
            .open(meta)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - TRANSFER_TTL - std::time::Duration::from_secs(1),
            )
            .unwrap();
        repo.garbage_collect(&[], false).unwrap();
        assert!(repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{old}"))
            .exists());
        repo.garbage_collect_mode(&[], false, true).unwrap();
        assert!(!repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{old}"))
            .exists());
        assert!(repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{live}"))
            .exists());
    }

    #[test]
    fn uploads_are_immutable_complete_and_hash_checked_before_use() {
        let t = Temp::new();
        let repo = Repository::init(&t.0).unwrap();
        let bytes = vec![0x52; TRANSFER_CHUNK + 17];
        let id = object_id(&bytes);
        assert!(repo
            .upload_slice(&id, bytes.len() as u64, 1, &bytes[..17])
            .is_err());
        repo.upload_slice(&id, bytes.len() as u64, 0, &bytes[..TRANSFER_CHUNK])
            .unwrap();
        repo.upload_slice(&id, bytes.len() as u64, 0, &bytes[..TRANSFER_CHUNK])
            .unwrap();
        assert!(repo
            .upload_slice(&id, bytes.len() as u64, 0, &vec![0; TRANSFER_CHUNK])
            .is_err());
        assert!(repo.read_upload(&id).is_err());
        repo.upload_slice(
            &id,
            bytes.len() as u64,
            TRANSFER_CHUNK as u64,
            &bytes[TRANSFER_CHUNK..],
        )
        .unwrap();
        assert_eq!(repo.read_upload(&id).unwrap(), bytes);
        let dir = repo
            .common_metadata
            .join("tmp")
            .join(format!("upload-{id}"));
        fs::write(dir.join("0"), vec![0; TRANSFER_CHUNK]).unwrap();
        assert!(repo.read_upload(&id).is_err());
        fs::write(dir.join("foreign"), b"retain").unwrap();
        assert!(repo.remove_upload(&id).is_err());
        assert!(dir.join("0").exists());
        fs::remove_file(dir.join("foreign")).unwrap();
        repo.remove_upload(&id).unwrap();
        assert!(!dir.exists());
        assert_eq!(repo.fsck().unwrap().commits, 0);
    }

    #[test]
    fn a_frozen_download_does_not_switch_to_a_later_repository_state() {
        let t = Temp::new();
        let repo = Repository::init(&t.0).unwrap();
        repo.commit(b"\0asm\x01\0\0\0", "initial").unwrap();
        let (state, expected) = repo.bundle_bytes(None).unwrap();
        let (advertised, first) = repo.download_slice(None, 0).unwrap();
        assert_eq!(advertised, Some(state));
        let id = crate::binary::hex(&first[8..40]);
        assert_eq!(id, object_id(&expected));
        repo.commit(b"\0asm\x01\0\0\0\0\x02\x01x", "later").unwrap();
        let (_, frozen) = repo.download_slice(Some(&id), 0).unwrap();
        assert_eq!(first, frozen);
        assert_eq!(&frozen[40..], expected);
        assert!(repo
            .download_slice(Some(&id), expected.len() as u64)
            .is_err());
        assert!(repo.download_slice(Some("../escape"), 0).is_err());
    }
}
