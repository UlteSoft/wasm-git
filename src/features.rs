//! Community lifecycle is metadata, never permission to remove a decoder.
//!
//! Keep RETAINED entries, flags, wire readers and regression fixtures when an
//! upstream proposal is withdrawn or replaced. New wire revisions need distinct
//! readers; never reinterpret historical bytes as a different proposal.

use std::fmt::Write;
use std::sync::OnceLock;

use clap::Parser;
use serde::Serialize;
use wasmparser::WasmFeatures;

use crate::standard::{FeatureEvidence, Standard};

#[derive(Debug, Serialize)]
pub struct UpstreamFeature {
    pub id: &'static str,
    pub name: &'static str,
    /// Numeric strings are active phases; finished is merged, not a phase guess.
    pub phase: &'static str,
    /// Applies only to affected_specs; a JS API version is not a Core opcode.
    pub spec_version: &'static str,
    pub affected_specs: &'static str,
    pub url: &'static str,
    pub rationale: &'static str,
}

macro_rules! proposal {
    ($id:literal, $name:literal, $phase:literal, $version:literal, $specs:literal, $url:literal, $why:literal) => {
        UpstreamFeature {
            id: $id,
            name: $name,
            phase: $phase,
            spec_version: $version,
            affected_specs: $specs,
            url: $url,
            rationale: $why,
        }
    };
}
include!("feature_upstream.rs");

static LEGACY_EXCEPTIONS: UpstreamFeature = proposal!(
    "legacy_exceptions", "Legacy Exception Handling", "inactive", "", "core",
    "https://github.com/WebAssembly/exception-handling/blob/main/proposals/exception-handling/legacy/Exceptions.md",
    "Superseded in October 2023; discouraged by its explainer and inactive in the community website features inventory. wasm-git retains the historical encoding."
);

static MEMTAG_EXTENSION: UpstreamFeature = proposal!(
    "memtag_extension", "uwvm2-ros VM instruction extension: WasmMTE", "experimental", "", "extension",
    "https://github.com/UlteSoft/wasm-git/blob/main/documents/UWVM2_ROS_EXTENSIONS.eu_US.md",
    "Nonstandard FF + ULEB32 extension-opcode namespace, with opcode-specific operands. Recognizes 19 WasmMTE instructions; reserved add and unregistered opcodes remain unknown. Recognition and address-width type validation do not claim execution or memory-safety enforcement."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// Known pinned binary grammar and typed instruction operands, not execution.
    TypedCore,
    /// Some typed projections remain opaque; see docs/standards.md.
    PartialCore,
    /// Independently recognized convention; alternate payloads remain opaque.
    CustomConvention,
    /// Import structure is audited; host builtin semantics are not executed.
    ImportStructure,
    /// Top-level framing only; no Component semantic validation.
    OuterFraming,
    /// Text syntax or host integration adds no decoder switch here.
    HostOrText,
    /// No implementation claim. Raw bytes are still retained and diffable.
    Unreviewed,
}

struct Retained {
    id: &'static str,
    proposal: &'static str,
    coverage: Coverage,
    flags: WasmFeatures,
}

macro_rules! retained {
    ($id:literal, $proposal:literal, $coverage:ident, $flag:ident) => {
        Retained {
            id: $id,
            proposal: $proposal,
            coverage: Coverage::$coverage,
            flags: WasmFeatures::$flag,
        }
    };
}

// Append-only decoder commitment. An upstream status update cannot edit this
// table. Referencing each flag also makes parser removals a compilation error.
#[rustfmt::skip]
static RETAINED: &[Retained] = &[
    retained!("mvp", "mvp", TypedCore, WASM1),
    retained!("mutable_global", "import_export_of_mutable_globals", TypedCore, MUTABLE_GLOBAL),
    retained!("saturating_float_to_int", "non-trapping_float-to-int_conversions", TypedCore, SATURATING_FLOAT_TO_INT),
    retained!("sign_extension", "sign-extension_operators", TypedCore, SIGN_EXTENSION),
    retained!("multi_value", "multi-value", TypedCore, MULTI_VALUE),
    retained!("reference_types", "reference_types", TypedCore, REFERENCE_TYPES),
    retained!("bulk_memory", "bulk_memory_operations", TypedCore, BULK_MEMORY),
    retained!("simd", "fixed-width_simd", TypedCore, SIMD),
    retained!("tail_call", "tail_call", TypedCore, TAIL_CALL),
    retained!("extended_const", "extended-const", TypedCore, EXTENDED_CONST),
    retained!("function_references", "function_references", TypedCore, FUNCTION_REFERENCES),
    retained!("gc", "garbage_collection", TypedCore, GC),
    retained!("multi_memory", "multi-memory", TypedCore, MULTI_MEMORY),
    retained!("relaxed_simd", "relaxed-simd", TypedCore, RELAXED_SIMD),
    retained!("exceptions", "exception_handling", TypedCore, EXCEPTIONS),
    retained!("memory64", "memory64", TypedCore, MEMORY64),
    retained!("threads", "threads", TypedCore, THREADS),
    retained!("compact_imports", "compact-import-section", TypedCore, COMPACT_IMPORTS),
    retained!("wide_arithmetic", "wide-arithmetic", TypedCore, WIDE_ARITHMETIC),
    retained!("shared_everything_threads", "shared-everything-threads", TypedCore, SHARED_EVERYTHING_THREADS),
    retained!("stack_switching", "stack-switching", TypedCore, STACK_SWITCHING),
    retained!("custom_descriptors", "custom-descs", TypedCore, CUSTOM_DESCRIPTORS),
    retained!("custom_page_sizes", "custom-page-sizes", TypedCore, CUSTOM_PAGE_SIZES),
    retained!("memory_control", "memory-control", TypedCore, MEMORY_CONTROL),
    retained!("legacy_exceptions", "legacy_exceptions", TypedCore, LEGACY_EXCEPTIONS),
    retained!("memtag_extension", "memtag_extension", TypedCore, MEMTAG_EXTENSION),
];

#[derive(Debug, Serialize)]
pub struct Feature {
    pub id: &'static str,
    pub upstream: &'static UpstreamFeature,
    pub lifecycle: &'static str,
    /// Phase >= 4 or finished is eligible for integration, not proof of support.
    pub standardization_ready: bool,
    pub coverage: Coverage,
    pub retained: bool,
    /// Existing grammar/convention recognized under the default all-known view.
    /// This is not an execution or complete semantic support claim.
    pub default_recognition: bool,
    /// None for proposals with no audited Core binary grammar here.
    pub selected_standard_allows: Option<bool>,
}

fn describe(upstream: &'static UpstreamFeature, standard: Standard) -> Feature {
    let retained = RETAINED.iter().find(|entry| entry.proposal == upstream.id);
    let (id, coverage, keep, allowed) = if let Some(entry) = retained {
        (
            entry.id,
            entry.coverage,
            true,
            Some(standard.features().contains(entry.flags)),
        )
    } else {
        let (coverage, keep, allowed) = match upstream.id {
            "branch-hinting" => (Coverage::CustomConvention, true, Some(true)),
            "js-string-builtins" => (Coverage::ImportStructure, true, Some(true)),
            "component-model" => (Coverage::OuterFraming, true, None),
            "annotations"
            | "javascript_bigint_to_webassembly_i64_integration"
            | "js-promise-integration"
            | "content-security-policy"
            | "ecmascript_module_integration"
            | "wasm_c_api"
            | "numeric-values-in-wat"
            | "js-types" => (Coverage::HostOrText, false, None),
            _ => (Coverage::Unreviewed, false, None),
        };
        (upstream.id, coverage, keep, allowed)
    };
    Feature {
        id,
        upstream,
        coverage,
        retained: keep,
        default_recognition: keep,
        selected_standard_allows: allowed,
        standardization_ready: matches!(upstream.phase, "4" | "5" | "finished"),
        lifecycle: match upstream.phase {
            "inactive" | "missing" if keep => "deprecated",
            "inactive" => "inactive",
            "missing" => "missing_review",
            "finished" => "finished",
            "5" => "standardized_unmerged",
            "4" => "standardizing",
            _ => "experimental",
        },
    }
}

pub fn catalog(standard: Standard) -> Vec<Feature> {
    UPSTREAM
        .iter()
        .chain(std::iter::once(&LEGACY_EXCEPTIONS))
        .chain(std::iter::once(&MEMTAG_EXTENSION))
        .map(|entry| describe(entry, standard))
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct DeprecationNotice {
    pub feature: &'static str,
    pub lifecycle: &'static str,
    pub upstream_phase: &'static str,
    pub source: &'static str,
    pub reason: &'static str,
    pub decoder_retained: bool,
}

/// Small, cached list; no catalogue lookup on the per-instruction hot path.
pub(crate) fn deprecated_used(evidence: &[FeatureEvidence]) -> Vec<DeprecationNotice> {
    static DEPRECATED: OnceLock<Vec<DeprecationNotice>> = OnceLock::new();
    DEPRECATED
        .get_or_init(|| {
            catalog(Standard::AllKnown)
                .into_iter()
                .filter(|entry| entry.lifecycle == "deprecated")
                .map(|entry| DeprecationNotice {
                    feature: entry.id,
                    lifecycle: "deprecated",
                    upstream_phase: entry.upstream.phase,
                    source: entry.upstream.url,
                    reason: entry.upstream.rationale,
                    decoder_retained: true,
                })
                .collect()
        })
        .iter()
        .filter(|notice| evidence.iter().any(|e| e.feature == notice.feature))
        .cloned()
        .collect()
}

#[derive(Parser)]
struct Args {
    /// Local feature ID or upstream proposal ID; omit to list the inventory.
    feature: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "all-known")]
    standard: Standard,
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    let args = match Args::try_parse_from(args) {
        Ok(args) => args,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string())
        }
        Err(error) => return Err(error.to_string()),
    };
    let mut entries = catalog(args.standard);
    if let Some(id) = args.feature {
        entries.retain(|entry| entry.id == id || entry.upstream.id == id);
        if entries.is_empty() {
            return Err(format!(
                "unknown feature {id:?}; use `features` to list known entries"
            ));
        }
    }
    let policy = "Implemented readers are permanent, including deprecated encodings. Community status never enables unknown grammar or disables retained grammar. Coverage describes the pinned decoder, not execution; unknown/custom bytes remain lossless. Metadata refresh is offline from normal commands.";
    if args.json {
        return serde_json::to_string_pretty(&serde_json::json!({
            "upstream_revision": UPSTREAM_REVISION, "upstream_sha256": UPSTREAM_SHA256,
            "source": "https://github.com/WebAssembly/proposals", "decoder": "wasmparser 0.249.0 + wasm-git patches",
            "selected_standard": args.standard, "retention_policy": policy, "features": entries,
            "core_validation_switches": crate::feature_set::CORE_FEATURES.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        })).map_err(|error| error.to_string());
    }
    let mut out = format!(
        "WebAssembly features; upstream {UPSTREAM_REVISION}; standard={}\n{policy}\n",
        args.standard
    );
    for entry in entries {
        let _ = writeln!(
            out,
            "{}: {} phase={} coverage={:?} retained={} profile_allowed={:?}\n  {}\n  {}",
            entry.id,
            entry.lifecycle,
            entry.upstream.phase,
            entry.coverage,
            entry.retained,
            entry.selected_standard_allows,
            entry.upstream.name,
            entry.upstream.url
        );
        if !entry.upstream.rationale.is_empty() {
            let _ = writeln!(out, "  {}", entry.upstream.rationale);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_withdrawal_never_disables_known_grammar() {
        for retained in RETAINED {
            let original = if retained.proposal == "legacy_exceptions" {
                &LEGACY_EXCEPTIONS
            } else if retained.proposal == "memtag_extension" {
                &MEMTAG_EXTENSION
            } else {
                UPSTREAM
                    .iter()
                    .find(|u| u.id == retained.proposal)
                    .expect("retained upstream record")
            };
            assert!(Standard::AllKnown.features().contains(retained.flags));
            let synthetic = Box::leak(Box::new(UpstreamFeature {
                id: original.id,
                name: original.name,
                phase: "inactive",
                spec_version: "",
                affected_specs: "",
                url: original.url,
                rationale: "test withdrawal",
            }));
            let feature = describe(synthetic, Standard::AllKnown);
            assert_eq!(feature.lifecycle, "deprecated");
            assert!(feature.retained && feature.default_recognition);
            assert_eq!(feature.selected_standard_allows, Some(true));
        }
    }

    #[test]
    fn ready_inventory_has_reviewed_coverage_and_preserves_core_boundaries() {
        for feature in catalog(Standard::Core3) {
            if feature.standardization_ready {
                assert_ne!(feature.coverage, Coverage::Unreviewed, "{}", feature.id);
            }
            if matches!(
                feature.id,
                "threads"
                    | "compact_imports"
                    | "wide_arithmetic"
                    | "legacy_exceptions"
                    | "memtag_extension"
            ) {
                assert_eq!(
                    feature.selected_standard_allows,
                    Some(false),
                    "{}",
                    feature.id
                );
            }
        }
        assert_eq!(
            RETAINED.len(),
            26,
            "update the permanent decoder regression inventory when adding features"
        );
    }
}
