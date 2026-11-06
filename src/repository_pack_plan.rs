//! Storage affinity follows actual history paths; it makes no rename or
//! semantic identity claim. The OID index stays sorted independently.
use super::*;

pub(super) const ANCHOR_INTERVAL: usize = 64;
#[derive(Clone, Copy)]
pub(super) struct Job {
    pub number: usize,
    pub previous: Option<usize>,
    pub anchor: usize,
}

pub(super) fn jobs(
    snapshot: &Snapshot,
    objects: &[(&String, &Vec<u8>)],
) -> RepositoryResult<Vec<Job>> {
    let by_id: BTreeMap<_, _> = objects
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();
    let mut assigned = vec![false; objects.len()];
    let mut groups = BTreeMap::<(u8, String), Vec<usize>>::new();
    let mut assign = |id: &str, path: &str| {
        if let Some(&i) = by_id.get(id) {
            if !assigned[i] {
                assigned[i] = true;
                groups
                    .entry((objects[i].1[8], path.into()))
                    .or_default()
                    .push(i);
            }
        }
    };
    // Newest objects become full anchors, making current checkout cheap.
    // Both branches and every merge parent remain independent immutable data.
    for id in validated_commit_order(&snapshot.objects)?.into_iter().rev() {
        if !by_id.contains_key(id.as_str()) {
            continue;
        }
        let Object::Commit(commit) = &snapshot.objects[&id] else {
            unreachable!()
        };
        assign(&id, "");
        if let Some(tree_id) = &commit.tree {
            assign(tree_id, "");
            if let Object::Tree(tree) = &snapshot.objects[tree_id] {
                for (path, blob) in &tree.entries {
                    assign(blob, path);
                }
            }
        } else {
            assign(&commit.wasm, commit.legacy_path.as_deref().unwrap_or(""));
        }
    }
    // Detached trees and blobs are kept by repack too. Size buckets bound
    // futile comparisons when no path provenance exists.
    for (id, object) in &snapshot.objects {
        if let Object::Tree(tree) = object {
            assign(id, "");
            for (path, blob) in &tree.entries {
                assign(blob, path);
            }
        }
    }
    for (i, (_, bytes)) in objects.iter().enumerate() {
        if !assigned[i] {
            groups
                .entry((bytes[8], format!("\0size:{}", bytes.len().ilog2())))
                .or_default()
                .push(i);
        }
    }
    let mut jobs = Vec::with_capacity(objects.len());
    for group in groups.values() {
        for chunk in group.chunks(ANCHOR_INTERVAL) {
            for (i, &number) in chunk.iter().enumerate() {
                jobs.push(Job {
                    number,
                    previous: i.checked_sub(1).map(|p| chunk[p]),
                    anchor: chunk[0],
                });
            }
        }
    }
    Ok(jobs)
}

// Find an immutable full anchor along real parent/path provenance. External
// anchors have depth zero; a rewritten pack never relies on a removable pack.
pub(super) fn external_bases(
    snapshot: &Snapshot,
    anchors: &BTreeSet<String>,
    requested: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    if anchors.is_empty() || requested.is_empty() {
        return result;
    }
    for object in snapshot.objects.values() {
        let Object::Commit(commit) = object else {
            continue;
        };
        let wanted = if let Some(tree) = &commit.tree {
            match snapshot.objects.get(tree) {
                Some(Object::Tree(t)) => t.entries.values().any(|id| requested.contains(id)),
                _ => false,
            }
        } else {
            requested.contains(&commit.wasm)
        };
        if !wanted {
            continue;
        }
        let mut pending: VecDeque<_> = commit.history_parents().iter().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop_front() {
            if seen.len() >= 1024 || !seen.insert(id.clone()) {
                continue;
            }
            let Some(Object::Commit(parent)) = snapshot.objects.get(&id) else {
                continue;
            };
            if let (Some(a), Some(b)) = (&commit.tree, &parent.tree) {
                if let (Some(Object::Tree(a)), Some(Object::Tree(b))) =
                    (snapshot.objects.get(a), snapshot.objects.get(b))
                {
                    for (path, target) in &a.entries {
                        if let Some(base) = b.entries.get(path) {
                            if target != base && anchors.contains(base) {
                                result.entry(target.clone()).or_insert_with(|| base.clone());
                            }
                        }
                    }
                }
            } else if commit.tree.is_none()
                && parent.tree.is_none()
                && commit.wasm != parent.wasm
                && anchors.contains(&parent.wasm)
            {
                result
                    .entry(commit.wasm.clone())
                    .or_insert_with(|| parent.wasm.clone());
            }
            pending.extend(parent.history_parents().iter().cloned());
        }
    }
    result
}
