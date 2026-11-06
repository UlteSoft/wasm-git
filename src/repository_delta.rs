//! Bounded block matching for storage, independent of semantic Wasm diff.
//! Hashes only select candidates; every copied byte is compared to the source.
use crate::binary::{Reader, Writer};
use std::sync::OnceLock;

const BLOCK: usize = 16;
const MAX_SAMPLES: usize = 32 * 1024;
const PROBES: usize = 4;

#[derive(Default)]
pub(super) struct Index {
    slots: OnceLock<Vec<u32>>,
}

fn slot(bytes: &[u8], mask: usize) -> usize {
    let value = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let hash = (value ^ (value >> 23)).wrapping_mul(0x9e3779b185ebca87);
    ((hash ^ (hash >> 32)) as usize) & mask
}

impl Index {
    /// At most 256 KiB per base, including multi-GiB objects. Large bases use
    /// wider sampling; target scan positions remain independent of alignment.
    fn build(base: &[u8]) -> Vec<u32> {
        let stride = base.len().div_ceil(MAX_SAMPLES).max(BLOCK).div_ceil(BLOCK) * BLOCK;
        let count = base.len() / stride;
        let size = (count.max(8) * 2).next_power_of_two();
        let mut slots = vec![0; size];
        for position in (0..base.len().saturating_sub(BLOCK - 1)).step_by(stride) {
            let first = slot(&base[position..], size - 1);
            for probe in 0..PROBES {
                let cell = &mut slots[(first + probe) & (size - 1)];
                if *cell == 0 {
                    *cell = (position + 1) as u32;
                    break;
                }
                let previous = *cell as usize - 1;
                if base[previous..previous + BLOCK] == base[position..position + BLOCK] {
                    break;
                }
            }
        }
        slots
    }

    fn longest(
        &self,
        base: &[u8],
        target: &[u8],
        hint: Option<usize>,
        misses: usize,
    ) -> (usize, usize) {
        // Follow the last copy's displacement before consulting the sparse
        // index. Dense small edits then need only their actual changed bytes,
        // even when large bases require widely spaced index samples.
        let at = |position: usize| {
            base.get(position..position.checked_add(BLOCK)?)
                .filter(|bytes| *bytes == &target[..BLOCK])
                .map(|_| {
                    (
                        position,
                        BLOCK + common_prefix(&base[position + BLOCK..], &target[BLOCK..]),
                    )
                })
        };
        if let Some(position) = hint {
            if let Some(found) = at(position) {
                return found;
            }
            // One-byte edits resume on the next position. Small insertions or
            // deletions usually resume within this cache-local neighborhood;
            // neither case needs to allocate/build a full base hash table.
            if misses > 0 && misses < 32 {
                for distance in 1..=32 {
                    if let Some(found) = position.checked_sub(distance).and_then(at) {
                        return found;
                    }
                    if let Some(found) = position.checked_add(distance).and_then(at) {
                        return found;
                    }
                }
            }
        }
        if misses < BLOCK {
            return (0, 0);
        }
        let slots = self.slots.get_or_init(|| Self::build(base));
        let mask = slots.len() - 1;
        let first = slot(target, mask);
        let mut best = (0, 0);
        for probe in 0..PROBES {
            let cell = slots[(first + probe) & mask];
            if cell == 0 {
                break;
            }
            let position = cell as usize - 1;
            if base[position..position + BLOCK] != target[..BLOCK] {
                continue;
            }
            let length = BLOCK + common_prefix(&base[position + BLOCK..], &target[BLOCK..]);
            if length > best.1 {
                best = (position, length);
            }
            if length == target.len() {
                break;
            }
        }
        best
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let mut position = 0;
    while len - position >= 8 {
        let left = u64::from_le_bytes(a[position..position + 8].try_into().unwrap());
        let right = u64::from_le_bytes(b[position..position + 8].try_into().unwrap());
        let different = left ^ right;
        if different != 0 {
            return position + different.trailing_zeros() as usize / 8;
        }
        position += 8;
    }
    while position < len && a[position] == b[position] {
        position += 1;
    }
    position
}

fn insert(w: &mut Writer, bytes: &[u8]) {
    for chunk in bytes.chunks(u32::MAX as usize) {
        w.byte(0);
        w.u32(chunk.len() as u32);
        w.0.extend_from_slice(chunk);
    }
}

fn copy(w: &mut Writer, mut offset: usize, mut len: usize) {
    while len != 0 {
        let count = len.min(u32::MAX as usize);
        w.byte(1);
        w.u32(offset as u32);
        w.u32(count as u32);
        len -= count;
        offset += count;
    }
}

/// Copy/insert instructions. A search budget changes compression choices only;
/// it never substitutes a guessed match or changes the reconstructed bytes.
pub(super) fn encode(base: &[u8], index: &Index, target: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut w = Writer::default();
    let mut position = 0;
    let mut literal = 0;
    let mut misses = 0_usize;
    let mut source_end = 0;
    let prefix = common_prefix(base, target);
    if prefix >= BLOCK {
        copy(&mut w, 0, prefix);
        position = prefix;
        literal = prefix;
        source_end = prefix;
    }
    while target.len().saturating_sub(position) >= BLOCK {
        if w.0.len().saturating_add(position - literal) > limit {
            return None;
        }
        let hint = source_end.checked_add(position - literal);
        let (mut offset, mut length) = index.longest(base, &target[position..], hint, misses);
        if length < BLOCK {
            misses += 1;
            position += (1 + (misses >> 6)).min(BLOCK);
            continue;
        }
        while position > literal && offset > 0 && target[position - 1] == base[offset - 1] {
            position -= 1;
            offset -= 1;
            length += 1;
        }
        insert(&mut w, &target[literal..position]);
        copy(&mut w, offset, length);
        source_end = offset + length;
        position += length;
        literal = position;
        misses = 0;
    }
    insert(&mut w, &target[literal..]);
    (w.0.len() <= limit).then_some(w.0)
}

pub(super) fn apply(base: &[u8], instructions: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let mut r = Reader::new(instructions);
    let mut output = Vec::new();
    output
        .try_reserve_exact(expected)
        .map_err(|e| format!("allocate delta output: {e}"))?;
    while r.remaining() != 0 {
        let bytes = match r.byte()? {
            0 => {
                let len = r.u32()? as usize;
                if len == 0 {
                    return Err("empty delta insert".into());
                }
                r.take(len)?
            }
            1 => {
                let offset = r.u32()? as usize;
                let len = r.u32()? as usize;
                if len == 0 {
                    return Err("empty delta copy".into());
                }
                let end = offset.checked_add(len).ok_or("delta copy overflow")?;
                base.get(offset..end).ok_or("delta copy exceeds base")?
            }
            _ => return Err("unknown delta instruction".into()),
        };
        if bytes.len() > expected.saturating_sub(output.len()) {
            return Err("delta exceeds decoded length".into());
        }
        output.extend_from_slice(bytes);
    }
    if output.len() != expected {
        return Err("delta decoded length mismatch".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x12345678_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    }
    #[test]
    fn scattered_changes_insertions_deletions_and_moves_reuse_internal_blocks() {
        let base = noise(256 * 1024);
        let index = Index::default();
        let mut changed = base.clone();
        for i in (100..changed.len()).step_by(4096) {
            changed[i] ^= 17;
        }
        let inserted = [&base[..7000], b"inserted block".as_slice(), &base[7000..]].concat();
        let deleted = [&base[..333], &base[977..]].concat();
        let moved = [&base[128 * 1024..], &base[..128 * 1024]].concat();
        for target in [changed, inserted, deleted, moved] {
            let delta = encode(&base, &index, &target, target.len() / 2).unwrap();
            assert!(delta.len() < 4096, "{}", delta.len());
            assert_eq!(apply(&base, &delta, target.len()).unwrap(), target);
        }
    }
    #[test]
    fn tiny_repetitive_and_deterministically_mutated_images_roundtrip() {
        let mut seed = 0xdeadbeef_u32;
        for len in [0, 1, 15, 16, 17, 64, 4096] {
            for base in [noise(len), vec![0; len]] {
                let index = Index::default();
                for _ in 0..32 {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let mut target = base.clone();
                    let p = seed as usize % (len + 1);
                    target.splice(p..p, seed.to_le_bytes());
                    if target.len() > 8 {
                        target.drain(1..5);
                    }
                    let delta = encode(&base, &index, &target, target.len() + 32).unwrap();
                    assert_eq!(apply(&base, &delta, target.len()).unwrap(), target);
                }
            }
        }
    }
    #[test]
    fn malformed_copy_insert_counts_and_output_bounds_fail_closed() {
        for delta in [
            vec![2],
            vec![0, 0, 0, 0, 0],
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![1, 0, 0, 0, 7, 0, 0, 0, 3],
            vec![0, 255, 255, 255, 255],
        ] {
            assert!(apply(b"01234567", &delta, 8).is_err());
        }
        let mut w = Writer::default();
        copy(&mut w, 0, 8);
        assert!(apply(b"01234567", &w.0, 7).is_err());
        assert!(apply(b"01234567", &w.0, 9).is_err());
    }

    #[test]
    fn sparse_index_preserves_dense_local_edits_in_large_bases() {
        let base = noise(8 * 1024 * 1024);
        let index = Index::default();
        let mut target = base.clone();
        for i in (1024..target.len()).step_by(97) {
            target[i] ^= 123;
        }
        let delta = encode(&base, &index, &target, target.len() / 2).unwrap();
        assert!(delta.len() < target.len() / 4);
        assert_eq!(apply(&base, &delta, target.len()).unwrap(), target);
    }
}
