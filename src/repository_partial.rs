//! Explicit shallow boundaries and verified, lazily materialized Wasm objects.
use super::*;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloneFilter {
    #[default]
    All,
    BlobNone,
    BlobLimit(u64),
    Paths(Vec<String>),
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOptions {
    pub boundaries: BTreeSet<String>,
    pub deepen: Option<u32>,
    pub depth: Option<u32>,
    pub branch: Option<String>,
    pub filter: CloneFilter,
}
impl SelectOptions {
    pub fn validate(&self) -> RepositoryResult<()> {
        for id in &self.boundaries {
            require_id(id)?;
        }
        if self.deepen == Some(0) {
            return Err("deepen must be positive".into());
        }
        if self.depth == Some(0) {
            return Err("depth must be positive".into());
        }
        if let Some(branch) = &self.branch {
            validate_ref_name(branch)?;
        }
        if let CloneFilter::Paths(paths) = &self.filter {
            if paths.is_empty() || paths.len() > 4096 {
                return Err("path filter requires 1..=4096 paths".into());
            }
            for path in paths {
                validate_tree_path(path.trim_end_matches('/'))?;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromiseSource {
    pub location: String,
    pub allow_insecure_http: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct PartialState {
    pub shallow: BTreeSet<String>,
    pub promised: BTreeSet<String>,
    pub sources: Vec<PromiseSource>,
    pub selection: Option<SelectOptions>,
}
#[derive(Debug)]
pub(super) struct PromisedWasm {
    id: String,
    repo: Option<Repository>,
    bytes: OnceLock<RepositoryResult<Vec<u8>>>,
}
pub(super) fn blob<'a>(snapshot: &'a Snapshot, id: &str) -> RepositoryResult<&'a [u8]> {
    match snapshot.objects.get(id) {
        Some(Object::Wasm(bytes)) => Ok(bytes),
        Some(Object::Promised(promise)) => promise
            .bytes
            .get_or_init(|| {
                let repo = promise
                    .repo
                    .as_ref()
                    .ok_or("promised object has no configured source")?;
                let raw = repo.resolve_promised(&promise.id)?;
                let (kind, payload) = verified_object_payload(&promise.id, &raw)?;
                if kind != WASM_KIND {
                    return Err("promisor returned a non-Wasm object".into());
                }
                validate_wasm_header(payload)?;
                Ok(payload.to_vec())
            })
            .as_ref()
            .map(Vec::as_slice)
            .map_err(Clone::clone),
        _ => Err(format!("missing or non-Wasm object {id}")),
    }
}
pub(super) fn apply(
    manifest: &Manifest,
    objects: &mut BTreeMap<String, Object>,
    repo: Option<&Repository>,
) -> RepositoryResult<()> {
    for id in &manifest.shallow {
        require_id(id)?;
    }
    for (id, object) in objects.iter_mut() {
        if let Object::Commit(commit) = object {
            commit.shallow = manifest.shallow.contains(id);
        } else if manifest.shallow.contains(id) {
            return Err("shallow boundary is not a commit".into());
        }
    }
    let needed: BTreeSet<_> = objects
        .values()
        .flat_map(|o| match o {
            Object::Commit(c) if c.tree.is_none() => vec![c.wasm.clone()],
            Object::Tree(t) => t.entries.values().cloned().collect(),
            _ => Vec::new(),
        })
        .collect();
    for id in &manifest.promised {
        require_id(id)?;
        match objects.get(id) {
            Some(Object::Wasm(_) | Object::Promised(_)) => (),
            Some(_) => return Err("only Wasm content may be promised".into()),
            None if needed.contains(id) => {
                objects.insert(
                    id.clone(),
                    Object::Promised(Arc::new(PromisedWasm {
                        id: id.clone(),
                        repo: repo.cloned(),
                        bytes: OnceLock::new(),
                    })),
                );
            }
            None => (), // Interrupted installation may leave unused declarations.
        }
    }
    Ok(())
}
impl Repository {
    pub(super) fn partial_state(&self) -> RepositoryResult<PartialState> {
        let path = self.common_metadata.join("partial.bin");
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PartialState::default()),
            Err(e) => Err(e.to_string()),
            Ok(_) => {
                let state: PartialState = crate::binary::decode(&read_regular(&path)?)?;
                for id in state.shallow.iter().chain(&state.promised) {
                    require_id(id)?;
                }
                if let Some(options) = &state.selection {
                    options.validate()?;
                }
                if state.sources.len() > 32 {
                    return Err("too many promisor sources".into());
                }
                Ok(state)
            }
        }
    }
    pub(super) fn install_partial_state(&self, manifest: &Manifest) -> RepositoryResult<()> {
        let mut state = self.partial_state()?;
        if manifest.shallow.is_empty()
            && manifest.promised.is_empty()
            && state.shallow.is_empty()
            && state.promised.is_empty()
        {
            return Ok(());
        }
        state.shallow.extend(manifest.shallow.iter().cloned());
        state.promised.extend(manifest.promised.iter().cloned());
        self.atomic_write(
            &self.common_metadata.join("partial.bin"),
            &crate::binary::encode(&state)?,
        )
    }
    pub fn configure_promisor(
        &self,
        sources: Vec<PromiseSource>,
        selection: Option<SelectOptions>,
    ) -> RepositoryResult<()> {
        if sources.is_empty() || sources.len() > 32 {
            return Err("configure 1..=32 promisor sources".into());
        }
        if let Some(options) = &selection {
            options.validate()?;
        }
        for source in &sources {
            crate::remote::validate_source(source)?;
        }
        let _lock = self.lock()?;
        let mut state = self.partial_state()?;
        state.sources = sources;
        state.selection = selection;
        self.atomic_write(
            &self.common_metadata.join("partial.bin"),
            &crate::binary::encode(&state)?,
        )
    }
    pub fn shallow_boundaries(&self) -> RepositoryResult<BTreeSet<String>> {
        Ok(self.partial_state()?.shallow)
    }
    pub fn clone_selection(&self) -> RepositoryResult<Option<SelectOptions>> {
        Ok(self.partial_state()?.selection)
    }
    pub fn promisor_sources(&self) -> RepositoryResult<Vec<PromiseSource>> {
        Ok(self.partial_state()?.sources)
    }
    pub fn partial_status(&self) -> RepositoryResult<Value> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        let state = self.partial_state()?;
        Ok(
            json!({"shallow":state.shallow,"promised_objects":snapshot.objects.values().filter(|o| matches!(o,Object::Promised(_))).count(),"sources":state.sources,"selection":state.selection}),
        )
    }
    pub(super) fn resolve_promised(&self, id: &str) -> RepositoryResult<Vec<u8>> {
        require_id(id)?;
        let state = self.partial_state()?;
        if !state.promised.contains(id) {
            return Err(format!("missing unpromised object {id}"));
        }
        let path = self.common_metadata.join("objects").join(id);
        if path.exists() {
            let raw = read_regular(&path)?;
            decode_object(id, &raw)?;
            return Ok(raw);
        }
        if let Some(raw) = pack_storage::read_object(self, id)? {
            return Ok(raw);
        }
        let mut failures = Vec::new();
        for source in &state.sources {
            let result = crate::remote::fetch_object_source(source, id).and_then(|raw| {
                if !matches!(decode_object(id, &raw)?, Object::Wasm(_)) {
                    return Err("promisor returned wrong object kind".into());
                }
                self.install_encoded_object(id, &raw)?;
                Ok(raw)
            });
            match result {
                Ok(raw) => return Ok(raw),
                Err(e) => failures.push(e),
            }
        }
        Err(format!(
            "promised object {id} unavailable from {} sources: {}",
            state.sources.len(),
            failures.join("; ")
        ))
    }
    pub fn hydrate(&self) -> RepositoryResult<usize> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        let missing: Vec<_> = snapshot
            .objects
            .iter()
            .filter_map(|(id, o)| matches!(o, Object::Promised(_)).then_some(id))
            .collect();
        let count = missing.len();
        for result in crate::parallel::map_owned(missing, |id| self.resolve_promised(id)) {
            result?;
        }
        Ok(count)
    }

    /// Only objects reachable from advertised references are served, even if a
    /// caller knows the ID of a dangling/private object in this store.
    pub fn reachable_object(&self, id: &str) -> RepositoryResult<Vec<u8>> {
        require_id(id)?;
        let _lock = self.lock()?;
        let head = self.current_branch()?;
        let mut pending: Vec<_> = self
            .read_refs()?
            .into_values()
            .chain(self.read_named_refs("tags", true)?.into_values())
            .chain(valid_id(&head).then_some(head))
            .collect();
        let mut seen = BTreeSet::new();
        let mut reachable = false;
        while let Some(commit_id) = pending.pop() {
            if !seen.insert(commit_id.clone()) {
                continue;
            }
            let commit = self.read_commit_id(&commit_id)?;
            if commit_id == id {
                reachable = true;
                break;
            }
            if let Some(tree_id) = &commit.tree {
                let Object::Tree(tree) = self.read_object(tree_id)? else {
                    return Err("public history contains a non-tree".into());
                };
                if tree_id == id || tree.entries.values().any(|blob| blob == id) {
                    reachable = true;
                    break;
                }
            } else if commit.wasm == id {
                reachable = true;
                break;
            }
            pending.extend(commit.history_parents().iter().cloned());
        }
        if !reachable {
            return Err("object is not publicly reachable in this repository".into());
        }
        let raw = self.read_encoded_object(id)?;
        decode_object(id, &raw)?;
        Ok(raw)
    }
    pub fn selective_bundle(
        &self,
        options: &SelectOptions,
    ) -> RepositoryResult<(RepositoryState, Vec<u8>)> {
        options.validate()?;
        let _lock = self.lock()?;
        let mut manifest = Manifest {
            format: 1,
            head: self.current_branch()?,
            refs: self.read_refs()?,
            tags: self.read_named_refs("tags", true)?,
            shallow: BTreeSet::new(),
            promised: BTreeSet::new(),
        };
        if let Some(branch) = &options.branch {
            let id = manifest
                .refs
                .get(branch)
                .cloned()
                .ok_or("selected branch does not exist")?;
            manifest.refs = BTreeMap::from([(branch.clone(), id)]);
            manifest.head = branch.clone();
            manifest.tags.clear();
        }
        let mut snapshot = Snapshot {
            manifest,
            upstreams: BTreeMap::new(),
            upstream_tips: BTreeMap::new(),
            checkouts: Vec::new(),
            head_reflog: Vec::new(),
            tracking: BTreeMap::new(),
            reflog: Vec::new(),
            encoded: BTreeMap::new(),
            objects: BTreeMap::new(),
        };
        let mut pending: VecDeque<_> = snapshot
            .manifest
            .refs
            .values()
            .chain(snapshot.manifest.tags.values())
            .chain(valid_id(&snapshot.manifest.head).then_some(&snapshot.manifest.head))
            .map(|id| (id.clone(), 1u32, None::<u32>))
            .collect();
        let mut blobs = BTreeSet::new();
        let mut included = BTreeSet::new();
        let inherited = self.partial_state()?;
        let mut visited = BTreeMap::new();
        while let Some((id, depth, mut budget)) = pending.pop_front() {
            if options.boundaries.contains(&id) {
                budget = Some(options.deepen.unwrap_or(0));
            }
            let rank = budget.unwrap_or(u32::MAX);
            if visited.get(&id).is_some_and(|old| *old >= rank) {
                continue;
            }
            visited.insert(id.clone(), rank);
            snapshot.manifest.shallow.remove(&id);
            let bytes = self.read_network_envelope(&id)?;
            let Object::Commit(commit) = decode_object(&id, &bytes)? else {
                return Err("history target is not a commit".into());
            };
            if !commit.parents.is_empty()
                && (options.depth.is_some_and(|limit| depth >= limit)
                    || budget == Some(0)
                    || inherited.shallow.contains(&id))
            {
                snapshot.manifest.shallow.insert(id.clone());
            } else {
                pending.extend(commit.parents.iter().map(|parent| {
                    (
                        parent.clone(),
                        depth.saturating_add(1),
                        budget.map(|n| n.saturating_sub(1)),
                    )
                }));
            }
            if let Some(tree_id) = &commit.tree {
                if !snapshot.objects.contains_key(tree_id) {
                    let raw = self.read_network_envelope(tree_id)?;
                    let Object::Tree(tree) = decode_object(tree_id, &raw)? else {
                        return Err("history tree has wrong kind".into());
                    };
                    for (path, blob) in &tree.entries {
                        blobs.insert(blob.clone());
                        if let CloneFilter::Paths(paths) = &options.filter {
                            if paths.iter().any(|p| {
                                path == p
                                    || path.starts_with(&format!("{}/", p.trim_end_matches('/')))
                            }) {
                                included.insert(blob.clone());
                            }
                        }
                    }
                    snapshot.encoded.insert(tree_id.clone(), raw);
                    snapshot.objects.insert(tree_id.clone(), Object::Tree(tree));
                }
            } else {
                blobs.insert(commit.wasm.clone());
            }
            snapshot.encoded.insert(id.clone(), bytes);
            snapshot.objects.insert(id, Object::Commit(commit));
        }
        let records = crate::parallel::map_owned(
            blobs.into_iter().collect(),
            |id| -> RepositoryResult<_> {
                let need = match &options.filter {
                    CloneFilter::BlobNone => false,
                    CloneFilter::Paths(_) => included.contains(&id),
                    _ => true,
                };
                if !need {
                    return Ok((id, None));
                }
                let bytes = self.read_network_envelope(&id)?;
                if !matches!(decode_object(&id, &bytes)?, Object::Wasm(_)) {
                    return Err("content is not Wasm".into());
                }
                if matches!(options.filter,CloneFilter::BlobLimit(limit) if bytes.len().saturating_sub(OBJECT_HEADER_LENGTH) as u64 > limit)
                {
                    Ok((id, None))
                } else {
                    Ok((id, Some(bytes)))
                }
            },
        );
        for record in records {
            let (id, bytes) = record?;
            if let Some(bytes) = bytes {
                snapshot
                    .objects
                    .insert(id.clone(), decode_object(&id, &bytes)?);
                snapshot.encoded.insert(id, bytes);
            } else {
                snapshot.manifest.promised.insert(id);
            }
        }
        snapshot.manifest.format = manifest_format(
            &snapshot.manifest.head,
            &snapshot.manifest.tags,
            &snapshot.objects,
        );
        apply(&snapshot.manifest, &mut snapshot.objects, None)?;
        validate_snapshot(&snapshot.manifest, &snapshot.objects)?;
        Ok((
            manifest_state(&snapshot.manifest),
            network_storage::compress(&encode_bundle(&snapshot)?)?,
        ))
    }
    pub fn install_selective_bundle(&self, bytes: &[u8]) -> RepositoryResult<RepositoryState> {
        let incoming = if network_storage::compressed(bytes) {
            network_storage::decode_transfer(bytes, None)?
        } else {
            decode_bundle(bytes)?
        };
        let _lock = self.lock()?;
        let current = self.snapshot()?;
        check_collisions(&current, &incoming)?;
        // Clear boundaries only for commits whose complete parents were supplied.
        let mut state = self.partial_state()?;
        for (id, o) in &incoming.objects {
            if matches!(o, Object::Commit(_)) {
                state.shallow.remove(id);
            }
        }
        state
            .shallow
            .extend(incoming.manifest.shallow.iter().cloned());
        state
            .promised
            .extend(incoming.manifest.promised.iter().cloned());
        self.install_snapshot_objects(&incoming)?;
        self.atomic_write(
            &self.common_metadata.join("partial.bin"),
            &crate::binary::encode(&state)?,
        )?;
        Ok(manifest_state(&incoming.manifest))
    }
}

impl Repository {
    pub(super) fn boundary_network_bundle(
        &self,
        revisions: &[String],
    ) -> RepositoryResult<(RepositoryState, Vec<u8>)> {
        if revisions.len() > 4096 {
            return Err("too many transfer roots".into());
        }
        let _lock = self.lock()?;
        let mut snapshot = self.snapshot()?;
        if !revisions.is_empty() {
            let mut refs = BTreeMap::new();
            for (n, revision) in revisions.iter().enumerate() {
                refs.insert(
                    format!("transfer-{n}"),
                    snapshot_revision(&snapshot, revision)?,
                );
            }
            snapshot.manifest.refs = refs;
            snapshot.manifest.tags.clear();
            snapshot.manifest.head = "transfer-0".into();
        }
        let reachable = reachable_objects(&snapshot.manifest, &snapshot.objects)?;
        snapshot.objects.retain(|id, _| reachable.contains(id));
        snapshot.encoded.retain(|id, _| reachable.contains(id));
        snapshot
            .manifest
            .shallow
            .retain(|id| reachable.contains(id));
        for id in &reachable {
            if !snapshot.encoded.contains_key(id) {
                let bytes = self.resolve_promised(id)?;
                snapshot
                    .objects
                    .insert(id.clone(), decode_object(id, &bytes)?);
                snapshot.encoded.insert(id.clone(), bytes);
            }
        }
        snapshot.manifest.promised.clear();
        snapshot.manifest.format = manifest_format(
            &snapshot.manifest.head,
            &snapshot.manifest.tags,
            &snapshot.objects,
        );
        Ok((
            manifest_state(&snapshot.manifest),
            network_storage::compress(&encode_bundle(&snapshot)?)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_promise_cannot_change_an_existing_object_kind() {
        let root = std::env::temp_dir().join(unique_name("promise-kind"));
        let repo = Repository::init(&root).unwrap();
        let id = repo.commit(WASM_HEADER, "known commit").unwrap();
        let current = repo.snapshot().unwrap();
        let mut incoming = current.clone();
        incoming.manifest.promised.insert(id.clone());
        incoming.objects.remove(&id);
        incoming.encoded.remove(&id);
        assert!(check_collisions(&current, &incoming)
            .unwrap_err()
            .contains("type conflicts"));
        repo.fsck().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
