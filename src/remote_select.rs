use super::*;
use crate::repository::{CloneFilter, PromiseSource, SelectOptions};
use clap::{Parser, Subcommand};
#[derive(Parser)]
struct Arguments {
    #[command(subcommand)]
    command: CommandArgs,
}
#[derive(Subcommand)]
enum CommandArgs {
    Clone {
        source: String,
        destination: PathBuf,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        allow_insecure_http: bool,
    },
    Fetch {
        repository: PathBuf,
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(long, conflicts_with_all=["deepen","unshallow"])]
        depth: Option<u32>,
        #[arg(long, conflicts_with = "unshallow")]
        deepen: Option<u32>,
        #[arg(long)]
        unshallow: bool,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        allow_insecure_http: bool,
    },
    Promisor {
        repository: PathBuf,
        #[arg(required = true)]
        sources: Vec<String>,
        #[arg(long)]
        allow_insecure_http: bool,
    },
    Hydrate {
        repository: PathBuf,
    },
    CloneStatus {
        repository: PathBuf,
    },
}
pub fn selective_requested(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| {
        ["--depth", "--deepen", "--unshallow", "--branch", "--filter"]
            .iter()
            .any(|o| a == o || a.starts_with(&format!("{o}=")))
    })
}
fn filter(value: Option<String>, default: CloneFilter) -> RepositoryResult<CloneFilter> {
    match value.as_deref() {
        None => Ok(default),
        Some("all") => Ok(CloneFilter::All),
        Some("blob:none") => Ok(CloneFilter::BlobNone),
        Some(v) if v.starts_with("blob:limit=") => v[11..]
            .parse()
            .map(CloneFilter::BlobLimit)
            .map_err(|_| "blob:limit requires bytes".into()),
        Some(v) if v.starts_with("path:") => Ok(CloneFilter::Paths(
            v[5..].split(',').map(str::to_owned).collect(),
        )),
        _ => Err("filter must be all, blob:none, blob:limit=N or path:PATH[,PATH]".into()),
    }
}
pub fn validate_source(source: &PromiseSource) -> RepositoryResult<()> {
    if is_remote_url(&source.location) {
        parse_url(&source.location)?;
    } else {
        Repository::open(&source.location)?;
    }
    Ok(())
}
pub fn fetch_object_source(source: &PromiseSource, id: &str) -> RepositoryResult<Vec<u8>> {
    if !valid_id(id) {
        return Err("invalid promised object ID".into());
    }
    if !is_remote_url(&source.location) {
        let repo = Repository::open(&source.location)?;
        let _gate = repo.service_lock(Duration::from_secs(15))?;
        return repo.reachable_object(id);
    }
    let remote = Remote {
        version: 1,
        name: "promisor".into(),
        url: source.location.clone(),
        allow_insecure_http: source.allow_insecure_http,
    };
    let mut raw = Vec::new();
    let mut total = None;
    loop {
        let (_, chunk) = exchange(
            &remote,
            Operation::ObjectSlice {
                id: id.into(),
                offset: raw.len() as u64,
            },
            &[],
            false,
        )?;
        if chunk.len() < 41 {
            return Err("truncated object slice".into());
        }
        let length = u64::from_be_bytes(chunk[..8].try_into().unwrap());
        if length > protocol::MAX_BUNDLE_BYTES
            || total.is_some_and(|n| n != length)
            || crate::binary::hex(&chunk[8..40]) != id
            || chunk.len() - 40 > protocol::TRANSFER_CHUNK
            || (raw.len() as u64).saturating_add((chunk.len() - 40) as u64) > length
        {
            return Err("object slice identity/length mismatch".into());
        }
        total = Some(length);
        raw.try_reserve(chunk.len() - 40)
            .map_err(|e| e.to_string())?;
        raw.extend_from_slice(&chunk[40..]);
        if raw.len() as u64 == length {
            break;
        }
    }
    if format!("{:x}", sha2::Sha256::digest(&raw)) != id {
        return Err("promised object checksum mismatch".into());
    }
    Ok(raw)
}
fn source(location: String, allow: bool) -> RepositoryResult<PromiseSource> {
    let location = if is_remote_url(&location) {
        location
    } else {
        fs::canonicalize(location)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned()
    };
    let source = PromiseSource {
        location,
        allow_insecure_http: allow,
    };
    validate_source(&source)?;
    Ok(source)
}
fn download(
    source: &PromiseSource,
    options: SelectOptions,
) -> RepositoryResult<(RepositoryState, Vec<u8>)> {
    if !is_remote_url(&source.location) {
        let (state, raw) = Repository::open(&source.location)?.selective_bundle(&options)?;
        return Ok((state, Repository::empty_network_bundle(&raw)?));
    }
    let remote = Remote {
        version: 1,
        name: "origin".into(),
        url: source.location.clone(),
        allow_insecure_http: source.allow_insecure_http,
    };
    let (response, bytes) = fetch_payload_selected(&remote, false, None, Some(options))?;
    let state = response.state.ok_or("missing selective clone state")?;
    Ok((state, bytes))
}
fn track(repo: &Repository, remote: &Remote, state: RepositoryState) -> RepositoryResult<()> {
    let _lock = config_lock(repo)?;
    atomic_record(
        repo,
        &config_dir(repo)?.join(format!("{}.tracking.bin", remote.name)),
        &Tracking {
            version: 1,
            remote: remote.name.clone(),
            url: remote.url.clone(),
            state,
        },
    )
}
pub fn run_selective(args: &[String]) -> RepositoryResult<String> {
    let parsed = match Arguments::try_parse_from(
        std::iter::once("wasm-git".to_owned()).chain(args.iter().cloned()),
    ) {
        Ok(v) => v,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => return Ok(e.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    match parsed.command {
        CommandArgs::Clone {
            source: location,
            destination,
            depth,
            branch,
            filter: spec,
            allow_insecure_http,
        } => {
            let source = source(location, allow_insecure_http)?;
            let mut options = SelectOptions {
                depth,
                branch,
                filter: filter(spec, CloneFilter::All)?,
                ..Default::default()
            };
            options.validate()?;
            let (state, bytes) = download(&source, options.clone())?;
            if Repository::bundle_state(&bytes)? != state {
                return Err("selective clone state mismatch".into());
            }
            let repo = Repository::clone_bundle_bytes(&bytes, &destination)?;
            options.depth = None;
            options.boundaries = repo.shallow_boundaries()?;
            repo.configure_promisor(vec![source.clone()], Some(options))?;
            if is_remote_url(&source.location) {
                let remote = Remote {
                    version: 1,
                    name: "origin".into(),
                    url: source.location.clone(),
                    allow_insecure_http: source.allow_insecure_http,
                };
                {
                    let _lock = config_lock(&repo)?;
                    atomic_record(&repo, &config_dir(&repo)?.join("origin.bin"), &remote)?;
                }
                track(&repo, &remote, state.clone())?;
                if let Some(tip) = state.branches.get(&state.head) {
                    repo.set_upstream_checked(&state.head, "origin", &state.head, tip, None)?;
                }
            }
            pretty(&repo.partial_status()?)
        }
        CommandArgs::Fetch {
            repository,
            remote,
            depth,
            deepen,
            unshallow,
            branch,
            filter: spec,
            allow_insecure_http,
        } => {
            let repo = Repository::open(repository)?;
            let mut options = repo.clone_selection()?.unwrap_or_default();
            if let Some(depth) = depth {
                options.depth = Some(depth);
                options.boundaries.clear();
                options.deepen = None;
            }
            if let Some(amount) = deepen {
                if amount == 0 {
                    return Err("deepen must be positive".into());
                }
                options.depth = None;
                options.boundaries = repo.shallow_boundaries()?;
                if options.boundaries.is_empty() {
                    return Err("repository is not shallow".into());
                }
                options.deepen = Some(amount);
            }
            if unshallow {
                options.depth = None;
                options.boundaries.clear();
                options.deepen = None;
            }
            if branch.is_some() {
                options.branch = branch;
            }
            options.filter = filter(spec, options.filter)?;
            options.validate()?;
            let configured = load(&repo, &remote).ok();
            let selected = if let Some(remote) = &configured {
                source(
                    remote.url.clone(),
                    allow_insecure_http || remote.allow_insecure_http,
                )?
            } else {
                repo.promisor_sources()?
                    .into_iter()
                    .next()
                    .ok_or("no promisor/remote configured")?
            };
            let (state, bytes) = download(&selected, options.clone())?;
            repo.install_selective_bundle(&bytes)?;
            options.depth = None;
            options.deepen = None;
            options.boundaries = repo.shallow_boundaries()?;
            repo.configure_promisor(vec![selected], Some(options))?;
            if let Some(remote) = configured {
                track(&repo, &remote, state)?;
            }
            pretty(&repo.partial_status()?)
        }
        CommandArgs::Promisor {
            repository,
            sources,
            allow_insecure_http,
        } => {
            let repo = Repository::open(repository)?;
            let sources = sources
                .into_iter()
                .map(|s| source(s, allow_insecure_http))
                .collect::<RepositoryResult<Vec<_>>>()?;
            repo.configure_promisor(sources, repo.clone_selection()?)?;
            pretty(&repo.partial_status()?)
        }
        CommandArgs::Hydrate { repository } => {
            pretty(&serde_json::json!({"materialized":Repository::open(repository)?.hydrate()?}))
        }
        CommandArgs::CloneStatus { repository } => {
            pretty(&Repository::open(repository)?.partial_status()?)
        }
    }
}
