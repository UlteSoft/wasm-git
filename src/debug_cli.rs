//! Read-only bidirectional symbol/source queries over original module offsets.
use crate::debug_info::{self, DebugInfo};
use clap::Parser;
use serde_json::json;
use std::fmt::Write;

#[derive(Parser)]
struct Args {
    input: String,
    /// Explicit source map, separate DWARF Wasm, or index:name symbol map; repeatable.
    #[arg(long)]
    debug_file: Vec<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    file: Option<String>,
    /// Absolute byte offset in the original Wasm file (decimal).
    #[arg(long)]
    offset: Option<usize>,
    /// Byte offset relative to the code section payload (DWARF PC).
    #[arg(long)]
    code_offset: Option<usize>,
    /// Go runtime virtual PC (decimal), distinct from a Wasm byte offset.
    #[arg(long)]
    go_pc: Option<u64>,
    /// Exact name or linkage name. All candidates are returned.
    #[arg(long)]
    symbol: Option<String>,
    #[arg(long)]
    function: Option<u32>,
    /// Exact recorded source path; no source file is opened.
    #[arg(long)]
    source: Option<String>,
    #[arg(long, requires = "source")]
    line: Option<u64>,
    #[arg(long)]
    json: bool,
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    let args = match Args::try_parse_from(
        std::iter::once("wasm-git debug-map").chain(args.iter().map(String::as_str)),
    ) {
        Ok(args) => args,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => return Ok(e.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    if [
        args.offset.is_some(),
        args.code_offset.is_some(),
        args.go_pc.is_some(),
        args.symbol.is_some(),
        args.function.is_some(),
        args.source.is_some(),
    ]
    .into_iter()
    .filter(|x| *x)
    .count()
        > 1
    {
        return Err(
            "choose one query: --offset, --code-offset, --go-pc, --symbol, --function, or --source [--line]"
                .into(),
        );
    }
    if args.file.is_some() && args.repo.is_none() {
        return Err("--file requires --repo".into());
    }
    let mut inputs =
        crate::read_inputs(&[&args.input], args.repo.as_deref(), args.file.as_deref())?;
    let input = inputs.pop().ok_or("missing module input")?;
    let mut debug = debug_info::read(&input.bytes);
    attach_files(&mut debug, &input.bytes, &args.debug_file)?;
    let offset = args.offset.or(args
        .code_offset
        .map(|n| {
            debug
                .code_payload
                .ok_or("module has no unambiguous code section")?
                .start
                .checked_add(n)
                .ok_or("code offset overflow")
        })
        .transpose()?);
    if let Some(n) = args.code_offset {
        if n >= debug.code_payload.ok_or("module has no code section")?.end
            - debug.code_payload.unwrap().start
        {
            return Err("code offset is outside the code section payload".into());
        }
    }
    let point = if let Some(offset) = offset {
        if offset >= input.bytes.len() {
            return Err("file offset is outside the original module".into());
        }
        Some(debug.lookup(offset))
    } else {
        None
    };
    let summary = debug.summary();
    if let Some(symbol) = &args.symbol {
        debug.names.retain(|n| n.name == *symbol);
        debug
            .symbols
            .retain(|s| s.name.as_ref() == Some(symbol) || s.linkage_name.as_ref() == Some(symbol));
        debug.lines.retain(|l| {
            debug.symbols.iter().any(|s| {
                s.ranges
                    .iter()
                    .any(|r| intersects(r.file_span, l.range.file_span))
            })
        });
        debug.go_lines.retain(|l| {
            debug
                .symbols
                .iter()
                .any(|s| s.function_index == Some(l.function_index))
        });
    } else if let Some(index) = args.function {
        debug.names.retain(|n| {
            (n.index_space == "function" && n.index == index)
                || (matches!(n.index_space, "local" | "label") && n.scope_index == Some(index))
        });
        debug.symbols.retain(|s| s.function_index == Some(index));
        debug.lines.retain(|l| l.range.function_index == index);
        debug.go_lines.retain(|l| l.function_index == index);
    } else if let Some(pc) = args.go_pc {
        debug
            .go_lines
            .retain(|l| l.virtual_pc[0] <= pc && pc < l.virtual_pc[1]);
        debug.symbols.retain(|s| {
            s.origin == "go_pclntab"
                && debug
                    .go_lines
                    .iter()
                    .any(|l| Some(l.function_index) == s.function_index)
        });
        debug.lines.clear();
        debug.names.clear();
    } else if let Some(source) = &args.source {
        let matches = |location: &crate::debug_info::SourceLocation| {
            location.file.as_deref() == Some(source.as_str())
                && args.line.is_none_or(|line| location.line == Some(line))
        };
        let calls: Vec<_> = debug
            .symbols
            .iter()
            .filter(|s| s.call_site.as_ref().is_some_and(matches))
            .flat_map(|s| s.ranges.iter().map(|r| r.file_span))
            .collect();
        debug.go_lines.retain(|l| matches(&l.location));
        debug.lines.retain(|l| {
            matches(&l.location) || calls.iter().any(|r| intersects(*r, l.range.file_span))
        });
        debug.symbols.retain(|s| {
            debug
                .go_lines
                .iter()
                .any(|l| Some(l.function_index) == s.function_index)
                || s.declaration.as_ref().is_some_and(matches)
                || s.ranges.iter().any(|r| {
                    debug
                        .lines
                        .iter()
                        .any(|l| intersects(r.file_span, l.range.file_span))
                })
        });
    }
    if args.json {
        let output = if let Some(point) = point {
            json!({"schema_version":1,"input":input.origin,"debug_summary":summary,"annotation":point})
        } else {
            json!({"schema_version":1,"input":input.origin,"debug_summary":summary,"debug_info":debug})
        };
        serde_json::to_string_pretty(&output).map_err(|e| e.to_string())
    } else {
        let mut out = format!("Debug mapping: {} (advisory compiler metadata; original Wasm offsets)\nmodule sha256={}\n", summary.status, summary.module_sha256.as_deref().unwrap_or("unavailable"));
        if let Some(point) = point {
            let _ = writeln!(
                out,
                "file offset={} code offset={:?} function={:?} instruction={:?}",
                point.file_offset, point.code_offset, point.function_index, point.instruction_span
            );
            debug.symbols = point.symbols;
            debug.lines = point.sources;
        }
        render(&debug, &mut out);
        Ok(out)
    }
}
fn intersects(a: crate::audit::Span, b: crate::audit::Span) -> bool {
    a.start < b.end && b.start < a.end
}
fn render(debug: &DebugInfo, out: &mut String) {
    for line in &debug.go_lines {
        let _ = writeln!(out, "Go virtual PC [{}..{}) function={} source {:?}:{:?} (resume-point coordinates; no instruction offset inferred)", line.virtual_pc[0], line.virtual_pc[1], line.function_index, line.location.file.as_deref().unwrap_or("<unknown>"), line.location.line);
    }
    for unit in &debug.units {
        let _ = writeln!(
            out,
            "unit {:?} DWARF{} language={:?} ({:?}) producer={:?} metadata@{} input={:?}",
            unit.name,
            unit.dwarf_version,
            unit.language,
            unit.language_code,
            unit.producer,
            unit.metadata_offset,
            unit.metadata_source
        );
    }
    for name in &debug.names {
        let _ = writeln!(
            out,
            "name {} {:?}/{} {:?} metadata@{}",
            name.index_space, name.scope_index, name.index, name.name, name.metadata_offset
        );
    }
    for s in &debug.symbols {
        let _ = writeln!(out, "{} {:?} linkage={:?} function={:?} origin={} metadata@{:?} declaration={:?} inline_call={:?}", s.kind, s.name, s.linkage_name, s.function_index, s.origin, s.metadata_offset, s.declaration, s.call_site);
        for r in &s.ranges {
            let _ = writeln!(
                out,
                "  file [{}..{}) code [{}..{}) {}",
                r.file_span.start,
                r.file_span.end,
                r.code_span.start,
                r.code_span.end,
                r.start_boundary
            );
        }
        for location in &s.locations {
            let _ = writeln!(
                out,
                "  {} availability={} declared_code_range={:?} range_interpretation={} slots={:?} operations={:?} complete={} evaluation={}",
                location.attribute,
                location.availability,
                location.declared_code_range,
                location.range_interpretation,
                location.wasm_slots,
                location.operations,
                location.complete,
                location.evaluation
            );
        }
    }
    for line in &debug.lines {
        let _ = writeln!(
            out,
            "source {:?}:{:?}:{:?} file [{}..{}) function={} discriminator={}",
            line.location.file.as_deref().unwrap_or("<unknown>"),
            line.location.line,
            line.location.column,
            line.range.file_span.start,
            line.range.file_span.end,
            line.range.function_index,
            line.discriminator
        );
    }
    for diagnostic in &debug.diagnostics {
        let _ = writeln!(out, "note: {}", diagnostic.escape_debug());
    }
}

/// Only explicitly selected files are read. Malformed conventions are visible
/// diagnostics and cannot disable the authoritative original-byte audit.
pub(crate) fn attach_files(
    debug: &mut DebugInfo,
    wasm: &[u8],
    paths: &[String],
) -> Result<(), String> {
    use std::io::Read;
    if paths.len() > 32 {
        return Err("at most 32 explicit debug inputs are allowed".into());
    }
    let mut remaining = 128 * 1024 * 1024;
    for path in paths {
        let file = std::fs::File::open(path).map_err(|e| format!("debug input {path:?}: {e}"))?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("debug input {path:?} is not a regular file"));
        }
        let mut data = Vec::new();
        file.take(remaining as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| format!("debug input {path:?}: {e}"))?;
        if data.len() > remaining {
            return Err("explicit debug inputs exceed the aggregate 128 MiB budget".into());
        }
        remaining -= data.len();
        if let Err(e) = debug.attach_debug_file(wasm, &data, path) {
            debug.note(format!(
                "explicit debug input {path:?}: {e}; no mappings inferred from this input"
            ));
        }
    }
    Ok(())
}

pub(crate) fn merge_module(module: &mut crate::ir::ModuleIr, extra: DebugInfo) {
    if extra.inputs.is_empty() && extra.diagnostics.is_empty() {
        return;
    }
    let debug = std::sync::Arc::make_mut(&mut module.debug_info);
    debug.merge_external(extra);
    for (index, name) in debug.function_hints() {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            module.function_names.entry(index)
        {
            entry.insert(name);
            module.function_symbol_sources.insert(index, "debug_symbol");
        }
    }
}
