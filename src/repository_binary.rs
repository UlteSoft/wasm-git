//! Canonical native objects and bundle manifests. Object references are raw
//! SHA-256 values; paths/messages alone use length-prefixed UTF-8.
use super::*;
use crate::binary::{Reader, Writer};

pub(super) fn encode_commit(commit: &Commit) -> RepositoryResult<Vec<u8>> {
    let mut w = Writer::default();
    match commit.format {
        1 if commit.tree.is_none() && commit.legacy_path.is_none() => {
            w.byte(1);
            w.oid(&commit.wasm)?;
        }
        2 if commit.wasm.is_empty() && commit.tree.is_some() => {
            w.byte(2);
            w.oid(commit.tree.as_deref().unwrap())?;
        }
        _ => return Err("inconsistent commit layout".into()),
    }
    w.u32(u32::try_from(commit.parents.len()).map_err(|_| "too many commit parents")?);
    for parent in &commit.parents {
        w.oid(parent)?;
    }
    w.string(&commit.message)?;
    match &commit.legacy_path {
        Some(path) => {
            w.byte(1);
            w.string(path)?;
        }
        None => w.byte(0),
    }
    match &commit.signature {
        None => w.byte(0),
        Some(signature) => {
            w.byte(1);
            w.bytes(&crate::binary::encode(signature)?)?;
        }
    }
    Ok(w.0)
}

pub(super) fn decode_commit(bytes: &[u8]) -> RepositoryResult<Commit> {
    let mut r = Reader::new(bytes);
    let format = r.byte()? as u32;
    if !matches!(format, 1 | 2) {
        return Err("invalid binary commit layout".into());
    }
    let target = r.oid()?;
    let count = r.count(32)?;
    let mut parents = Vec::with_capacity(count);
    for _ in 0..count {
        parents.push(r.oid()?);
    }
    let message = r.string()?;
    let legacy_path = match r.byte()? {
        0 => None,
        1 => Some(r.string()?),
        _ => return Err("invalid commit path flag".into()),
    };
    let signature = match r.byte()? {
        0 => None,
        1 => Some(crate::binary::decode(r.bytes()?)?),
        _ => return Err("invalid author signature flag".into()),
    };
    r.end()?;
    Ok(Commit {
        shallow: false,
        signature,
        format,
        wasm: if format == 1 {
            target.clone()
        } else {
            String::new()
        },
        parents,
        message,
        tree: (format == 2).then_some(target),
        legacy_path,
    })
}

fn write_refs(w: &mut Writer, refs: &BTreeMap<String, String>) -> RepositoryResult<()> {
    w.u32(u32::try_from(refs.len()).map_err(|_| "too many binary references")?);
    for (name, id) in refs {
        w.string(name)?;
        w.oid(id)?;
    }
    Ok(())
}
fn read_refs(r: &mut Reader<'_>) -> RepositoryResult<BTreeMap<String, String>> {
    let count = r.count(36)?;
    let mut refs = BTreeMap::new();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let name = r.string()?;
        if previous.as_ref().is_some_and(|p| p >= &name) {
            return Err("binary references duplicated or out of order".into());
        }
        let id = r.oid()?;
        previous = Some(name.clone());
        refs.insert(name, id);
    }
    Ok(refs)
}
pub(super) fn encode_tree(tree: &Tree) -> RepositoryResult<Vec<u8>> {
    if tree.format != 1 {
        return Err("invalid tree layout".into());
    }
    let mut w = Writer::default();
    write_refs(&mut w, &tree.entries)?;
    Ok(w.0)
}
pub(super) fn decode_tree(bytes: &[u8]) -> RepositoryResult<Tree> {
    let mut r = Reader::new(bytes);
    let entries = read_refs(&mut r)?;
    r.end()?;
    Ok(Tree { format: 1, entries })
}
pub(super) fn encode_manifest(manifest: &Manifest) -> RepositoryResult<Vec<u8>> {
    if !(1..=4).contains(&manifest.format) {
        return Err("invalid bundle manifest layout".into());
    }
    let mut w = Writer::default();
    w.byte(manifest.format as u8);
    w.string(&manifest.head)?;
    write_refs(&mut w, &manifest.refs)?;
    write_refs(&mut w, &manifest.tags)?;
    for ids in [&manifest.shallow, &manifest.promised] {
        w.u32(u32::try_from(ids.len()).map_err(|_| "too many selective object IDs")?);
        for id in ids {
            w.oid(id)?;
        }
    }
    Ok(w.0)
}
pub(super) fn decode_manifest(bytes: &[u8]) -> RepositoryResult<Manifest> {
    let mut r = Reader::new(bytes);
    let format = r.byte()? as u32;
    let head = r.string()?;
    let refs = read_refs(&mut r)?;
    let tags = read_refs(&mut r)?;
    let mut sets = Vec::new();
    for _ in 0..2 {
        let count = r.count(32)?;
        let mut ids = BTreeSet::new();
        for _ in 0..count {
            let id = r.oid()?;
            if ids.last().is_some_and(|v| v >= &id) {
                return Err("selective IDs not canonical".into());
            }
            ids.insert(id);
        }
        sets.push(ids);
    }
    let promised = sets.pop().unwrap();
    let shallow = sets.pop().unwrap();
    r.end()?;
    Ok(Manifest {
        shallow,
        promised,
        format,
        head,
        refs,
        tags,
    })
}
