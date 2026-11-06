//! Explicit local maintenance for native Wasm stores and worktrees.
use crate::query_commands::QueryOutput;
use crate::repository::{ReflogCutoff, ReflogExpire, Repository, RepositoryResult};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "wasm-git")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Inspect or explicitly expire timestamped binary reference history.
    Reflog {
        dir: String,
        #[command(subcommand)]
        action: Option<ReflogCommand>,
    },
    /// Export frozen conflict sides and optionally stage reviewed binary output.
    Mergetool {
        dir: String,
        #[arg(long)]
        output: String,
        #[arg(long)]
        tool: Option<String>,
        #[arg(long, requires = "tool")]
        stage: bool,
        #[arg(long = "delete", action = clap::ArgAction::Append, requires = "stage")]
        deletions: Vec<String>,
        paths: Vec<String>,
    },
    /// Preview or explicitly remove untracked regular files and empty directories.
    Clean {
        dir: String,
        #[arg(short = 'n', long, required_unless_present = "force")]
        dry_run: bool,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'd')]
        directories: bool,
        #[arg(short = 'x', conflicts_with = "only_ignored")]
        all: bool,
        #[arg(short = 'X')]
        only_ignored: bool,
        #[arg(short = 'e', long = "exclude", action = clap::ArgAction::Append, allow_hyphen_values = true)]
        excludes: Vec<String>,
        #[arg(short = 'q', long)]
        quiet: bool,
        paths: Vec<String>,
    },
    /// Compress native objects without changing their IDs or original Wasm bytes.
    Repack { dir: String },
    /// Compress history and report unreachable objects; prune only with --prune=now.
    Gc {
        dir: String,
        /// Explicit alias for the default history compression.
        #[arg(long)]
        pack: bool,
        #[arg(long, value_parser = ["now", "never"], default_value = "never")]
        prune: String,
        #[arg(short = 'n', long)]
        dry_run: bool,
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Run integrity checking and optional explicit native loose-object GC.
    Maintenance {
        dir: String,
        #[arg(value_parser = ["run"], default_value = "run")]
        action: String,
        #[arg(long, value_parser = ["verify", "reflog-expire", "gc"], action = clap::ArgAction::Append)]
        task: Vec<String>,
        #[arg(long, value_parser = ["now", "never"], default_value = "never")]
        prune: String,
    },
}

#[derive(Subcommand)]
enum ReflogCommand {
    Show {
        #[arg(default_value = "HEAD")]
        reference: String,
    },
    List,
    Exists {
        reference: String,
    },
    Write {
        reference: String,
        old: String,
        new: String,
        message: String,
    },
    Delete {
        selector: String,
        #[arg(short = 'n', long)]
        dry_run: bool,
        #[arg(long)]
        rewrite: bool,
    },
    Expire {
        #[arg(long, conflicts_with = "references")]
        all: bool,
        #[arg(long, requires = "all")]
        single_worktree: bool,
        #[arg(long, default_value = "90.days.ago")]
        expire: String,
        #[arg(long, default_value = "30.days.ago")]
        expire_unreachable: String,
        #[arg(short = 'n', long)]
        dry_run: bool,
        #[arg(long)]
        rewrite: bool,
        references: Vec<String>,
    },
}

fn expiry(repo: &Repository) -> RepositoryResult<serde_json::Value> {
    repo.expire_reflogs(&ReflogExpire {
        references: Vec::new(),
        all: true,
        single_worktree: false,
        expire: ReflogCutoff::parse("90.days.ago")?,
        expire_unreachable: ReflogCutoff::parse("30.days.ago")?,
        dry_run: false,
        rewrite: false,
    })
}

pub fn handles(command: &str) -> bool {
    matches!(
        command,
        "clean" | "gc" | "repack" | "maintenance" | "mergetool" | "reflog"
    )
}

pub fn run(args: &[String]) -> RepositoryResult<QueryOutput> {
    let cli = match Cli::try_parse_from(
        std::iter::once("wasm-git").chain(args.iter().map(String::as_str)),
    ) {
        Ok(cli) => cli,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(QueryOutput::new(error.to_string(), 0))
        }
        Err(error) => return Err(error.to_string()),
    };
    let (output, quiet) = match cli.command {
        Command::Reflog { dir, action } => {
            let repo = Repository::open(dir)?;
            let value = match action {
                None => serde_json::to_value(repo.reflog()?).map_err(|e| e.to_string())?,
                Some(ReflogCommand::Show { reference }) => {
                    serde_json::to_value(repo.reflog_entries(&reference)?)
                        .map_err(|e| e.to_string())?
                }
                Some(ReflogCommand::List) => {
                    serde_json::to_value(repo.reflog_names()?).map_err(|e| e.to_string())?
                }
                Some(ReflogCommand::Exists { reference }) => {
                    return Ok(QueryOutput::new(
                        String::new(),
                        i32::from(repo.reflog_entries(&reference)?.is_empty()),
                    ))
                }
                Some(ReflogCommand::Write {
                    reference,
                    old,
                    new,
                    message,
                }) => {
                    let endpoint = |id: String| if id == "0".repeat(64) { None } else { Some(id) };
                    repo.write_reflog_entry(&reference, endpoint(old), endpoint(new), &message)?;
                    serde_json::json!({"written":true,"references_changed":false})
                }
                Some(ReflogCommand::Delete {
                    selector,
                    dry_run,
                    rewrite,
                }) => repo.delete_reflog_entry(&selector, dry_run, rewrite)?,
                Some(ReflogCommand::Expire {
                    references,
                    all,
                    single_worktree,
                    expire,
                    expire_unreachable,
                    dry_run,
                    rewrite,
                }) => repo.expire_reflogs(&ReflogExpire {
                    references,
                    all,
                    single_worktree,
                    expire: ReflogCutoff::parse(&expire)?,
                    expire_unreachable: ReflogCutoff::parse(&expire_unreachable)?,
                    dry_run,
                    rewrite,
                })?,
            };
            (
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,
                false,
            )
        }
        Command::Repack { dir } => (
            serde_json::to_string_pretty(&Repository::open(dir)?.repack()?)
                .map_err(|e| e.to_string())?,
            false,
        ),
        Command::Mergetool {
            dir,
            output,
            tool,
            stage,
            deletions,
            paths,
        } => (
            crate::porcelain::mergetool(&dir, &output, tool.as_deref(), stage, &paths, &deletions)?,
            false,
        ),
        Command::Clean {
            dir,
            dry_run,
            force: _,
            directories,
            all,
            only_ignored,
            excludes,
            quiet,
            paths,
        } => (
            crate::porcelain::clean(
                &dir,
                dry_run,
                directories,
                all,
                only_ignored,
                &excludes,
                &paths,
            )?,
            quiet,
        ),
        Command::Gc {
            dir,
            prune,
            dry_run,
            quiet,
            pack: _,
        } => {
            let output = crate::porcelain::garbage_collect_with_pack(
                &dir,
                prune == "now" && !dry_run,
                !dry_run,
            )?;
            (output, quiet)
        }

        Command::Maintenance {
            dir,
            action: _,
            task,
            prune,
        } => {
            if task.iter().collect::<std::collections::BTreeSet<_>>().len() != task.len() {
                return Err("duplicate maintenance task".into());
            }
            if prune != "never" && !task.is_empty() && !task.iter().any(|task| task == "gc") {
                return Err("--prune requires the gc maintenance task".into());
            }
            let mut report = serde_json::Map::new();
            if task.is_empty() || task.iter().any(|task| task == "verify") {
                report.insert(
                    "verify".into(),
                    serde_json::to_value(Repository::open(&dir)?.fsck()?)
                        .map_err(|error| error.to_string())?,
                );
            }
            if task.is_empty() || task.iter().any(|task| task == "gc") {
                if task.is_empty() || task.iter().any(|task| task == "reflog-expire") {
                    report.insert("reflog-expire".into(), expiry(&Repository::open(&dir)?)?);
                }
                report.insert(
                    "gc".into(),
                    serde_json::from_str(&crate::porcelain::garbage_collect_with_pack(
                        &dir,
                        prune == "now",
                        true,
                    )?)
                    .map_err(|error| error.to_string())?,
                );
            } else if task.iter().any(|task| task == "reflog-expire") {
                report.insert("reflog-expire".into(), expiry(&Repository::open(&dir)?)?);
            }
            (
                serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?,
                false,
            )
        }
    };
    Ok(QueryOutput::new(
        if quiet {
            String::new()
        } else {
            format!("{output}\n")
        },
        0,
    ))
}
