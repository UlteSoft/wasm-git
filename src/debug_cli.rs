//! Read-only bidirectional symbol/source queries over original module offsets.
use crate::debug_info::{self, DebugInfo};
use clap::Parser;
use serde_json::json;
use std::fmt::Write;

#[derive(Parser)]
struct Args {
    input: String,
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
            "choose one query: --offset, --code-offset, --symbol, --function, or --source [--line]"
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
    } else if let Some(index) = args.function {
        debug.symbols.retain(|s| s.function_index == Some(index));
        debug.lines.retain(|l| l.range.function_index == index);
    } else if let Some(source) = &args.source {
        let matches = |location: &crate::debug_info::SourceLocation| {
            location.file.as_ref() == source
                && args.line.is_none_or(|line| location.line == Some(line))
        };
        let calls: Vec<_> = debug
            .symbols
            .iter()
            .filter(|s| s.call_site.as_ref().is_some_and(matches))
            .flat_map(|s| s.ranges.iter().map(|r| r.file_span))
            .collect();
        debug.lines.retain(|l| {
            matches(&l.location) || calls.iter().any(|r| intersects(*r, l.range.file_span))
        });
        debug.symbols.retain(|s| {
            s.ranges.iter().any(|r| {
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
    for s in &debug.symbols {
        let _ = writeln!(out, "{} {:?} linkage={:?} function={:?} origin={} metadata@{} declaration={:?} inline_call={:?}", s.kind, s.name, s.linkage_name, s.function_index, s.origin, s.metadata_offset, s.declaration, s.call_site);
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
            line.location.file,
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
