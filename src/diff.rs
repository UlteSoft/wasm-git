//! Module-, function-, and operator-level diff engine.
//! Consumes Normalized Diff IR and produces structured deltas.
//!
//! Phase 1: diff operators now compare using typed ParsedOperator
//! (Opcode + Immediate) instead of string-based OperatorIr.

use std::collections::BTreeMap;

use crate::export_dependencies::{DependencyGraph, DependencyRoot};
use serde::Serialize;

use crate::ir::{
    ExportIr, ExternalKindIr, FunctionIr, FunctionKindIr, Immediate, ImportIr, ModuleIr,
    NormalizedModule, ParsedOperator, TypeIr, TypeRefIr,
};
use crate::matcher::{
    match_functions, match_functions_with_status, unmatched_new_function_ids,
    unmatched_old_function_ids, FunctionMatch,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiffReport {
    pub old: String,
    pub new: String,
    pub summary: DiffSummary,
    pub function_matching: FunctionMatchingStatus,
    pub function_matches: Vec<FunctionMatch>,
    pub changes: Vec<DiffChange>,
    pub call_binding_notes: Vec<FunctionBindingNotes>,
    pub export_binding_notes: Vec<ExportBindingDelta>,
    pub export_dependency_graph: DependencyGraph,
    pub debug_context: [crate::debug_info::Summary; 2],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionMatchingStatus {
    pub similarity_search_complete: bool,
    pub similarity_pair_limit: usize,
    pub unclassified_old_function_indices: Vec<u32>,
    pub unclassified_new_function_indices: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffSummary {
    pub types_added: usize,
    pub types_removed: usize,
    pub imports_added: usize,
    pub imports_removed: usize,
    pub exports_added: usize,
    pub exports_removed: usize,
    pub functions_added: usize,
    pub functions_removed: usize,
    pub functions_changed: usize,
    pub operator_replacements: usize,
    pub operator_insertions: usize,
    pub operator_deletions: usize,
    pub control_region_moves: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffChange {
    TypeAdded { id: String },
    TypeRemoved { id: String },
    ImportAdded { id: String },
    ImportRemoved { id: String },
    ExportAdded { id: String },
    ExportRemoved { id: String },
    FunctionAdded { id: String },
    FunctionRemoved { id: String },
    FunctionChanged { function: Box<FunctionDelta> },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionDelta {
    pub old_id: String,
    pub new_id: String,
    pub old_source_index: u32,
    pub new_source_index: u32,
    /// Untrusted custom symbols for review, never source identity proofs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_symbol: Option<String>,
    pub symbol_sources: [Option<&'static str>; 2],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_annotations: Vec<crate::debug_info::InstructionChange>,
    /// Classifies the retained raw operand edits; counters still include them.
    pub change_kind: FunctionChangeKind,
    pub type_changed: bool,
    pub locals_changed: bool,
    pub direct_calls_changed: bool,
    pub call_bindings: Vec<CallBindingDelta>,
    pub operators: Vec<OperatorDelta>,
    pub instruction_alignment: crate::structured_diff::AlignmentReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionChangeKind {
    BodyOrInterfaceChange,
    ConfirmedReferenceRelocation,
    UnresolvedReferenceChange,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionBindingNotes {
    pub old_id: String,
    pub new_id: String,
    pub call_bindings: Vec<CallBindingDelta>,
}

/// Positional call/reference evidence, separate from original operand edits.
/// Unique targets require export-independent static dependency evidence.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetMatchEvidence {
    pub old_source_index: u32,
    pub new_source_index: u32,
    pub reason: crate::matcher::MatchReason,
    #[serde(serialize_with = "crate::matcher::serialize_score")]
    pub confidence: f32,
    pub ambiguous: bool,
    pub reference_context_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportBindingEvidence {
    pub module: String,
    pub name: String,
    pub type_id: String,
    pub exact: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TargetEvidence {
    pub kind: String,
    pub id: String,
    pub source_index: u32,
    pub type_index: u32,
    pub type_id: String,
    pub export_names: Vec<String>,
    pub import: Option<ImportBindingEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallBindingSite {
    pub operator_index: usize,
    pub offset: u64,
    pub opcode: String,
    pub function_index: u32,
    pub target: Option<TargetEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallBindingDelta {
    pub pairing: String,
    pub relation: String,
    pub target_changed: Option<bool>,
    pub identity_unproven: bool,
    pub match_scope: String,
    pub old: Option<CallBindingSite>,
    pub new: Option<CallBindingSite>,
    pub target_match: Option<TargetMatchEvidence>,
    pub dependency_proof: Option<DependencyRoot>,
}

/// Export membership is paired by name/kind, independently of function roles.
/// Original indices remain evidence even when a separate body candidate moves.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportBindingSite {
    pub export_index: u32,
    pub name: String,
    pub kind: String,
    pub item_index: u32,
    pub exact: bool,
    pub target: Option<TargetEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportBindingDelta {
    pub pairing: String,
    pub relation: String,
    pub target_changed: Option<bool>,
    pub identity_unproven: bool,
    pub match_scope: String,
    pub old: ExportBindingSite,
    pub new: ExportBindingSite,
    pub target_match: Option<TargetMatchEvidence>,
    pub dependency_proof: Option<DependencyRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperatorDelta {
    Replace {
        index: usize,
        new_index: usize,
        old: OperatorRecord,
        new: OperatorRecord,
    },
    Insert {
        index: usize,
        operator: OperatorRecord,
    },
    Delete {
        index: usize,
        operator: OperatorRecord,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorRecord {
    pub offset: u64,
    pub opcode: String,
    pub text: String,
}

pub fn diff_modules(
    old_name: &str,
    old: &NormalizedModule,
    new_name: &str,
    new: &NormalizedModule,
) -> DiffReport {
    let same_module = old.module.same_function_projection(&new.module);
    let export_dependency_graph = if same_module {
        DependencyGraph::empty()
    } else {
        DependencyGraph::build(old, new)
    };
    let (role_matches, similarity_search_complete) = match_functions_with_status(old, new);
    let function_matches = implementation_matches(old, new, role_matches, &export_dependency_graph);
    let used_old: std::collections::BTreeSet<_> = function_matches
        .iter()
        .map(|m| m.old_source_index)
        .collect();
    let used_new: std::collections::BTreeSet<_> = function_matches
        .iter()
        .map(|m| m.new_source_index)
        .collect();
    let function_matching = FunctionMatchingStatus {
        similarity_search_complete,
        similarity_pair_limit: crate::matcher::SIMILARITY_PAIR_LIMIT,
        unclassified_old_function_indices: if similarity_search_complete {
            Vec::new()
        } else {
            old.functions
                .iter()
                .filter(|f| !used_old.contains(&f.source_index))
                .map(|f| f.source_index)
                .collect()
        },
        unclassified_new_function_indices: if similarity_search_complete {
            Vec::new()
        } else {
            new.functions
                .iter()
                .filter(|f| !used_new.contains(&f.source_index))
                .map(|f| f.source_index)
                .collect()
        },
    };
    let mut changes = Vec::new();
    let mut call_binding_notes = Vec::new();

    push_collection_changes(
        ids_by_type(&old.types),
        ids_by_type(&new.types),
        |id| DiffChange::TypeRemoved { id },
        |id| DiffChange::TypeAdded { id },
        &mut changes,
    );
    push_collection_changes(
        ids_by_import(&old.imports),
        ids_by_import(&new.imports),
        |id| DiffChange::ImportRemoved { id },
        |id| DiffChange::ImportAdded { id },
        &mut changes,
    );
    push_collection_changes(
        ids_by_export(&old.exports),
        ids_by_export(&new.exports),
        |id| DiffChange::ExportRemoved { id },
        |id| DiffChange::ExportAdded { id },
        &mut changes,
    );

    if similarity_search_complete {
        for id in unmatched_old_function_ids(old, &function_matches) {
            changes.push(DiffChange::FunctionRemoved { id });
        }
        for id in unmatched_new_function_ids(new, &function_matches) {
            changes.push(DiffChange::FunctionAdded { id });
        }
    }

    let old_functions = old
        .functions
        .iter()
        .map(|function| (function.source_index, function))
        .collect::<BTreeMap<_, _>>();
    let new_functions = new
        .functions
        .iter()
        .map(|function| (function.source_index, function))
        .collect::<BTreeMap<_, _>>();

    let context = BindingContext::new(old, new, &export_dependency_graph, same_module);
    let function_deltas = crate::parallel::map(function_matches.len(), |index| {
        if same_module {
            return (None, None);
        }
        let function_match = &function_matches[index];
        let old_function = old_functions[&function_match.old_source_index];
        let new_function = new_functions[&function_match.new_source_index];
        let alignment =
            crate::structured_diff::align(&old_function.operators, &new_function.operators);
        let mut bindings =
            diff_call_bindings(old_function, new_function, old, new, &context, &alignment);
        let mut delta = diff_function(
            old_function,
            new_function,
            &mut bindings,
            alignment,
            [
                old.function_names
                    .get(&old_function.source_index)
                    .map(String::as_str),
                new.function_names
                    .get(&new_function.source_index)
                    .map(String::as_str),
            ],
        );
        if let Some(delta) = &mut delta {
            delta.symbol_sources = [
                old.function_symbol_sources
                    .get(&old_function.source_index)
                    .copied(),
                new.function_symbol_sources
                    .get(&new_function.source_index)
                    .copied(),
            ];
            delta.source_annotations = crate::debug_info::annotate_changes(
                &old.debug_info,
                &new.debug_info,
                &delta.operators,
            );
        }
        let notes = (delta.is_none() && !bindings.is_empty()).then(|| FunctionBindingNotes {
            old_id: old_function.id.clone(),
            new_id: new_function.id.clone(),
            call_bindings: bindings,
        });
        (delta, notes)
    });
    for (delta, notes) in function_deltas {
        if let Some(function) = delta {
            changes.push(DiffChange::FunctionChanged {
                function: Box::new(function),
            });
        }
        if let Some(notes) = notes {
            call_binding_notes.push(notes);
        }
    }

    let summary = summarize(&changes);
    let export_binding_notes = diff_export_bindings(old, new, &context);
    DiffReport {
        debug_context: [old.debug_info.summary(), new.debug_info.summary()],
        old: old_name.to_owned(),
        new: new_name.to_owned(),
        summary,
        function_matching,
        function_matches,
        changes,
        call_binding_notes,
        export_binding_notes,
        export_dependency_graph,
    }
}

fn implementation_matches(
    old: &NormalizedModule,
    new: &NormalizedModule,
    mut matches: Vec<FunctionMatch>,
    graph: &DependencyGraph,
) -> Vec<FunctionMatch> {
    let forward: BTreeMap<_, _> = matches
        .iter()
        .map(|m| (m.old_source_index, m.new_source_index))
        .collect();
    let reverse: BTreeMap<_, _> = matches
        .iter()
        .map(|m| (m.new_source_index, m.old_source_index))
        .collect();
    let replacements: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| {
            forward
                .get(&node.old_source_index)
                .is_some_and(|n| *n != node.new_source_index)
                || reverse
                    .get(&node.new_source_index)
                    .is_some_and(|n| *n != node.old_source_index)
        })
        .collect();
    let old_replaced: std::collections::BTreeSet<_> =
        replacements.iter().map(|n| n.old_source_index).collect();
    let new_replaced: std::collections::BTreeSet<_> =
        replacements.iter().map(|n| n.new_source_index).collect();
    matches.retain(|m| {
        !old_replaced.contains(&m.old_source_index) && !new_replaced.contains(&m.new_source_index)
    });
    for node in replacements {
        let a = &old.functions[node.old_source_index as usize];
        let b = &new.functions[node.new_source_index as usize];
        matches.push(FunctionMatch {
            old_id: a.id.clone(),
            new_id: b.id.clone(),
            old_source_index: a.source_index,
            new_source_index: b.source_index,
            confidence: 1.0,
            similarity: 1.0,
            reason: crate::matcher::MatchReason::SameMatchedReferenceBody,
            reference_context_complete: true,
            ambiguous: false,
            symbol_hint: None,
        });
    }
    // A unique exported role may evolve its signature. This is explicitly a
    // role correspondence, not a proof of historical implementation identity.
    let mut old_roles = BTreeMap::<_, Vec<_>>::new();
    let mut new_roles = BTreeMap::<_, Vec<_>>::new();
    for (module, roles) in [(old, &mut old_roles), (new, &mut new_roles)] {
        for f in &module.functions {
            if !f.export_names.is_empty() {
                roles.entry(f.export_names.clone()).or_default().push(f);
            }
        }
    }
    for (role, old_group) in old_roles {
        let Some(new_group) = new_roles.get(&role) else {
            continue;
        };
        let ([a], [b]) = (&old_group[..], &new_group[..]) else {
            continue;
        };
        if a.type_id == b.type_id
            || matches.iter().any(|m| {
                m.old_source_index == a.source_index || m.new_source_index == b.source_index
            })
        {
            continue;
        }
        matches.push(FunctionMatch {
            old_id: a.id.clone(),
            new_id: b.id.clone(),
            old_source_index: a.source_index,
            new_source_index: b.source_index,
            confidence: 0.75,
            similarity: 0.0,
            reason: crate::matcher::MatchReason::SameExportRole,
            reference_context_complete: a
                .fingerprint
                .as_ref()
                .is_some_and(|f| f.reference_context_complete)
                && b.fingerprint
                    .as_ref()
                    .is_some_and(|f| f.reference_context_complete),
            ambiguous: true,
            symbol_hint: None,
        });
    }
    matches.sort_by(|a, b| {
        a.old_id
            .cmp(&b.old_id)
            .then(a.new_id.cmp(&b.new_id))
            .then(a.old_source_index.cmp(&b.old_source_index))
    });
    matches
}

fn locals_equivalent(old: &[crate::ir::LocalIr], new: &[crate::ir::LocalIr]) -> bool {
    let mut a = 0;
    let mut b = 0;
    let mut x = 0;
    let mut y = 0;
    loop {
        while x == 0 && a < old.len() {
            x = old[a].count;
            a += 1;
        }
        while y == 0 && b < new.len() {
            y = new[b].count;
            b += 1;
        }
        if x == 0 || y == 0 {
            return x == 0 && y == 0;
        }
        if old[a - 1].value_type != new[b - 1].value_type {
            return false;
        }
        let shared = x.min(y);
        x -= shared;
        y -= shared;
    }
}

fn diff_function(
    old: &FunctionIr,
    new: &FunctionIr,
    call_bindings: &mut Vec<CallBindingDelta>,
    alignment: crate::structured_diff::Alignment,
    symbols: [Option<&str>; 2],
) -> Option<FunctionDelta> {
    let type_changed = old.type_id != new.type_id;
    let locals_changed = !locals_equivalent(&old.locals, &new.locals);
    let direct_calls_changed = old.direct_calls != new.direct_calls;
    let operators = diff_aligned_operators(&old.operators, &new.operators, &alignment);

    if !type_changed
        && !locals_changed
        && !direct_calls_changed
        && operators.is_empty()
        && alignment.report.regions.is_empty()
        && alignment.report.branch_targets.is_empty()
        && !call_bindings
            .iter()
            .any(|binding| binding.target_changed == Some(true))
    {
        return None;
    }

    let reference_only = !type_changed
        && !locals_changed
        && alignment.report.complete
        && alignment
            .report
            .regions
            .iter()
            .all(|region| region.relation == "modified" && !region.ambiguous)
        && alignment.report.branch_targets.is_empty()
        && !operators.is_empty()
        && operators.iter().all(|delta| match delta {
            OperatorDelta::Replace {
                index, new_index, ..
            } => {
                old.operators[*index].opcode == new.operators[*new_index].opcode
                    && matches!(
                        (
                            &old.operators[*index].immediate,
                            &new.operators[*new_index].immediate
                        ),
                        (Immediate::Call(_), Immediate::Call(_))
                            | (Immediate::RefFunc(_), Immediate::RefFunc(_))
                    )
            }
            _ => false,
        });
    let change_kind = if reference_only
        && !call_bindings.iter().any(|b| b.target_changed == Some(true))
    {
        let relocated: std::collections::BTreeSet<_> = call_bindings
            .iter()
            .filter(|b| {
                b.relation == "same_target_index_relocated" && b.target_changed == Some(false)
            })
            .filter_map(|b| {
                Some((
                    b.old.as_ref()?.operator_index,
                    b.new.as_ref()?.operator_index,
                ))
            })
            .collect();
        if call_bindings.iter().all(|b| b.target_changed == Some(false))
            && operators.iter().all(|d| matches!(d, OperatorDelta::Replace {index,new_index,..} if relocated.contains(&(*index,*new_index)))) {
            FunctionChangeKind::ConfirmedReferenceRelocation
        } else {
            FunctionChangeKind::UnresolvedReferenceChange
        }
    } else {
        FunctionChangeKind::BodyOrInterfaceChange
    };
    Some(FunctionDelta {
        old_id: old.id.clone(),
        new_id: new.id.clone(),
        old_source_index: old.source_index,
        new_source_index: new.source_index,
        old_symbol: symbols[0].map(str::to_owned),
        new_symbol: symbols[1].map(str::to_owned),
        symbol_sources: [None, None],
        source_annotations: Vec::new(),
        change_kind,
        type_changed,
        locals_changed,
        direct_calls_changed,
        call_bindings: std::mem::take(call_bindings),
        operators,
        instruction_alignment: alignment.report,
    })
}

fn target_function(module: &ModuleIr, index: u32) -> Option<&FunctionIr> {
    module
        .functions
        .get(index as usize)
        .filter(|target| target.source_index == index)
        .or_else(|| {
            module
                .functions
                .iter()
                .find(|target| target.source_index == index)
        })
}

fn binding_site(
    module: &ModuleIr,
    index: usize,
    operator: &ParsedOperator,
) -> Option<CallBindingSite> {
    let function_index = match &operator.immediate {
        Immediate::Call(index) | Immediate::RefFunc(index) => *index,
        _ => return None,
    };
    let target = target_evidence(module, function_index);
    Some(CallBindingSite {
        operator_index: index,
        offset: operator.offset,
        opcode: operator.opcode.as_str().into(),
        function_index,
        target,
    })
}

fn target_evidence(module: &ModuleIr, function_index: u32) -> Option<TargetEvidence> {
    target_function(module, function_index).map(|target| {
        let import = if target.kind == FunctionKindIr::Imported {
            module
                .imports
                .iter()
                .filter(|import| import.kind == ExternalKindIr::Func)
                .nth(function_index as usize)
                .and_then(|import| match &import.type_ref {
                    TypeRefIr::Func { type_id, exact, .. } => Some(ImportBindingEvidence {
                        module: import.module.clone(),
                        name: import.name.clone(),
                        type_id: type_id.clone(),
                        exact: *exact,
                    }),
                    _ => None,
                })
        } else {
            None
        };
        TargetEvidence {
            kind: if target.kind == FunctionKindIr::Imported {
                "imported"
            } else {
                "defined"
            }
            .into(),
            id: target.id.clone(),
            source_index: target.source_index,
            type_index: target.type_index,
            type_id: target.type_id.clone(),
            export_names: target.export_names.clone(),
            import,
        }
    })
}

fn export_site(module: &ModuleIr, export: &ExportIr) -> ExportBindingSite {
    ExportBindingSite {
        export_index: export.source_index,
        name: export.name.clone(),
        kind: export.kind.as_str().into(),
        item_index: export.item_index,
        exact: export.exact,
        target: (export.kind == ExternalKindIr::Func)
            .then(|| target_evidence(module, export.item_index))
            .flatten(),
    }
}

fn export_neutral(module: &NormalizedModule) -> NormalizedModule {
    let mut neutral = module.module.clone();
    neutral.exports.clear();
    neutral.function_names.clear();
    let imports: Vec<_> = module
        .imports
        .iter()
        .filter(|import| import.kind == ExternalKindIr::Func)
        .map(|import| match &import.type_ref {
            TypeRefIr::Func { type_id, exact, .. } => Some(ImportBindingEvidence {
                module: import.module.clone(),
                name: import.name.clone(),
                type_id: type_id.clone(),
                exact: *exact,
            }),
            _ => None,
        })
        .collect();
    for function in &mut neutral.functions {
        function.export_names.clear();
        function.fingerprint = None;
        if function.kind == FunctionKindIr::Imported {
            if let Some(Some(binding)) = imports.get(function.source_index as usize) {
                // Import strings may themselves contain identity delimiters.
                // This independent view uses an unambiguous structural key.
                function.id = crate::binary::text_key([
                    binding.module.as_str(),
                    binding.name.as_str(),
                    binding.type_id.as_str(),
                    if binding.exact { "1" } else { "0" },
                ]);
            }
        }
    }
    crate::normalize::normalize_module(&crate::resolve::resolve_module(crate::ir::ParsedModule {
        module: neutral,
    }))
}

fn match_evidence(candidate: &FunctionMatch) -> TargetMatchEvidence {
    TargetMatchEvidence {
        old_source_index: candidate.old_source_index,
        new_source_index: candidate.new_source_index,
        reason: candidate.reason,
        confidence: candidate.confidence,
        ambiguous: candidate.ambiguous,
        reference_context_complete: candidate.reference_context_complete,
    }
}

fn export_groups(module: &ModuleIr) -> BTreeMap<(String, String), Vec<&ExportIr>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for export in &module.exports {
        groups
            .entry((export.name.clone(), export.kind.as_str().to_string()))
            .or_default()
            .push(export);
    }
    groups
}

fn reference_free(module: &ModuleIr, index: u32) -> bool {
    target_function(module, index).is_some_and(|function| {
        function
            .operators
            .iter()
            .all(|op| !matches!(op.immediate, Immediate::Call(_) | Immediate::RefFunc(_)))
    })
}

fn import_counts(module: &ModuleIr) -> BTreeMap<Vec<u8>, usize> {
    let mut counts = BTreeMap::new();
    for import in &module.imports {
        if let TypeRefIr::Func { type_id, exact, .. } = &import.type_ref {
            let binding = ImportBindingEvidence {
                module: import.module.clone(),
                name: import.name.clone(),
                type_id: type_id.clone(),
                exact: *exact,
            };
            *counts
                .entry(crate::binary::encode(&binding).expect("binding serializes"))
                .or_default() += 1;
        }
    }
    counts
}

struct BindingContext<'a> {
    proofs: &'a DependencyGraph,
    ambiguous_candidates: BTreeMap<u32, TargetMatchEvidence>,
    old_imports: BTreeMap<Vec<u8>, usize>,
    new_imports: BTreeMap<Vec<u8>, usize>,
}

impl<'a> BindingContext<'a> {
    fn new(
        old: &NormalizedModule,
        new: &NormalizedModule,
        proofs: &'a DependencyGraph,
        same_module: bool,
    ) -> Self {
        let ambiguous_candidates = if same_module {
            BTreeMap::new()
        } else {
            match_functions(&export_neutral(old), &export_neutral(new))
                .iter()
                .filter(|candidate| candidate.ambiguous)
                .map(|candidate| (candidate.old_source_index, match_evidence(candidate)))
                .collect()
        };
        Self {
            proofs,
            ambiguous_candidates,
            old_imports: import_counts(old),
            new_imports: import_counts(new),
        }
    }

    fn unique_imports(&self, old: &ImportBindingEvidence, new: &ImportBindingEvidence) -> bool {
        self.old_imports
            .get(&crate::binary::encode(old).expect("binding serializes"))
            == Some(&1)
            && self
                .new_imports
                .get(&crate::binary::encode(new).expect("binding serializes"))
                == Some(&1)
    }

    fn candidate(
        &self,
        module: &ModuleIr,
        index: u32,
        ambiguous_fallback: bool,
    ) -> (Option<DependencyRoot>, Option<TargetMatchEvidence>) {
        let root = self.proofs.root(index);
        let candidate = root
            .as_ref()
            .map(|root| TargetMatchEvidence {
                old_source_index: root.old_source_index,
                new_source_index: root.new_source_index,
                reason: if target_function(module, index)
                    .is_some_and(|function| function.kind == FunctionKindIr::Imported)
                {
                    crate::matcher::MatchReason::SameStableId
                } else if root.depth == 0 {
                    crate::matcher::MatchReason::SameExactBodyHash
                } else {
                    crate::matcher::MatchReason::SameMatchedReferenceBody
                },
                confidence: if root.depth == 0 { 1.0 } else { 0.9 },
                ambiguous: false,
                reference_context_complete: true,
            })
            .or_else(|| {
                ambiguous_fallback
                    .then(|| self.ambiguous_candidates.get(&index).cloned())
                    .flatten()
            });
        (root, candidate)
    }
}

fn diff_export_bindings(
    old: &NormalizedModule,
    new: &NormalizedModule,
    context: &BindingContext<'_>,
) -> Vec<ExportBindingDelta> {
    if old.module.same_function_projection(&new.module)
        || old.exports.is_empty()
        || new.exports.is_empty()
    {
        return Vec::new();
    }
    let old_groups = export_groups(old);
    let new_groups = export_groups(new);
    let mut notes = Vec::new();
    for (key, old_exports) in old_groups {
        let Some(new_exports) = new_groups.get(&key) else {
            continue;
        };
        let duplicate_name = old_exports.len() != 1 || new_exports.len() != 1;
        for (a, b) in old_exports.iter().zip(new_exports) {
            let old_site = export_site(old, a);
            let new_site = export_site(new, b);
            // Ambiguous leaf candidates explain uncertainty, never uniqueness.
            let (dependency_proof, candidate) = if a.kind == ExternalKindIr::Func {
                context.candidate(
                    old,
                    a.item_index,
                    reference_free(old, a.item_index) && reference_free(new, b.item_index),
                )
            } else {
                (None, None)
            };
            let raw_changed = a.item_index != b.item_index || a.exact != b.exact;
            let (relation, changed) = if duplicate_name {
                if !raw_changed {
                    continue;
                }
                ("ambiguous_target", None)
            } else if a.exact != b.exact {
                // A changed descriptor flag is not an index relocation, even
                // for a manually constructed IR outside valid Core exports.
                ("export_descriptor_changed", None)
            } else {
                match (&old_site.target, &new_site.target) {
                    (Some(x), Some(y)) if x.kind == "imported" && y.kind == "imported" => {
                        match (&x.import, &y.import) {
                            (Some(p), Some(q)) if p != q => ("different_target", Some(true)),
                            (Some(p), Some(q)) if context.unique_imports(p, q) => {
                                if !raw_changed {
                                    continue;
                                }
                                ("candidate_same_target_index_relocated", Some(false))
                            }
                            (Some(_), Some(_)) => {
                                if !raw_changed {
                                    continue;
                                }
                                ("ambiguous_target", None)
                            }
                            _ => ("unresolved_target", None),
                        }
                    }
                    (Some(x), Some(y)) if x.kind != y.kind => ("different_target", Some(true)),
                    (Some(_), Some(_)) => {
                        if let Some(candidate) = candidate.as_ref() {
                            if candidate.ambiguous {
                                if !raw_changed {
                                    continue;
                                }
                                ("ambiguous_target", None)
                            } else if candidate.reference_context_complete
                                && dependency_proof.is_some()
                            {
                                if candidate.new_source_index != b.item_index {
                                    ("different_target", Some(true))
                                } else {
                                    if !raw_changed {
                                        continue;
                                    }
                                    ("candidate_same_target_index_relocated", Some(false))
                                }
                            } else {
                                if !raw_changed {
                                    continue;
                                }
                                ("unresolved_target", None)
                            }
                        } else {
                            if !raw_changed {
                                continue;
                            }
                            ("unresolved_target", None)
                        }
                    }
                    _ => {
                        if !raw_changed {
                            continue;
                        }
                        ("unresolved_target", None)
                    }
                }
            };
            notes.push(ExportBindingDelta {
                pairing: "export_name_kind".into(),
                relation: relation.into(),
                target_changed: changed,
                identity_unproven: true,
                match_scope: "exports_removed_from_candidate_matching".into(),
                old: old_site,
                new: new_site,
                target_match: candidate,
                dependency_proof,
            });
        }
    }
    notes
}

fn diff_call_bindings(
    old: &FunctionIr,
    new: &FunctionIr,
    old_module: &ModuleIr,
    new_module: &ModuleIr,
    context: &BindingContext<'_>,
    alignment: &crate::structured_diff::Alignment,
) -> Vec<CallBindingDelta> {
    let mut bindings = Vec::new();
    let positions = alignment
        .old_to_new
        .iter()
        .enumerate()
        .map(|(i, mapped)| (Some(i), *mapped))
        .chain(
            alignment
                .new_used
                .iter()
                .enumerate()
                .filter(|(_, used)| !**used)
                .map(|(i, _)| (None, Some(i))),
        );
    for (old_index, new_index) in positions {
        let old_site =
            old_index.and_then(|index| binding_site(old_module, index, &old.operators[index]));
        let new_site =
            new_index.and_then(|index| binding_site(new_module, index, &new.operators[index]));
        if old_site.is_none() && new_site.is_none() {
            continue;
        }
        let (dependency_proof, target_match) = old_site
            .as_ref()
            .map(|site| context.candidate(old_module, site.function_index, true))
            .unwrap_or((None, None));
        let (relation, target_changed) = match (&old_site, &new_site) {
            (Some(_), Some(_)) if old_index.is_some_and(|i| alignment.ambiguous_old[i]) => {
                ("ambiguous_site", None)
            }
            (Some(a), Some(b)) => match (&a.target, &b.target) {
                (Some(old_target), Some(new_target)) => {
                    if old_target.kind == "imported" && new_target.kind == "imported" {
                        match (&old_target.import, &new_target.import) {
                            (Some(old_import), Some(new_import)) if old_import == new_import => {
                                if !context.unique_imports(old_import, new_import) {
                                    ("ambiguous_target", None)
                                } else {
                                    if a.function_index == b.function_index {
                                        continue;
                                    }
                                    ("same_target_index_relocated", Some(false))
                                }
                            }
                            (Some(_), Some(_)) => ("different_target", Some(true)),
                            _ => ("unresolved_target", None),
                        }
                    } else if old_target.kind != new_target.kind {
                        ("different_target", Some(true))
                    } else if let Some(root) = &dependency_proof {
                        if root.new_source_index != b.function_index {
                            ("different_target", Some(true))
                        } else {
                            if a.function_index == b.function_index {
                                continue;
                            }
                            ("same_target_index_relocated", Some(false))
                        }
                    } else if target_match
                        .as_ref()
                        .is_some_and(|candidate| candidate.ambiguous)
                    {
                        ("ambiguous_target", None)
                    } else {
                        // Equal indices alone do not establish continuity:
                        // removing a callee can reuse its slot for a different
                        // implementation. Without a body/dependency witness,
                        // retain an informational note even at the same index;
                        // this does not assert a caller instruction change.
                        ("unresolved_target", None)
                    }
                }
                _ => ("unresolved_target", None),
            },
            (Some(_), None) => ("site_removed", None),
            (None, Some(_)) => ("site_added", None),
            (None, None) => unreachable!(),
        };
        bindings.push(CallBindingDelta {
            pairing: "wasm_region_instruction_alignment".into(),
            relation: relation.into(),
            target_changed,
            identity_unproven: true,
            match_scope: "exports_removed_from_candidate_matching".into(),
            old: old_site,
            new: new_site,
            target_match,
            dependency_proof,
        });
    }
    bindings
}

fn diff_aligned_operators(
    old: &[ParsedOperator],
    new: &[ParsedOperator],
    alignment: &crate::structured_diff::Alignment,
) -> Vec<OperatorDelta> {
    let mut deltas = Vec::new();
    for (i, mapped) in alignment.old_to_new.iter().enumerate() {
        match mapped {
            Some(j) if old[i].opcode != new[*j].opcode || old[i].immediate != new[*j].immediate => {
                deltas.push(OperatorDelta::Replace {
                    index: i,
                    new_index: *j,
                    old: operator_record(&old[i]),
                    new: operator_record(&new[*j]),
                });
            }
            Some(_) => {}
            None => deltas.push(OperatorDelta::Delete {
                index: i,
                operator: operator_record(&old[i]),
            }),
        }
    }
    for (i, used) in alignment.new_used.iter().enumerate() {
        if !used {
            deltas.push(OperatorDelta::Insert {
                index: i,
                operator: operator_record(&new[i]),
            });
        }
    }
    deltas
}

fn operator_record(operator: &ParsedOperator) -> OperatorRecord {
    OperatorRecord {
        offset: operator.offset,
        opcode: operator.opcode.as_str().to_owned(),
        text: operator.display_text(),
    }
}

fn ids_by_type(items: &[TypeIr]) -> Vec<String> {
    items.iter().map(|item| item.id.clone()).collect()
}

fn ids_by_import(items: &[ImportIr]) -> Vec<String> {
    items.iter().map(|item| item.id.clone()).collect()
}

fn ids_by_export(items: &[ExportIr]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            format!(
                "{}:target:{}:exact:{}",
                item.id, item.item_index, item.exact
            )
        })
        .collect()
}

fn push_collection_changes(
    mut old_ids: Vec<String>,
    mut new_ids: Vec<String>,
    removed: impl Fn(String) -> DiffChange,
    added: impl Fn(String) -> DiffChange,
    changes: &mut Vec<DiffChange>,
) {
    old_ids.sort();
    new_ids.sort();

    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old_ids.len() || new_index < new_ids.len() {
        match (old_ids.get(old_index), new_ids.get(new_index)) {
            (Some(old), Some(new)) if old == new => {
                old_index += 1;
                new_index += 1;
            }
            (Some(old), new) if new.is_none_or(|new| old < new) => {
                changes.push(removed(old.clone()));
                old_index += 1;
            }
            (_, Some(new)) => {
                changes.push(added(new.clone()));
                new_index += 1;
            }
            _ => unreachable!(),
        }
    }
}

fn summarize(changes: &[DiffChange]) -> DiffSummary {
    let mut summary = DiffSummary {
        types_added: 0,
        types_removed: 0,
        imports_added: 0,
        imports_removed: 0,
        exports_added: 0,
        exports_removed: 0,
        functions_added: 0,
        functions_removed: 0,
        functions_changed: 0,
        operator_replacements: 0,
        operator_insertions: 0,
        operator_deletions: 0,
        control_region_moves: 0,
    };

    for change in changes {
        match change {
            DiffChange::TypeAdded { .. } => summary.types_added += 1,
            DiffChange::TypeRemoved { .. } => summary.types_removed += 1,
            DiffChange::ImportAdded { .. } => summary.imports_added += 1,
            DiffChange::ImportRemoved { .. } => summary.imports_removed += 1,
            DiffChange::ExportAdded { .. } => summary.exports_added += 1,
            DiffChange::ExportRemoved { .. } => summary.exports_removed += 1,
            DiffChange::FunctionAdded { .. } => summary.functions_added += 1,
            DiffChange::FunctionRemoved { .. } => summary.functions_removed += 1,
            DiffChange::FunctionChanged { function } => {
                summary.functions_changed += 1;
                summary.control_region_moves += function
                    .instruction_alignment
                    .regions
                    .iter()
                    .filter(|r| r.relation == "moved")
                    .count();
                for operator in &function.operators {
                    match operator {
                        OperatorDelta::Replace { .. } => summary.operator_replacements += 1,
                        OperatorDelta::Insert { .. } => summary.operator_insertions += 1,
                        OperatorDelta::Delete { .. } => summary.operator_deletions += 1,
                    }
                }
            }
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::{diff_modules, DiffChange, OperatorDelta};
    use crate::normalize::normalize_module;
    use crate::parse::parse_module;
    use crate::resolve::resolve_module;

    #[test]
    fn collection_deltas_preserve_duplicate_multiplicity() {
        let mut changes = Vec::new();
        super::push_collection_changes(
            vec!["same".into(), "same".into()],
            vec!["same".into()],
            |id| DiffChange::ImportRemoved { id },
            |id| DiffChange::ImportAdded { id },
            &mut changes,
        );
        assert_eq!(
            changes,
            vec![DiffChange::ImportRemoved { id: "same".into() }]
        );
    }

    #[test]
    fn export_target_changes_are_reported() {
        let old = normalized_fixture(include_bytes!("../tests/fixtures/old.wasm"));
        let mut new = old.clone();
        new.module.exports[0].item_index += 1;
        let report = diff_modules("old", &old, "new", &new);
        assert_eq!(
            (report.summary.exports_added, report.summary.exports_removed),
            (1, 1)
        );
    }

    fn normalized_fixture(bytes: &[u8]) -> crate::ir::NormalizedModule {
        let resolved = resolve_module(parse_module(bytes).unwrap());
        normalize_module(&resolved)
    }

    fn duplicate_empty_functions_wasm() -> Vec<u8> {
        vec![
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: [] -> []
            0x03, 0x04, 0x03, 0x00, 0x00, 0x00, // three defined funcs
            0x0a, 0x0a, 0x03, // code section, three bodies
            0x02, 0x00, 0x0b, 0x02, 0x00, 0x0b, 0x02, 0x00, 0x0b,
        ]
    }

    fn similar_functions_wasm(first_variant: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\0asm");
        bytes.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(&[0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f]);
        bytes.extend_from_slice(&[0x03, 0x03, 0x02, 0x00, 0x00]);
        bytes.extend_from_slice(&[
            0x0a, 0x11, 0x02, 0x07, 0x00, 0x20, 0x00, 0x41, 0x01, 0x6a, 0x0b, 0x07, 0x00, 0x20,
            0x00,
        ]);
        if first_variant {
            bytes.extend_from_slice(&[0x41, 0x02, 0x6a, 0x0b]);
        } else {
            bytes.extend_from_slice(&[0x41, 0x03, 0x6a, 0x0b]);
        }
        bytes
    }

    #[test]
    fn reports_canonical_fixture_as_single_operator_replacement() {
        let old_module = normalized_fixture(include_bytes!("../tests/fixtures/old.wasm"));
        let new_module = normalized_fixture(include_bytes!("../tests/fixtures/new.wasm"));

        let report = diff_modules("old.wasm", &old_module, "new.wasm", &new_module);

        assert_eq!(report.summary.functions_changed, 1);
        assert_eq!(report.summary.operator_replacements, 1);
        assert_eq!(report.summary.operator_insertions, 0);
        assert_eq!(report.summary.operator_deletions, 0);
        assert_eq!(report.summary.exports_added, 0);
        assert_eq!(report.summary.exports_removed, 0);
        assert_eq!(report.changes.len(), 1);

        let DiffChange::FunctionChanged { function } = &report.changes[0] else {
            panic!("expected changed function");
        };
        assert_eq!(function.old_id, "func:export:add:type:i32,i32->i32");
        assert_eq!(function.new_id, "func:export:add:type:i32,i32->i32");
        assert!(!function.type_changed);
        assert!(!function.locals_changed);
        assert!(!function.direct_calls_changed);
        assert_eq!(function.operators.len(), 1);

        let OperatorDelta::Replace {
            index, old, new, ..
        } = &function.operators[0]
        else {
            panic!("expected replacement");
        };
        assert_eq!(*index, 1);
        assert_eq!(old.text, "LocalGet { local_index: 1 }");
        assert_eq!(new.text, "I32Const { value: 1 }");
    }

    #[test]
    fn reports_no_function_churn_for_identical_duplicate_functions() {
        let wasm = duplicate_empty_functions_wasm();
        let old_module = normalized_fixture(&wasm);
        let new_module = normalized_fixture(&wasm);

        let report = diff_modules("same.wasm", &old_module, "same.wasm", &new_module);

        assert_eq!(report.summary.functions_added, 0);
        assert_eq!(report.summary.functions_removed, 0);
        assert_eq!(report.summary.functions_changed, 0);
        assert_eq!(report.function_matches.len(), 3);
        assert!(report.changes.is_empty());
    }

    #[test]
    fn reports_similar_functions_without_added_or_removed_churn() {
        let old_module = normalized_fixture(&similar_functions_wasm(true));
        let new_module = normalized_fixture(&similar_functions_wasm(false));

        let report = diff_modules("old.wasm", &old_module, "new.wasm", &new_module);

        assert_eq!(report.summary.functions_added, 0);
        assert_eq!(report.summary.functions_removed, 0);
        assert_eq!(report.summary.functions_changed, 1);
        assert_eq!(report.function_matches.len(), 2);
    }
}
