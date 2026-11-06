//! Selection is an audit view, never a whole-module patch. Scan outer framing
//! first; only selected custom payloads are decoded. Core field interpretation
//! retains whole-module context. No guessed framing for future binary versions.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::str::FromStr;

const COMPONENT: &[u8; 8] = b"\0asm\x0d\0\x01\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    All,
    Kind(String),
    Id(u8),
    Ordinal(usize),
    Opaque,
}

impl FromStr for Selector {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "all" {
            return Ok(Self::All);
        }
        if value == "opaque" {
            return Ok(Self::Opaque);
        }
        if let Some(id) = value.strip_prefix("id:") {
            return id
                .parse()
                .map(Self::Id)
                .map_err(|_| "section id must be 0..255".into());
        }
        if let Some(index) = value.strip_prefix("ordinal:") {
            return index
                .parse()
                .map(Self::Ordinal)
                .map_err(|_| "section ordinal must be a nonnegative integer".into());
        }
        if value.starts_with("custom:")
            || (0..=13).any(|id| value == section_name(id))
            || (1..=11).any(|id| value == component_kind(id))
        {
            return Ok(Self::Kind(value.into()));
        }
        Err(format!("unknown section selector {value:?}; use a section kind (code, data, custom), custom:NAME, id:N, ordinal:N, opaque, or all"))
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::Opaque => f.write_str("opaque"),
            Self::Kind(kind) => f.write_str(kind),
            Self::Id(id) => write!(f, "id:{id}"),
            Self::Ordinal(n) => write!(f, "ordinal:{n}"),
        }
    }
}

impl Selector {
    fn matches(&self, section: &AuditSection) -> bool {
        match self {
            Self::All => true,
            Self::Kind(kind) => section.kind == *kind || (kind == "custom" && section.id == 0),
            Self::Id(id) => section.id == *id && section.kind != "opaque_encoding",
            Self::Ordinal(n) => section.ordinal == *n && section.kind != "opaque_encoding",
            Self::Opaque => matches!(
                section.status.as_str(),
                "opaque" | "malformed" | "partially_decoded"
            ),
        }
    }
}

pub(crate) fn check_header(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 8 || !bytes.starts_with(b"\0asm") {
        return Err("expected an eight-byte WebAssembly header with Wasm magic".into());
    }
    Ok(())
}

fn component_kind(id: u8) -> &'static str {
    match id {
        0 => "custom",
        1 => "core_module",
        2 => "core_instance",
        3 => "core_type",
        4 => "component",
        5 => "component_instance",
        6 => "alias",
        7 => "component_type",
        8 => "canonical",
        9 => "component_start",
        10 => "component_import",
        11 => "component_export",
        _ => "unknown",
    }
}

fn selected(section: &AuditSection, selectors: &[Selector]) -> bool {
    selectors.iter().any(|s| s.matches(section))
}

/// The current Core and Component encodings share id/u32-length framing.
/// This reads no unselected custom convention and no core instruction stream.
pub(super) fn inspect_envelopes(
    bytes: &[u8],
    standard: Standard,
    selectors: &[Selector],
) -> Result<AuditModule, String> {
    check_header(bytes)?;
    let core = bytes.starts_with(b"\0asm\x01\0\0\0");
    let component = bytes.starts_with(COMPONENT);
    let mut module = AuditModule {
        byte_len: bytes.len(),
        format: if core {
            "core WebAssembly binary, version 1"
        } else if component {
            "WebAssembly component, version 13, layer 1 (outer sections only)"
        } else {
            "unrecognized WebAssembly encoding (opaque bytes after header)"
        }
        .into(),
        validation: "not_validated".into(),
        sections: Vec::new(),
        diagnostics: Vec::new(),
        standard: StandardReport::pending(standard),
    };
    module.standard.interpretation = "outer framing and selected custom conventions only; no Core or Component semantic validation".into();
    if !core && !component {
        module.sections.push(AuditSection {
            ordinal: 0,
            id: 255,
            kind: "opaque_encoding".into(),
            path: "opaque_encoding[0]".into(),
            span: Span::new(8, bytes.len()),
            header_span: Span::new(8, 8),
            payload_span: Span::new(8, bytes.len()),
            status: "opaque".into(),
            regions: vec![Region::new(
                "opaque_encoding[0]/bytes",
                Span::new(8, bytes.len()),
                "unknown version/layer; no section or instruction boundaries inferred",
            )],
            standard_support: StandardSupport::default(),
        });
        issue(
            &mut module.diagnostics,
            "warning",
            "unknown_encoding",
            4,
            "unknown version/layer; original bytes remain auditable and restorable",
        );
        return Ok(module);
    }
    let mut reader = BinaryReader::new(&bytes[8..], 8);
    let mut counts = BTreeMap::<u8, usize>::new();
    while !reader.eof() {
        let start = reader.original_position();
        let id = reader.read_u8().map_err(|e| e.to_string())?;
        let count = counts.entry(id).or_default();
        let kind = if core {
            section_name(id)
        } else {
            component_kind(id)
        };
        let path = if kind == "unknown" {
            format!("unknown:{id}[{count}]")
        } else {
            format!("{kind}[{count}]")
        };
        *count += 1;
        let size = reader.read_var_u32();
        let (payload_start, end, framing_error) = match size {
            Ok(size) => {
                let at = reader.original_position();
                if size as usize > reader.bytes_remaining() {
                    (
                        at,
                        bytes.len(),
                        Some("section payload extends beyond file".to_owned()),
                    )
                } else {
                    reader
                        .read_bytes(size as usize)
                        .map_err(|e| e.to_string())?;
                    (at, at + size as usize, None)
                }
            }
            Err(error) => (bytes.len(), bytes.len(), Some(error.to_string())),
        };
        let mut section = AuditSection {
            ordinal: module.sections.len(),
            id,
            kind: kind.into(),
            path,
            span: Span::new(start, end),
            header_span: Span::new(start, payload_start),
            payload_span: Span::new(payload_start, end),
            status: "opaque".into(),
            regions: Vec::new(),
            standard_support: StandardSupport::default(),
        };
        if let Some(reason) = framing_error {
            section.status = "malformed".into();
            section.regions.push(Region::new(
                format!("{}/framing", section.path),
                section.span,
                &reason,
            ));
            issue(
                &mut module.diagnostics,
                "error",
                "section_framing",
                start,
                reason,
            );
            module.sections.push(section);
            break;
        }
        if id == 0 {
            let payload = &bytes[payload_start..end];
            let mut name_reader = BinaryReader::new(payload, payload_start);
            let name = (|| -> Result<&str, String> {
                let len = name_reader.read_var_u32().map_err(|e| e.to_string())? as usize;
                if len > name_reader.bytes_remaining() {
                    return Err("custom name extends beyond its section payload".into());
                }
                let raw = name_reader.read_bytes(len).map_err(|e| e.to_string())?;
                std::str::from_utf8(raw).map_err(|e| e.to_string())
            })();
            match name {
                Ok(name) => section.kind = format!("custom:{name}"),
                Err(error) => {
                    section.status = "malformed".into();
                    issue(
                        &mut module.diagnostics,
                        "error",
                        "custom_name",
                        payload_start,
                        error,
                    );
                }
            }
            if selected(&section, selectors) {
                section.status = "decoded".into();
                if let Err(error) = custom(&mut section, payload, &mut module.diagnostics) {
                    section.status = "malformed".into();
                    issue(
                        &mut module.diagnostics,
                        "error",
                        "custom_name",
                        error.offset(),
                        error.to_string(),
                    );
                }
            }
        }
        if section.regions.is_empty() {
            section.regions.push(Region::new(
                format!("{}/opaque", section.path),
                section.payload_span,
                "payload not interpreted in this section view",
            ));
        }
        module.sections.push(section);
    }
    Ok(module)
}

#[derive(Debug, Serialize)]
pub struct Site {
    pub ordinal: usize,
    pub id: u8,
    pub kind: String,
    pub path: String,
    pub status: String,
    pub span: Span,
    pub header_span: Span,
    pub payload_span: Span,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub old: Option<Site>,
    pub new: Option<Site>,
    pub pairing: &'static str,
    pub ambiguous: bool,
    pub identity_unproven: bool,
    pub content_changed: bool,
    pub payload_changed: bool,
    pub position_changed: bool,
    pub exact_minimal_within_pair: bool,
    pub edits: Vec<ByteEdit>,
    /// Paths relative to each paired section; contexts retain original paths.
    pub fields: Vec<SemanticChange>,
}

#[derive(Debug, Serialize)]
pub struct SelectionDiff {
    pub kind: &'static str,
    pub selectors: Vec<String>,
    pub unmatched_selectors: Vec<String>,
    pub coverage: &'static str,
    pub cross_section_dependencies: &'static str,
    pub whole_module_patch: bool,
    pub validation_scope: [&'static str; 2],
    /// False means missing matches may lie beyond an untrusted boundary/name.
    pub selection_complete: [bool; 2],
    pub module_binary_changed: bool,
    pub selected_content_changed: bool,
    pub selected_position_changed: bool,
    pub old: AuditModule,
    pub new: AuditModule,
    pub changes: Vec<Change>,
}

fn raw<'a>(bytes: &'a [u8], section: &AuditSection) -> &'a [u8] {
    &bytes[section.span.start..section.span.end]
}

pub(crate) fn inspect_selected(
    bytes: &[u8],
    standard: Standard,
    selectors: &[Selector],
) -> Result<(AuditModule, &'static str, bool), String> {
    let mut module = inspect_envelopes(bytes, standard, selectors)?;
    let complete = !module.diagnostics.iter().any(|d| {
        matches!(
            d.code.as_str(),
            "section_framing" | "custom_name" | "unknown_encoding"
        )
    });
    let scope = if bytes.starts_with(b"\0asm\x01\0\0\0")
        && module
            .sections
            .iter()
            .any(|s| s.id != 0 && selected(s, selectors))
    {
        module = inspect_with_standard(bytes, standard)?;
        "whole_core_module; displayed evidence limited to selection"
    } else {
        "outer_framing_and_selected_custom_only; semantic_validation_not_performed"
    };
    module.sections.retain(|s| selected(s, selectors));
    module.standard.evidence.retain(|e| {
        let at = module
            .sections
            .partition_point(|s| s.span.end <= e.span.start);
        module
            .sections
            .get(at)
            .is_some_and(|s| s.span.intersects(e.span))
    });
    module.standard.refresh_deprecations();
    Ok((module, scope, complete))
}

/// Repeated selectors are a union. Section offsets always refer to the original
/// files, but each edit list is LOCAL to its pair (possibly moved/reordered).
pub fn compare(
    old: &[u8],
    new: &[u8],
    standard: Standard,
    selectors: &[Selector],
) -> Result<SelectionDiff, String> {
    if selectors.is_empty() {
        return Err("section selection cannot be empty".into());
    }
    let (a, b) = crate::parallel::join(
        old.len().saturating_add(new.len()),
        || inspect_selected(old, standard, selectors),
        || inspect_selected(new, standard, selectors),
    );
    let ((a, sa, complete_a), (b, sb, complete_b)) = (a?, b?);
    let mut groups = BTreeMap::<(&str, u8, &str), (Vec<usize>, Vec<usize>)>::new();
    for (i, s) in a.sections.iter().enumerate() {
        groups
            .entry((&a.format, s.id, &s.kind))
            .or_default()
            .0
            .push(i);
    }
    for (i, s) in b.sections.iter().enumerate() {
        groups
            .entry((&b.format, s.id, &s.kind))
            .or_default()
            .1
            .push(i);
    }
    let mut pairs = Vec::new();
    for ((_, id, kind), (left, right)) in groups {
        let ambiguous = left.len() > 1 || right.len() > 1 || (id == 0 && kind == "custom");
        let mut buckets = BTreeMap::<[u8; 32], VecDeque<usize>>::new();
        for &j in &right {
            buckets
                .entry(Sha256::digest(raw(new, &b.sections[j])).into())
                .or_default()
                .push_back(j);
        }
        let mut remaining_old = Vec::new();
        let mut used = BTreeSet::new();
        for i in left {
            let bytes = raw(old, &a.sections[i]);
            let hash: [u8; 32] = Sha256::digest(bytes).into();
            let candidate = buckets.get_mut(&hash).and_then(|bucket| {
                // Hashes accelerate lookup; equality is always byte-verified.
                let pos = bucket
                    .iter()
                    .position(|&j| raw(new, &b.sections[j]) == bytes)?;
                bucket.remove(pos)
            });
            if let Some(j) = candidate {
                used.insert(j);
                pairs.push((Some(i), Some(j), "byte_equal", ambiguous));
            } else {
                remaining_old.push(i);
            }
        }
        let remaining_new: Vec<_> = right.into_iter().filter(|j| !used.contains(j)).collect();
        if let ([i], [j]) = (&remaining_old[..], &remaining_new[..]) {
            pairs.push((
                Some(*i),
                Some(*j),
                "remaining_same_kind_or_custom_name",
                ambiguous,
            ));
        } else {
            pairs.extend(
                remaining_old
                    .into_iter()
                    .map(|i| (Some(i), None, "unpaired", ambiguous)),
            );
            pairs.extend(
                remaining_new
                    .into_iter()
                    .map(|j| (None, Some(j), "unpaired", ambiguous)),
            );
        }
    }
    pairs.sort_by_key(|&(i, j, _, _)| {
        (
            i.map(|i| a.sections[i].ordinal),
            j.map(|j| b.sections[j].ordinal),
        )
    });
    let ca = ContextIndex::new(&a);
    let cb = ContextIndex::new(&b);
    let changes = crate::parallel::map(pairs.len(), |index| {
        let (i, j, pairing, ambiguous) = pairs[index];
        let x = i.map(|i| &a.sections[i]);
        let y = j.map(|j| &b.sections[j]);
        let av = x.map_or(&[][..], |s| raw(old, s));
        let bv = y.map_or(&[][..], |s| raw(new, s));
        let content_changed = x.is_none() || y.is_none() || av != bv;
        let position_changed = x
            .zip(y)
            .is_some_and(|(x, y)| x.span.start != y.span.start || x.ordinal != y.ordinal);
        if !content_changed && !position_changed {
            return None;
        }
        let (spans, exact) = byte_spans(av, bv);
        let (ao, bo) = (
            x.map_or(old.len(), |s| s.span.start),
            y.map_or(new.len(), |s| s.span.start),
        );
        let edits = spans
            .into_iter()
            .map(|(p, q)| {
                let ap = Span::new(p.start + ao, p.end + ao);
                let bp = Span::new(q.start + bo, q.end + bo);
                ByteEdit {
                    old_span: ap,
                    new_span: bp,
                    old_hex: hex(&av[p.start..p.end]),
                    new_hex: hex(&bv[q.start..q.end]),
                    old_context: ca.context(ap),
                    new_context: cb.context(bp),
                }
            })
            .collect();
        fn fields(section: Option<&AuditSection>) -> BTreeMap<&str, &Region> {
            let Some(s) = section else {
                return BTreeMap::new();
            };
            let mut regions = BTreeMap::new();
            flatten(&s.regions, &mut regions);
            regions
                .into_iter()
                .map(|(path, r)| (path.strip_prefix(&s.path).unwrap_or(path), r))
                .collect()
        }
        let site = |bytes: &[u8], s: &AuditSection| Site {
            ordinal: s.ordinal,
            id: s.id,
            kind: s.kind.clone(),
            path: s.path.clone(),
            status: s.status.clone(),
            span: s.span,
            header_span: s.header_span,
            payload_span: s.payload_span,
            sha256: hex(&Sha256::digest(raw(bytes, s))),
        };
        Some(Change {
            old: x.map(|s| site(old, s)),
            new: y.map(|s| site(new, s)),
            pairing,
            ambiguous,
            identity_unproven: true,
            content_changed,
            payload_changed: x.map(|s| &old[s.payload_span.start..s.payload_span.end])
                != y.map(|s| &new[s.payload_span.start..s.payload_span.end]),
            position_changed,
            exact_minimal_within_pair: exact,
            edits,
            fields: region_changes(fields(x), fields(y)),
        })
    })
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let unmatched_selectors = selectors
        .iter()
        .filter(|s| {
            !a.sections
                .iter()
                .chain(&b.sections)
                .any(|section| s.matches(section))
        })
        .map(ToString::to_string)
        .collect();
    Ok(SelectionDiff {
        kind: "section_diff", selectors: selectors.iter().map(ToString::to_string).collect(), unmatched_selectors,
        coverage: "selected_sections_only; header and unselected bytes excluded; per-pair edits may refer to moved sections",
        cross_section_dependencies: "not_evaluated; use full diff for call/export/signature-input dependency evidence",
        whole_module_patch: false, validation_scope: [sa, sb], module_binary_changed: old != new,
        selection_complete: [complete_a, complete_b],
        selected_content_changed: changes.iter().any(|c| c.content_changed), selected_position_changed: changes.iter().any(|c| c.position_changed),
        old: a, new: b, changes,
    })
}

impl SelectionDiff {
    pub fn to_text(&self, short: bool) -> String {
        let state = if self.selected_content_changed {
            "changed"
        } else if self.selection_complete == [true, true] {
            "identical"
        } else {
            "no observed change; selection incomplete"
        };
        let mut out = format!("Selected sections: {:?}\nSelected content: {}; position changes={}; whole-module patch=false\nValidation: old={}; new={}\n", self.selectors, state, self.selected_position_changed, self.validation_scope[0], self.validation_scope[1]);
        if self.selection_complete != [true, true] {
            let _ = writeln!(out, "Selection complete: old={}, new={}; framing/encoding/name errors can hide matches; absence is not proved.", self.selection_complete[0], self.selection_complete[1]);
        }
        out.push_str("Cross-section call/export/signature dependencies: use full diff.\n");
        self.old.standard.deprecations_text(&mut out, "old ");
        self.new.standard.deprecations_text(&mut out, "new ");
        if !self.unmatched_selectors.is_empty() {
            let _ = writeln!(out, "No matching section: {:?}", self.unmatched_selectors);
        }
        for c in &self.changes {
            let label = |s: &Option<Site>| {
                s.as_ref()
                    .map(|s| {
                        format!(
                            "{} ({:?}, {}) [{}..{})",
                            s.path, s.kind, s.status, s.span.start, s.span.end
                        )
                    })
                    .unwrap_or_else(|| "absent".into())
            };
            let _ = writeln!(out, "{} -> {}: content_changed={}, payload_changed={}, position_changed={}, ambiguous={}; {} byte edits", label(&c.old), label(&c.new), c.content_changed, c.payload_changed, c.position_changed, c.ambiguous, c.edits.len());
            if short {
                continue;
            }
            for field in &c.fields {
                let _ = writeln!(
                    out,
                    "  field {:?}: {:?} -> {:?}",
                    field.path,
                    field.old.as_ref().map(|c| &c.description),
                    field.new.as_ref().map(|c| &c.description)
                );
            }
            for edit in &c.edits {
                let preview = |s: &str| {
                    if s.len() > 128 {
                        format!("{}… ({} bytes; full bytes in JSON)", &s[..128], s.len() / 2)
                    } else {
                        s.into()
                    }
                };
                let _ = writeln!(
                    out,
                    "  old [{}..{}) {} -> new [{}..{}) {}",
                    edit.old_span.start,
                    edit.old_span.end,
                    preview(&edit.old_hex),
                    edit.new_span.start,
                    edit.new_span.end,
                    preview(&edit.new_hex)
                );
            }
        }
        out
    }
}
