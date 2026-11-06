//! Custom-section correspondence is content evidence, not historical identity.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize)]
pub struct CustomSite {
    pub ordinal: usize,
    pub span: Span,
    /// Excludes the outer custom name and its length.
    pub data_span: Span,
    pub interpretation: String,
    pub sha256: String,
    pub data_sha256: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct SignatureInputEvidence {
    pub convention: String,
    pub verification: String,
    pub old_sha256: Option<String>,
    pub new_sha256: Option<String>,
    pub changed: Option<bool>,
    pub note: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct CustomChange {
    pub name: String,
    pub old: Option<CustomSite>,
    pub new: Option<CustomSite>,
    pub content_changed: bool,
    pub data_changed: bool,
    /// Absolute position/index change, not necessarily an intentional move.
    pub position_changed: bool,
    pub pairing: String,
    pub ambiguous: bool,
    pub decoded_changes: Vec<SemanticChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_input: Option<SignatureInputEvidence>,
}
struct Entry<'a> {
    section: &'a AuditSection,
    data: &'a [u8],
    site: CustomSite,
}
fn entries<'a>(module: &'a AuditModule, bytes: &'a [u8]) -> BTreeMap<&'a str, Vec<Entry<'a>>> {
    let mut out = BTreeMap::<&str, Vec<Entry<'_>>>::new();
    for s in &module.sections {
        if s.id != 0 {
            continue;
        }
        let Some(name) = s.kind.strip_prefix("custom:") else {
            continue;
        };
        // The first region is the successfully decoded Core custom name.
        let data_span = Span::new(s.regions[0].span.end, s.payload_span.end);
        let data = &bytes[data_span.start..data_span.end];
        out.entry(name).or_default().push(Entry {
            section: s,
            data,
            site: CustomSite {
                ordinal: s.ordinal,
                span: s.span,
                data_span,
                interpretation: s.status.clone(),
                sha256: custom_extensions::digest(&bytes[s.span.start..s.span.end]),
                data_sha256: custom_extensions::digest(data),
            },
        });
    }
    out
}
fn signing_input(module: &AuditModule, bytes: &[u8], name: &str) -> Option<String> {
    // Do not infer signing scope from truncated or unrecognized section framing.
    if bytes.get(..8) != Some(b"\0asm\x01\0\0\0") {
        return None;
    }
    let mut end = 8;
    let mut signed = Vec::new();
    for s in &module.sections {
        if s.span.start != end || s.span.end > bytes.len() {
            return None;
        }
        end = s.span.end;
        if s.id == 0 && !s.kind.starts_with("custom:") {
            return None;
        }
        if s.kind.strip_prefix("custom:") == Some(name) {
            signed.push(s);
        }
    }
    if end != bytes.len() {
        return None;
    }
    if name == "uwvm.wsea.v2" {
        if signed.is_empty() {
            return Some(custom_extensions::digest(bytes));
        }
        if signed.len() != 1 || signed[0].span.end != bytes.len() {
            return None;
        }
        let s = signed[0];
        let mut r = ConventionReader::new(&bytes[s.span.start..s.span.end], s.span.start);
        if r.byte().ok()? != 0 {
            return None;
        }
        let size = r.u32().ok()? as usize;
        if size != s.span.end - r.position() || r.string().ok()? != name {
            return None;
        }
        return Some(custom_extensions::digest(&bytes[..s.span.start]));
    }
    let mut hash = Sha256::new();
    let mut start = 0;
    for s in signed {
        hash.update(&bytes[start..s.span.start]);
        start = s.span.end;
    }
    hash.update(&bytes[start..]);
    Some(format!("{:x}", hash.finalize()))
}
fn signature_input(
    old: &AuditModule,
    a: &[u8],
    new: &AuditModule,
    b: &[u8],
    name: &str,
) -> Option<SignatureInputEvidence> {
    if !matches!(name, "wasm-cosign" | "uwvm.wsea.v2") {
        return None;
    }
    let old_sha256 = signing_input(old, a, name);
    let new_sha256 = signing_input(new, b, name);
    let changed = old_sha256
        .as_ref()
        .zip(new_sha256.as_ref())
        .map(|(a, b)| a != b);
    Some(SignatureInputEvidence {
        convention: name.into(), verification: "not_performed".into(), old_sha256, new_sha256, changed,
        note: if name == "wasm-cosign" {
            "Claimed input excludes all custom sections named wasm-cosign, preserving every other byte. Name/shape recognition and hashes do not authenticate a signature."
        } else {
            "Claimed input excludes one final uwvm.wsea.v2 section with canonical outer lengths. Multiple/non-final containers or incomplete framing leave scope unresolved. Envelope decoding does not verify COSE, PKI or CRLs."
        }.into(),
    })
}
fn decoded(a: Option<&Entry<'_>>, b: Option<&Entry<'_>>) -> Vec<SemanticChange> {
    fn regions<'a>(sections: Option<&'a Entry<'_>>) -> BTreeMap<&'a str, &'a Region> {
        fn walk<'a>(regions: &'a [Region], prefix: &str, out: &mut BTreeMap<&'a str, &'a Region>) {
            for r in regions {
                // Container descriptions carry fields too (for example a
                // dylink/linking subsection id); child-only comparison would
                // hide those changes from the compact custom view.
                out.insert(r.path.strip_prefix(prefix).unwrap_or(&r.path), r);
                walk(&r.children, prefix, out);
            }
        }
        let mut out = BTreeMap::new();
        if let Some(s) = sections {
            walk(&s.section.regions, &s.section.path, &mut out);
        }
        out
    }
    region_changes(regions(a), regions(b))
}
pub(super) fn compare(
    old: &AuditModule,
    a: &[u8],
    new: &AuditModule,
    b: &[u8],
) -> Vec<CustomChange> {
    let mut left = entries(old, a);
    let mut right = entries(new, b);
    let names: BTreeSet<_> = left.keys().chain(right.keys()).copied().collect();
    let mut out = Vec::new();
    for name in names {
        let left = left.remove(name).unwrap_or_default();
        let right = right.remove(name).unwrap_or_default();
        let signature = signature_input(old, a, new, b, name);
        let mut add = |l: Option<usize>, r: Option<usize>, pairing: &str, ambiguous: bool| {
            let x = l.map(|i| &left[i]);
            let y = r.map(|i| &right[i]);
            let content_changed = match (x, y) {
                (Some(x), Some(y)) => {
                    a[x.site.span.start..x.site.span.end] != b[y.site.span.start..y.site.span.end]
                }
                _ => true,
            };
            let data_changed = x.map(|e| e.data) != y.map(|e| e.data);
            let position_changed = x.zip(y).is_some_and(|(x, y)| {
                x.site.ordinal != y.site.ordinal || x.site.span.start != y.site.span.start
            });
            // A retained signature still matters when its claimed signed input changed.
            if !content_changed
                && !position_changed
                && signature.as_ref().is_none_or(|s| s.changed == Some(false))
            {
                return;
            }
            out.push(CustomChange {
                name: name.into(),
                old: x.map(|e| e.site.clone()),
                new: y.map(|e| e.site.clone()),
                content_changed,
                data_changed,
                position_changed,
                pairing: pairing.into(),
                ambiguous,
                decoded_changes: if data_changed {
                    decoded(x, y)
                } else {
                    Vec::new()
                },
                signature_input: signature.clone(),
            });
        };
        let mut buckets = BTreeMap::<&str, VecDeque<usize>>::new();
        for (i, e) in right.iter().enumerate() {
            buckets.entry(&e.site.data_sha256).or_default().push_back(i);
        }
        let mut used_left = BTreeSet::new();
        let mut used_right = BTreeSet::new();
        for (i, e) in left.iter().enumerate() {
            if let Some(bucket) = buckets.get_mut(e.site.data_sha256.as_str()) {
                // Always verify bytes; a digest collision is never identity evidence.
                if let Some(j) = bucket.front().copied().filter(|j| e.data == right[*j].data) {
                    bucket.pop_front();
                    used_left.insert(i);
                    used_right.insert(j);
                    add(
                        Some(i),
                        Some(j),
                        "equal_payload_bytes",
                        left.len() > 1 || right.len() > 1,
                    );
                }
            }
        }
        let unmatched_left: Vec<_> = (0..left.len()).filter(|i| !used_left.contains(i)).collect();
        let unmatched_right: Vec<_> = (0..right.len())
            .filter(|i| !used_right.contains(i))
            .collect();
        if unmatched_left.len() == 1 && unmatched_right.len() == 1 {
            add(
                Some(unmatched_left[0]),
                Some(unmatched_right[0]),
                "remaining_same_name",
                left.len() > 1 || right.len() > 1,
            );
        } else {
            let ambiguous = left.len() > 1 || right.len() > 1;
            for i in unmatched_left {
                add(Some(i), None, "unpaired", ambiguous);
            }
            for j in unmatched_right {
                add(None, Some(j), "unpaired", ambiguous);
            }
        }
    }
    out
}
