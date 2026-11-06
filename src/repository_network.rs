//! Reachability-limited, negotiated binary transfers. Independent Zstandard
//! frames share the native worker pool. A thin transfer is verified against the
//! receiver's immutable objects before any installation or ref publication.
use super::*;
use crate::protocol::MAX_BUNDLE_BYTES;

const MAGIC: &[u8; 8] = b"UWGNET01";
const DELTA_MAGIC: &[u8; 8] = b"UWGTRN01";
const BLOCK: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct FetchOptions {
    pub selection: Option<SelectOptions>,
    pub have: Vec<String>,
}

fn closure(
    roots: impl IntoIterator<Item = String>,
    snapshot: &Snapshot,
) -> RepositoryResult<BTreeSet<String>> {
    let mut pending: Vec<_> = roots.into_iter().collect();
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        match snapshot
            .objects
            .get(&id)
            .ok_or_else(|| format!("missing transfer object {id}"))?
        {
            Object::Commit(c) => {
                pending.extend(c.parents.iter().cloned());
                pending.push(c.tree.as_ref().unwrap_or(&c.wasm).clone());
            }
            Object::Tree(t) => pending.extend(t.entries.values().cloned()),
            _ => (),
        }
    }
    Ok(visited)
}

pub(super) fn compressed(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

pub(super) fn compress(bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err("decoded transfer exceeds 4 GiB".into());
    }
    let blocks = crate::parallel::map_owned(bytes.chunks(BLOCK).collect(), |b| {
        zstd::bulk::compress(b, 3).map_err(|e| e.to_string())
    });
    let mut output = MAGIC.to_vec();
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    for block in blocks {
        let block = block?;
        output.extend_from_slice(&(block.len() as u32).to_be_bytes());
        output.extend_from_slice(&block);
    }
    Ok(output)
}

pub(super) fn decompress(bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
    let mut cursor = Cursor { bytes, position: 0 };
    if cursor.take(8)? != MAGIC {
        return Err("invalid network bundle magic".into());
    }
    let total = cursor.number()?;
    if total == 0 || total as u64 > MAX_BUNDLE_BYTES {
        return Err("decoded transfer exceeds 4 GiB".into());
    }
    let mut blocks = Vec::new();
    let mut remaining = total;
    while remaining > 0 {
        let size = u32::from_be_bytes(cursor.take(4)?.try_into().unwrap()) as usize;
        if size == 0 || size > BLOCK + 65536 {
            return Err("invalid compressed transfer block".into());
        }
        let expected = remaining.min(BLOCK);
        blocks.push((cursor.take(size)?, expected));
        remaining -= expected;
    }
    if cursor.position != bytes.len() {
        return Err("trailing compressed transfer data".into());
    }
    let decoded =
        crate::parallel::map_owned(blocks, |(b, expected)| -> RepositoryResult<Vec<u8>> {
            let frame = zstd::zstd_safe::find_frame_compressed_size(b)
                .map_err(|e| format!("invalid transfer frame: {e}"))?;
            if frame != b.len() {
                return Err("trailing bytes after transfer frame".into());
            }
            let raw = zstd::bulk::decompress(b, expected).map_err(|e| e.to_string())?;
            if raw.len() != expected {
                return Err("decoded transfer block length mismatch".into());
            }
            Ok(raw)
        });
    let mut result = Vec::new();
    result.try_reserve_exact(total).map_err(|e| e.to_string())?;
    for block in decoded {
        result.extend_from_slice(&block?);
    }
    Ok(result)
}

// Delta bases are restricted to commit closures the peer advertised. These are
// storage byte copies, independent of semantic Wasm diff and function matching.
fn delta_candidates(snapshot: &Snapshot, known: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut candidates = BTreeMap::new();
    // Root content stays full in this transfer. This permits self-contained
    // clone deltas while keeping in-transfer dependency depth exactly one.
    let mut anchors = BTreeSet::new();
    for object in snapshot.objects.values() {
        if let Object::Commit(c) = object {
            if c.parents.is_empty() {
                if let Some(tree) = c.tree.as_ref().and_then(|id| snapshot.objects.get(id)) {
                    if let Object::Tree(t) = tree {
                        anchors.extend(t.entries.values().cloned());
                    }
                } else {
                    anchors.insert(c.wasm.clone());
                }
            }
        }
    }
    for (id, object) in &snapshot.objects {
        let Object::Commit(commit) = object else {
            continue;
        };
        if known.contains(id) {
            continue;
        }
        let mut queue: VecDeque<_> = commit.parents.iter().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(parent) = queue.pop_front() {
            if seen.len() >= 1024 {
                break;
            }
            if !seen.insert(parent.clone()) {
                continue;
            }
            let Some(Object::Commit(base)) = snapshot.objects.get(&parent) else {
                continue;
            };
            if !known.contains(&parent) && !base.parents.is_empty() {
                queue.extend(base.parents.iter().cloned());
                continue;
            }
            if let (Some(new_tree), Some(old_tree)) = (&commit.tree, &base.tree) {
                if let (Some(Object::Tree(new)), Some(Object::Tree(old))) = (
                    snapshot.objects.get(new_tree),
                    snapshot.objects.get(old_tree),
                ) {
                    for (path, id) in &new.entries {
                        if let Some(base) = old.entries.get(path) {
                            if id != base && !anchors.contains(id) {
                                candidates.entry(id.clone()).or_insert_with(|| base.clone());
                            }
                        }
                    }
                }
            } else if commit.tree.is_none()
                && base.tree.is_none()
                && commit.wasm != base.wasm
                && !anchors.contains(&commit.wasm)
            {
                candidates
                    .entry(commit.wasm.clone())
                    .or_insert_with(|| base.wasm.clone());
            }
            break;
        }
    }
    candidates
}

pub(super) fn encode_transfer(
    repo: &Repository,
    snapshot: &Snapshot,
    known: &BTreeSet<String>,
) -> RepositoryResult<Vec<u8>> {
    let manifest = encode_manifest(&snapshot.manifest)?;
    let expanded = snapshot
        .encoded
        .values()
        .try_fold(24usize.saturating_add(manifest.len()), |sum, bytes| {
            sum.checked_add(40).and_then(|n| n.checked_add(bytes.len()))
        })
        .ok_or("expanded transfer size overflow")?;
    if expanded as u64 > MAX_BUNDLE_BYTES {
        return Err("expanded transfer exceeds 4 GiB".into());
    }
    let candidates = delta_candidates(snapshot, known);
    let base_ids: BTreeSet<_> = candidates
        .iter()
        .filter(|(id, _)| snapshot.encoded.get(*id).is_some_and(|b| b.len() >= 4096))
        .map(|(_, base)| base.clone())
        .collect();
    let bases: BTreeMap<_, _> = base_ids
        .into_iter()
        .map(|id| (id, std::sync::OnceLock::<RepositoryResult<Vec<u8>>>::new()))
        .collect();
    let records = crate::parallel::map_owned(
        snapshot.encoded.iter().collect(),
        |(id, bytes)| -> RepositoryResult<Vec<u8>> {
            let mut record = crate::binary::oid(id)?.to_vec();
            let mut chosen = None;
            if let Some(base_id) = candidates.get(id) {
                if let Some(cell) = bases.get(base_id) {
                    let base = if let Some(bytes) = snapshot.encoded.get(base_id) {
                        bytes
                    } else {
                        cell.get_or_init(|| {
                            let bytes = repo.read_network_envelope(base_id)?;
                            verify_wasm(base_id, &bytes)?;
                            Ok(bytes)
                        })
                        .as_ref()
                        .map_err(Clone::clone)?
                    };
                    if base.len() <= bytes.len().saturating_mul(2)
                        && bytes.len() <= base.len().saturating_mul(2)
                    {
                        if let Some(delta) = pack_storage::transfer_delta(base, bytes) {
                            if delta.len().saturating_add(64) < bytes.len() / 2 {
                                chosen = Some((base_id, delta));
                            }
                        }
                    }
                }
            }
            if let Some((base, delta)) = chosen {
                record.push(1);
                record.extend_from_slice(&crate::binary::oid(base)?);
                record.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
                record.extend_from_slice(&(delta.len() as u64).to_be_bytes());
                record.extend_from_slice(&delta);
            } else {
                record.push(0);
                record.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
                record.extend_from_slice(bytes);
            }
            Ok(record)
        },
    );
    let mut raw = DELTA_MAGIC.to_vec();
    raw.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    raw.extend_from_slice(&manifest);
    raw.extend_from_slice(&(records.len() as u64).to_be_bytes());
    for record in records {
        let record = record?;
        if raw.len().saturating_add(record.len()) as u64 > MAX_BUNDLE_BYTES {
            return Err("transfer exceeds 4 GiB".into());
        }
        raw.extend_from_slice(&record);
    }
    compress(&raw)
}

fn verify_wasm(id: &str, bytes: &[u8]) -> RepositoryResult<()> {
    let (kind, payload) = verified_object_payload(id, bytes)?;
    if kind != WASM_KIND {
        return Err("network content or delta base is not Wasm".into());
    }
    validate_wasm_header(payload)
}

fn expand_transfer(raw: &[u8], base: Option<&Snapshot>) -> RepositoryResult<Vec<u8>> {
    if !raw.starts_with(DELTA_MAGIC) {
        return Ok(raw.to_vec());
    }
    let mut cursor = Cursor {
        bytes: raw,
        position: 8,
    };
    let length = cursor.number()?;
    let manifest = cursor.take(length)?;
    let count = cursor.number()?;
    if count > raw.len().saturating_sub(cursor.position) / 41 {
        return Err("transfer record count exceeds bytes".into());
    }
    let mut bundle = BUNDLE_MAGIC.to_vec();
    bundle.extend_from_slice(&(length as u64).to_be_bytes());
    bundle.extend_from_slice(manifest);
    bundle.extend_from_slice(&(count as u64).to_be_bytes());
    let mut records = Vec::new();
    let mut full = BTreeMap::new();
    let mut previous = None;
    let mut expanded = bundle.len();
    for _ in 0..count {
        let id: [u8; 32] = cursor.take(32)?.try_into().unwrap();
        if previous.is_some_and(|p| p >= id) {
            return Err("duplicate or unsorted transfer object".into());
        }
        previous = Some(id);
        let mode = cursor.take(1)?[0];
        let (base_id, expected) = match mode {
            0 => (None, None),
            1 => (
                Some(crate::binary::hex(cursor.take(32)?)),
                Some(cursor.number()?),
            ),
            _ => return Err("unknown transfer encoding".into()),
        };
        let length = cursor.number()?;
        let bytes = cursor.take(length)?;
        let expected = expected.unwrap_or(length);
        expanded = expanded
            .checked_add(40)
            .and_then(|n| n.checked_add(expected))
            .ok_or("expanded transfer size overflow")?;
        if expanded as u64 > MAX_BUNDLE_BYTES {
            return Err("expanded transfer exceeds 4 GiB".into());
        }
        if mode == 0 {
            full.insert(crate::binary::hex(&id), bytes);
        }
        records.push((id, base_id, expected, bytes));
    }
    if cursor.position != raw.len() {
        return Err("trailing transfer records".into());
    }
    for (id, base_id, expected, bytes) in records {
        let restored;
        let bytes = if let Some(base_id) = base_id {
            let bytes_base = base
                .and_then(|b| b.encoded.get(&base_id))
                .map(Vec::as_slice)
                .or_else(|| full.get(&base_id).copied())
                .ok_or("missing negotiated/full delta base; chains and cycles are forbidden")?;
            restored = pack_storage::apply_transfer_delta(bytes_base, bytes, expected)?;
            &restored
        } else {
            bytes
        };
        bundle.extend_from_slice(&id);
        bundle.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        bundle.extend_from_slice(bytes);
    }
    Ok(bundle)
}

pub(super) fn decode_transfer(bytes: &[u8], base: Option<&Snapshot>) -> RepositoryResult<Snapshot> {
    let raw = decompress(bytes)?;
    decode_bundle_with_base(&expand_transfer(&raw, base)?, base)
}

impl Repository {
    pub fn network_fetch(
        &self,
        options: Option<&FetchOptions>,
    ) -> RepositoryResult<(RepositoryState, Vec<u8>)> {
        if let Some(selection) = options.and_then(|o| o.selection.as_ref()) {
            return self.selective_bundle(selection);
        }
        self.network_bundle(&[], options.map_or(&[], |o| o.have.as_slice()))
    }

    /// Advertise verified reference targets without
    /// decompressing Wasm blobs. Transfer/fsck still verify complete object data.
    pub fn advertised_state(&self) -> RepositoryResult<RepositoryState> {
        let _lock = self.lock()?;
        let head = self.current_branch()?;
        let branches = self.read_refs()?;
        let tags = self.read_named_refs("tags", true)?;
        let mut pending: Vec<_> = branches.values().chain(tags.values()).cloned().collect();
        if valid_id(&head) {
            pending.push(head.clone());
        }
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            self.read_commit_id(&id)?;
        }
        let format = 1; // RepositoryState schema version, not the object manifest layout.
        Ok(RepositoryState {
            format,
            head,
            branches,
            tags,
        })
    }
    /// Only public branch/tag/HEAD closures leave a hosting repository. `have`
    /// roots known here let the sender omit their complete dependency closure.
    /// Explicit local bundle backups retain their separate full-store behavior.
    pub fn network_bundle(
        &self,
        revisions: &[String],
        have: &[String],
    ) -> RepositoryResult<(RepositoryState, Vec<u8>)> {
        if !self.partial_state()?.shallow.is_empty() {
            return self.boundary_network_bundle(revisions);
        }
        if have.len() > 65536 || revisions.len() > 4096 {
            return Err("too many negotiation roots".into());
        }
        let _lock = self.lock()?;
        let head = self.current_branch()?;
        let refs = self.read_refs()?;
        let tags = self.read_named_refs("tags", true)?;
        let mut manifest = Manifest {
            shallow: BTreeSet::new(),
            promised: BTreeSet::new(),
            format: 1,
            head,
            refs,
            tags,
        };
        if !revisions.is_empty() {
            let mut refs = BTreeMap::new();
            for (i, revision) in revisions.iter().enumerate() {
                // Callers resolve revision expressions before negotiation. Raw
                // IDs and direct advertised branch/tag names are unambiguous.
                let id = if valid_id(revision) {
                    revision.clone()
                } else if revision == "HEAD" {
                    manifest.tip().cloned().ok_or("unborn HEAD")?
                } else {
                    manifest
                        .refs
                        .get(revision)
                        .or_else(|| manifest.tags.get(revision))
                        .cloned()
                        .ok_or("unknown negotiation revision")?
                };
                refs.insert(format!("transfer-{i}"), id);
            }
            manifest.head = "transfer-0".into();
            manifest.refs = refs;
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
        let mut pending: Vec<_> = snapshot
            .manifest
            .refs
            .values()
            .chain(snapshot.manifest.tags.values())
            .cloned()
            .collect();
        if valid_id(&snapshot.manifest.head) {
            pending.push(snapshot.manifest.head.clone());
        }
        while let Some(id) = pending.pop() {
            if snapshot.objects.contains_key(&id) {
                continue;
            }
            let bytes = self.read_network_envelope(&id)?;
            let Object::Commit(commit) = decode_object(&id, &bytes)? else {
                return Err("negotiation root or parent is not a commit".into());
            };
            pending.extend(commit.parents.iter().cloned());
            if let Some(tree_id) = &commit.tree {
                if !snapshot.objects.contains_key(tree_id) {
                    let bytes = self.read_network_envelope(tree_id)?;
                    let Object::Tree(tree) = decode_object(tree_id, &bytes)? else {
                        return Err("commit content is not a tree".into());
                    };
                    for blob in tree.entries.values() {
                        snapshot
                            .objects
                            .entry(blob.clone())
                            .or_insert_with(|| Object::Wasm(Vec::new()));
                    }
                    snapshot.objects.insert(tree_id.clone(), Object::Tree(tree));
                    snapshot.encoded.insert(tree_id.clone(), bytes);
                }
            } else {
                snapshot
                    .objects
                    .entry(commit.wasm.clone())
                    .or_insert_with(|| Object::Wasm(Vec::new()));
            }
            snapshot.objects.insert(id.clone(), Object::Commit(commit));
            snapshot.encoded.insert(id, bytes);
        }
        validated_commit_order(&snapshot.objects)?;
        let mut roots: Vec<_> = snapshot
            .manifest
            .refs
            .values()
            .chain(snapshot.manifest.tags.values())
            .cloned()
            .collect();
        if valid_id(&snapshot.manifest.head) {
            roots.push(snapshot.manifest.head.clone());
        }
        let reachable = closure(roots, &snapshot)?;
        let mut known = Vec::new();
        for id in have {
            require_id(id)?;
            // Unknown haves are normal after independent GC/force-push. They
            // never permit a caller to fetch hidden or unadvertised objects.
            if reachable.contains(id) && matches!(snapshot.objects.get(id), Some(Object::Commit(_)))
            {
                known.push(id.clone());
            }
        }
        let known = closure(known, &snapshot)?;
        snapshot.objects.retain(|id, _| reachable.contains(id));
        snapshot.manifest.format = manifest_format(
            &snapshot.manifest.head,
            &snapshot.manifest.tags,
            &snapshot.objects,
        );
        validate_snapshot(&snapshot.manifest, &snapshot.objects)?;
        snapshot
            .encoded
            .retain(|id, _| reachable.contains(id) && !known.contains(id));
        let missing: Vec<_> = reachable
            .difference(&known)
            .filter(|id| !snapshot.encoded.contains_key(*id))
            .cloned()
            .collect();
        let blobs = crate::parallel::map_owned(missing, |id| {
            let bytes = self.read_network_envelope(&id)?;
            verify_wasm(&id, &bytes)?;
            Ok::<_, String>((id, bytes))
        });
        for blob in blobs {
            let (id, bytes) = blob?;
            snapshot.encoded.insert(id, bytes);
        }
        Ok((
            manifest_state(&snapshot.manifest),
            encode_transfer(self, &snapshot, &known)?,
        ))
    }

    /// Turn either wire representation into a fully verified, self-contained
    /// native bundle. Local base objects are never trusted without verification.
    pub fn complete_network_bundle(&self, bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
        if !compressed(bytes) {
            decode_bundle(bytes)?;
            return Ok(bytes.to_vec());
        }
        let _lock = self.lock()?;
        let base = self.snapshot()?;
        let incoming = decode_transfer(bytes, Some(&base))?;
        if !incoming.manifest.shallow.is_empty() || !incoming.manifest.promised.is_empty() {
            return Err(
                "receive needs a complete graph; deepen or materialize missing objects".into(),
            );
        }
        encode_bundle(&incoming)
    }

    pub fn install_network_bundle(
        &self,
        bytes: &[u8],
        advertised: Option<&RepositoryState>,
    ) -> RepositoryResult<RepositoryState> {
        let _lock = self.lock()?;
        let base = self.snapshot()?;
        let incoming = if compressed(bytes) {
            decode_transfer(bytes, Some(&base))?
        } else {
            decode_bundle(bytes)?
        };
        let state = manifest_state(&incoming.manifest);
        if advertised.is_some_and(|s| s != &state) {
            return Err("remote advertisement does not match the verified bundle".into());
        }
        check_collisions(&base, &incoming)?;
        self.install_snapshot_objects(&incoming)?;
        Ok(state)
    }

    pub fn empty_network_bundle(bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
        if !compressed(bytes) {
            decode_bundle(bytes)?;
            return Ok(bytes.to_vec());
        }
        encode_bundle(&decode_transfer(bytes, None)?)
    }

    pub fn negotiation_haves(&self) -> RepositoryResult<Vec<String>> {
        let _lock = self.lock()?;
        let head = self.current_branch()?;
        let mut roots: BTreeSet<_> = self
            .read_refs()?
            .into_values()
            .chain(self.read_named_refs("tags", true)?.into_values())
            .collect();
        if valid_id(&head) {
            roots.insert(head);
        }
        let directory = self.common_metadata.join("remotes");
        if directory.exists() {
            require_directory(&directory)?;
            for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".tracking.bin"))
                {
                    continue;
                }
                let record: crate::remote::Tracking =
                    crate::binary::decode(&read_regular(&entry.path())?)?;
                if record.state.format != 1 {
                    return Err("invalid tracking state".into());
                }
                roots.extend(record.state.branches.into_values());
                roots.extend(record.state.tags.into_values());
                if valid_id(&record.state.head) {
                    roots.insert(record.state.head);
                }
            }
        }
        let roots: Vec<_> = roots.into_iter().take(65536).collect();
        for id in &roots {
            self.read_commit_id(id)?;
        }
        Ok(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compressed_transfer_checks_each_frame_length_total_and_trailing_data() {
        assert!(Repository::empty_network_bundle(b"not a native bundle").is_err());
        let original = vec![0x77; BLOCK * 2 + 23];
        let encoded = compress(&original).unwrap();
        assert_eq!(decompress(&encoded).unwrap(), original);
        for cut in [0, 8, 16, encoded.len() - 1] {
            assert!(decompress(&encoded[..cut]).is_err());
        }
        let mut bad = encoded.clone();
        bad.push(0);
        assert!(decompress(&bad).is_err());
        let mut bad = encoded.clone();
        bad[8..16].copy_from_slice(&(MAX_BUNDLE_BYTES + 1).to_be_bytes());
        assert!(decompress(&bad).is_err());
        let mut bad = compress(b"single frame").unwrap();
        let size = u32::from_be_bytes(bad[16..20].try_into().unwrap());
        bad[16..20].copy_from_slice(&(size + 1).to_be_bytes());
        bad.push(0);
        assert!(decompress(&bad).unwrap_err().contains("trailing"));
    }
}
