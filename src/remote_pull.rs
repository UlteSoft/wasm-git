//! CLI for authenticated PR records and exact original-byte audit inputs.
use super::*;
use crate::pull::*;
use clap::Parser;

#[derive(clap::Parser)]
#[command(name = "wasm-git pr")]
struct Args {
    url: String,
    #[arg(long, global = true)]
    allow_insecure_http: bool,
    #[command(subcommand)]
    command: Action,
}
#[derive(clap::Subcommand)]
enum Action {
    Create {
        #[arg(long)]
        source_repository: String,
        #[arg(long)]
        source_branch: String,
        #[arg(long, default_value = "main")]
        base_branch: String,
        #[arg(long, allow_hyphen_values = true)]
        title: String,
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        body: String,
        #[arg(long)]
        draft: bool,
    },
    List {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 30)]
        limit: u16,
    },
    Show {
        number: u64,
    },
    Update {
        number: u64,
        #[arg(long)]
        version: u64,
        #[arg(long, allow_hyphen_values = true)]
        title: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
    Close {
        number: u64,
        #[arg(long)]
        version: u64,
    },
    Reopen {
        number: u64,
        #[arg(long)]
        version: u64,
    },
    Ready {
        number: u64,
        #[arg(long)]
        version: u64,
    },
    Draft {
        number: u64,
        #[arg(long)]
        version: u64,
    },
    Comment {
        number: u64,
        #[arg(long)]
        version: u64,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
    },
    Review {
        number: u64,
        #[arg(long)]
        version: u64,
        #[arg(long, value_enum)]
        decision: ReviewDecision,
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        body: String,
    },
    Check {
        number: u64,
        #[arg(long)]
        version: u64,
        #[arg(long)]
        context: String,
        #[arg(long, value_enum)]
        state: CheckState,
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        description: String,
    },
    Files {
        number: u64,
        #[arg(long)]
        version: u64,
        #[arg(long, value_enum, default_value = "merge-base")]
        against: DiffBase,
    },
    Diff {
        number: u64,
        #[arg(long, value_enum, default_value = "merge-base")]
        against: DiffBase,
        #[arg(long)]
        file: Option<String>,
        #[arg(long, value_enum, default_value = "text")]
        format: crate::DiffFormat,
        #[arg(long)]
        compact: bool,
        #[arg(long = "section")]
        sections: Vec<crate::audit::section::Selector>,
    },
    Merge {
        number: u64,
        #[arg(long)]
        version: u64,
        #[arg(long)]
        head: String,
        #[arg(long)]
        base: String,
        #[arg(long, value_enum, default_value = "merge")]
        method: MergeMethod,
        #[arg(short, long, allow_hyphen_values = true)]
        message: String,
    },
    Recover {
        number: u64,
        #[arg(long)]
        version: u64,
    },
}
fn call(remote: &Remote, command: &PullCommand) -> RepositoryResult<PullReply> {
    let payload = crate::binary::encode(command)?;
    let expected = format!("{:x}", sha2::Sha256::digest(&payload));
    let (response, bytes) = exchange(
        remote,
        Operation::PullRequest {},
        &payload,
        remote.allow_insecure_http,
    )?;
    if response.tip.as_deref() != Some(&expected) {
        return Err(if command.mutates() {
            "pull publication outcome is unknown: receipt does not match the request"
        } else {
            "pull response does not match the request"
        }
        .into());
    }
    crate::binary::decode(&bytes).map_err(|e| {
        if command.mutates() {
            format!("pull publication outcome is unknown: invalid result: {e}")
        } else {
            e
        }
    })
}
fn read_file(
    remote: &Remote,
    p: &PullRecord,
    path: &str,
    old: bool,
    expected: &str,
    against: DiffBase,
) -> RepositoryResult<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut total = None;
    loop {
        let chunk = call(
            remote,
            &PullCommand::File {
                number: p.number,
                version: p.version,
                path: path.into(),
                old,
                offset: bytes.len() as u64,
                against,
            },
        )?
        .chunk
        .ok_or("missing pull file chunk")?;
        if chunk.id != expected
            || chunk.offset != bytes.len() as u64
            || chunk.total > protocol::MAX_BUNDLE_BYTES
            || total.is_some_and(|n| n != chunk.total)
            || chunk.bytes.len() > protocol::TRANSFER_CHUNK
            || chunk.offset.saturating_add(chunk.bytes.len() as u64) > chunk.total
        {
            return Err("inconsistent pull file chunk".into());
        }
        total = Some(chunk.total);
        if chunk.bytes.is_empty() && bytes.len() as u64 != chunk.total {
            return Err("pull file transfer made no progress".into());
        }
        bytes.extend(chunk.bytes);
        if bytes.len() as u64 == chunk.total {
            break;
        }
    }
    if crate::repository::wasm_payload_id(&bytes)? != expected {
        return Err("pull file checksum does not match its advertised original object".into());
    }
    Ok(bytes)
}
fn diff(
    remote: &Remote,
    number: u64,
    file: Option<String>,
    format: crate::DiffFormat,
    compact: bool,
    sections: Vec<crate::audit::section::Selector>,
    against: DiffBase,
) -> RepositoryResult<String> {
    let mut reply = call(remote, &PullCommand::Show { number })?;
    if reply.records.len() != 1 || reply.records[0].number != number {
        return Err("invalid pull record reply".into());
    }
    let p = reply.records.remove(0);
    let compared = call(
        remote,
        &PullCommand::Files {
            number,
            version: p.version,
            against,
        },
    )?;
    let base = compared.diff_base.ok_or("missing pull comparison base")?;
    let files = compared.files;
    let selected = if let Some(path) = file {
        files
            .into_iter()
            .find(|f| f.path == path)
            .ok_or("requested path is not changed in this pull request")?
    } else {
        if files.is_empty() {
            if format == crate::DiffFormat::Json {
                return pretty(
                    &serde_json::json!({"pull":number,"version":p.version,"base":base,"target":p.base,"head":p.head,"files":[]}),
                );
            }
            return Ok("No file changes in the selected pull request snapshot".into());
        }
        if files.len() != 1 {
            return Err(
                "this pull request changes multiple files; use pr files, then pr diff --file PATH"
                    .into(),
            );
        }
        files.into_iter().next().unwrap()
    };
    let old = selected
        .old
        .as_ref()
        .map(|id| read_file(remote, &p, &selected.path, true, id, against))
        .transpose()?;
    let new = selected
        .new
        .as_ref()
        .map(|id| read_file(remote, &p, &selected.path, false, id, against))
        .transpose()?;
    match (old, new) {
        (Some(old), Some(new)) => crate::render_diff(
            &crate::DiffArgs {
                old_path: base.clone(),
                new_path: p.head.clone(),
                format,
                short: false,
                compact,
                sections,
                repo: Some(remote.url.clone()),
                file: Some(selected.path.clone()),
                standard: crate::standard::Standard::AllKnown,
            },
            &[
                crate::Input {
                    bytes: old,
                    origin: serde_json::json!({"commit":base,"target":p.base,"repository":remote.url,"pull":number,"version":p.version,"file":selected.path}),
                },
                crate::Input {
                    bytes: new,
                    origin: serde_json::json!({"commit":p.head,"repository":remote.url,"pull":number,"version":p.version,"file":selected.path}),
                },
            ],
        ),
        (old, new) => {
            let status = if new.is_some() { "added" } else { "deleted" };
            let bytes = new.or(old).ok_or("pull file has neither side")?;
            let audit = crate::audit::inspect(&bytes)?;
            if format == crate::DiffFormat::Json {
                pretty(
                    &serde_json::json!({"status":status,"file":selected,"pull":number,"version":p.version,"audit":audit}),
                )
            } else {
                Ok(format!(
                    "{status} {} ({} original bytes, {} sections)",
                    selected.path,
                    bytes.len(),
                    audit.sections.len()
                ))
            }
        }
    }
}
pub(super) fn run(args: &[String]) -> RepositoryResult<String> {
    let args = match Args::try_parse_from(
        std::iter::once("wasm-git pr".into()).chain(args.iter().cloned()),
    ) {
        Ok(args) => args,
        Err(e)
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return Ok(e.to_string())
        }
        Err(e) => return Err(e.to_string()),
    };
    parse_url(&args.url)?;
    let remote = Remote {
        version: 1,
        name: "pull-request".into(),
        url: args.url,
        allow_insecure_http: args.allow_insecure_http,
    };
    let command = match args.command {
        Action::Create {
            source_repository,
            source_branch,
            base_branch,
            title,
            body,
            draft,
        } => PullCommand::Create {
            source_repository,
            source_branch,
            base_branch,
            title,
            body,
            draft,
        },
        Action::List { after, limit } => PullCommand::List { after, limit },
        Action::Show { number } => PullCommand::Show { number },
        Action::Update {
            number,
            version,
            title,
            body,
            refresh,
        } => PullCommand::Update {
            number,
            version,
            title,
            body,
            refresh,
        },
        Action::Close { number, version } => PullCommand::Close { number, version },
        Action::Reopen { number, version } => PullCommand::Reopen { number, version },
        Action::Ready { number, version } => PullCommand::Ready { number, version },
        Action::Draft { number, version } => PullCommand::Draft { number, version },
        Action::Comment {
            number,
            version,
            body,
        } => PullCommand::Comment {
            number,
            version,
            body,
        },
        Action::Review {
            number,
            version,
            decision,
            body,
        } => PullCommand::Review {
            number,
            version,
            decision,
            body,
        },
        Action::Check {
            number,
            version,
            context,
            state,
            description,
        } => PullCommand::Check {
            number,
            version,
            context,
            state,
            description,
        },
        Action::Files {
            number,
            version,
            against,
        } => PullCommand::Files {
            number,
            version,
            against,
        },
        Action::Diff {
            number,
            file,
            format,
            compact,
            sections,
            against,
        } => return diff(&remote, number, file, format, compact, sections, against),
        Action::Merge {
            number,
            version,
            head,
            base,
            method,
            message,
        } => PullCommand::Merge {
            number,
            version,
            head,
            base,
            method,
            message,
        },
        Action::Recover { number, version } => PullCommand::Recover { number, version },
    };
    pretty(&call(&remote, &command)?)
}
