//! Exact Core feature policy for cross-tool validation. Grammar recognition in
//! audit/diff always remains all-known, independently of this permission set.
use clap::Parser;
use serde_json::json;
use wasmparser::{Validator, WasmFeatures as F};

pub const CORE_FEATURES: &[(&str, F)] = &[
    ("mutable-globals", F::MUTABLE_GLOBAL),
    ("saturating-float-to-int", F::SATURATING_FLOAT_TO_INT),
    ("sign-extension", F::SIGN_EXTENSION),
    ("multi-value", F::MULTI_VALUE),
    ("bulk-memory", F::BULK_MEMORY),
    ("reference-types", F::REFERENCE_TYPES),
    ("simd", F::SIMD),
    ("relaxed-simd", F::RELAXED_SIMD),
    ("tail-call", F::TAIL_CALL),
    ("extended-const", F::EXTENDED_CONST),
    ("function-references", F::FUNCTION_REFERENCES),
    ("gc", F::GC),
    ("multi-memory", F::MULTI_MEMORY),
    ("memory64", F::MEMORY64),
    ("exceptions", F::EXCEPTIONS),
    ("threads", F::THREADS),
    ("wide-arithmetic", F::WIDE_ARITHMETIC),
    ("custom-page-sizes", F::CUSTOM_PAGE_SIZES),
    ("memtag", F::MEMTAG_EXTENSION),
    ("floats", F::FLOATS),
    ("gc-types", F::GC_TYPES),
    ("bulk-memory-opt", F::BULK_MEMORY_OPT),
    ("call-indirect-overlong", F::CALL_INDIRECT_OVERLONG),
    ("legacy-exceptions", F::LEGACY_EXCEPTIONS),
    ("shared-everything-threads", F::SHARED_EVERYTHING_THREADS),
    ("memory-control", F::MEMORY_CONTROL),
    ("stack-switching", F::STACK_SWITCHING),
    ("custom-descriptors", F::CUSTOM_DESCRIPTORS),
    ("compact-imports", F::COMPACT_IMPORTS),
];

pub fn exact(names: &[String]) -> Result<F, String> {
    let mut flags = F::empty();
    for name in names {
        let flag = CORE_FEATURES
            .iter()
            .find(|(n, _)| name == n)
            .ok_or_else(|| format!("unknown Core feature {name:?}"))?
            .1;
        flags |= flag;
    }
    Ok(flags)
}
pub fn validate(bytes: &[u8], features: F) -> Result<(), String> {
    if !bytes.starts_with(b"\0asm\x01\0\0\0") {
        return Err("expected original Core Wasm bytes".into());
    }
    Validator::new_with_features(features)
        .validate_all(bytes)
        .map_err(|e| e.to_string())?;
    validate_memtag_metadata(bytes, features.contains(F::MEMTAG_EXTENSION))?;
    Ok(())
}

fn validate_memtag_metadata(bytes: &[u8], enabled: bool) -> Result<(), String> {
    use wasmparser::{BinaryReader, Parser, Payload, TypeRef};
    let mut memories = Vec::new();
    let mut payload = None;
    for item in Parser::new(0).parse_all(bytes) {
        match item.map_err(|e| e.to_string())? {
            Payload::ImportSection(s) => {
                for i in s.into_imports() {
                    if let TypeRef::Memory(m) = i.map_err(|e| e.to_string())?.ty {
                        memories.push(m);
                    }
                }
            }
            Payload::MemorySection(s) => {
                for m in s {
                    memories.push(m.map_err(|e| e.to_string())?);
                }
            }
            Payload::CustomSection(s) if s.name() == "memtag.globals" => {
                return Err("obsolete memtag.globals metadata".into())
            }
            Payload::CustomSection(s) if s.name() == "uwvm.memtag.linear.globals" => {
                if !enabled {
                    return Err("memtag metadata requires the memtag feature".into());
                }
                if payload.replace(s.data()).is_some() {
                    return Err("duplicate memtag globals metadata".into());
                }
            }
            _ => {}
        }
    }
    let Some(bytes) = payload else {
        return Ok(());
    };
    let mut reader = BinaryReader::new(bytes, 0);
    let checked = (|| -> Result<(), String> {
        let version = reader.read_var_u64().map_err(|e| e.to_string())?;
        if version != 2 {
            return Err("memtag globals requires version 2".into());
        }
        let count = reader.read_var_u64().map_err(|e| e.to_string())?;
        if count > (reader.bytes_remaining() / 3) as u64 {
            return Err("impossible memtag globals descriptor count".into());
        }
        let mut ranges = Vec::new();
        for _ in 0..count {
            let index = reader.read_var_u32().map_err(|e| e.to_string())?;
            let address = reader.read_var_u64().map_err(|e| e.to_string())?;
            let length = reader.read_var_u64().map_err(|e| e.to_string())?;
            let memory = memories
                .get(index as usize)
                .ok_or("memtag memory index out of bounds")?;
            let end = address
                .checked_add(length)
                .ok_or("memtag global range overflow")?;
            let size = (memory.initial as u128) << memory.page_size_log2.unwrap_or(16);
            if end as u128 > size {
                return Err("memtag global exceeds minimum memory size".into());
            }
            if length > 0 {
                ranges.push((index, address / 16, (end - 1) / 16));
            }
        }
        if !reader.eof() {
            return Err("trailing memtag globals bytes".into());
        }
        ranges.sort_unstable();
        if ranges
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0 && pair[0].2 >= pair[1].1)
        {
            return Err("overlapping memtag global granules".into());
        }
        Ok(())
    })();
    checked
}

#[derive(Parser)]
#[command(
    about = "Validate original Core bytes with an exact feature set; audit decoding stays all-known"
)]
struct Args {
    files: Vec<std::path::PathBuf>,
    /// Exact comma-separated features. No baseline is silently added.
    #[arg(long, value_delimiter = ',', conflicts_with = "config")]
    features: Option<Vec<String>>,
    /// Read the exact features array from a wasm-fuzzer configuration.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    #[arg(long)]
    json: bool,
    /// List every Core feature understood by this pinned decoder.
    #[arg(long, conflicts_with_all=["files", "features", "config"])]
    list_features: bool,
}

pub(crate) fn run(arguments: &[String]) -> Result<String, String> {
    let args = match Args::try_parse_from(arguments) {
        Ok(args) => args,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => return Ok(e.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    if args.list_features {
        return serde_json::to_string_pretty(
            &CORE_FEATURES
                .iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| e.to_string());
    }
    if args.files.is_empty() {
        return Err("validate requires at least one file".into());
    }
    let names: Vec<String> = if let Some(config) = args.config {
        let bytes = std::fs::read(config).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        serde_json::from_value(
            value
                .get("features")
                .cloned()
                .ok_or("config requires features")?,
        )
        .map_err(|e| e.to_string())?
    } else {
        args.features
            .unwrap_or_else(|| CORE_FEATURES.iter().map(|(n, _)| (*n).into()).collect())
    };
    let features = exact(&names)?;
    let mut reports = Vec::new();
    for file in args.files {
        let bytes = std::fs::read(&file).map_err(|e| e.to_string())?;
        validate(&bytes, features).map_err(|e| format!("{}: {e}", file.display()))?;
        reports.push(
            json!({"file":file,"bytes":bytes.len(),"validation":"valid","features":names,
            "decoder_boundary":"all-known","validator_input":"original_bytes"}),
        );
    }
    if args.json {
        serde_json::to_string_pretty(&reports).map_err(|e| e.to_string())
    } else {
        Ok(format!(
            "{} original modules valid under the exact feature policy",
            reports.len()
        ))
    }
}
