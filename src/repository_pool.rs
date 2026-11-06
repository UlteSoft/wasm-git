//! Replicated immutable object pools. Publication uses each node's normal
//! verified receive/CAS path; source references remain authoritative locally.
use super::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    pub version: u32,
    pub identity: String,
    pub nodes: Vec<PromiseSource>,
}
impl Repository {
    /// One gate covers replication, eviction, release, reconfiguration and
    /// forks across all worktrees. Never unlink this inode: a waiting process
    /// must not lock a different file while an earlier owner is still active.
    pub(crate) fn pool_lock(&self) -> RepositoryResult<File> {
        let path = self.common_metadata.join("pool.gate");
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err("invalid object pool gate".into());
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| e.to_string())?;
        file.try_lock()
            .map_err(|e| format!("object pool operation is busy: {e}"))?;
        Ok(file)
    }

    pub fn pool_config(&self) -> RepositoryResult<PoolConfig> {
        let config: PoolConfig =
            crate::binary::decode(&read_regular(&self.common_metadata.join("pool.bin"))?)?;
        if config.version != 1
            || !valid_id(&config.identity)
            || config.nodes.is_empty()
            || config.nodes.len() > 32
        {
            return Err("invalid object pool configuration".into());
        }
        Ok(config)
    }
    pub fn attach_pool(&self, nodes: Vec<PromiseSource>) -> RepositoryResult<PoolConfig> {
        let _pool_lock = self.pool_lock()?;
        if nodes.is_empty() || nodes.len() > 32 {
            return Err("object pool requires 1..=32 nodes".into());
        }
        for node in &nodes {
            crate::remote::validate_source(node)?;
            if !crate::remote::is_remote_url(&node.location)
                && crate::platform_fs::canonicalize(&node.location).map_err(|e| e.to_string())?
                    == crate::platform_fs::canonicalize(&self.root).map_err(|e| e.to_string())?
            {
                return Err("repository cannot be its own pool node".into());
            }
        }
        let unique: BTreeSet<_> = nodes.iter().map(|s| &s.location).collect();
        if unique.len() != nodes.len() {
            return Err("duplicate pool nodes".into());
        }
        let _lock = self.lock()?;
        let mut random = [0; 32];
        getrandom::fill(&mut random).map_err(|e| e.to_string())?;
        let identity = if self.common_metadata.join("pool.bin").exists() {
            self.pool_config()?.identity
        } else {
            object_id(&random)
        };
        let config = PoolConfig {
            version: 1,
            identity,
            nodes,
        };
        self.atomic_write(
            &self.common_metadata.join("pool.bin"),
            &crate::binary::encode(&config)?,
        )?;
        Ok(config)
    }
    /// All stored commits are pinned so reflogs, pending operations, and
    /// otherwise dangling recoverable commits remain readable after eviction.
    pub fn pool_pin_roots(&self) -> RepositoryResult<Vec<String>> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        Ok(snapshot
            .objects
            .iter()
            .filter_map(|(id, o)| matches!(o, Object::Commit(_)).then_some(id.clone()))
            .collect())
    }
    pub fn pool_publication(&self) -> RepositoryResult<(Vec<String>, Vec<u8>)> {
        let _lock = self.lock()?;
        let mut snapshot = self.snapshot()?;
        if !snapshot.manifest.shallow.is_empty() {
            return Err("unshallow before publishing to an independent object pool".into());
        }
        let promised: Vec<_> = snapshot
            .objects
            .iter()
            .filter_map(|(id, o)| matches!(o, Object::Promised(_)).then_some(id.clone()))
            .collect();
        for id in promised {
            let bytes = self.resolve_promised(&id)?;
            snapshot
                .objects
                .insert(id.clone(), decode_object(&id, &bytes)?);
            snapshot.encoded.insert(id, bytes);
        }
        snapshot.manifest.promised.clear();
        let roots = snapshot
            .objects
            .iter()
            .filter_map(|(id, o)| matches!(o, Object::Commit(_)).then_some(id.clone()))
            .collect();
        Ok((
            roots,
            network_storage::compress(&encode_bundle(&snapshot)?)?,
        ))
    }
    // The caller holds pool_lock through acknowledgement and eviction, so a
    // concurrent release cannot remove the just-verified node pins.
    pub(crate) fn evict_pool_blobs(
        &self,
        config: &PoolConfig,
        replicated: &BTreeSet<String>,
    ) -> RepositoryResult<usize> {
        let _lock = self.lock()?;
        if crate::binary::encode(&self.pool_config()?)? != crate::binary::encode(config)? {
            return Err("pool configuration changed; local data retained".into());
        }
        let mut snapshot = self.snapshot()?;
        let reachable = reachable_from_roots(replicated.iter().cloned(), &snapshot.objects)?;
        let blobs: BTreeSet<_> = snapshot
            .objects
            .iter()
            .filter_map(|(id, o)| {
                (matches!(o, Object::Wasm(_)) && reachable.contains(id)).then_some(id.clone())
            })
            .collect();
        let mut state = self.partial_state()?;
        state.sources = config.nodes.clone();
        state.promised.extend(blobs.iter().cloned());
        // Promises become durable before any local copy is removed.
        self.atomic_write(
            &self.common_metadata.join("partial.bin"),
            &crate::binary::encode(&state)?,
        )?;
        let keep = snapshot
            .encoded
            .keys()
            .filter(|id| !blobs.contains(*id))
            .cloned()
            .collect();
        // Repack retains full graph metadata but may omit only the acknowledged
        // Wasm content. Shared pack links elsewhere remain independent.
        pack_storage::repack(self, &snapshot, &keep)?;
        snapshot.manifest.promised.extend(blobs.iter().cloned());
        Ok(blobs.len())
    }
}
