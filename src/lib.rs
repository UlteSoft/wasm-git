//! Shared native Wasm engine for the client and server executables.
extern crate self as wasm_git;
// Parsing always starts from a complete in-memory input and addressable slice
// origins. The upstream u64 cursor is therefore representable as usize here;
// keep this conversion explicit instead of silently truncating on 32-bit hosts.
pub(crate) fn input_offset(offset: u64) -> usize {
    usize::try_from(offset).expect("parser offset belongs to an addressable input slice")
}

pub(crate) fn input_range(range: std::ops::Range<u64>) -> std::ops::Range<usize> {
    input_offset(range.start)..input_offset(range.end)
}

pub mod analysis;
mod archive_commands;
pub mod audit;
pub mod binary;
mod bisect_commands;
pub mod blame;
mod checkout;
mod command_args;
mod command_output;
mod core_view;
mod debug_cli;
pub mod debug_info;
mod decode_error;
mod describe_commands;
mod diff;
mod diff_plumbing;
mod export_dependencies;
pub mod feature_set;
pub mod features;
mod fingerprint;
pub mod grammar;
mod history_commands;
mod history_query_commands;
pub mod ir;
mod maintenance_commands;
mod matcher;
pub mod merge;
mod merge_base_commands;
mod normalize;
mod notes_commands;
mod object_batch;
mod operation_commands;
mod operator_fields;
mod output;
pub mod parallel;
pub mod parse;
pub mod patch;
pub mod pathspec;
mod platform_fs;
mod plumbing_commands;
mod porcelain;
pub mod protocol;
pub mod pull;
mod query_commands;
mod rebase_commands;
mod ref_commands;
mod remote;
mod replay_commands;
mod report;
pub mod repository;
mod resolve;
#[cfg(feature = "server")]
mod server;
pub mod standard;
mod structured_diff;
pub mod trace;
mod tree_output;
mod tree_resolution;
mod value_guard;
mod worktree_commands;

use clap::{Parser, ValueEnum};
use serde_json::{json, Value};
use std::path::Path;
use wasm_git::standard::Standard;

#[derive(Debug, Parser)]
struct InspectArgs {
    filepath: String,
    /// Explicit compiler sidecar; repeatable. Never follows embedded URLs.
    #[arg(long)]
    debug_file: Vec<String>,
    #[arg(long)]
    json: bool,
    /// Read a committed revision instead of a filesystem path.
    #[arg(long)]
    repo: Option<String>,
    /// Select one path in a committed file tree.
    #[arg(long, requires = "repo")]
    file: Option<String>,
    /// Audit against core1, core2, core3, or every decoder-known proposal.
    #[arg(long, default_value = "all-known")]
    standard: Standard,
}

#[derive(Debug, Parser)]
struct DiffArgs {
    old_path: String,
    new_path: String,
    #[arg(long, value_enum, default_value_t = DiffFormat::Text)]
    format: DiffFormat,
    #[arg(long)]
    short: bool,
    /// Show changed functions and exact byte edits without full region trees.
    #[arg(long, conflicts_with = "short")]
    compact: bool,
    /// Focus on sections; repeat for a union: code, custom:NAME, id:N, ordinal:N, all.
    #[arg(long = "section")]
    sections: Vec<audit::section::Selector>,
    /// Interpret OLD and NEW as repository revisions.
    #[arg(long)]
    repo: Option<String>,
    #[arg(long, requires = "repo")]
    file: Option<String>,
    /// Select allowed features without changing instruction boundary decoding.
    #[arg(long, default_value = "all-known")]
    standard: Standard,
    /// Source map, DWARF Wasm, or symbol map for the old module; repeatable.
    #[arg(long, conflicts_with = "sections")]
    old_debug_file: Vec<String>,
    /// Explicit compiler sidecar for the new module; repeatable.
    #[arg(long, conflicts_with = "sections")]
    new_debug_file: Vec<String>,
}

#[derive(Debug, Parser)]
struct WorktreeDiffArgs {
    repo_dir: String,
    #[arg(long)]
    cached: bool,
    #[arg(long)]
    file: Option<String>,
    #[arg(long, value_enum, default_value_t = DiffFormat::Text)]
    format: DiffFormat,
    #[arg(long)]
    short: bool,
    /// Show changed functions and exact byte edits without full region trees.
    #[arg(long, conflicts_with = "short")]
    compact: bool,
    /// Focus on sections; repeat for a union.
    #[arg(long = "section")]
    sections: Vec<audit::section::Selector>,
    #[arg(long, default_value = "all-known")]
    standard: Standard,
    /// Source map, DWARF Wasm, or symbol map for the old module; repeatable.
    #[arg(long, conflicts_with = "sections")]
    old_debug_file: Vec<String>,
    /// Explicit compiler sidecar for the new module; repeatable.
    #[arg(long, conflicts_with = "sections")]
    new_debug_file: Vec<String>,
}

#[derive(Debug, Parser)]
struct PatchArgs {
    old_path: String,
    new_path: String,
    /// Create a new patch file; existing destinations are never replaced.
    #[arg(long)]
    output: String,
    /// Interpret OLD and NEW as revisions in one repository snapshot.
    #[arg(long)]
    repo: Option<String>,
    #[arg(long, requires = "repo")]
    file: Option<String>,
}

#[derive(Debug, Parser)]
struct ApplyArgs {
    old_path: String,
    patch_path: String,
    /// Publish verified bytes to a new file.
    #[arg(long, required_unless_present = "check", conflicts_with = "check")]
    output: Option<String>,
    /// Verify and audit the result without writing a file.
    #[arg(long)]
    check: bool,
    /// Interpret OLD as a repository revision; PATCH is always a file.
    #[arg(long)]
    repo: Option<String>,
    #[arg(long, requires = "repo")]
    file: Option<String>,
    /// Audit the verified result against the selected Core standard.
    #[arg(long, default_value = "all-known")]
    standard: Standard,
}

#[derive(Debug, Parser)]
struct TraceArgs {
    repo_dir: String,
    revision: String,
    /// Trace this exact file path; renames are separate delete/add events.
    #[arg(long)]
    file: Option<String>,
    /// Nonempty half-open byte range in the selected revision: START:END.
    #[arg(long, required_unless_present = "path", conflicts_with = "path")]
    range: Option<String>,
    /// Exact audit section/region path in the selected revision, or module.
    #[arg(long)]
    path: Option<String>,
    /// Global parent-edge processing budget; zero returns explicit boundaries.
    #[arg(long, default_value_t = 256)]
    max_edges: usize,
    /// Global input-byte comparison budget; snapshot loading is separate.
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    max_bytes: usize,
}

#[derive(Debug, Parser)]
struct BlameArgs {
    repo_dir: String,
    revision: String,
    #[arg(long)]
    file: Option<String>,
    #[arg(long, conflicts_with = "path")]
    range: Option<String>,
    #[arg(long)]
    path: Option<String>,
    #[arg(long)]
    first_parent: bool,
    #[arg(long, default_value_t = 256)]
    max_edges: usize,
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    max_bytes: usize,
}

#[derive(clap::Parser)]
struct SquashArgs {
    repo_dir: String,
    target: String,
    #[arg(long, required = true)]
    squash: bool,
    #[arg(short = 'm', long = "message", allow_hyphen_values = true)]
    message: Option<String>,
}

struct Input {
    bytes: Vec<u8>,
    origin: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DiffFormat {
    Text,
    Json,
}

fn cli_error(error: &str) {
    use std::io::Write;
    // Filenames, server responses and custom names can contain terminal control
    // sequences. Keep diagnostics printable even when stderr itself is closed.
    let _ = writeln!(
        std::io::stderr().lock(),
        "error: {}",
        report::safe_text(error)
    );
}

fn cli_args() -> Vec<String> {
    std::env::args_os()
        .enumerate()
        .map(|(index, value)| {
            value.into_string().unwrap_or_else(|_| {
                cli_error(&format!("argument {index} is not valid UTF-8"));
                std::process::exit(1);
            })
        })
        .collect()
}

fn cli_stdout(output: &str, newline: bool) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let result = stdout.write_all(output.as_bytes()).and_then(|()| {
        if newline {
            stdout.write_all(b"\n")?;
        }
        stdout.flush()
    });
    match result {
        Ok(()) => (),
        // A closed consumer is expected (for example `... | head`). The caller
        // still determines the command's exit status, including conflicts.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => (),
        Err(error) => {
            cli_error(&format!("write command output: {error}"));
            std::process::exit(1);
        }
    }
}

pub fn client_main() {
    let mut args = cli_args();
    let mut format = command_output::Format::default_output();
    let mut format_seen = false;
    while args.get(1).is_some_and(|arg| {
        arg == "--threads"
            || arg.starts_with("--threads=")
            || arg == "--format"
            || arg.starts_with("--format=")
    }) {
        let option = args.remove(1);
        let value = if let Some((_, value)) = option.split_once('=') {
            value.to_owned()
        } else if args.len() > 1 {
            args.remove(1)
        } else {
            String::new()
        };
        let result = if option.starts_with("--format") {
            if format_seen {
                Err("duplicate global --format".to_owned())
            } else {
                format_seen = true;
                command_output::Format::parse(&value).map(|selected| format = selected)
            }
        } else {
            value
                .parse::<usize>()
                .map_err(|_| "--threads requires an integer".to_owned())
                .and_then(parallel::configure)
        };
        if let Err(error) = result {
            format.emit("", "", 1, Some(&error), false);
            std::process::exit(1);
        }
    }
    if args.len() < 2 {
        format.emit("", "", 1, Some(&usage_text()), false);
        std::process::exit(1);
    }
    // A global machine format requests structured command data where the
    // command has a JSON view. Explicit command-local projections still win.
    if format.machine() && !command_args::help_requested(&args[1..]) {
        let command = args[1].as_str();
        let has_projection = args[2..].iter().any(|a| {
            matches!(
                a.as_str(),
                "--json"
                    | "--format"
                    | "-z"
                    | "--name-only"
                    | "--object-only"
                    | "--quiet"
                    | "-q"
                    | "--hash"
            ) || a.starts_with("--format=")
        });
        if !has_projection && command == "diff" {
            // Append before pathspecs so no filename becomes an option.
            let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
            args.splice(at..at, ["--format".to_owned(), "json".to_owned()]);
        } else if !has_projection
            && matches!(
                command,
                "features" | "ls-tree" | "show-ref" | "for-each-ref"
            )
        {
            let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
            args.insert(at, "--json".to_owned());
        }
    }
    // Batch objects and running servers own stdout as a stream protocol. Reject
    // envelopes before reading stdin, opening a listener or changing state.
    if format.machine()
        && ((args[1] == "cat-file" && object_batch::requested(&args[1..]))
            || matches!(args[1].as_str(), "server" | "serve"))
    {
        format.emit(&args[1], "", 1, Some("streaming commands require --format native; their stdout has its own binary/readiness protocol"), false);
        std::process::exit(1);
    }
    if args[1] == "cat-file" && object_batch::requested(&args[1..]) {
        if let Err(error) = object_batch::run(&args[1..]) {
            cli_error(&error);
            std::process::exit(1);
        }
        return;
    }
    if query_commands::handles(&args[1])
        || query_commands::branch_listing_requested(&args[1..])
        || plumbing_commands::handles(&args[1])
        || porcelain::index_command_handles(&args[1])
        || maintenance_commands::handles(&args[1])
        || args[1] == "worktree"
        || args[1] == "notes"
        || history_query_commands::handles(&args[1])
        || ref_commands::handles(&args[1])
        || diff_plumbing::handles(&args[1])
        || args[1] == "merge-base"
        || args[1] == "bisect"
    {
        let output = if porcelain::index_command_handles(&args[1]) {
            porcelain::index_command(&args[1..])
        } else if args[1] == "merge-base" {
            merge_base_commands::run(&args[1..])
        } else if diff_plumbing::handles(&args[1]) {
            diff_plumbing::run(&args[1..])
        } else if ref_commands::handles(&args[1]) {
            ref_commands::run(&args[1..])
        } else if history_query_commands::handles(&args[1]) {
            history_query_commands::run(&args[1..])
        } else if args[1] == "notes" {
            notes_commands::run(&args[1..])
        } else if args[1] == "worktree" {
            worktree_commands::run(&args[1..])
        } else if maintenance_commands::handles(&args[1]) {
            maintenance_commands::run(&args[1..])
        } else if plumbing_commands::handles(&args[1]) {
            plumbing_commands::run(&args[1..])
        } else if args[1] == "bisect" {
            bisect_commands::bisect_query(&args[1..])
        } else {
            query_commands::run(&args[1..])
        };
        match output {
            Ok(output) => {
                format.emit(&args[1], &output.stdout, output.exit_code, None, false);
                if output.exit_code != 0 {
                    std::process::exit(output.exit_code);
                }
            }
            Err(error) => {
                let code = if args[1] == "merge-base" { 2 } else { 1 };
                format.emit(&args[1], "", code, Some(&error), false);
                std::process::exit(code);
            }
        }
        return;
    }
    let result = match args[1].as_str() {
        "version" | "--version" | "-v" if args.len() == 2 => Ok(format!("wasm-git {}", env!("CARGO_PKG_VERSION"))),
        "version" if args.len() == 3 && args[2] == "--build-options" => serde_json::to_string_pretty(&json!({"version":env!("CARGO_PKG_VERSION"),"backend":"native_wasm_git","original_wasm_bytes":true,"pointer_width":usize::BITS,"object_hash":"sha256","client":cfg!(feature="client"),"server":cfg!(feature="server"),"host_os":if cfg!(target_os="wasi") { "wasi" } else { std::env::consts::OS },"host_arch":std::env::consts::ARCH,"host_threads":!cfg!(target_family="wasm"),"host_processes":!cfg!(target_family="wasm"),"host_network":!cfg!(target_family="wasm"),"directory_fsync":cfg!(unix)})).map_err(|error|error.to_string()),
        "pool" => remote::pool_cli(&args[1..]),
        "features" => features::run(&args[1..]),
        "validate" => feature_set::run(&args[1..]),
        "pr" => remote::pull_cli(&args[2..]),
        "clone" | "fetch" if remote::selective_requested(&args[1..]) => remote::run_selective(&args[1..]),
        "promisor" | "hydrate" | "clone-status" => remote::run_selective(&args[1..]),
        "keygen" | "trust-author" | "untrust-author" | "commit-signed" | "sign-commit" | "verify-commit" => repository::signing_cli(&args[1..]),
        "inspect" => cmd_inspect(&args[2..]),
        "debug-map" => debug_cli::run(&args[2..]),
        "diff" => cmd_diff(&args[2..]),
        "patch" => cmd_patch(&args[2..]),
        "apply" => cmd_apply(&args[2..]),
        "trace" => cmd_trace(&args[2..]),
        "blame" => cmd_blame(&args[2..]),
        "merge" if !command_args::option_indices(&args[1..],"--squash").is_empty() => {
            match SquashArgs::try_parse_from(&args[1..]) {
                Ok(parsed)=>porcelain::squash_merge(&parsed.repo_dir,&parsed.target,parsed.message.as_deref()),
                Err(error) if error.kind()==clap::error::ErrorKind::DisplayHelp=>Ok(error.to_string()),
                Err(error)=>Err(error.to_string()),
            }
        }
        "checkout" => checkout::run(&args[1..]),
        "show"
            if args
                .iter()
                .any(|arg| arg == "--file" || arg.starts_with("--file=")) =>
        {
            tree_output::export_file(&args[1..])
        }
        #[cfg(feature = "server")]
        "server" | "serve" => server::run(&args[1..]),
        #[cfg(not(feature = "server"))]
        "server" | "serve" => Err("server support is not compiled; build --features server or use wasm-git-server".into()),
        "pull" if args.len() >= 3 && !command_args::help_requested(&args[1..]) => {
            porcelain::with_clean_history(&args[2], |_| remote::run(&args[1..]))
        }
        "remote" | "fetch" | "pull" | "push" => remote::run(&args[1..]),
        "clone" | "fork" if args.get(2).is_some_and(|url| remote::is_remote_url(url)) => {
            remote::run(&args[1..])
        }
        "cherry-pick" | "revert" => replay_commands::run(&args[1..]),
        "merge" | "rebase"
            if operation_commands::requested(&args[1..]) =>
        {
            operation_commands::run(&args[1..])
        }
        "rebase" => history_commands::run(&args[1..]),
        "status" | "add" | "rm" | "mv" | "restore" | "reset" | "stash" | "config" | "tag"
        | "reflog" | "rev-parse" | "cat-file" | "ls-files" => porcelain::run(&args[1..]),
        "commit" if args.get(3).is_some_and(|arg| arg.starts_with('-')) => {
            porcelain::run(&args[1..])
        }
        "branch" | "switch" => porcelain::run(&args[1..]),
        "commit" if args.len() >= 3 && !command_args::help_requested(&args[1..]) => {
            porcelain::with_no_pending(&args[2], |_| repository::run(&args[1..]))
        }
        "merge" if tree_resolution::requested(&args[1..]) => tree_resolution::run(&args[2..]),
        "merge" if args.len() >= 3 && !command_args::help_requested(&args[1..]) => {
            porcelain::with_clean_history(&args[2], |_| repository::run(&args[1..]))
        }
        "init" | "commit" | "log" | "show" | "clone" | "fork" | "bundle" | "unbundle" | "fsck" | "merge" => repository::run(&args[1..]),
        "help" | "--help" | "-h" => Ok(usage_text()),
        other => Err(if format.machine() { format!("unknown command: {other}") } else { format!("unknown command: {other}\n{}", usage_text()) }),
    };
    match result {
        Ok(output) if !output.is_empty() => {
            let conflict = matches!(
                args[1].as_str(),
                "merge" | "pull" | "cherry-pick" | "revert" | "rebase"
            ) && serde_json::from_str::<Value>(&output)
                .is_ok_and(|report| report["status"] == "conflict" || report["status"] == "empty");
            let rejected = args[1] == "push"
                && serde_json::from_str::<Value>(&output).is_ok_and(|report| {
                    matches!(
                        report["status"].as_str(),
                        Some("partial" | "rejected" | "unknown")
                    )
                });
            let code = if conflict { 2 } else { i32::from(rejected) };
            format.emit(&args[1], &output, code, None, true);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Ok(_) => format.emit(&args[1], "", 0, None, false),
        Err(error) => {
            format.emit(&args[1], "", 1, Some(&error), false);
            std::process::exit(1);
        }
    }
}

fn cmd_inspect(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return Err("usage: wasm-git inspect <FILE|REVISION> [--repo DIR] [--json]".into());
    }
    let parsed = match InspectArgs::try_parse_from(
        std::iter::once("wasm-git inspect").chain(args.iter().map(String::as_str)),
    ) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string());
        }
        Err(error) => return Err(error.to_string()),
    };
    if let Some(root) = &parsed.repo {
        let revisions = repository::read_selected_file_revisions(
            Path::new(root),
            &[&parsed.filepath],
            parsed.file.as_deref(),
        )?;
        let revision = revisions
            .into_iter()
            .next()
            .ok_or("missing revision snapshot")?;
        if parsed.file.is_none() && revision.legacy.is_none() {
            if !parsed.debug_file.is_empty() {
                return Err("--debug-file on a file tree requires --file".into());
            }
            return tree_output::inspect_tree(&parsed.filepath, revision, parsed.standard);
        }
        let input = tree_output::select_input(&parsed.filepath, revision, parsed.file.as_deref())?;
        return serde_json::to_string_pretty(&inspect_input_with_debug(
            &input,
            parsed.standard,
            &parsed.debug_file,
        )?)
        .map_err(|e| e.to_string());
    }
    let inputs = read_inputs(&[&parsed.filepath], None, None)?;
    let input = &inputs[0];
    serde_json::to_string_pretty(&inspect_input_with_debug(
        input,
        parsed.standard,
        &parsed.debug_file,
    )?)
    .map_err(|e| e.to_string())
}

fn inspect_input(input: &Input, standard: Standard) -> Result<Value, String> {
    inspect_input_with_debug(input, standard, &[])
}

fn inspect_input_with_debug(
    input: &Input,
    standard: Standard,
    paths: &[String],
) -> Result<Value, String> {
    let audit = audit::inspect_with_standard(&input.bytes, standard)
        .map_err(|error| format!("wasm parse error: {error}"))?;
    let mut extra = debug_info::DebugInfo::default();
    debug_cli::attach_files(&mut extra, &input.bytes, paths)?;
    let (mut output, status) = match normalize_bytes_with_debug_names(&input.bytes, &audit, true) {
        Ok(mut module) => {
            debug_cli::merge_module(&mut module.module, extra);
            (report::inspect_value(&module), json!({"available": true}))
        }
        Err(error) => {
            let mut debug = debug_info::read(&input.bytes);
            debug.merge_external(extra);
            (
                json!({"module": null, "summary": null, "debug_info": debug}),
                json!({"available": false, "reason": error}),
            )
        }
    };
    output["schema_version"] = json!(1);
    output["function_analysis"] = analysis_scope(status, standard);
    output["input"] = input.origin.clone();
    output["audit"] = serde_json::to_value(audit).map_err(|error| error.to_string())?;
    Ok(output)
}

fn cmd_diff(args: &[String]) -> Result<String, String> {
    if args
        .first()
        .is_some_and(|root| Path::new(root).join(".wasm-git").is_dir())
        && (args.len() == 1 || args.get(1).is_some_and(|arg| arg.starts_with('-')))
    {
        let parsed = match WorktreeDiffArgs::try_parse_from(
            std::iter::once("wasm-git diff DIR").chain(args.iter().map(String::as_str)),
        ) {
            Ok(parsed) => parsed,
            Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
                return Ok(error.to_string())
            }
            Err(error) => return Err(error.to_string()),
        };
        let inputs = porcelain::diff_snapshot(&parsed.repo_dir, parsed.cached)?;
        let labels = DiffArgs {
            old_path: if parsed.cached { "HEAD" } else { "INDEX" }.into(),
            new_path: if parsed.cached { "INDEX" } else { "WORKTREE" }.into(),
            format: parsed.format,
            short: parsed.short,
            compact: parsed.compact,
            sections: parsed.sections,
            old_debug_file: parsed.old_debug_file,
            new_debug_file: parsed.new_debug_file,
            repo: None,
            file: parsed.file,
            standard: parsed.standard,
        };
        return tree_output::render_files(
            &labels,
            inputs,
            json!({"kind":if parsed.cached{"commit"}else{"index"}}),
            json!({"kind":if parsed.cached{"index"}else{"worktree"}}),
        );
    }
    if args.len() < 2
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return Err("usage: wasm-git diff <OLD> <NEW> [--repo DIR] [--format text|json]".into());
    }
    let parsed = match DiffArgs::try_parse_from(
        std::iter::once("wasm-git diff").chain(args.iter().map(String::as_str)),
    ) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string());
        }
        Err(error) => return Err(error.to_string()),
    };
    if let Some(root) = &parsed.repo {
        let revisions = repository::read_selected_file_revisions(
            Path::new(root),
            &[&parsed.old_path, &parsed.new_path],
            parsed.file.as_deref(),
        )?;
        if parsed.file.is_some() || revisions.iter().any(|r| r.legacy.is_none()) {
            return tree_output::diff_revisions(&parsed, revisions);
        }
    }
    let inputs = read_inputs(
        &[&parsed.old_path, &parsed.new_path],
        parsed.repo.as_deref(),
        parsed.file.as_deref(),
    )?;
    render_diff(&parsed, &inputs)
}

fn render_diff(parsed: &DiffArgs, inputs: &[Input]) -> Result<String, String> {
    let old = &inputs[0].bytes;
    let new = &inputs[1].bytes;
    if !parsed.sections.is_empty() {
        let mut report = audit::section::compare(old, new, parsed.standard, &parsed.sections)?;
        if parsed.format == DiffFormat::Text || parsed.short {
            return Ok(report.to_text(parsed.short));
        }
        let omitted = parsed.compact.then(|| {
            [&mut report.old, &mut report.new].map(|module| {
                module.sections.clear();
                let before = module.standard.evidence.len();
                module.standard.omit_supported_evidence();
                before - module.standard.evidence.len()
            })
        });
        let mut value = serde_json::to_value(report).map_err(|e| e.to_string())?;
        value["schema_version"] = json!(1);
        value["inputs"] = json!({"old":inputs[0].origin,"new":inputs[1].origin});
        value["region_trees_omitted"] = json!(parsed.compact);
        if let Some([old, new]) = omitted {
            value["supported_evidence_omitted"] = json!({"old":old,"new":new});
        }
        return serde_json::to_string_pretty(&value).map_err(|e| e.to_string());
    }
    let bindings = if parsed.repo.is_some() {
        format!(
            "Resolved repository inputs: old={}, new={}\n",
            inputs[0].origin["commit"].as_str().unwrap(),
            inputs[1].origin["commit"].as_str().unwrap()
        )
    } else {
        String::new()
    };
    let audit = audit::compare_with_standard(old, new, parsed.standard)
        .map_err(|error| format!("wasm parse error: {error}"))?;
    // Read explicit inputs before normalization so I/O errors never disappear
    // inside the optional semantic comparison.
    let mut old_debug = debug_info::DebugInfo::default();
    let mut new_debug = debug_info::DebugInfo::default();
    debug_cli::attach_files(&mut old_debug, old, &parsed.old_debug_file)?;
    debug_cli::attach_files(&mut new_debug, new, &parsed.new_debug_file)?;
    let external_context = [old_debug.summary(), new_debug.summary()];
    let comparison = normalize_bytes(old, &audit.old).and_then(|mut old_module| {
        normalize_bytes(new, &audit.new).map(|mut new_module| {
            debug_cli::merge_module(&mut old_module.module, old_debug);
            debug_cli::merge_module(&mut new_module.module, new_debug);
            diff::diff_modules(&parsed.old_path, &old_module, &parsed.new_path, &new_module)
        })
    });
    if parsed.short {
        let mut deprecations = String::new();
        audit
            .old
            .standard
            .deprecations_text(&mut deprecations, "old ");
        audit
            .new
            .standard
            .deprecations_text(&mut deprecations, "new ");
        let functions = match &comparison {
            Ok(report) => report::diff_short(report),
            Err(error) => format!("Function comparison unavailable: {error}"),
        };
        return Ok(format!(
            "{bindings}{functions}\n{custom}{deprecations}Binary audit: {} ({} edit groups, exact_minimal={})\nDiagnostics: old={}, new={}\nSelected standard: {}; validation: old={}, new={} (function interpretation decodes all known proposals)",
            if audit.changed { "changed" } else { "identical" },
            audit.edits.len(), audit.exact_minimal,
            audit.old.diagnostics.len(), audit.new.diagnostics.len(),
            parsed.standard, audit.old.standard.validation, audit.new.standard.validation, custom = audit.custom_short()
        ));
    }
    match parsed.format {
        DiffFormat::Json => {
            let (mut output, status) = match comparison {
                Ok(report) => (
                    serde_json::to_value(&report).unwrap(),
                    json!({"available": true}),
                ),
                Err(error) => (
                    json!({"old": parsed.old_path, "new": parsed.new_path,
                        "summary": null, "function_matches": [], "changes": [], "debug_context": external_context,
                        "call_binding_notes": [], "export_binding_notes": [],
                        "export_dependency_graph": null}),
                    json!({"available": false, "reason": error}),
                ),
            };
            output["schema_version"] = json!(1);
            output["binary_changed"] = json!(audit.changed);
            output["function_analysis"] = analysis_scope(status, parsed.standard);
            output["inputs"] = json!({"old": &inputs[0].origin, "new": &inputs[1].origin});
            // Function analysis is complete. Omit presentation-only trees before
            // constructing JSON Values, rather than allocating then dropping
            // every region/evidence map. Exact edits and diagnostics stay intact.
            let mut audit = audit;
            let omitted = parsed.compact.then(|| {
                let positional = audit.semantic_changes.len();
                audit.semantic_changes.clear();
                let evidence = [&mut audit.old, &mut audit.new].map(|module| {
                    module.sections.clear();
                    let before = module.standard.evidence.len();
                    module.standard.omit_supported_evidence();
                    before - module.standard.evidence.len()
                });
                (positional, evidence)
            });
            let mut audit = serde_json::to_value(audit).map_err(|error| error.to_string())?;
            if let Some((positional, evidence)) = omitted {
                for (side, count) in ["old", "new"].into_iter().zip(evidence) {
                    audit[side].as_object_mut().unwrap().remove("sections");
                    audit[side]["standard"]["supported_evidence_omitted"] = json!(count);
                }
                audit.as_object_mut().unwrap().remove("semantic_changes");
                audit["view"] = json!({"kind":"compact_lossless_edits","region_trees_omitted":true,"positional_changes_omitted":positional});
            }
            output["audit"] = audit;
            serde_json::to_string_pretty(&output).map_err(|error| error.to_string())
        }
        DiffFormat::Text => {
            let functions = match comparison {
                Ok(report) if parsed.compact => report::diff_compact(&report),
                Ok(report) => report::diff_text(&report),
                Err(error) => format!("Function comparison unavailable: {error}"),
            };
            if parsed.compact {
                let mut raw = format!(
                    "Binary audit: {} ({} exact byte-edit groups; exact_minimal={})\n",
                    if audit.changed {
                        "changed"
                    } else {
                        "identical"
                    },
                    audit.edits.len(),
                    audit.exact_minimal
                );
                audit.old.standard.deprecations_text(&mut raw, "old ");
                audit.new.standard.deprecations_text(&mut raw, "new ");
                raw.push_str(&audit.custom_summary());
                for edit in &audit.edits {
                    let preview = |hex: &str| {
                        if hex.len() <= 128 {
                            hex.to_owned()
                        } else {
                            format!(
                                "{}... ({} bytes; complete bytes in JSON/patch)",
                                &hex[..128],
                                hex.len() / 2
                            )
                        }
                    };
                    raw.push_str(&format!(
                        "  old [{}..{}) {} -> new [{}..{}) {}\n",
                        edit.old_span.start,
                        edit.old_span.end,
                        preview(&edit.old_hex),
                        edit.new_span.start,
                        edit.new_span.end,
                        preview(&edit.new_hex)
                    ));
                }
                raw.push_str(&format!(
                    "Selected standard: {}; validation: old={}, new={}\n",
                    parsed.standard, audit.old.standard.validation, audit.new.standard.validation
                ));
                for (side, module) in [("old", &audit.old), ("new", &audit.new)] {
                    for d in &module.diagnostics {
                        raw.push_str(&format!(
                            "  {side} {} @{}: {}\n",
                            d.code,
                            d.offset,
                            d.message.escape_debug()
                        ));
                    }
                }
                Ok(format!("{bindings}{functions}\n{raw}"))
            } else {
                Ok(format!("{bindings}{}\n\nFunction interpretation (all decoder-known proposals; selected-standard validity is reported above):\n{functions}", audit.to_text()))
            }
        }
    }
}

fn analysis_scope(mut status: Value, standard: Standard) -> Value {
    status["decoding_standard"] = json!("all-known");
    status["selected_standard"] = json!(standard);
    status["standard_validity_is_separate"] = json!(true);
    status
}

fn read_inputs(
    inputs: &[&str],
    repo: Option<&str>,
    file: Option<&str>,
) -> Result<Vec<Input>, String> {
    match repo {
        Some(directory) if file.is_none() => {
            repository::read_revisions(Path::new(directory), inputs).map(|resolved| {
                inputs
                    .iter()
                    .zip(resolved)
                    .map(|(reference, revision)| Input {
                        bytes: revision.bytes,
                        origin: json!({"revision":reference,"commit":revision.commit}),
                    })
                    .collect()
            })
        }
        Some(directory) => {
            repository::read_selected_file_revisions(Path::new(directory), inputs, file)
                .and_then(|resolved| tree_output::select_inputs(inputs, resolved, file))
        }
        None => inputs
            .iter()
            .map(|path| {
                std::fs::read(path)
                    .map(|bytes| Input {
                        bytes,
                        origin: json!({"path": path}),
                    })
                    .map_err(|error| format!("cannot read {path}: {error}"))
            })
            .collect(),
    }
}

fn patch_summary(patch: &wasm_git::patch::BytePatch) -> Value {
    json!({
        "format": patch.format,
        "old_sha256": patch.old_sha256,
        "new_sha256": patch.new_sha256,
        "old_len": patch.old_len,
        "new_len": patch.new_len,
        "edit_count": patch.edits.len()
    })
}

fn cmd_patch(args: &[String]) -> Result<String, String> {
    let parsed = match PatchArgs::try_parse_from(
        std::iter::once("wasm-git patch").chain(args.iter().map(String::as_str)),
    ) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string());
        }
        Err(error) => return Err(error.to_string()),
    };
    let inputs = read_inputs(
        &[&parsed.old_path, &parsed.new_path],
        parsed.repo.as_deref(),
        parsed.file.as_deref(),
    )?;
    let patch = wasm_git::patch::create(&inputs[0].bytes, &inputs[1].bytes)?;
    let encoded = patch.to_bytes()?;
    output::write_export_file(Path::new(&parsed.output), &encoded)?;
    serde_json::to_string_pretty(&json!({
        "schema_version": 1,
        "patch": patch_summary(&patch),
        "written": true,
        "output": parsed.output,
        "inputs": {"old": &inputs[0].origin, "new": &inputs[1].origin}
    }))
    .map_err(|error| error.to_string())
}

fn cmd_apply(args: &[String]) -> Result<String, String> {
    let parsed = match ApplyArgs::try_parse_from(
        std::iter::once("wasm-git apply").chain(args.iter().map(String::as_str)),
    ) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string());
        }
        Err(error) => return Err(error.to_string()),
    };
    let inputs = read_inputs(
        &[&parsed.old_path],
        parsed.repo.as_deref(),
        parsed.file.as_deref(),
    )?;
    let encoded = std::fs::read(&parsed.patch_path)
        .map_err(|error| format!("cannot read {}: {error}", parsed.patch_path))?;
    let patch = wasm_git::patch::BytePatch::from_bytes(&encoded)?;
    let bytes = patch.apply(&inputs[0].bytes)?;
    // Recompute explanations from verified bytes. The artifact supplies no
    // trusted feature, context, validation, or minimum-edit claims.
    let audit = audit::compare_with_standard(&inputs[0].bytes, &bytes, parsed.standard)?;
    if let Some(destination) = &parsed.output {
        output::write_export_file(Path::new(destination), &bytes)?;
    }
    serde_json::to_string_pretty(&json!({
        "schema_version": 1,
        "patch": patch_summary(&patch),
        "written": parsed.output.is_some(),
        "output": parsed.output,
        "inputs": {"old": &inputs[0].origin, "patch": {"path": parsed.patch_path}},
        "audit": audit
    }))
    .map_err(|error| error.to_string())
}

fn normalize_bytes(
    bytes: &[u8],
    audit: &audit::AuditModule,
) -> Result<ir::NormalizedModule, String> {
    normalize_bytes_with_debug_names(bytes, audit, false)
}

fn normalize_bytes_with_debug_names(
    bytes: &[u8],
    audit: &audit::AuditModule,
    names: bool,
) -> Result<ir::NormalizedModule, String> {
    if !bytes.starts_with(b"\0asm\x01\0\0\0") {
        return Err("typed function analysis is available for Core version 1 only; other Wasm encodings retain lossless byte/outer-section audit".into());
    }
    if let Some(section) = audit
        .sections
        .iter()
        .find(|section| section.id != 0 && section.status != "decoded")
    {
        return Err(format!(
            "incomplete Core grammar at {} ({}); function analysis cannot use unproven declaration or instruction boundaries; see the lossless binary audit",
            section.path, section.status
        ));
    }
    if let Some(entry) = audit
        .standard
        .evidence
        .iter()
        .find(|entry| entry.status == "invalid_encoding")
    {
        return Err(format!(
            "invalid Core encoding at {} [{},{}); function analysis cannot normalize an invalid wire form; see the lossless binary audit",
            entry.path, entry.span.start, entry.span.end
        ));
    }
    (if names {
        parse::parse_module_with_debug_names(bytes)
    } else {
        parse::parse_module(bytes)
    })
    .map(|parsed| parsed.resolve().normalize())
    .map_err(|error| error.to_string())
}

fn parse_trace_range(value: &str) -> Result<audit::Span, String> {
    let (start, end) = value
        .split_once(':')
        .ok_or("trace range must be START:END in decimal bytes")?;
    let number = |text: &str| -> Result<usize, String> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("trace offsets must contain only decimal ASCII digits".into());
        }
        let value = text
            .parse::<u64>()
            .map_err(|_| "trace offset exceeds u64")?;
        usize::try_from(value).map_err(|_| "trace offset exceeds this host's address space".into())
    };
    let span = audit::Span {
        start: number(start)?,
        end: number(end)?,
    };
    if span.start >= span.end {
        return Err("trace range must be nonempty with START < END".into());
    }
    Ok(span)
}

fn trace_path_span(module: &audit::AuditModule, path: &str) -> Result<audit::Span, String> {
    if path == "module" {
        return Ok(audit::Span {
            start: 0,
            end: module.byte_len,
        });
    }
    let mut matches = Vec::new();
    let mut pending = Vec::new();
    for section in &module.sections {
        if section.path == path {
            matches.push(section.span);
        }
        pending.extend(&section.regions);
    }
    while let Some(region) = pending.pop() {
        if region.path == path {
            matches.push(region.span);
        }
        pending.extend(&region.children);
    }
    match matches.as_slice() {
        [span] if span.start < span.end => Ok(*span),
        [_] => Err("trace path selects an empty region; select a nonempty byte range".into()),
        [] => Err(format!(
            "trace path is not present in the selected revision: {path:?}"
        )),
        _ => Err(format!(
            "trace path is ambiguous in the selected revision: {path:?}"
        )),
    }
}

fn cmd_blame(args: &[String]) -> Result<String, String> {
    let parsed = match BlameArgs::try_parse_from(
        std::iter::once("wasm-git blame").chain(args.iter().map(String::as_str)),
    ) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string())
        }
        Err(error) => return Err(error.to_string()),
    };
    let (history, absent) = if let Some(file) = &parsed.file {
        let selected =
            repository::read_file_history(Path::new(&parsed.repo_dir), &parsed.revision, file)?;
        (selected.history, selected.absent)
    } else {
        (
            repository::read_history(Path::new(&parsed.repo_dir), &parsed.revision)?,
            Default::default(),
        )
    };
    let bytes = &history.blobs[&history.commits[&history.tip].wasm];
    let query = if let Some(range) = &parsed.range {
        parse_trace_range(range)?
    } else {
        trace_path_span(
            &audit::inspect(bytes)?,
            parsed.path.as_deref().unwrap_or("module"),
        )?
    };
    let limits = wasm_git::trace::TraceLimits {
        max_edges: parsed.max_edges,
        max_compared_bytes: parsed.max_bytes,
        ..Default::default()
    };
    let report = wasm_git::blame::blame(&history, &absent, query, parsed.first_parent, limits)?;
    serde_json::to_string_pretty(&json!({"schema_version":1,"repository_verified":true,"file":parsed.file,"selection":{"range":parsed.range,"path":parsed.path},"blame":report})).map_err(|error| error.to_string())
}

fn cmd_trace(args: &[String]) -> Result<String, String> {
    let parsed = match TraceArgs::try_parse_from(
        std::iter::once("wasm-git trace").chain(args.iter().map(String::as_str)),
    ) {
        Ok(parsed) => parsed,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(error.to_string());
        }
        Err(error) => return Err(error.to_string()),
    };
    let numeric_range = parsed.range.as_deref().map(parse_trace_range).transpose()?;
    let (history, absent, legacy_path, tip_legacy) = if let Some(file) = &parsed.file {
        let selected =
            repository::read_file_history(Path::new(&parsed.repo_dir), &parsed.revision, file)?;
        (
            selected.history,
            selected.absent,
            selected.legacy_path,
            selected.tip_legacy,
        )
    } else {
        (
            repository::read_history(Path::new(&parsed.repo_dir), &parsed.revision)?,
            Default::default(),
            None,
            false,
        )
    };
    let commit = history
        .commits
        .get(&history.tip)
        .ok_or("trace snapshot is missing its tip")?;
    let bytes = history
        .blobs
        .get(&commit.wasm)
        .ok_or("trace snapshot is missing its tip blob")?;
    let span = if let Some(range) = numeric_range {
        range
    } else {
        let module = audit::inspect(bytes)?;
        trace_path_span(
            &module,
            parsed
                .path
                .as_deref()
                .ok_or("trace requires --range or --path")?,
        )?
    };
    if span.end > bytes.len() {
        return Err(format!(
            "trace range exceeds the selected revision's {} bytes",
            bytes.len()
        ));
    }
    let limits = wasm_git::trace::TraceLimits {
        max_edges: parsed.max_edges,
        max_compared_bytes: parsed.max_bytes,
        ..Default::default()
    };
    let report = wasm_git::trace::trace_with_absences(&history, &absent, span, limits)?;
    let mut selection = json!({"kind": if parsed.path.is_some() { "path" } else { "range" }, "path":parsed.path,"span":span});
    if let Some(file) = parsed.file {
        selection["file"] = json!(file);
        selection["file_identity"] = json!(if tip_legacy {
            "explicit selected path; legacy commits have no filename"
        } else {
            "exact_path; rename identity is not inferred"
        });
        selection["legacy_path"] = json!(legacy_path);
    }
    serde_json::to_string_pretty(&json!({
        "schema_version": 1,
        "repository_verified": true,
        "selection": selection,
        "trace": report
    }))
    .map_err(|error| error.to_string())
}

fn usage_text() -> String {
    let mut lines = Vec::new();
    let mut line = |text: &str| lines.push(text.to_owned());
    line("Global output: wasm-git [--format text|json|binary-v1|native] [--threads N] COMMAND ...");
    line("JSON/binary-v1 are versioned application responses; command-local --format retains its own meaning.");
    line("Additional storage commands: clone --depth/--filter; fetch --deepen/--unshallow; clone-status; hydrate; promisor; keygen; trust-author; untrust-author; commit-signed; sign-commit; verify-commit; pool attach/status/sync/release; pr URL create/list/show/update/comment/review/check/diff/merge/recover. Signing supports --format native|ssh|openpgp. Use COMMAND --help.");
    line("usage: wasm-git <command> [<args>]\n");
    line("  version [--build-options]");
    line("  features [FEATURE] [--standard STANDARD] [--json]");
    line("  validate FILE... [--features FEATURE,... | --config FUZZER_CONFIG] [--json]");
    line("  blame <DIR> <REV> [--file PATH] [--range START:END|--path REGION] [--first-parent]");
    line("  hash-object <DIR> [-w] [-t wasm|tree|commit] (--stdin|FILE) | mktree <DIR> [-z]");
    line("  write-tree <DIR> | read-tree <DIR> <REV|--empty> | commit-tree <DIR> <TREE> [-p PARENT]... -m MESSAGE");
    line("  update-ref <DIR> [-d] [-m MESSAGE] <REF> [NEW] [OLD] | rev-list <DIR> [OPTIONS] [REVISIONS...]");
    line("  merge-base <DIR> [-a] [--octopus|--independent|--is-ancestor|--fork-point] <REVISIONS...> [--text|--json]");
    line(
        "  symbolic-ref <DIR> [--short] HEAD [refs/heads/BRANCH] | check-ref-format [--branch] REF",
    );
    line("  diff-files <DIR> | diff-index <DIR> <REV> [--cached] | diff-tree <DIR> <OLD> <NEW> [OPTIONS]");
    line("  cat-file <DIR> --batch|--batch-check|--batch-command [--buffer] [-z|-Z]");
    line(
        "  clean <DIR> -n|-f [-d] [-x|-X] [-e PATTERN] [-- PATH...] | gc <DIR> [--prune=now] [-n]",
    );
    line("  maintenance <DIR> run [--task verify|gc] | worktree <DIR> add|list|remove|move|lock|unlock|prune|repair [OPTIONS]");
    line("  notes <DIR> [--ref NAMESPACE] list|show|add|append|copy|remove [OPTIONS]");
    line("  shortlog <DIR> [REV] [--group path|subject] [-s] [-n] | range-diff <DIR> <BASE..TIP> <BASE..TIP>");
    line(
        "  mergetool <DIR> --output NEW_DIR [--tool PROGRAM --stage] [--delete PATH] [-- PATH...]",
    );
    line(
        "  inspect <FILE|REV> [--repo DIR] [--file PATH] [--json] [--standard core1|core2|core3|all-known]"
    );
    line("  debug-map <FILE|REV> [--repo DIR] [--file PATH] [--offset N|--code-offset N|--symbol NAME|--function N|--source PATH [--line N]] [--json]");
    line(
        "  diff <OLD> <NEW> [--repo DIR] [--file PATH] [--section SELECTOR]... [--short] [--format text|json] [--standard STANDARD]"
    );
    line("  diff <DIR> [--cached] [--file PATH] [--short] [--format text|json] [--standard STANDARD]");
    line("  patch <OLD> <NEW> --output NEW_PATCH [--repo DIR --file PATH]");
    line(
        "  apply <OLD> <PATCH> (--check | --output NEW_WASM) [--repo DIR --file PATH] [--standard STANDARD]"
    );
    line(
        "  trace <DIR> <REV> [--file PATH] (--range START:END | --path PATH) [--max-edges N] [--max-bytes N]"
    );
    line("  init <DIR> | commit <DIR> <WASM> -m <MESSAGE> | log <DIR> [REV]");
    line("  show <DIR> <REV> [--file PATH] [--output NEW_FILE] | checkout <DIR> <REV> <NEW_FILE> [--file PATH]");
    line("  checkout <DIR> <BRANCH> | checkout <DIR> -b <NAME> [REV]");
    line("  checkout <DIR> [REV] -- <PATH>...");
    line("  branch <DIR> [NAME [REV]|-r|-a|-M [OLD] NEW|-c [OLD] NEW|-C [OLD] NEW|-f NAME [REV]] | switch <DIR> <NAME|-> [or --detach REV|--orphan NAME]");
    line("  branch <DIR> -u <REMOTE/BRANCH> [NAME] | branch <DIR> --unset-upstream [NAME] | branch <DIR> -vv");
    line("  clone <SOURCE_DIR> <NEW_DIR> | bundle <DIR> <NEW_BUNDLE_FILE>");
    line("  unbundle <DIR> <BUNDLE> <BRANCH> | fsck <DIR>");
    line("  merge <DIR> <REV> -m <MESSAGE> | merge-base <DIR> <LEFT> <RIGHT>");
    line("  merge <DIR> <REV> --ff-only [-m MESSAGE]");
    line("  merge <DIR> <TARGET_ID> -m <MESSAGE> --resolved-index --expect-head <HEAD_ID> [--expect-branch NAME]");
    line("  add <DIR> <RELATIVE_WASM>... | status <DIR> | commit <DIR> -m <MESSAGE>");
    line("  restore <DIR> [--source REV] | reset <DIR> [--soft|--mixed|--hard] [REV]");
    line(
        "  rm <DIR> [--cached] [FILE...] | mv <DIR> <OLD> <NEW> | stash <DIR> [push|list|apply|pop|drop]"
    );
    line("  tag <DIR> [NAME [REV]|-d NAME] | reflog <DIR> | rev-parse <DIR> <REV>");
    line("  cat-file <DIR> <REV> | ls-files <DIR> | config <DIR> [KEY [VALUE]]");
    line("  ls-files <DIR> [-s|--unmerged] [-m|-d|-o|-i|-k] [-t|-v|-f] [--exclude-standard] [-z|--json|--format STR] [-- PATHSPEC...]");
    line("  checkout-index <DIR> [-a] [-f] [-n] [--prefix STR] [--stage 1|2|3|all] [--temp] [--ignore-skip-worktree-bits] [--stdin] [-z] [-- PATH...]");
    line("  write-tree <DIR> [--prefix DIR/]");
    line("  read-tree <DIR> [-m BASE [OURS] THEIRS|--empty|TREE] [-u] [-n] [--prefix DIR/]");
    line("  update-index <DIR> [--add|--remove|--force-remove] PATH... | --cacheinfo MODE,ID,PATH | --index-info | --stdin [-z]");
    line("  update-index <DIR> --[no-]assume-unchanged|--[no-]skip-worktree|--[no-]fsmonitor-valid PATH...");
    line(
        "  update-index <DIR> --refresh|--really-refresh [--ignore-missing] [--unmerged] [PATH...]",
    );
    line("  add <DIR> -N|--intent-to-add PATH... | add <DIR> --sparse PATH... | commit <DIR> -a -m MESSAGE");
    line("  merge-index <DIR> --all|-- PATHSPEC... [--dry-run] | restore <DIR> --ours|--theirs|--merge|-m|--ignore-unmerged -- PATHSPEC...");
    line("  ls-tree <DIR> <REV> [-r] [--json|--name-only|--object-only] [-z] [-- PATH...]");
    line("  show-ref <DIR> [--head] [--heads|--tags] [--verify] [--quiet|--hash|--json] [PATTERN...]");
    line("  branch <DIR> --list [-r|-a] [--contains [REV]] [--merged [REV]] [--points-at OBJECT] [--format STR|--json] [PATTERN...] | branch <DIR> --show-current");
    line("  for-each-ref <DIR> [--json|--format STR] [--sort KEY]... [--count N] [PATTERN...]");
    line("  count-objects <DIR> [-v] [-H] | ls-remote <URL|DIR [REMOTE]> [--json] [PATTERN...]");
    line(
        "  cherry-pick|revert <DIR> <REV>... [--sequence|--stdin] [-n] [--reverse] | rebase <DIR> <UPSTREAM> [--onto REV]"
    );
    line("  cherry-pick|revert <DIR> REV... [--mainline N] [-m MESSAGE] [--empty stop|drop|keep] [--allow-empty] [--ff] [-x]");
    line("  cherry-pick|revert <DIR> <TARGET_ID> [--mainline N] --resolved-index --expect-head <HEAD_ID> [--expect-branch NAME] [-m MESSAGE]");
    line("  merge|cherry-pick|revert <DIR> <REV> --record-conflict [-m MESSAGE] [--mainline N]");
    line("  merge|cherry-pick|revert <DIR> --continue --resolved-index | --abort | --quit");
    line("  cherry-pick|revert <DIR> --skip");
    line("  rebase <DIR> <UPSTREAM> [--onto REV] --record-conflict");
    line("  rebase <DIR> --continue --resolved-index | --abort | --skip | --quit");
    line("  remote <DIR> [add NAME URL|remove NAME|-v] | fetch <DIR> [REMOTE]");
    line("  push <DIR> [REMOTE] [SRC:DST] | pull <DIR> [REMOTE] [BRANCH] -m <MESSAGE>");
    line("  push <DIR> [REMOTE] [LOCAL_BRANCH:REMOTE_BRANCH] --set-upstream");
    line("  pull <DIR> [REMOTE] [BRANCH] --ff-only [-m MESSAGE]");
    line("  clone <http(s)://HOST/REPO|ssh://USER@HOST/ROOT/REPO> <NEW_DIR>");
    line("  describe <DIR> [REV] [--tags] [--exact-match] [--always] [--long] [--first-parent] [--abbrev N]");
    line("  archive <DIR> <REV> --output NEW_TAR [--format tar] [--prefix PREFIX/] [--legacy-path NAME] [-- PATH...]");
    line("  bisect <DIR> start [--no-checkout] [--first-parent] <BAD> <GOOD>...");
    line("  bisect <DIR> good|skip [REV...] | bad [REV] | status | next | log | reset");
    line("  server --root <DIR> [--listen ADDRESS] [--token-file FILE --allow-write]");
    line("  server --stdio --root <DIR> [--allow-force]");
    lines.join("\n")
}

/// Standalone server entry point. No client command dispatch is linked here.
#[cfg(feature = "server")]
pub fn server_main() {
    let mut args: Vec<String> = cli_args().into_iter().skip(1).collect();
    let mut format = command_output::Format::Native;
    if args
        .first()
        .is_some_and(|a| a == "--format" || a.starts_with("--format="))
    {
        let option = args.remove(0);
        let value = option
            .split_once('=')
            .map(|(_, v)| v.to_owned())
            .unwrap_or_else(|| {
                if args.is_empty() {
                    String::new()
                } else {
                    args.remove(0)
                }
            });
        format = command_output::Format::parse(&value).unwrap_or_else(|error| {
            cli_error(&error);
            std::process::exit(1);
        });
    }
    if args.first().is_some_and(|a| a == "server" || a == "serve") {
        args.remove(0);
    }
    let command = args.first().cloned().unwrap_or_default();
    if format.machine()
        && !matches!(
            command.as_str(),
            "compile-policy" | "compile-protection" | "audit-log" | "--help" | "-h"
        )
    {
        format.emit("server", "", 1, Some("running servers require --format native; stdout carries readiness or the binary network protocol"), false);
        std::process::exit(1);
    }
    args.insert(0, "server".to_owned());
    match server::run(&args) {
        Ok(output) => format.emit(&command, &output, 0, None, true),
        Err(error) => {
            format.emit(&command, "", 1, Some(&error), false);
            std::process::exit(1);
        }
    }
}
