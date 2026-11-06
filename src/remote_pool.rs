use super::*;
use crate::repository::{PoolConfig, PromiseSource};
use clap::{Parser, Subcommand};
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: PoolCommand,
}
#[derive(Subcommand)]
enum PoolCommand {
    Attach {
        repository: PathBuf,
        #[arg(required = true)]
        nodes: Vec<String>,
        #[arg(long)]
        allow_insecure_http: bool,
    },
    Status {
        repository: PathBuf,
    },
    Sync {
        repository: PathBuf,
        #[arg(long)]
        evict: bool,
    },
    Release {
        repository: PathBuf,
    },
}
fn remote(node: &PromiseSource) -> Remote {
    Remote {
        version: 1,
        name: "pool".into(),
        url: node.location.clone(),
        allow_insecure_http: node.allow_insecure_http,
    }
}
fn state(node: &PromiseSource) -> RepositoryResult<RepositoryState> {
    if is_remote_url(&node.location) {
        advertise(&remote(node), false)?
            .0
            .state
            .ok_or("pool node lacks state".into())
    } else {
        Repository::open(&node.location)?.advertised_state()
    }
}
fn publish(
    node: &PromiseSource,
    updates: Vec<protocol::RefUpdate>,
    bundle: &[u8],
) -> RepositoryResult<()> {
    if updates.is_empty() {
        return Ok(());
    }
    if is_remote_url(&node.location) {
        let payload = crate::binary::encode(&protocol::BatchPublication {
            updates,
            bundle: bundle.to_vec(),
        })?;
        let expected = format!("{:x}", sha2::Sha256::digest(&payload));
        let (response, body) =
            exchange(&remote(node), Operation::PublishBatch {}, &payload, false)?;
        if !body.is_empty() || response.tip.as_deref() != Some(&expected) {
            return Err("pool publication outcome unknown; local objects retained".into());
        }
        Ok(())
    } else {
        Repository::open(&node.location)?
            .receive_batch_checked(bundle, &updates, |_| Ok(()))
            .map_err(|e| e.message)
    }
}
fn sync_node(
    node: &PromiseSource,
    config: &PoolConfig,
    roots: &[String],
    bundle: &[u8],
) -> RepositoryResult<()> {
    let state = state(node)?;
    let prefix = format!("pool/{}/", &config.identity[..32]);
    let updates = roots
        .iter()
        .filter_map(|id| {
            let name = format!("{prefix}{id}");
            (state.branches.get(&name) != Some(id)).then(|| protocol::RefUpdate {
                namespace: RefNamespace::Branch,
                expected: state.branches.get(&name).cloned(),
                name,
                new: Some(id.clone()),
                force: false,
            })
        })
        .collect::<Vec<_>>();
    for chunk in updates.chunks(4096) {
        publish(node, chunk.to_vec(), bundle)?;
    }
    let verified = state_after(node)?;
    if roots
        .iter()
        .any(|id| verified.branches.get(&format!("{prefix}{id}")) != Some(id))
    {
        return Err("pool node did not retain requested pins".into());
    }
    Ok(())
}
fn state_after(node: &PromiseSource) -> RepositoryResult<RepositoryState> {
    state(node)
}
pub fn run(args: &[String]) -> RepositoryResult<String> {
    let args = match Args::try_parse_from(args) {
        Ok(v) => v,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => return Ok(e.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    match args.command {
        PoolCommand::Attach {
            repository,
            nodes,
            allow_insecure_http,
        } => {
            let nodes = nodes
                .into_iter()
                .map(|location| {
                    let location = if is_remote_url(&location) {
                        location
                    } else {
                        fs::canonicalize(location)
                            .map_err(|e| e.to_string())?
                            .to_string_lossy()
                            .into_owned()
                    };
                    Ok(PromiseSource {
                        location,
                        allow_insecure_http,
                    })
                })
                .collect::<RepositoryResult<_>>()?;
            pretty(&Repository::open(repository)?.attach_pool(nodes)?)
        }
        PoolCommand::Status { repository } => pretty(&Repository::open(repository)?.pool_config()?),
        PoolCommand::Sync { repository, evict } => {
            let repo = Repository::open(repository)?;
            let _pool_lock = repo.pool_lock()?;
            let config = repo.pool_config()?;
            let (roots, bundle) = repo.pool_publication()?;
            let receipts = crate::parallel::map_owned(config.nodes.iter().collect(), |node| {
                sync_node(node, &config, &roots, &bundle)
            });
            let errors: Vec<_> = receipts
                .into_iter()
                .enumerate()
                .filter_map(|(i, r)| r.err().map(|e| format!("node {i}: {e}")))
                .collect();
            if !errors.is_empty() {
                return Err(format!(
                    "pool replication incomplete; local data retained; {}",
                    errors.join("; ")
                ));
            }
            let removed = if evict {
                repo.evict_pool_blobs(&config, &roots.iter().cloned().collect())?
            } else {
                0
            };
            pretty(
                &serde_json::json!({"replicas":config.nodes.len(),"pinned_commits":roots.len(),"evicted_wasm_objects":removed,"identity":config.identity}),
            )
        }
        PoolCommand::Release { repository } => {
            let repo = Repository::open(repository)?;
            let _pool_lock = repo.pool_lock()?;
            let config = repo.pool_config()?;
            repo.hydrate()?;
            for node in &config.nodes {
                let state = state(node)?;
                let updates = state
                    .branches
                    .into_iter()
                    .filter(|(name, _)| {
                        name.starts_with(&format!("pool/{}/", &config.identity[..32]))
                    })
                    .map(|(name, id)| protocol::RefUpdate {
                        namespace: RefNamespace::Branch,
                        name,
                        expected: Some(id),
                        new: None,
                        force: false,
                    })
                    .collect::<Vec<_>>();
                for chunk in updates.chunks(4096) {
                    publish(node, chunk.to_vec(), &[])?;
                }
            }
            Ok("released pool pins after materializing local promised objects".into())
        }
    }
}

pub fn fork_pins(source: &Repository, destination: &Repository) -> RepositoryResult<()> {
    let original = source.pool_config()?;
    let config = destination.attach_pool(original.nodes)?;
    let roots = destination.pool_pin_roots()?;
    for node in &config.nodes {
        sync_node(node, &config, &roots, &[])?;
    }
    Ok(())
}
