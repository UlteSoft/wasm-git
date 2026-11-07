//! Immutable indexed history packs. Bounded block deltas reuse arbitrary source
//! ranges, compressed with Zstandard on the shared all-CPU worker pool.
//! Object IDs remain hashes of the original envelopes, independent of packing.
use super::*;
use std::cell::RefCell;
use std::sync::OnceLock;
#[path = "repository_delta.rs"]
mod block_delta;
#[path = "repository_pack_plan.rs"]
mod planning;
use std::io::{Seek, SeekFrom};

const MAGIC: &[u8; 8] = b"WGTPACK4";
const MAX_INDEX: u64 = 64 * 1024 * 1024;
const MAX_OBJECT: u64 = 4 * 1024 * 1024 * 1024;
const FIXED_INDEX: usize = 256 * 4;
const ENTRY_BYTES: usize = 72;
const MAX_DEPTH: u8 = 8;
const COMPRESSION_LEVEL: i32 = 6;
const FULL: u32 = u32::MAX;
const EXTERNAL: u32 = u32::MAX - 1;

// Separate sorted raw OIDs from records, as Git idx v2 does, so binary search
// touches a compact identifier table. No per-object strings or maps on reads.
struct Index {
    fanout: [u32; 256],
    ids: Vec<[u8; 32]>,
    entries: Vec<Entry>,
}
#[derive(Clone)]
struct Entry {
    offset: u64,
    compressed_len: u64,
    raw_len: u64,
    base: u32,
    payload_len: u64,
    crc: u32,
    external: [u8; 32],
}
impl Index {
    fn find(&self, id: &str) -> RepositoryResult<Option<usize>> {
        let raw = crate::binary::oid(id)?;
        let bucket = raw[0] as usize;
        let start = if bucket == 0 {
            0
        } else {
            self.fanout[bucket - 1] as usize
        };
        let end = self.fanout[bucket] as usize;
        Ok(self.ids[start..end]
            .binary_search(&raw)
            .ok()
            .map(|i| start + i))
    }
}

pub(super) fn has_packs(repo: &Repository) -> RepositoryResult<bool> {
    Ok(!paths(repo)?.is_empty())
}

pub(super) fn supported_format(bytes: &[u8]) -> bool {
    bytes == FORMAT
}

fn paths(repo: &Repository) -> RepositoryResult<Vec<PathBuf>> {
    let directory = repo.common_metadata.join("packs");
    match fs::symlink_metadata(&directory) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
        Ok(_) => require_directory(&directory)?,
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid pack filename")?;
        require_id(name.strip_suffix(".pack").ok_or("invalid pack filename")?)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("pack must be a regular file".into());
        }
        result.push(entry.path());
    }
    result.sort();
    Ok(result)
}

fn index(file: &mut File) -> RepositoryResult<(Index, u64)> {
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut header = [0; 12];
    file.read_exact(&mut header)
        .map_err(|e| format!("read pack header: {e}"))?;
    if &header[..8] != MAGIC {
        return Err("unsupported binary pack format".into());
    }
    let count = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    let len = count
        .checked_mul(32 + ENTRY_BYTES)
        .and_then(|n| n.checked_add(FIXED_INDEX))
        .ok_or("pack index overflow")?;
    let start = 12_u64 + len as u64 + 32;
    if len as u64 > MAX_INDEX || start > size {
        return Err("invalid pack index length".into());
    }
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    let mut checksum = [0; 32];
    file.read_exact(&mut checksum).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    hash.update(header);
    hash.update(&bytes);
    if hash.finalize().as_slice() != checksum {
        return Err("pack index checksum mismatch".into());
    }
    let mut r = crate::binary::Reader::new(&bytes);
    let mut fanout = [0_u32; 256];
    let mut last = 0;
    for value in &mut fanout {
        *value = r.u32()?;
        if *value < last || *value as usize > count {
            return Err("invalid pack fanout".into());
        }
        last = *value;
    }
    if last as usize != count {
        return Err("pack fanout/count mismatch".into());
    }
    let mut ids = Vec::<[u8; 32]>::with_capacity(count);
    let mut expected_fanout = [0_u32; 256];
    for _ in 0..count {
        let id: [u8; 32] = r.take(32)?.try_into().unwrap();
        if ids.last().is_some_and(|old| old >= &id) {
            return Err("pack OIDs duplicated or out of order".into());
        }
        expected_fanout[id[0] as usize] += 1;
        ids.push(id);
    }
    for i in 1..256 {
        expected_fanout[i] += expected_fanout[i - 1];
    }
    if expected_fanout != fanout {
        return Err("pack fanout does not describe OID table".into());
    }
    let mut entries = Vec::<Entry>::with_capacity(count);
    for _ in 0..count {
        let entry = Entry {
            offset: r.u64()?,
            compressed_len: r.u64()?,
            raw_len: r.u64()?,
            base: r.u32()?,
            payload_len: r.u64()?,
            crc: r.u32()?,
            external: r.take(32)?.try_into().unwrap(),
        };
        if (entry.base == EXTERNAL) == (entry.external == [0; 32])
            || entry.compressed_len == 0
            || entry.raw_len > MAX_OBJECT
            || entry.raw_len < OBJECT_HEADER_LENGTH as u64
            || entry.payload_len == 0
            || entry.payload_len > entry.raw_len + 5
            || (entry.base == FULL && entry.payload_len != entry.raw_len)
        {
            return Err("invalid pack entry bounds".into());
        }
        entries.push(entry);
    }
    // Physical data order follows history; the cache-friendly OID table is
    // independently sorted. Validate the entire dependency graph by offsets.
    let mut order: Vec<_> = (0..count).collect();
    order.sort_unstable_by_key(|&i| entries[i].offset);
    let mut end = 0_u64;
    let mut depths = vec![0_u8; count];
    for i in order {
        let entry = &entries[i];
        if entry.offset != end {
            return Err("pack entries overlap or contain a gap".into());
        }
        end = end
            .checked_add(entry.compressed_len)
            .ok_or("pack entry length overflow")?;
        if entry.base == EXTERNAL {
            depths[i] = 1;
        } else if entry.base != FULL {
            let base = entries
                .get(entry.base as usize)
                .ok_or("missing delta base")?;
            if base.offset >= entry.offset {
                return Err("pack delta base must precede its child".into());
            }
            let depth = depths[entry.base as usize] + 1;
            if depth > MAX_DEPTH {
                return Err("pack delta depth exceeds 8".into());
            }
            depths[i] = depth;
        }
    }
    r.end()?;
    if start.checked_add(end) != Some(size) {
        return Err("pack payload length mismatch".into());
    }
    Ok((
        Index {
            fanout,
            ids,
            entries,
        },
        start,
    ))
}

thread_local! {
    static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

fn compress(bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
    COMPRESSOR.with(|cell| {
        let mut context = cell.borrow_mut();
        if context.is_none() {
            *context =
                Some(zstd::bulk::Compressor::new(COMPRESSION_LEVEL).map_err(|e| e.to_string())?);
        }
        context
            .as_mut()
            .unwrap()
            .compress(bytes)
            .map_err(|e| format!("compress pack: {e}"))
    })
}

fn decompress(entry: &Entry, compressed: &[u8], base: Option<&[u8]>) -> RepositoryResult<Vec<u8>> {
    if crc32fast::hash(compressed) != entry.crc {
        return Err("packed object CRC mismatch".into());
    }
    let frame_len = zstd::zstd_safe::find_frame_compressed_size(compressed)
        .map_err(|e| format!("invalid packed frame: {e}"))?;
    if frame_len != compressed.len() {
        return Err("trailing bytes after packed frame".into());
    }
    let length =
        usize::try_from(entry.payload_len).map_err(|_| "pack payload exceeds host size")?;
    let payload = DECOMPRESSOR.with(|cell| {
        let mut context = cell.borrow_mut();
        if context.is_none() {
            // Single-pass bulk decoding writes into the bounded output, with
            // no separately allocated sliding window dictated by the frame.
            *context = Some(zstd::bulk::Decompressor::new().map_err(|e| e.to_string())?);
        }
        context
            .as_mut()
            .unwrap()
            .decompress(compressed, length)
            .map_err(|e| format!("decompress pack: {e}"))
    })?;
    if payload.len() != length {
        return Err("packed payload length mismatch".into());
    }
    if let Some(base) = base {
        block_delta::apply(
            base,
            &payload,
            usize::try_from(entry.raw_len).map_err(|_| "decoded object exceeds host size")?,
        )
    } else {
        Ok(payload)
    }
}

fn decode(
    repo: &Repository,
    seen: &mut BTreeSet<String>,
    depth: u8,
    file: &mut File,
    index: &Index,
    position: (u64, usize),
    verify_envelopes: bool,
) -> RepositoryResult<Vec<u8>> {
    let (start, number) = position;
    if depth > MAX_DEPTH {
        return Err("cross-pack delta depth exceeds 8".into());
    }
    let entry = &index.entries[number];
    // index() already proves acyclicity and the maximum chain depth. A point
    // lookup retains only the current base and result, not a cloned chain.
    let base = if entry.base == FULL {
        None
    } else if entry.base == EXTERNAL {
        let id = crate::binary::hex(&entry.external);
        Some(
            read_recursive(repo, &id, true, seen, depth + 1)?
                .ok_or("missing cross-pack delta base")?,
        )
    } else {
        Some(decode(
            repo,
            seen,
            depth + 1,
            file,
            index,
            (start, entry.base as usize),
            verify_envelopes,
        )?)
    };
    file.seek(SeekFrom::Start(
        start
            .checked_add(entry.offset)
            .ok_or("pack offset overflow")?,
    ))
    .map_err(|e| e.to_string())?;
    let mut compressed = vec![
        0;
        usize::try_from(entry.compressed_len)
            .map_err(|_| "pack object exceeds host size")?
    ];
    file.read_exact(&mut compressed)
        .map_err(|e| e.to_string())?;
    let bytes = decompress(entry, &compressed, base.as_deref())?;
    if verify_envelopes {
        decode_object(&crate::binary::hex(&index.ids[number]), &bytes)?;
    }
    Ok(bytes)
}

pub(super) fn read_object(repo: &Repository, id: &str) -> RepositoryResult<Option<Vec<u8>>> {
    read_for_verification(repo, id, true)
}

// CRC, frame bounds and delta bounds always run. The network caller verifies
// the final original envelope exactly once, without allocating a second Wasm
// copy for each intermediate delta base. Full fsck still checks every object.
fn read_for_verification(
    repo: &Repository,
    id: &str,
    verify: bool,
) -> RepositoryResult<Option<Vec<u8>>> {
    read_recursive(repo, id, verify, &mut BTreeSet::new(), 0)
}

fn read_recursive(
    repo: &Repository,
    id: &str,
    verify: bool,
    seen: &mut BTreeSet<String>,
    depth: u8,
) -> RepositoryResult<Option<Vec<u8>>> {
    if depth > MAX_DEPTH || !seen.insert(id.into()) {
        return Err("cross-pack delta cycle or depth exceeds 8".into());
    }
    for path in paths(repo)? {
        let mut file = File::open(&path).map_err(|e| e.to_string())?;
        let (index, start) = index(&mut file)?;
        if let Some(number) = index.find(id)? {
            return decode(
                repo,
                seen,
                depth,
                &mut file,
                &index,
                (start, number),
                verify,
            )
            .map(Some);
        }
    }
    Ok(None)
}

pub(super) fn load_all(repo: &Repository) -> RepositoryResult<BTreeMap<String, Vec<u8>>> {
    struct Packed {
        raw: Vec<u8>,
        index: Index,
        start: u64,
        first: usize,
    }
    let mut packs = Vec::new();
    let mut nodes = Vec::new();
    let mut by_id = BTreeMap::new();
    for path in paths(repo)? {
        let raw = read_regular(&path)?;
        if path.file_stem().and_then(|s| s.to_str()) != Some(object_id(&raw).as_str()) {
            return Err("pack checksum mismatch".into());
        }
        let mut file = File::open(&path).map_err(|e| e.to_string())?;
        let (index, start) = index(&mut file)?;
        let first = nodes.len();
        for (number, id) in index.ids.iter().enumerate() {
            by_id.entry(*id).or_insert(nodes.len());
            nodes.push((packs.len(), number));
        }
        packs.push(Packed {
            raw,
            index,
            start,
            first,
        });
    }
    let dependencies: Vec<Option<usize>> = nodes
        .iter()
        .map(|&(p, n)| {
            let entry = &packs[p].index.entries[n];
            match entry.base {
                FULL => Ok(None),
                EXTERNAL => by_id
                    .get(&entry.external)
                    .copied()
                    .map(Some)
                    .ok_or_else(|| "missing cross-pack delta base".to_owned()),
                base => Ok(Some(packs[p].first + base as usize)),
            }
        })
        .collect::<RepositoryResult<_>>()?;
    // Validate the whole multi-pack graph once. A bounded DFS includes cached
    // ancestor depths; splitting a chain across files cannot reset its budget.
    fn visit(
        i: usize,
        edges: &[Option<usize>],
        states: &mut [u8],
        depths: &mut [u8],
        stack: u8,
    ) -> RepositoryResult<u8> {
        if stack > MAX_DEPTH {
            return Err("cross-pack delta depth exceeds 8".into());
        }
        match states[i] {
            1 => return Err("cross-pack delta cycle".into()),
            2 => return Ok(depths[i]),
            _ => (),
        }
        states[i] = 1;
        let depth = if let Some(parent) = edges[i] {
            visit(parent, edges, states, depths, stack + 1)?
                .checked_add(1)
                .ok_or("delta depth overflow")?
        } else {
            0
        };
        if depth > MAX_DEPTH {
            return Err("cross-pack delta depth exceeds 8".into());
        }
        states[i] = 2;
        depths[i] = depth;
        Ok(depth)
    }
    let mut depths = vec![0; nodes.len()];
    let mut states = vec![0; nodes.len()];
    for i in 0..nodes.len() {
        visit(i, &dependencies, &mut states, &mut depths, 0)?;
    }
    let mut decoded: Vec<Option<Vec<u8>>> = (0..nodes.len()).map(|_| None).collect();
    for depth in 0..=MAX_DEPTH {
        let jobs: Vec<_> = (0..nodes.len()).filter(|&i| depths[i] == depth).collect();
        let layer = crate::parallel::map_owned(jobs, |i| -> RepositoryResult<_> {
            let (p, n) = nodes[i];
            let pack = &packs[p];
            let entry = &pack.index.entries[n];
            let start = pack
                .start
                .checked_add(entry.offset)
                .ok_or("pack offset overflow")?;
            let end = start
                .checked_add(entry.compressed_len)
                .ok_or("pack length overflow")?;
            let compressed = pack
                .raw
                .get(
                    usize::try_from(start).map_err(|_| "pack exceeds host size")?
                        ..usize::try_from(end).map_err(|_| "pack exceeds host size")?,
                )
                .ok_or("pack entry exceeds file")?;
            let base = dependencies[i]
                .map(|parent| {
                    decoded[parent]
                        .as_deref()
                        .ok_or("undecoded cross-pack base")
                })
                .transpose()?;
            Ok((i, decompress(entry, compressed, base)?))
        });
        for result in layer {
            let (i, bytes) = result?;
            decoded[i] = Some(bytes);
        }
    }
    let mut result = BTreeMap::new();
    for (i, bytes) in decoded.into_iter().enumerate() {
        let (p, n) = nodes[i];
        let id = crate::binary::hex(&packs[p].index.ids[n]);
        let bytes = bytes.ok_or("missing decoded packed object")?;
        if result.get(&id).is_some_and(|old| old != &bytes) {
            return Err("packed object collision".into());
        }
        // snapshot verifies every original envelope exactly once after all
        // physical representations agree. No cached result bypasses hashing.
        result.insert(id, bytes);
    }
    Ok(result)
}

pub(super) fn transfer_delta(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    block_delta::encode(
        base,
        &block_delta::Index::default(),
        target,
        (target.len() / 2).min(16 * 1024 * 1024),
    )
}
pub(super) fn apply_transfer_delta(
    base: &[u8],
    instructions: &[u8],
    expected: usize,
) -> RepositoryResult<Vec<u8>> {
    block_delta::apply(base, instructions, expected)
}

pub(super) fn repack(
    repo: &Repository,
    snapshot: &Snapshot,
    keep: &BTreeSet<String>,
) -> RepositoryResult<Value> {
    let old_packs = paths(repo)?;
    let directory = repo.common_metadata.join("packs");
    if !directory.exists() {
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
    }
    require_directory(&directory)?;
    // A fork owns another link to these immutable packs. Keep a complete shared
    // base when every object is retained, and pack only the private increment.
    // Explicit pruning can still rewrite a base that contains discarded objects.
    let mut shared_packs = BTreeSet::new();
    let mut shared_ids = BTreeSet::new();
    let mut external_anchors = BTreeSet::new();
    for path in &old_packs {
        #[cfg(unix)]
        let shared = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(path).map_err(|e| e.to_string())?.nlink() > 1
        };
        #[cfg(not(unix))]
        let shared = false;
        if shared {
            let mut file = File::open(path).map_err(|e| e.to_string())?;
            let (index, _) = index(&mut file)?;
            let ids: BTreeSet<_> = index.ids.iter().map(|id| crate::binary::hex(id)).collect();
            if ids.is_subset(keep) {
                external_anchors.extend(
                    index
                        .ids
                        .iter()
                        .zip(&index.entries)
                        .filter(|(_, e)| e.base == FULL)
                        .map(|(id, _)| crate::binary::hex(id)),
                );
                shared_ids.extend(ids);
                shared_packs.insert(path.clone());
            }
        }
    }
    let objects: Vec<_> = snapshot
        .encoded
        .iter()
        .filter(|(id, _)| keep.contains(*id) && !shared_ids.contains(*id))
        .collect();
    if FIXED_INDEX as u64 + objects.len() as u64 * (32 + ENTRY_BYTES as u64) > MAX_INDEX {
        return Err("pack index exceeds 64 MiB".into());
    }
    if objects
        .iter()
        .any(|(_, bytes)| bytes.len() as u64 > MAX_OBJECT)
    {
        return Err("pack object exceeds the 4 GiB decoded-object limit".into());
    }
    struct Candidate {
        external: [u8; 32],
        base: u32,
        payload_len: u64,
        data: Vec<u8>,
    }
    struct Encoded {
        number: usize,
        fallback: Candidate,
        local: Option<Candidate>,
    }
    let anchors: Vec<OnceLock<block_delta::Index>> =
        (0..objects.len()).map(|_| OnceLock::new()).collect();
    let jobs = planning::jobs(snapshot, &objects)?;
    let requested = jobs
        .iter()
        .filter(|job| job.number == job.anchor)
        .map(|job| objects[job.number].0.clone())
        .collect();
    let external_bases = planning::external_bases(snapshot, &external_anchors, &requested);
    let encoded = crate::parallel::map_owned(jobs, |job| -> RepositoryResult<Encoded> {
        let bytes = objects[job.number].1;
        let mut fallback = Candidate {
            base: FULL,
            external: [0; 32],
            payload_len: bytes.len() as u64,
            data: compress(bytes)?,
        };
        let mut local = None;
        if bytes.len() >= 128 && fallback.data.len() > 64 {
            // Always keep the full-anchor alternative so enforcing depth never
            // needs a second pass or a last-minute full recompression.
            for base_number in [
                Some(job.anchor).filter(|&i| i != job.number),
                job.previous.filter(|&i| i != job.anchor),
            ]
            .into_iter()
            .flatten()
            {
                let base = objects[base_number].1;
                if base.len() > bytes.len().saturating_mul(2)
                    || bytes.len() > base.len().saturating_mul(2)
                {
                    continue;
                }
                let previous_index;
                let block_index = if base_number == job.anchor {
                    anchors[base_number].get_or_init(block_delta::Index::default)
                } else {
                    previous_index = block_delta::Index::default();
                    &previous_index
                };
                let limit = (bytes.len() / 2).min(16 * 1024 * 1024);
                if let Some(delta) = block_delta::encode(base, block_index, bytes, limit) {
                    let data = compress(&delta)?;
                    if data.len().saturating_add(16) < fallback.data.len() {
                        let candidate = Candidate {
                            base: base_number as u32,
                            external: [0; 32],
                            payload_len: delta.len() as u64,
                            data,
                        };
                        if base_number == job.anchor {
                            fallback = candidate;
                        } else {
                            local = Some(candidate);
                        }
                    }
                }
            }
        }
        if let Some(base_id) = external_bases.get(objects[job.number].0) {
            if let Some(base) = snapshot.encoded.get(base_id) {
                if let Some(delta) = transfer_delta(base, bytes) {
                    let data = compress(&delta)?;
                    if data.len().saturating_add(48) < fallback.data.len() {
                        fallback = Candidate {
                            base: EXTERNAL,
                            external: crate::binary::oid(base_id)?,
                            payload_len: delta.len() as u64,
                            data,
                        };
                    }
                }
            }
        }
        Ok(Encoded {
            number: job.number,
            fallback,
            local,
        })
    });
    let mut entries: Vec<Option<Entry>> = (0..objects.len()).map(|_| None).collect();
    let ids: Vec<_> = objects
        .iter()
        .map(|(id, _)| crate::binary::oid(id))
        .collect::<RepositoryResult<_>>()?;
    let mut data = Vec::new();
    let mut raw_bytes = 0_u64;
    let mut deltas = 0;
    let mut cross_pack_deltas = 0;
    let mut depths = vec![0_u8; objects.len()];
    for object in encoded {
        let object = object?;
        let chosen = object
            .local
            .filter(|c| depths[c.base as usize] < MAX_DEPTH)
            .unwrap_or(object.fallback);
        if chosen.base != FULL {
            depths[object.number] = if chosen.base == EXTERNAL {
                cross_pack_deltas += 1;
                1
            } else {
                depths[chosen.base as usize] + 1
            };
            deltas += 1;
        }
        let bytes = objects[object.number].1;
        entries[object.number] = Some(Entry {
            offset: data.len() as u64,
            compressed_len: chosen.data.len() as u64,
            raw_len: bytes.len() as u64,
            base: chosen.base,
            payload_len: chosen.payload_len,
            crc: crc32fast::hash(&chosen.data),
            external: chosen.external,
        });
        raw_bytes = raw_bytes
            .checked_add(bytes.len() as u64)
            .ok_or("packed byte count overflow")?;
        data.extend_from_slice(&chosen.data);
    }
    let count = u32::try_from(entries.len()).map_err(|_| "too many packed objects")?;
    if FIXED_INDEX as u64 + u64::from(count) * (32 + ENTRY_BYTES as u64) > MAX_INDEX {
        return Err("pack index exceeds 64 MiB".into());
    }
    let mut w = crate::binary::Writer(MAGIC.to_vec());
    w.u32(count);
    let mut fanout = [0_u32; 256];
    for id in &ids {
        fanout[id[0] as usize] += 1;
    }
    for i in 1..256 {
        fanout[i] += fanout[i - 1];
    }
    for n in fanout {
        w.u32(n);
    }
    for id in &ids {
        w.0.extend_from_slice(id);
    }
    for entry in entries {
        let entry = entry.ok_or("missing encoded pack entry")?;
        w.u64(entry.offset);
        w.u64(entry.compressed_len);
        w.u64(entry.raw_len);
        w.u32(entry.base);
        w.u64(entry.payload_len);
        w.u32(entry.crc);
        w.0.extend_from_slice(&entry.external);
    }
    let mut raw = w.0;
    let checksum = Sha256::digest(&raw);
    raw.extend_from_slice(&checksum);
    raw.extend_from_slice(&data);
    let id = object_id(&raw);
    let destination = directory.join(format!("{id}.pack"));
    if !destination.exists() {
        let temporary = repo.common_metadata.join("tmp").join(unique_name("pack"));
        write_new_file(&temporary, &raw)?;
        let result = fs::hard_link(&temporary, &destination).map_err(|e| e.to_string());
        let _ = fs::remove_file(&temporary);
        result?;
    } else if read_regular(&destination)? != raw {
        return Err("pack collision".into());
    }
    sync_directory(&directory)?;
    // Complete pack first, then loose commits child-before-parent, then old
    // packs. Every interrupted prefix still contains complete dependencies.
    let mut order = validated_commit_order(&snapshot.objects)?;
    order.reverse();
    for kind in [TREE_KIND, WASM_KIND] {
        order.extend(
            snapshot
                .encoded
                .iter()
                .filter(|(_, b)| b[8] == kind)
                .map(|(id, _)| id.clone()),
        );
    }
    for id in order {
        let path = repo.common_metadata.join("objects").join(&id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                if read_regular(&path)? != snapshot.encoded[&id] {
                    return Err("loose object changed during repack".into());
                }
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    sync_directory(&repo.common_metadata.join("objects"))?;
    for path in old_packs {
        if path != destination && !shared_packs.contains(&path) {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
    }
    sync_directory(&directory)?;
    Ok(
        json!({"packing":true,"pack":id,"objects":keep.len(),"shared_base_packs":shared_packs.len(),"shared_base_objects":shared_ids.len(),"raw_object_bytes":raw_bytes,"pack_bytes":raw.len(),"delta_objects":deltas,"cross_pack_delta_objects":cross_pack_deltas,"max_delta_depth":depths.iter().max().copied().unwrap_or(0),"delta_depth_limit":MAX_DEPTH,"codec":"zstd","compression_level":COMPRESSION_LEVEL,"delta_algorithm":"block_copy_insert","anchor_interval":planning::ANCHOR_INTERVAL,"threads":crate::parallel::worker_count(),"original_object_ids_preserved":true}),
    )
}

pub(super) fn link_packs(
    source: &Repository,
    destination: &Repository,
    required: bool,
) -> RepositoryResult<BTreeSet<String>> {
    let source_paths = paths(source)?;
    let mut ids = BTreeSet::new();
    if source_paths.is_empty() {
        return Ok(ids);
    }
    let directory = destination.common_metadata.join("packs");
    fs::create_dir(&directory).map_err(|e| e.to_string())?;
    for path in source_paths {
        let target = directory.join(path.file_name().ok_or("invalid pack path")?);
        match fs::hard_link(&path, target) {
            Ok(()) => {
                let mut file = File::open(&path).map_err(|e| e.to_string())?;
                ids.extend(
                    index(&mut file)?
                        .0
                        .ids
                        .iter()
                        .map(|id| crate::binary::hex(id)),
                );
            }
            Err(e) if required => return Err(format!("fork requires one filesystem: {e}")),
            Err(_) => (),
        }
    }
    sync_directory(&directory)?;
    Ok(ids)
}

impl Repository {
    pub(super) fn read_encoded_object(&self, id: &str) -> RepositoryResult<Vec<u8>> {
        let path = self.common_metadata.join("objects").join(id);
        match fs::symlink_metadata(&path) {
            Ok(_) => read_regular(&path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                read_object(self, id)?.map_or_else(|| self.resolve_promised(id), Ok)
            }
            Err(e) => Err(e.to_string()),
        }
    }
    pub(super) fn read_network_envelope(&self, id: &str) -> RepositoryResult<Vec<u8>> {
        require_id(id)?;
        let path = self.common_metadata.join("objects").join(id);
        match fs::symlink_metadata(&path) {
            Ok(_) => read_regular(&path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                read_for_verification(self, id, false)?
                    .map_or_else(|| self.resolve_promised(id), Ok)
            }
            Err(e) => Err(e.to_string()),
        }
    }
    pub fn repack(&self) -> RepositoryResult<Value> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        let keep = snapshot.objects.keys().cloned().collect();
        repack(self, &snapshot, &keep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Build independent physical order and OID order, including valid outer
    // checksums. The negative cases must reach the inner format validators.
    fn raw_pack(objects: &[(String, u32, u64, u64, Vec<u8>)]) -> Vec<u8> {
        let mut sorted: Vec<_> = (0..objects.len()).collect();
        sorted.sort_by_key(|&i| &objects[i].0);
        let mut numbers = vec![0; objects.len()];
        for (number, &physical) in sorted.iter().enumerate() {
            numbers[physical] = number as u32;
        }
        let mut offsets = Vec::new();
        let mut data = Vec::new();
        for object in objects {
            offsets.push(data.len() as u64);
            data.extend_from_slice(&object.4);
        }
        let mut w = crate::binary::Writer(MAGIC.to_vec());
        w.u32(objects.len() as u32);
        let mut fanout = [0_u32; 256];
        for object in objects {
            fanout[crate::binary::oid(&object.0).unwrap()[0] as usize] += 1;
        }
        for i in 1..256 {
            fanout[i] += fanout[i - 1];
        }
        for n in fanout {
            w.u32(n);
        }
        for &i in &sorted {
            w.oid(&objects[i].0).unwrap();
        }
        for &i in &sorted {
            let object = &objects[i];
            w.u64(offsets[i]);
            w.u64(object.4.len() as u64);
            w.u64(object.2);
            w.u32(if object.1 == FULL {
                FULL
            } else {
                numbers[object.1 as usize]
            });
            w.u64(object.3);
            w.u32(crc32fast::hash(&object.4));
            w.0.extend_from_slice(&[0; 32]);
        }
        let digest = Sha256::digest(&w.0);
        w.0.extend_from_slice(&digest);
        w.0.extend(data);
        w.0
    }
    #[test]
    fn valid_outer_checksums_cannot_hide_malformed_delta_instructions_or_extra_frames() {
        let base = encode_object(WASM_KIND, b"\0asm\x01\0\0\0").unwrap();
        let target = encode_object(WASM_KIND, b"\0asm\x01\0\0\0\0\x02\x01x").unwrap();
        for (case, instructions, extra, expected) in [
            (0, vec![2], false, "unknown delta"),
            (1, vec![1, 0, 0, 1, 0, 0, 0, 0, 1], false, "exceeds base"),
            (2, vec![0, 0, 0, 0, 0], false, "empty delta"),
            (3, vec![0, 0, 0, 0, 1, 0], false, "length mismatch"),
            (4, vec![0, 0, 0, 0, 1, 0], true, "trailing bytes"),
        ] {
            let path = std::env::temp_dir().join(format!(
                "wasm-git-pack-bounds-{}-{case}",
                std::process::id()
            ));
            let repo = Repository::init(&path).unwrap();
            fs::create_dir(repo.common_metadata.join("packs")).unwrap();
            let mut compressed = compress(&instructions).unwrap();
            if extra {
                compressed.extend(compress(b"extra").unwrap());
            }
            let raw = raw_pack(&[
                (
                    object_id(&base),
                    FULL,
                    base.len() as u64,
                    base.len() as u64,
                    compress(&base).unwrap(),
                ),
                (
                    object_id(&target),
                    0,
                    target.len() as u64,
                    instructions.len() as u64,
                    compressed,
                ),
            ]);
            let pack = repo
                .common_metadata
                .join("packs")
                .join(format!("{}.pack", object_id(&raw)));
            fs::write(pack, &raw).unwrap();
            let error = repo.fsck().unwrap_err();
            assert!(error.contains(expected), "{case}: {error}");
            assert_eq!(
                read_regular(&repo.common_metadata.join("HEAD")).unwrap(),
                encode_reference("main", false).unwrap()
            );
            fs::remove_dir_all(path).unwrap();
        }
    }
    #[test]
    fn network_fast_reads_still_verify_original_object_ids_with_valid_pack_checksums() {
        let path = std::env::temp_dir().join(unique_name("wasm-git-network-hash"));
        let repo = Repository::init(&path).unwrap();
        let tip = repo.commit(WASM_HEADER, "base").unwrap();
        let blob = repo.read_commit_id(&tip).unwrap().wasm;
        let wrong = encode_object(WASM_KIND, b"\0asm\x01\0\0\0\0\x02\x01x").unwrap();
        let raw = raw_pack(&[(
            blob.clone(),
            FULL,
            wrong.len() as u64,
            wrong.len() as u64,
            compress(&wrong).unwrap(),
        )]);
        fs::create_dir(repo.common_metadata.join("packs")).unwrap();
        fs::write(
            repo.common_metadata
                .join("packs")
                .join(format!("{}.pack", object_id(&raw))),
            raw,
        )
        .unwrap();
        fs::remove_file(repo.common_metadata.join("objects").join(blob)).unwrap();
        assert!(repo
            .network_bundle(&[], &[])
            .unwrap_err()
            .contains("object hash mismatch"));
        assert_eq!(repo.read_refs().unwrap().get("main"), Some(&tip));
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn cyclic_and_excessively_deep_bases_are_rejected_by_the_index() {
        for (label, length) in [("cycle", 2), ("depth", 10)] {
            let objects: Vec<_> = (0..length)
                .map(|i| {
                    (
                        format!("{i:064x}"),
                        if i == 0 {
                            FULL
                        } else if label == "cycle" {
                            i as u32
                        } else {
                            i as u32 - 1
                        },
                        64,
                        if i == 0 { 64 } else { 9 },
                        vec![1, 2, 3],
                    )
                })
                .collect();
            let raw = raw_pack(&objects);
            let path = std::env::temp_dir().join(format!(
                "wasm-git-pack-index-{}-{label}",
                std::process::id()
            ));
            fs::write(&path, &raw).unwrap();
            let mut file = File::open(&path).unwrap();
            let error = index(&mut file).err().unwrap();
            assert!(
                error.contains(if label == "cycle" { "precede" } else { "depth" }),
                "{error}"
            );
            fs::remove_file(path).unwrap();
        }
    }
}

#[cfg(test)]
mod cross_pack_tests {
    use super::*;
    fn external(repo: &Repository, id: &str, base: &str, delta: &[u8], raw_len: usize) {
        let compressed = zstd::bulk::compress(delta, 1).unwrap();
        let oid = crate::binary::oid(id).unwrap();
        let mut w = crate::binary::Writer(MAGIC.to_vec());
        w.u32(1);
        for bucket in 0..256 {
            w.u32(u32::from(bucket >= oid[0] as usize));
        }
        w.0.extend_from_slice(&oid);
        w.u64(0);
        w.u64(compressed.len() as u64);
        w.u64(raw_len as u64);
        w.u32(EXTERNAL);
        w.u64(delta.len() as u64);
        w.u32(crc32fast::hash(&compressed));
        w.oid(base).unwrap();
        let checksum = Sha256::digest(&w.0);
        w.0.extend_from_slice(&checksum);
        w.0.extend(compressed);
        let directory = repo.common_metadata.join("packs");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(format!("{}.pack", object_id(&w.0))), w.0).unwrap();
    }
    #[test]
    fn external_cycles_and_missing_bases_fail_before_reconstruction() {
        let root = std::env::temp_dir().join(unique_name("cross-pack-cycle"));
        let repo = Repository::init(&root).unwrap();
        let a = "1".repeat(64);
        let b = "2".repeat(64);
        external(&repo, &a, &b, &[0], 25);
        assert!(repo.fsck().unwrap_err().contains("missing cross-pack"));
        external(&repo, &b, &a, &[0], 25);
        assert!(repo.fsck().unwrap_err().contains("cycle"));
        assert!(read_object(&repo, &a).unwrap_err().contains("cycle"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn depth_limit_counts_all_packs_not_each_file_independently() {
        let root = std::env::temp_dir().join(unique_name("cross-pack-depth"));
        let repo = Repository::init(&root).unwrap();
        let mut raw = b"\0asm\x01\0\0\0\0\x83\x04\x01x".to_vec();
        raw.extend(vec![0; 513]);
        let mut previous = encode_object(WASM_KIND, &raw).unwrap();
        let mut base = object_id(&previous);
        // A packed full anchor starts the chain.
        let mut snapshot = repo.snapshot().unwrap();
        snapshot.encoded.insert(base.clone(), previous.clone());
        snapshot
            .objects
            .insert(base.clone(), decode_object(&base, &previous).unwrap());
        repack(&repo, &snapshot, &BTreeSet::from([base.clone()])).unwrap();
        let mut last = String::new();
        for n in 1..=9 {
            raw[20] = n;
            let next = encode_object(WASM_KIND, &raw).unwrap();
            let id = object_id(&next);
            let delta = block_delta::encode(&previous, &block_delta::Index::default(), &next, 1024)
                .unwrap();
            external(&repo, &id, &base, &delta, next.len());
            previous = next;
            base = id.clone();
            last = id;
        }
        assert!(read_object(&repo, &last).unwrap_err().contains("depth"));
        assert!(repo.fsck().unwrap_err().contains("depth"));
        fs::remove_dir_all(root).unwrap();
    }
}
