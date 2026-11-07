//! Named remotes and bounded, lossless RPC over HTTP(S) and SSH.
//!
//! A fetch installs verified objects and records an advertisement separately from
//! local branches. Every push uses a compare-and-swap lease, including ordinary
//! fast-forward pushes. Authentication secrets are never stored in configuration.

use crate::protocol::{self, Operation, RefNamespace, Request, Response};
use crate::repository::{Repository, RepositoryResult, RepositoryState};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MAX_TRANSFER: usize = protocol::MAX_PACKET_BYTES;
const MAX_CONFIG: u64 = 1024 * 1024;
const MAX_TRACKING: u64 = 64 * 1024 * 1024;
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(300);
const USAGE: &str = "selective clone/fetch: --depth N, --deepen N, --unshallow, --branch NAME, --filter all|blob:none|blob:limit=N|path:PATH; clone-status DIR; hydrate DIR; promisor DIR SOURCE...; remote <DIR> [add <NAME> <URL>|remove <NAME>|rename <OLD> <NEW>|-v|show <NAME>]; fetch <DIR> [REMOTE]; push <DIR> [REMOTE] [SOURCE:DESTINATION] [-u|--set-upstream] [--force-with-lease <OLD_ID|absent>]; push <DIR> [REMOTE] --tags|--all|--mirror [--atomic]; push supports --dry-run, --force/-f, +SOURCE:DESTINATION, full-ref wildcard mappings, ^DESTINATION exclusions, : matching branches, --prune, and -o/--push-option VALUE; push <DIR> REMOTE SOURCE:DESTINATION [SOURCE:DESTINATION ...] [--atomic]; push <DIR> [REMOTE] --delete <BRANCH|refs/tags/TAG> [--force-with-lease <OLD_ID>]; pull <DIR> [REMOTE] [BRANCH] [--ff-only] [-m <MESSAGE>] (message required unless --ff-only); clone <URL> <NEW_DIR>; HTTP URLs are http(s)://HOST/REPOSITORY; SSH URLs are ssh://USER@HOST/ABSOLUTE/ROOT/REPOSITORY; --allow-insecure-http explicitly permits non-loopback cleartext HTTP";
const LS_REMOTE_USAGE: &str = "ls-remote URL [PATTERN...] [--heads|--branches] [--tags] [--symref] [--refs] [--exit-code] [--json] [--allow-insecure-http]; ls-remote DIR [REMOTE [PATTERN...]] [OPTIONS]. DIR alone reads the existing origin configuration. Patterns match full refs or slash-delimited tails with * and ?; [] and backslash escapes are unsupported. -- ends options. Only HTTP(S) and SSH advertisements are queried; no objects are downloaded or verified, and no local repository metadata is written. --refs suppresses HEAD and its symbolic target. --exit-code returns 2 when no matching object-ID reference is advertised, including an unborn HEAD even if --symref prints its target.";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[path = "remote_pull.rs"]
mod pulls;
#[path = "remote_refspec.rs"]
mod refspec;
pub fn pull_cli(args: &[String]) -> RepositoryResult<String> {
    pulls::run(args)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Remote {
    version: u32,
    name: String,
    url: String,
    allow_insecure_http: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tracking {
    version: u32,
    remote: String,
    url: String,
    pub(crate) state: RepositoryState,
}

/// A local view of an upstream. A missing remote or tracking branch is `None`,
/// while malformed metadata and unverifiable tracked objects remain errors.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamStatus {
    pub remote: String,
    pub branch: String,
    pub tip: Option<String>,
}

#[derive(Debug)]
enum Endpoint {
    Http {
        endpoint: String,
        repository: String,
        cleartext_remote: bool,
    },
    Ssh {
        host: String,
        port: Option<u16>,
        root: String,
        repository: String,
    },
}

impl Endpoint {
    fn repository(&self) -> &str {
        match self {
            Self::Http { repository, .. } | Self::Ssh { repository, .. } => repository,
        }
    }
}

pub fn is_remote_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://") || value.starts_with("ssh://")
}

fn name(value: &str) -> RepositoryResult<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || !value.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err("names must start with an ASCII letter or digit and contain at most 64 ASCII letters, digits, '-' or '_'".into());
    }
    Ok(())
}

fn branch_name(value: &str) -> RepositoryResult<()> {
    crate::repository::validate_ref_name(value).map_err(|_| "invalid remote branch name".into())
}

fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_state(state: &RepositoryState) -> RepositoryResult<()> {
    if state.format != 1 {
        return Err("unsupported remote repository state version".into());
    }
    crate::repository::validate_head_identity(&state.head)?;
    for (reference, id) in state.branches.iter().chain(state.tags.iter()) {
        branch_name(reference)?;
        if !valid_id(id) {
            return Err("remote state contains an invalid commit ID".into());
        }
    }
    Ok(())
}

fn authority(value: &str, ssh: bool) -> RepositoryResult<(String, Option<u16>, bool)> {
    let (user, address) = if let Some((user, host)) = value.split_once('@') {
        if !ssh
            || user.is_empty()
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || user.starts_with('-')
        {
            return Err("invalid URL user or HTTP user information".into());
        }
        (Some(user), host)
    } else {
        (None, value)
    };
    let (host, port) = if address.starts_with('[') {
        let end = address.find(']').ok_or("unterminated IPv6 URL host")?;
        let host = &address[1..end];
        if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err("invalid IPv6 URL host".into());
        }
        let rest = &address[end + 1..];
        let port = if rest.is_empty() {
            None
        } else {
            Some(rest.strip_prefix(':').ok_or("invalid URL authority")?)
        };
        (host, port)
    } else {
        let (host, port) = address
            .split_once(':')
            .map_or((address, None), |(h, p)| (h, Some(p)));
        if host.is_empty()
            || host.starts_with('-')
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return Err("invalid URL host".into());
        }
        (host, port)
    };
    let port = port
        .map(|p| {
            p.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| "invalid URL port".to_owned())
        })
        .transpose()?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let display_host = if host.contains(':') && !ssh {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok((
        user.map_or(display_host.clone(), |u| format!("{u}@{display_host}")),
        port,
        loopback,
    ))
}

fn parse_url(value: &str) -> RepositoryResult<Endpoint> {
    if value.bytes().any(|b| b <= b' ' || b == 0x7f) || value.contains(['?', '#', '\\', '%']) {
        return Err(
            "remote URL contains unsupported characters, escaping, query or fragment".into(),
        );
    }
    let (scheme, rest) = value.split_once("://").ok_or("remote URL needs a scheme")?;
    let (authority_text, path) = rest
        .split_once('/')
        .ok_or("remote URL needs a repository path")?;
    match scheme {
        "http" | "https" => {
            protocol::repository_name(path)?;
            let (host, port, loopback) = authority(authority_text, false)?;
            let authority = port.map_or(host.clone(), |p| format!("{host}:{p}"));
            Ok(Endpoint::Http {
                endpoint: format!("{scheme}://{authority}/v1/wasm-git"),
                repository: path.to_owned(),
                cleartext_remote: scheme == "http" && !loopback,
            })
        }
        "ssh" => {
            let (host, port, _) = authority(authority_text, true)?;
            let (root, repository) = path
                .split_once("//")
                .or_else(|| path.rsplit_once('/'))
                .ok_or("SSH URL needs an absolute server root and repository")?;
            protocol::repository_name(repository)?;
            if root
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            {
                return Err("SSH server root must have ordinary absolute path components".into());
            }
            Ok(Endpoint::Ssh {
                host,
                port,
                root: format!("/{root}"),
                repository: repository.to_owned(),
            })
        }
        _ => Err("supported remote URL schemes are http, https and ssh".into()),
    }
}

fn regular(path: &Path) -> RepositoryResult<Vec<u8>> {
    let limit = if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".tracking.bin"))
    {
        MAX_TRACKING
    } else {
        MAX_CONFIG
    };
    let metadata = fs::symlink_metadata(path).map_err(|e| format!("read remote metadata: {e}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err("remote metadata must be a bounded regular file".into());
    }
    let file = File::open(path).map_err(|error| format!("open remote metadata: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("inspect opened remote metadata: {error}"))?
        .is_file()
    {
        return Err("opened remote metadata must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read remote metadata: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err("remote metadata exceeds its configured size limit".into());
    }
    Ok(bytes)
}

fn directory(path: &Path) -> RepositoryResult<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else {
                Err("remote metadata directory must not be a symlink".into())
            }
        }
        Err(e) => Err(format!("create remote metadata directory: {e}")),
    }
}

struct ConfigLock(PathBuf, File);
impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = self.1.sync_all();
        let _ = fs::remove_file(&self.0);
    }
}

fn config_dir(repo: &Repository) -> RepositoryResult<PathBuf> {
    let path = repo.common_metadata().join("remotes");
    directory(&path)?;
    Ok(path)
}

fn config_lock(repo: &Repository) -> RepositoryResult<ConfigLock> {
    let path = repo.common_metadata().join("remote.lock");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            format!(
                "cannot lock remote configuration; inspect .wasm-git/remote.lock before retrying: {e}"
            )
        })?;
    Ok(ConfigLock(path, file))
}

fn load(repo: &Repository, remote: &str) -> RepositoryResult<Remote> {
    name(remote)?;
    let directory =
        existing_config_dir(repo)?.ok_or("remote configuration directory does not exist")?;
    let config: Remote = crate::binary::decode(&regular(&directory.join(format!("{remote}.bin")))?)
        .map_err(|e| format!("invalid remote configuration: {e}"))?;
    if config.version != 1 || config.name != remote {
        return Err("remote configuration version or name mismatch".into());
    }
    parse_url(&config.url)?;
    Ok(config)
}

fn optional_tracking(repo: &Repository, remote: &Remote) -> RepositoryResult<Option<Tracking>> {
    let Some(directory) = existing_config_dir(repo)? else {
        return Ok(None);
    };
    let path = directory.join(format!("{}.tracking.bin", remote.name));
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read remote tracking: {e}")),
        Ok(_) => {
            let tracking: Tracking = crate::binary::decode(&regular(&path)?)
                .map_err(|e| format!("invalid remote tracking: {e}"))?;
            if tracking.version != 1 || tracking.remote != remote.name || tracking.url != remote.url
            {
                return Err("remote tracking identity mismatch".into());
            }
            validate_state(&tracking.state)?;
            Ok(Some(tracking))
        }
    }
}

fn tracking_spec(specification: &str) -> RepositoryResult<(&str, &str)> {
    let (remote, branch) = specification
        .split_once('/')
        .ok_or("upstream must be REMOTE/BRANCH from a previously fetched remote")?;
    name(remote)?;
    branch_name(branch)?;
    Ok((remote, branch))
}

fn existing_config_dir(repo: &Repository) -> RepositoryResult<Option<PathBuf>> {
    let path = repo.common_metadata().join("remotes");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("inspect remote configuration directory: {error}")),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(Some(path)),
        Ok(_) => Err("remote metadata directory must be a regular directory".into()),
    }
}

fn load_readonly(repo: &Repository, remote: &str) -> RepositoryResult<Remote> {
    name(remote)?;
    let directory = existing_config_dir(repo)?.ok_or("remote has not been configured")?;
    let config: Remote = crate::binary::decode(&regular(&directory.join(format!("{remote}.bin")))?)
        .map_err(|error| format!("invalid remote configuration: {error}"))?;
    if config.version != 1 || config.name != remote {
        return Err("remote configuration version or name mismatch".into());
    }
    parse_url(&config.url)?;
    Ok(config)
}

/// Inspect one advertisement without fetching or creating local metadata. The
/// reference IDs remain unverified claims until a separate object download.
pub fn ls_remote_query(args: &[String]) -> RepositoryResult<crate::query_commands::QueryOutput> {
    use crate::query_commands::{tail_ref_match, validate_ref_pattern, QueryOutput};

    if crate::command_args::help_requested(args) {
        return Ok(QueryOutput::new(format!("{LS_REMOTE_USAGE}\n"), 0));
    }
    if args.first().map(String::as_str) != Some("ls-remote") {
        return Err(LS_REMOTE_USAGE.into());
    }
    let mut flags = BTreeSet::new();
    let mut operands = Vec::new();
    let mut literal = false;
    for arg in args.iter().skip(1) {
        if !literal && arg == "--" {
            literal = true;
        } else if !literal && arg.starts_with('-') {
            let flag = match arg.as_str() {
                "--branches" => "--heads",
                "--heads"
                | "--tags"
                | "--symref"
                | "--refs"
                | "--exit-code"
                | "--json"
                | "--allow-insecure-http" => arg.as_str(),
                _ => return Err(LS_REMOTE_USAGE.into()),
            };
            if !flags.insert(flag) {
                return Err(format!("ls-remote option cannot be repeated: {flag}"));
            }
        } else {
            operands.push(arg.as_str());
        }
    }
    let target = operands.first().ok_or(LS_REMOTE_USAGE)?;
    let direct = is_remote_url(target);
    if !direct && target.contains("://") {
        return Err("ls-remote URL schemes are http, https and ssh".into());
    }
    let patterns = if direct {
        &operands[1..]
    } else {
        operands.get(2..).unwrap_or_default()
    };
    for pattern in patterns {
        validate_ref_pattern(pattern)?;
    }
    let (remote, configured_name) = if direct {
        parse_url(target)?;
        (
            Remote {
                version: 1,
                name: "advertisement".into(),
                url: (*target).into(),
                allow_insecure_http: false,
            },
            None,
        )
    } else {
        let remote_name = operands.get(1).copied().unwrap_or("origin");
        name(remote_name)?;
        let repo = Repository::open(target)?;
        (load_readonly(&repo, remote_name)?, Some(remote_name))
    };
    let (response, payload) = advertise(&remote, flags.contains("--allow-insecure-http"))?;
    if !payload.is_empty() {
        return Err("ls-remote advertisement unexpectedly contains an object payload".into());
    }
    if response.tip.is_some() {
        return Err("ls-remote advertisement unexpectedly contains an update tip".into());
    }
    let state = response
        .state
        .ok_or("advertisement lacks repository state")?;
    validate_state(&state)?;
    let matches = |reference: &str| -> RepositoryResult<bool> {
        if patterns.is_empty() {
            return Ok(true);
        }
        for pattern in patterns {
            if tail_ref_match(reference, pattern)? {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let heads = flags.contains("--heads");
    let tags = flags.contains("--tags");
    let all = !heads && !tags;
    let include_head = all && !flags.contains("--refs") && matches("HEAD")?;
    let head_tip = state.tip();
    let symbolic = include_head && flags.contains("--symref") && !state.detached();
    let mut references = Vec::new();
    let mut text = String::new();
    let symbolic_target = format!("refs/heads/{}", state.head);
    if symbolic {
        text.push_str(&format!("ref: {symbolic_target}\tHEAD\n"));
    }
    if include_head {
        if let Some(id) = head_tip {
            text.push_str(&format!("{id}\tHEAD\n"));
            references.push(serde_json::json!({"name":"HEAD","id":id}));
        }
    }
    for (prefix, selected, entries) in [
        ("refs/heads/", all || heads, &state.branches),
        ("refs/tags/", all || tags, &state.tags),
    ] {
        if selected {
            for (name, id) in entries {
                let reference = format!("{prefix}{name}");
                if matches(&reference)? {
                    text.push_str(&format!("{id}\t{reference}\n"));
                    references.push(serde_json::json!({"name":reference,"id":id}));
                }
            }
        }
    }
    let exit_code = if flags.contains("--exit-code") && references.is_empty() {
        2
    } else {
        0
    };
    if flags.contains("--json") {
        let mut output = QueryOutput::json(serde_json::json!({
            "schema_version":1,
            "command":"ls-remote",
            "remote":configured_name,
            "url":remote.url,
            "advertised_only":true,
            "objects_verified":false,
            "unverified_objects":true,
            "head_branch":state.head,
            "head_tip":head_tip,
            "references":references,
            "symbolic_refs":if symbolic {
                vec![serde_json::json!({"name":"HEAD","target":symbolic_target})]
            } else { Vec::new() }
        }))?;
        output.exit_code = exit_code;
        Ok(output)
    } else {
        Ok(QueryOutput::new(text, exit_code))
    }
}

fn verified_tracking_tip(
    repo: &Repository,
    remote: &Remote,
    branch: &str,
) -> RepositoryResult<Option<String>> {
    let Some(tracking) = optional_tracking(repo, remote)? else {
        return Ok(None);
    };
    let Some(tip) = tracking.state.branches.get(branch) else {
        return Ok(None);
    };
    if repo.resolve(&format!("{}/{branch}", remote.name))? != *tip {
        return Err("verified remote tracking tip changed".into());
    }
    Ok(Some(tip.clone()))
}

/// Called while a repository snapshot is already locked. Verify the same
/// remote/tracking identity without recursively acquiring the writer lock.
pub(crate) fn snapshot_upstream_tip(
    repo: &Repository,
    upstream: &crate::repository::Upstream,
    expected: Option<&String>,
) -> RepositoryResult<String> {
    if existing_config_dir(repo)?.is_none() {
        return Err("upstream has no configured remote".into());
    }
    let remote = load(repo, &upstream.remote)?;
    let tracking = optional_tracking(repo, &remote)?.ok_or("upstream has no fetched branch")?;
    let tip = tracking
        .state
        .branches
        .get(&upstream.branch)
        .ok_or("upstream has no fetched branch")?;
    if expected != Some(tip) {
        return Err("upstream tracking changed while capturing the repository snapshot".into());
    }
    Ok(tip.clone())
}

/// The lock order is remote configuration, then the repository writer. No
/// network request is made while configuring an already fetched upstream.
pub fn configure_upstream(
    repo: &Repository,
    local: &str,
    specification: &str,
) -> RepositoryResult<()> {
    branch_name(local)?;
    let (remote_name, branch) = tracking_spec(specification)?;
    let _lock = config_lock(repo)?;
    let (state, upstreams) = repo.state_with_upstreams()?;
    let tip = state
        .branches
        .get(local)
        .ok_or("upstream requires a committed local branch")?;
    existing_config_dir(repo)?.ok_or("remote has not been configured")?;
    let remote = load(repo, remote_name)?;
    verified_tracking_tip(repo, &remote, branch)?
        .ok_or("upstream branch has not been fetched or is gone; fetch it before configuring")?;
    repo.set_upstream_checked(local, remote_name, branch, tip, upstreams.get(local))?;
    Ok(())
}

pub fn unset_upstream(repo: &Repository, local: &str) -> RepositoryResult<()> {
    branch_name(local)?;
    let _lock = config_lock(repo)?;
    let (state, upstreams) = repo.state_with_upstreams()?;
    let tip = state
        .branches
        .get(local)
        .ok_or("upstream requires a committed local branch")?;
    let previous = upstreams.get(local).ok_or("local branch has no upstream")?;
    repo.unset_upstream_checked(local, tip, previous)?;
    Ok(())
}

pub fn upstream_status(repo: &Repository, local: &str) -> RepositoryResult<Option<UpstreamStatus>> {
    branch_name(local)?;
    let _lock = config_lock(repo)?;
    let (state, upstreams) = repo.state_with_upstreams()?;
    if !state.branches.contains_key(local) && state.head != local {
        return Err("local branch does not exist".into());
    }
    let Some(upstream) = upstreams.get(local) else {
        return Ok(None);
    };
    let path = existing_config_dir(repo)?
        .map(|directory| directory.join(format!("{}.bin", upstream.remote)));
    let tip = match path.map(fs::symlink_metadata).transpose() {
        Ok(None) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("inspect upstream remote: {error}")),
        Ok(Some(_)) => {
            verified_tracking_tip(repo, &load(repo, &upstream.remote)?, &upstream.branch)?
        }
    };
    Ok(Some(UpstreamStatus {
        remote: upstream.remote.clone(),
        branch: upstream.branch.clone(),
        tip,
    }))
}

fn atomic_record<T: Serialize>(repo: &Repository, path: &Path, value: &T) -> RepositoryResult<()> {
    let bytes = crate::binary::encode(value).map_err(|e| format!("encode remote metadata: {e}"))?;
    let temp = repo.common_metadata().join("tmp").join(unique("remote"));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("create remote metadata: {e}"))?;
        file.write_all(&bytes)
            .map_err(|e| format!("write remote metadata: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync remote metadata: {e}"))?;
        // Replacing a symlink is safe, but silently accepting corrupt metadata is
        // not. Existing metadata must pass the same regular-file boundary.
        match fs::symlink_metadata(path) {
            Ok(_) => {
                regular(path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("inspect remote metadata: {e}")),
        }
        fs::rename(&temp, path).map_err(|e| format!("publish remote metadata: {e}"))?;
        crate::output::sync_directory(path.parent().ok_or("missing remote metadata parent")?)
    })();
    let _ = fs::remove_file(temp);
    result
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        crate::platform_fs::process_id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

struct SecretHeader(PathBuf);
impl Drop for SecretHeader {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn secret_header() -> RepositoryResult<Option<SecretHeader>> {
    let Some(token) = std::env::var_os("WASM_GIT_REMOTE_TOKEN") else {
        return Ok(None);
    };
    let token = token
        .into_string()
        .map_err(|_| "remote token must be UTF-8")?;
    if token.is_empty() || token.len() > 4096 || token.bytes().any(|b| !(33..=126).contains(&b)) {
        return Err("remote token must be 1..4096 printable ASCII bytes without whitespace".into());
    }
    let path = std::env::temp_dir().join(unique("wasm-git-http-header"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| format!("create private authentication header: {e}"))?;
    let guard = SecretHeader(path);
    file.write_all(format!("Authorization: Bearer {token}\n").as_bytes())
        .map_err(|e| format!("write private authentication header: {e}"))?;
    Ok(Some(guard))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn rpc_timeout(value: Option<&str>) -> RepositoryResult<Duration> {
    match value {
        None => Ok(DEFAULT_RPC_TIMEOUT),
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=3600).contains(n))
            .map(Duration::from_secs)
            .ok_or_else(|| "WASM_GIT_RPC_TIMEOUT_SECONDS must be an integer in 1..=3600".into()),
    }
}

fn subprocess(
    command: Command,
    input: Vec<u8>,
    label: &str,
    timeout: Duration,
) -> RepositoryResult<Vec<u8>> {
    subprocess_deadline(command, input, label, timeout + Duration::from_secs(5))
}

/// The budget covers process exit and all three pipes. In particular, a proxy
/// process inheriting stdout must not turn joining a reader into an unbounded
/// wait after the immediate curl/ssh process has already exited. Pipe workers
/// report through a channel; an expired call drops their handles rather than
/// waiting for an untrusted descendant to close inherited descriptors.
fn subprocess_deadline(
    mut command: Command,
    input: Vec<u8>,
    label: &str,
    timeout: Duration,
) -> RepositoryResult<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    enum Completed {
        Input(std::io::Result<()>),
        Output(std::io::Result<Vec<u8>>),
        Errors(std::io::Result<Vec<u8>>),
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {label} transport: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("missing transport stdin")?;
    let stdout = child.stdout.take().ok_or("missing transport stdout")?;
    let stderr = child.stderr.take().ok_or("missing transport stderr")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let writer = sender.clone();
    std::thread::spawn(move || {
        let _ = writer.send(Completed::Input(stdin.write_all(&input)));
    });
    let reader = sender.clone();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((MAX_TRANSFER + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = reader.send(Completed::Output(result));
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.take(65537).read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(Completed::Errors(result));
    });
    let mut status = None;
    let mut written = None;
    let mut bytes = None;
    let mut errors_finished = false;
    let result = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => break Err(format!("wait for {label} transport: {error}")),
            }
        }
        if let Some(status) = status {
            if !status.success() {
                break Err(format!("{label} transport exited unsuccessfully ({status}); check endpoint, authentication and transport configuration"));
            }
            if errors_finished && written.is_some() && bytes.is_some() {
                match written.take().ok_or("missing transport writer result")? {
                    Ok(()) => break Ok(bytes.take().ok_or("missing transport response")?),
                    Err(error) => break Err(format!("write {label} transport: {error}")),
                }
            }
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|value| !value.is_zero())
        else {
            break Err(format!(
                "{label} transport exceeded its {} millisecond process-and-pipe deadline",
                timeout.as_millis()
            ));
        };
        match receiver.recv_timeout(remaining.min(Duration::from_millis(20))) {
            Ok(Completed::Input(result)) => written = Some(result),
            Ok(Completed::Output(result)) => match result {
                Ok(output) if output.len() <= MAX_TRANSFER => bytes = Some(output),
                Ok(_) => break Err("remote response exceeds 64 MiB limit".into()),
                Err(error) => break Err(format!("read {label} transport: {error}")),
            },
            Ok(Completed::Errors(result)) => {
                if let Err(error) = result {
                    break Err(format!("read {label} transport errors: {error}"));
                }
                errors_finished = true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) if status.is_some() => {
                break Err("transport pipe workers ended without a complete response".into())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(remaining.min(Duration::from_millis(20)))
            }
        }
    };
    if status.is_none() {
        let _ = child.kill();
        // Reaping is independent of the RPC budget. No reader thread or wait on
        // a potentially stalled child can extend the caller's deadline.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    result
}

fn exchange(
    remote: &Remote,
    operation: Operation,
    payload: &[u8],
    allow_http: bool,
) -> RepositoryResult<(Response, Vec<u8>)> {
    let configured_timeout = std::env::var("WASM_GIT_RPC_TIMEOUT_SECONDS")
        .map(Some)
        .or_else(|e| {
            if matches!(e, std::env::VarError::NotPresent) {
                Ok(None)
            } else {
                Err(e.to_string())
            }
        })?;
    let timeout = rpc_timeout(configured_timeout.as_deref())?;
    if payload.len() > MAX_TRANSFER / 2
        && matches!(
            &operation,
            Operation::Push { .. }
                | Operation::UpdateRef { .. }
                | Operation::PublishBatch {}
                | Operation::PublishOptions {}
                | Operation::CheckBatch {}
                | Operation::PublishPartial { .. }
        )
    {
        if payload.len() as u64 > protocol::MAX_BUNDLE_BYTES {
            return Err("bundle exceeds the 4 GiB transfer limit".into());
        }
        let transfer = format!("{:x}", sha2::Sha256::digest(payload));
        for (i, chunk) in payload.chunks(protocol::TRANSFER_CHUNK).enumerate() {
            exchange(
                remote,
                Operation::UploadSlice {
                    transfer: transfer.clone(),
                    total: payload.len() as u64,
                    offset: (i * protocol::TRANSFER_CHUNK) as u64,
                },
                chunk,
                allow_http,
            )?;
        }
        return exchange(
            remote,
            Operation::ApplyUpload {
                transfer,
                operation: Box::new(operation),
            },
            &[],
            allow_http,
        );
    }
    let endpoint = parse_url(&remote.url)?;
    let publishes = (matches!(&operation, Operation::PullRequest {})
        && crate::binary::decode::<crate::pull::PullCommand>(payload).is_ok_and(|p| p.mutates()))
        || matches!(
            &operation,
            Operation::Push { .. }
                | Operation::UpdateRef { .. }
                | Operation::PublishBatch {}
                | Operation::PublishOptions {}
                | Operation::PublishPartial { dry_run: false }
                | Operation::Fork { .. }
        )
        || matches!(&operation, Operation::ApplyUpload { operation, .. } if !matches!(**operation, Operation::CheckBatch {} | Operation::PublishPartial { dry_run: true }));
    let uncertain = |error: String| {
        if publishes {
            format!("remote publication outcome is unknown: {error}; inspect the remote before retrying")
        } else {
            error
        }
    };
    let request = Request {
        version: 1,
        repository: endpoint.repository().to_owned(),
        operation,
    };
    let mut encoded = Vec::new();
    protocol::write_packet(&mut encoded, &request, payload)?;
    if encoded.len() > MAX_TRANSFER {
        return Err("remote request exceeds 64 MiB limit".into());
    }
    let bytes = match endpoint {
        Endpoint::Http {
            endpoint,
            cleartext_remote,
            ..
        } => {
            if cleartext_remote && !(allow_http || remote.allow_insecure_http) {
                return Err("non-loopback HTTP requires --allow-insecure-http; use HTTPS to protect credentials and repository bytes".into());
            }
            let header = secret_header()?;
            let mut command = Command::new("curl");
            let timeout_text = timeout.as_secs().to_string();
            command.args([
                "-q",
                "--noproxy",
                "*",
                "--silent",
                "--show-error",
                "--fail-with-body",
                "--proto",
                "=http,https",
                "--connect-timeout",
                "10",
                "--max-time",
                &timeout_text,
                "--max-filesize",
                "67108864",
                "--request",
                "POST",
                "--header",
                "Content-Type: application/octet-stream",
                "--header",
                "Expect:",
                "--data-binary",
                "@-",
            ]);
            if let Some(header) = &header {
                command
                    .arg("--header")
                    .arg(format!("@{}", header.0.display()));
            }
            command
                .arg("--url")
                .arg(endpoint)
                .env_remove("WASM_GIT_REMOTE_TOKEN");
            subprocess(command, encoded, "HTTP", timeout).map_err(uncertain)?
        }
        Endpoint::Ssh {
            host, port, root, ..
        } => {
            let executable =
                std::env::var_os("WASM_GIT_SSH_PROGRAM").unwrap_or_else(|| "ssh".into());
            let program = std::env::var("WASM_GIT_REMOTE_PROGRAM")
                .unwrap_or_else(|_| "wasm-git-server".into());
            if program.is_empty() || program.contains(['\0', '\r', '\n']) {
                return Err("invalid remote executable path".into());
            }
            let mut command = Command::new(executable);
            command.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
            if let Some(port) = port {
                command.arg("-p").arg(port.to_string());
            }
            command
                .arg("--")
                .arg(host)
                .arg(format!(
                    "{} server --stdio --root {}",
                    shell_quote(&program),
                    shell_quote(&root)
                ))
                .env_remove("WASM_GIT_REMOTE_TOKEN");
            subprocess(command, encoded, "SSH", timeout).map_err(uncertain)?
        }
    };
    let mut cursor = std::io::Cursor::new(&bytes);
    let (response, payload): (Response, Vec<u8>) =
        protocol::read_packet(&mut cursor).map_err(uncertain)?;
    if cursor.position() != bytes.len() as u64 {
        return Err(uncertain(
            "remote response has trailing bytes after its single RPC frame".into(),
        ));
    }
    if response.version != 1 {
        return Err(uncertain("unsupported remote response version".into()));
    }
    if response.publication_unknown {
        let reason = if response.ok {
            "successful response illegally marks publication as unknown"
        } else {
            response
                .error
                .as_deref()
                .unwrap_or("unspecified publication failure")
        };
        return Err(format!("remote publication outcome is unknown: {reason}"));
    }
    if !response.ok {
        return Err(format!(
            "remote rejected request: {}",
            response.error.as_deref().unwrap_or("unspecified error")
        ));
    }
    if response.error.is_some() {
        return Err(uncertain(
            "successful remote response contains an error".into(),
        ));
    }
    Ok((response, payload))
}

fn fetch_payload(
    remote: &Remote,
    allow_http: bool,
    repo: Option<&Repository>,
) -> RepositoryResult<(Response, Vec<u8>)> {
    let selection = repo.map(Repository::clone_selection).transpose()?.flatten();
    fetch_payload_selected(remote, allow_http, repo, selection)
}

fn fetch_payload_selected(
    remote: &Remote,
    allow_http: bool,
    repo: Option<&Repository>,
    selection: Option<crate::repository::SelectOptions>,
) -> RepositoryResult<(Response, Vec<u8>)> {
    let options = crate::binary::encode(&crate::repository::FetchOptions {
        selection,
        have: repo
            .map(Repository::negotiation_haves)
            .transpose()?
            .unwrap_or_default(),
    })?;
    match exchange(remote, Operation::Fetch {}, &options, allow_http) {
        Ok(result) => {
            return if repo.is_some() {
                Ok(result)
            } else {
                complete_fetch(result, None)
            }
        }
        Err(error) if error.contains("RPC packet exceeds 64 MiB") => (),
        Err(error) => return Err(error),
    }
    let mut bytes = Vec::new();
    let mut expected: Option<String> = None;
    let mut total = None;
    let mut first = None;
    loop {
        let (response, chunk) = exchange(
            remote,
            Operation::FetchSlice {
                offset: bytes.len() as u64,
                expected: expected.clone(),
            },
            &options,
            allow_http,
        )?;
        if chunk.len() < 40 {
            return Err("truncated sliced bundle header".into());
        }
        let length = u64::from_be_bytes(chunk[..8].try_into().unwrap());
        let id = crate::binary::hex(&chunk[8..40]);
        if length > protocol::MAX_BUNDLE_BYTES
            || total.is_some_and(|n| n != length)
            || expected.as_ref().is_some_and(|previous| previous != &id)
        {
            return Err("sliced bundle identity or length changed".into());
        }
        if first.is_none() {
            first = Some(response);
            total = Some(length);
            expected = Some(id);
        }
        let payload = &chunk[40..];
        if payload.is_empty()
            || payload.len() > protocol::TRANSFER_CHUNK
            || (bytes.len() as u64)
                .checked_add(payload.len() as u64)
                .is_none_or(|n| n > length)
        {
            return Err("invalid sliced bundle bounds".into());
        }
        bytes
            .try_reserve(payload.len())
            .map_err(|e| e.to_string())?;
        bytes.extend_from_slice(payload);
        if bytes.len() as u64 == length {
            break;
        }
    }
    if format!("{:x}", sha2::Sha256::digest(&bytes)) != expected.unwrap() {
        return Err("sliced bundle checksum mismatch".into());
    }
    let result = (first.ok_or("missing sliced advertisement")?, bytes);
    if repo.is_some() {
        Ok(result)
    } else {
        complete_fetch(result, None)
    }
}

fn complete_fetch(
    (mut response, payload): (Response, Vec<u8>),
    repo: Option<&Repository>,
) -> RepositoryResult<(Response, Vec<u8>)> {
    if !payload.starts_with(b"WGTNET01") {
        return Ok((response, payload));
    }
    let bytes = match repo {
        Some(repo) => repo.complete_network_bundle(&payload)?,
        None => Repository::empty_network_bundle(&payload)?,
    };
    let state = Repository::bundle_state(&bytes)?;
    if response.state.as_ref().is_some_and(|s| s != &state) {
        return Err("negotiated advertisement does not match verified bundle".into());
    }
    response.state = Some(state);
    Ok((response, bytes))
}

fn advertise(remote: &Remote, allow_http: bool) -> RepositoryResult<(Response, Vec<u8>)> {
    match exchange(remote, Operation::Advertise {}, &[], allow_http) {
        Ok(result) => return Ok(result),
        Err(error) if error.contains("RPC metadata length") => (),
        Err(error) => return Err(error),
    }
    let mut after = None;
    let mut expected = None;
    let mut state: Option<RepositoryState> = None;
    let mut received = 0u64;
    loop {
        let (mut response, payload) = exchange(
            remote,
            Operation::RefsPage {
                after: after.clone(),
                expected: expected.clone(),
            },
            &[],
            allow_http,
        )?;
        received = received.saturating_add(payload.len() as u64);
        if received > MAX_TRACKING {
            return Err("reference advertisement exceeds 64 MiB".into());
        }
        let page: protocol::RefPage = crate::binary::decode(&payload)?;
        if expected.as_ref().is_some_and(|e| e != &page.generation)
            || page.state.branches.len() + page.state.tags.len() > 256
        {
            return Err("invalid reference page generation or size".into());
        }
        expected = Some(page.generation);
        if let Some(state) = &mut state {
            if state.head != page.state.head || state.format != page.state.format {
                return Err("reference page HEAD changed".into());
            }
            for (name, id) in page.state.branches {
                if state.branches.insert(name, id).is_some() {
                    return Err("duplicate paged branch".into());
                }
            }
            for (name, id) in page.state.tags {
                if state.tags.insert(name, id).is_some() {
                    return Err("duplicate paged tag".into());
                }
            }
        } else {
            state = Some(page.state);
        }
        if page.next.is_none() {
            let digest = format!(
                "{:x}",
                sha2::Sha256::digest(crate::binary::encode(
                    state.as_ref().ok_or("empty reference pages")?
                )?)
            );
            if expected.as_ref() != Some(&digest) {
                return Err("paged reference snapshot digest mismatch".into());
            }
            response.state = state;
            return Ok((response, Vec::new()));
        }
        if page
            .next
            .as_ref()
            .is_none_or(|next| after.as_ref().is_some_and(|a| next <= a))
        {
            return Err("reference page cursor did not advance".into());
        }
        after = page.next;
    }
}

fn fetch(
    repo: &Repository,
    remote: &Remote,
    allow_http: bool,
) -> RepositoryResult<RepositoryState> {
    let baseline = {
        let _lock = config_lock(repo)?;
        if load(repo, &remote.name)? != *remote {
            return Err("remote configuration changed before fetching; retry fetch".into());
        }
        optional_tracking(repo, remote)?
    };
    let (response, payload) = fetch_payload(remote, allow_http, Some(repo))?;
    if payload.is_empty() {
        return Err("fetch response lacks its verified bundle".into());
    }
    if !payload.starts_with(b"WGTNET01") && response.state.is_none() {
        return Err("fetch response lacks repository state".into());
    }
    let installed = if repo.clone_selection()?.is_some() {
        repo.install_selective_bundle(&payload)?
    } else {
        repo.install_network_bundle(&payload, response.state.as_ref())?
    };
    let _lock = config_lock(repo)?;
    if load(repo, &remote.name)? != *remote {
        return Err("remote configuration changed while fetching; objects were installed but tracking was not updated".into());
    }
    // Compare the complete entry, including absence, rather than commit ancestry:
    // a separate fetch may legitimately observe a forced remote rewind.
    if optional_tracking(repo, remote)? != baseline {
        return Err("remote tracking changed while fetching; objects were installed but tracking was not updated; retry fetch".into());
    }
    let tracking = Tracking {
        version: 1,
        remote: remote.name.clone(),
        url: remote.url.clone(),
        state: installed.clone(),
    };
    atomic_record(
        repo,
        &config_dir(repo)?.join(format!("{}.tracking.bin", remote.name)),
        &tracking,
    )?;
    Ok(installed)
}

fn pretty<T: Serialize>(value: &T) -> RepositoryResult<String> {
    serde_json::to_string_pretty(value).map_err(|e| format!("encode remote result: {e}"))
}

fn reference(value: &str) -> RepositoryResult<(RefNamespace, &str)> {
    let (namespace, value) = if let Some(value) = value.strip_prefix("refs/tags/") {
        (RefNamespace::Tag, value)
    } else {
        (
            RefNamespace::Branch,
            value.strip_prefix("refs/heads/").unwrap_or(value),
        )
    };
    branch_name(value)?;
    Ok((namespace, value))
}

fn remote_tip(
    state: &RepositoryState,
    namespace: &RefNamespace,
    reference: &str,
) -> Option<String> {
    match namespace {
        RefNamespace::Branch => state.branches.get(reference),
        RefNamespace::Tag => state.tags.get(reference),
    }
    .cloned()
}

fn lease(value: &str) -> RepositoryResult<Option<String>> {
    if value == "absent" {
        return Ok(None);
    }
    if !valid_id(value) {
        return Err("lease must be a complete lowercase commit ID or 'absent'".into());
    }
    Ok(Some(value.to_owned()))
}

#[derive(Clone, Copy)]
struct PushMode {
    atomic: bool,
    dry_run: bool,
    allow_http: bool,
}

struct RefChange<'a> {
    namespace: RefNamespace,
    reference: &'a str,
    expected: Option<String>,
    new: Option<&'a str>,
    force: bool,
}

fn update_ref(
    repo: &Repository,
    remote: &Remote,
    change: RefChange<'_>,
    have: &[String],
    options: &[String],
    mode: PushMode,
) -> RepositoryResult<()> {
    let RefChange {
        namespace,
        reference,
        expected,
        new,
        force,
    } = change;
    let bundle = if let Some(id) = new {
        repo.network_bundle(&[id.to_owned()], have)?.1
    } else {
        Vec::new()
    };
    if !options.is_empty() || mode.dry_run {
        return send_publication(
            remote,
            protocol::BatchPublication {
                updates: vec![protocol::RefUpdate {
                    namespace,
                    name: reference.into(),
                    expected,
                    new: new.map(str::to_owned),
                    force,
                }],
                bundle,
            },
            options,
            mode,
        );
    }
    let operation = if matches!(namespace, RefNamespace::Branch) && new.is_some() {
        // The original branch operation remains wire-compatible.
        Operation::Push {
            branch: reference.to_owned(),
            expected,
            force,
        }
    } else {
        Operation::UpdateRef {
            namespace,
            name: reference.to_owned(),
            expected,
            new: new.map(str::to_owned),
            force,
        }
    };
    let (response, payload) = exchange(remote, operation, &bundle, mode.allow_http)?;
    if !payload.is_empty() || response.tip.as_deref() != new {
        return Err("remote acknowledgement does not match the requested exact ref update; publication outcome is unknown, inspect the remote before retrying".into());
    }
    Ok(())
}

fn publish_atomic(
    repo: &Repository,
    remote: &Remote,
    updates: Vec<protocol::RefUpdate>,
    state: &RepositoryState,
    options: &[String],
    mode: PushMode,
) -> RepositoryResult<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let wants: Vec<_> = updates
        .iter()
        .filter_map(|u| u.new.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let have: Vec<_> = state
        .branches
        .values()
        .chain(state.tags.values())
        .cloned()
        .collect();
    let bundle = if wants.is_empty() {
        Vec::new()
    } else {
        repo.network_bundle(&wants, &have)?.1
    };
    send_publication(
        remote,
        protocol::BatchPublication { updates, bundle },
        options,
        mode,
    )
}

fn send_publication(
    remote: &Remote,
    publication: protocol::BatchPublication,
    options: &[String],
    mode: PushMode,
) -> RepositoryResult<()> {
    protocol::validate_push_options(options)?;
    let (operation, payload) = if mode.dry_run {
        (
            Operation::CheckBatch {},
            crate::binary::encode(&protocol::OptionsPublication {
                publication,
                options: options.to_vec(),
            })?,
        )
    } else if options.is_empty() {
        (
            Operation::PublishBatch {},
            crate::binary::encode(&publication)?,
        )
    } else {
        (
            Operation::PublishOptions {},
            crate::binary::encode(&protocol::OptionsPublication {
                publication,
                options: options.to_vec(),
            })?,
        )
    };
    let expected = format!("{:x}", sha2::Sha256::digest(&payload));
    let (response, bytes) = exchange(remote, operation, &payload, mode.allow_http)?;
    if !bytes.is_empty() || response.tip.as_deref() != Some(&expected) {
        if mode.dry_run {
            return Err(
                "dry-run validation receipt does not match the requested transaction".into(),
            );
        }
        return Err("atomic publication outcome is unknown: receipt does not match the requested transaction".into());
    }
    Ok(())
}

fn publish_selection(
    repo: &Repository,
    remote: &Remote,
    updates: Vec<protocol::RefUpdate>,
    state: &RepositoryState,
    options: &[String],
    mode: PushMode,
) -> RepositoryResult<(serde_json::Value, Vec<protocol::RefUpdate>)> {
    let mut names = BTreeSet::new();
    for update in &updates {
        let namespace = if matches!(update.namespace, RefNamespace::Branch) {
            "heads"
        } else {
            "tags"
        };
        if !names.insert(format!("refs/{namespace}/{}", update.name)) {
            return Err("duplicate push destination".into());
        }
    }
    if mode.atomic {
        let changed: Vec<_> = updates
            .iter()
            .filter(|u| u.expected != u.new)
            .cloned()
            .collect();
        publish_atomic(repo, remote, changed.clone(), state, options, mode)?;
        return Ok((
            serde_json::json!({"remote":remote.name,"status":if mode.dry_run {"dry_run"} else {"pushed"},"atomic":true,"updates":changed}),
            updates,
        ));
    }
    let changed: Vec<_> = updates
        .iter()
        .filter(|u| u.expected != u.new)
        .cloned()
        .collect();
    let mut result = Vec::new();
    if !changed.is_empty() {
        let wants: Vec<_> = changed
            .iter()
            .filter_map(|u| u.new.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let have: Vec<_> = state
            .branches
            .values()
            .chain(state.tags.values())
            .cloned()
            .collect();
        let bundle = if wants.is_empty() {
            Vec::new()
        } else {
            repo.network_bundle(&wants, &have)?.1
        };
        let payload = crate::binary::encode(&protocol::OptionsPublication {
            publication: protocol::BatchPublication {
                updates: changed.clone(),
                bundle,
            },
            options: options.to_vec(),
        })?;
        let expected = format!("{:x}", sha2::Sha256::digest(&payload));
        let receipt = exchange(remote, Operation::PublishPartial { dry_run: mode.dry_run }, &payload, mode.allow_http).and_then(|(response, bytes)| {
            let receipt = crate::binary::decode::<protocol::PartialReceipt>(&bytes);
            if response.tip.as_deref() != Some(&expected) || !receipt.as_ref().is_ok_and(|r| r.rejections.len() == changed.len()) {
                return Err(if mode.dry_run { "invalid dry-run validation receipt" } else { "remote publication outcome is unknown: invalid per-reference receipt; inspect the remote before retrying" }.into());
            }
            receipt
        });
        match receipt {
            Ok(receipt) => result = receipt.rejections,
            Err(error) => {
                if mode.dry_run {
                    return Err(error);
                }
                let status = if error.starts_with("remote rejected request:") {
                    "rejected"
                } else {
                    "unknown"
                };
                let rows: Vec<_> = updates.iter().map(|u| serde_json::json!({
                    "namespace":if matches!(u.namespace, RefNamespace::Branch){"branch"}else{"tag"},
                    "name":u.name,"tip":u.new,"status":if u.expected == u.new {"up_to_date"}else{status},"error":error,
                })).collect();
                return Ok((
                    serde_json::json!({"remote":remote.name,"status":status,"atomic":false,"updates":rows}),
                    Vec::new(),
                ));
            }
        }
    }
    let mut reasons = result.into_iter();
    let mut rows = Vec::new();
    let mut confirmed = Vec::new();
    let mut failures = 0;
    for update in updates {
        let unchanged = update.expected == update.new;
        let reason = if unchanged {
            None
        } else {
            reasons.next().ok_or("missing per-reference result")?
        };
        let status = if reason.is_some() {
            "rejected"
        } else if unchanged {
            "up_to_date"
        } else if mode.dry_run {
            "dry_run"
        } else if update.new.is_some() {
            "pushed"
        } else {
            "deleted"
        };
        let namespace = if matches!(update.namespace, RefNamespace::Branch) {
            "branch"
        } else {
            "tag"
        };
        rows.push(serde_json::json!({"namespace":namespace,"name":update.name,"tip":update.new,"status":status,"error":reason}));
        if reason.is_some() {
            failures += 1;
        } else {
            confirmed.push(update);
        }
    }
    let status = if failures == 0 {
        if mode.dry_run {
            "dry_run"
        } else {
            "pushed"
        }
    } else if failures == rows.len() {
        "rejected"
    } else {
        "partial"
    };
    Ok((
        serde_json::json!({"remote":remote.name,"status":status,"atomic":false,"updates":rows}),
        confirmed,
    ))
}

fn push_command(root: &str, args: &[&str], allow_http: bool) -> RepositoryResult<String> {
    let mut positional = Vec::new();
    let mut tags = false;
    let mut all = false;
    let mut mirror = false;
    let mut prune = false;
    let mut atomic = false;
    let mut dry_run = false;
    let mut force = false;
    let mut delete = false;
    let mut set_upstream = false;
    let mut explicit_lease = None;
    let mut push_options = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "-o" | "--push-option" => {
                index += 1;
                push_options.push(
                    args.get(index)
                        .ok_or("missing push option value")?
                        .to_string(),
                );
            }
            option if option.starts_with("--push-option=") => {
                push_options.push(option[14..].into())
            }
            option if option.starts_with("-o") && option.len() > 2 => {
                push_options.push(option[2..].into())
            }
            "--tags" if !tags => tags = true,
            "--all" | "--branches" if !all => all = true,
            "--mirror" if !mirror => mirror = true,
            "--prune" if !prune => prune = true,
            "--atomic" if !atomic => atomic = true,
            "--dry-run" | "-n" if !dry_run => dry_run = true,
            "--force" | "-f" if !force => force = true,
            "--delete" if !delete => delete = true,
            "-u" | "--set-upstream" if !set_upstream => set_upstream = true,
            "--force-with-lease" if explicit_lease.is_none() => {
                index += 1;
                explicit_lease = Some(lease(args.get(index).ok_or(USAGE)?)?);
            }
            option if option.starts_with('-') => {
                return Err(format!("unsupported or repeated push option: {option}"))
            }
            value => positional.push(value),
        }
        index += 1;
    }
    protocol::validate_push_options(&push_options)?;
    let mode = PushMode {
        atomic,
        dry_run,
        allow_http,
    };
    let patterns = positional.iter().skip(1).any(|s| refspec::special(s));
    let batch = tags || all || mirror || positional.len() > 2 || patterns || prune;
    if (batch && explicit_lease.is_some())
        || ([tags, all, mirror].into_iter().filter(|v| *v).count() > 1)
        || ((tags || all || mirror) && (delete || explicit_lease.is_some() || positional.len() > 1))
        || (delete && positional.is_empty())
        || (set_upstream && (tags || mirror || delete))
        || (delete && (patterns || prune))
    {
        return Err(USAGE.into());
    }
    let (explicit_remote, specification) = if delete && positional.len() == 1 {
        (None, positional.first().copied())
    } else {
        (positional.first().copied(), positional.get(1).copied())
    };
    let repo = Repository::open(root)?;
    let (local, upstreams, remote) = {
        let _lock = config_lock(&repo)?;
        let (local, upstreams) = repo.state_with_upstreams()?;
        let remote_name = explicit_remote.unwrap_or_else(|| {
            upstreams
                .get(&local.head)
                .map_or("origin", |binding| binding.remote.as_str())
        });
        let remote = load(&repo, remote_name)?;
        (local, upstreams, remote)
    };
    let mut binding_source = None;
    let planned = if batch {
        None
    } else if delete {
        let (namespace, reference) = reference(specification.ok_or(USAGE)?)?;
        Some((namespace, reference.to_owned(), None))
    } else if let Some(specification) = specification {
        let specification = if let Some(specification) = specification.strip_prefix('+') {
            force = true;
            specification
        } else {
            specification
        };
        let (source, destination) = specification.split_once(':').unwrap_or((
            specification,
            if specification == "HEAD" {
                &local.head
            } else {
                specification
            },
        ));
        let (namespace, reference) = reference(destination)?;
        let tip = if source.is_empty() {
            if set_upstream {
                return Err("cannot set upstream for a deletion".into());
            }
            None
        } else if set_upstream {
            if !matches!(namespace, RefNamespace::Branch) {
                return Err("push --set-upstream requires a branch destination".into());
            }
            let branch = if source == "HEAD" {
                local.head.as_str()
            } else {
                source.strip_prefix("refs/heads/").unwrap_or(source)
            };
            branch_name(branch)?;
            let tip = local
                .branches
                .get(branch)
                .cloned()
                .ok_or("push --set-upstream source must be a committed local branch")?;
            binding_source = Some((
                branch.to_owned(),
                tip.clone(),
                upstreams.get(branch).cloned(),
            ));
            Some(tip)
        } else {
            Some(repo.resolve(source)?)
        };
        Some((namespace, reference.to_owned(), tip))
    } else {
        if local.detached() {
            return Err("detached HEAD push requires an explicit SOURCE:DESTINATION".into());
        }
        let source = local
            .tip()
            .cloned()
            .ok_or("current branch has no commits")?;
        if upstreams
            .get(&local.head)
            .is_some_and(|binding| binding.remote == remote.name && binding.branch != local.head)
        {
            return Err("simple push refused: local branch name differs from its upstream branch; specify SOURCE:DESTINATION explicitly".into());
        }
        if set_upstream {
            binding_source = Some((
                local.head.clone(),
                source.clone(),
                upstreams.get(&local.head).cloned(),
            ));
        }
        Some((RefNamespace::Branch, local.head.clone(), Some(source)))
    };
    let (advertisement, empty) = advertise(&remote, allow_http)?;
    if !empty.is_empty() {
        return Err("advertisement unexpectedly contains a bundle".into());
    }
    let state = advertisement
        .state
        .ok_or("advertisement lacks repository state")?;
    validate_state(&state)?;
    if batch {
        let mut updates = Vec::new();
        let mut bindings = std::collections::BTreeMap::new();
        if tags || all || mirror {
            for (namespace, local_refs, remote_refs, selected) in [
                (
                    RefNamespace::Branch,
                    &local.branches,
                    &state.branches,
                    all || mirror,
                ),
                (RefNamespace::Tag, &local.tags, &state.tags, tags || mirror),
            ] {
                if !selected {
                    continue;
                }
                for (name, new) in local_refs {
                    if set_upstream {
                        bindings.insert(name.clone(), name.clone());
                    }
                    updates.push(protocol::RefUpdate {
                        namespace: namespace.clone(),
                        name: name.clone(),
                        expected: remote_refs.get(name).cloned(),
                        new: Some(new.clone()),
                        force: mirror || force,
                    });
                }
                if mirror || prune {
                    for (name, old) in remote_refs {
                        if !local_refs.contains_key(name) {
                            updates.push(protocol::RefUpdate {
                                namespace: namespace.clone(),
                                name: name.clone(),
                                expected: Some(old.clone()),
                                new: None,
                                force: false,
                            });
                        }
                    }
                }
            }
        } else {
            let default = if specification.is_none() {
                if local.detached() {
                    return Err("detached HEAD push requires an explicit SOURCE:DESTINATION".into());
                }
                if upstreams
                    .get(&local.head)
                    .is_some_and(|b| b.remote == remote.name && b.branch != local.head)
                {
                    return Err("simple push refused: specify SOURCE:DESTINATION explicitly".into());
                }
                Some(format!(
                    "refs/heads/{}:refs/heads/{}",
                    local.head, local.head
                ))
            } else {
                None
            };
            let specs = if let Some(default) = &default {
                vec![default.as_str()]
            } else {
                positional[1..].to_vec()
            };
            let expanded = refspec::expand(&specs, &local, &state, prune)?;
            let mut bound_sources = BTreeSet::new();
            for spec in &expanded {
                let (spec, forced) = spec
                    .strip_prefix('+')
                    .map_or((spec.as_str(), force), |s| (s, true));
                let (namespace, name, new) = if delete {
                    let (ns, name) = reference(spec)?;
                    (ns, name, None)
                } else {
                    let (source, dest) = spec
                        .split_once(':')
                        .unwrap_or((spec, if spec == "HEAD" { &local.head } else { spec }));
                    let (ns, name) = reference(dest)?;
                    if set_upstream && !source.is_empty() {
                        if !matches!(ns, RefNamespace::Branch) {
                            return Err("push --set-upstream requires branch destinations".into());
                        }
                        let branch = if source == "HEAD" {
                            &local.head
                        } else {
                            source.strip_prefix("refs/heads/").unwrap_or(source)
                        };
                        branch_name(branch)?;
                        if !local.branches.contains_key(branch)
                            || !bound_sources.insert(branch.to_owned())
                        {
                            return Err("each upstream source must be a committed local branch with one destination".into());
                        }
                        bindings.insert(name.to_owned(), branch.to_owned());
                    }
                    (
                        ns,
                        name,
                        if source.is_empty() {
                            None
                        } else {
                            Some(repo.resolve(source)?)
                        },
                    )
                };
                updates.push(protocol::RefUpdate {
                    expected: remote_tip(&state, &namespace, name),
                    namespace,
                    name: name.into(),
                    force: forced && new.is_some(),
                    new,
                });
            }
        }
        let (mut report, confirmed) =
            publish_selection(&repo, &remote, updates, &state, &push_options, mode)?;
        if set_upstream {
            report["upstream_configured"] = false.into();
        }
        if set_upstream && !dry_run && confirmed.iter().any(|u| u.new.is_some()) {
            let _lock = config_lock(&repo)?;
            if load(&repo, &remote.name)? != remote {
                return Err("remote refs were acknowledged; remote configuration changed before saving upstreams".into());
            }
            for update in confirmed {
                if let Some(tip) = &update.new {
                    let branch = bindings
                        .get(&update.name)
                        .ok_or("missing confirmed upstream source")?;
                    repo.set_upstream_checked(
                        branch,
                        &remote.name,
                        &update.name,
                        tip,
                        upstreams.get(branch),
                    )
                    .map_err(|error| {
                        format!(
                            "remote refs were acknowledged; local upstream update failed: {error}"
                        )
                    })?;
                }
            }
            report["upstream_configured"] = true.into();
        }
        return pretty(&report);
    }
    let (namespace, reference, source) = planned.ok_or("missing push update")?;
    let delete = source.is_none();
    let actual = remote_tip(&state, &namespace, &reference);
    let expected = explicit_lease.clone().unwrap_or_else(|| actual.clone());
    if actual != expected {
        return Err("remote ref does not match the explicit force-with-lease expectation".into());
    }
    if delete && actual.is_none() {
        return Err("remote reference does not exist".into());
    }
    let namespace_text = match &namespace {
        RefNamespace::Branch => "branch",
        RefNamespace::Tag => "tag",
    };
    if atomic {
        publish_atomic(
            &repo,
            &remote,
            vec![protocol::RefUpdate {
                namespace: namespace.clone(),
                name: reference.clone(),
                expected: expected.clone(),
                new: source.clone(),
                force: (force || explicit_lease.is_some()) && !delete,
            }],
            &state,
            &push_options,
            mode,
        )?;
    } else {
        update_ref(
            &repo,
            &remote,
            RefChange {
                namespace,
                reference: &reference,
                expected,
                new: source.as_deref(),
                force: (force || explicit_lease.is_some()) && !delete,
            },
            &state
                .branches
                .values()
                .chain(state.tags.values())
                .cloned()
                .collect::<Vec<_>>(),
            &push_options,
            mode,
        )?;
    }
    if let Some((branch, tip, previous)) = binding_source.filter(|_| !dry_run) {
        let save = (|| {
            let _lock = config_lock(&repo)?;
            if load(&repo, &remote.name)? != remote {
                return Err("remote configuration changed during push".into());
            }
            repo.set_upstream_checked(&branch, &remote.name, &reference, &tip, previous.as_ref())?;
            Ok::<(), String>(())
        })();
        save.map_err(|error| format!("remote ref was published and exactly acknowledged; local upstream update failed: {error}; inspect the local upstream before retrying"))?;
    }
    pretty(
        &serde_json::json!({"remote":remote.name,"namespace":namespace_text,"name":reference,"tip":source,"status":if dry_run {"dry_run"} else if delete {"deleted"} else {"pushed"},"upstream_configured":set_upstream && !dry_run}),
    )
}

#[path = "remote_pool.rs"]
mod pool;
/// Dispatch remote commands; arguments start with the subcommand, not argv[0].
#[path = "remote_select.rs"]
mod selection;
pub(crate) use pool::fork_pins as fork_pool_pins;
pub use pool::run as pool_cli;
pub use selection::{fetch_object_source, run_selective, selective_requested, validate_source};

pub fn run(args: &[String]) -> RepositoryResult<String> {
    if selective_requested(args) {
        return run_selective(args);
    }
    if crate::command_args::help_requested(args) {
        return Ok(USAGE.into());
    }
    let transport_flags = crate::command_args::option_indices(args, "--allow-insecure-http");
    let ff_only_flags = if args.first().is_some_and(|command| command == "pull") {
        crate::command_args::option_indices(args, "--ff-only")
    } else {
        Vec::new()
    };
    if ff_only_flags.len() > 1 {
        return Err("pull --ff-only cannot be repeated".into());
    }
    let no_ff_flags = if args.first().is_some_and(|command| command == "pull") {
        crate::command_args::option_indices(args, "--no-ff")
    } else {
        Vec::new()
    };
    if no_ff_flags.len() > 1 {
        return Err("pull --no-ff cannot be repeated".into());
    }
    if !no_ff_flags.is_empty() && !ff_only_flags.is_empty() {
        return Err("pull --no-ff and --ff-only are mutually exclusive".into());
    }
    let no_ff = !no_ff_flags.is_empty();
    let ff_only = !ff_only_flags.is_empty();
    let allow_http = !transport_flags.is_empty();
    let args: Vec<&str> = args
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            (!transport_flags.contains(&index)
                && !ff_only_flags.contains(&index)
                && !no_ff_flags.contains(&index))
            .then_some(arg.as_str())
        })
        .collect();
    match args.as_slice() {
        ["fork", url, destination] => {
            protocol::repository_name(destination)?;
            parse_url(url)?;
            let remote = Remote {
                version: 1,
                name: "fork".into(),
                url: (*url).into(),
                allow_insecure_http: allow_http,
            };
            let (response, bytes) = exchange(
                &remote,
                Operation::Fork {
                    destination: (*destination).into(),
                },
                &[],
                allow_http,
            )?;
            if !bytes.is_empty() {
                return Err("fork acknowledgement contains an unexpected payload; publication outcome is unknown, inspect the destination before retrying".into());
            }
            let state = response.state;
            if let Some(state) = &state {
                validate_state(state)?;
            }
            if state.is_none()
                && response.tip.as_deref() != Some(&format!("fork-created:{destination}"))
            {
                return Err("fork receipt missing; publication outcome is unknown".into());
            }
            pretty(
                &serde_json::json!({"status":"forked","repository":destination,"state":state,"storage":"shared_immutable_objects"}),
            )
        }
        ["remote", root] | ["remote", root, "-v"] => {
            let repo = Repository::open(root)?;
            let _lock = config_lock(&repo)?;
            let mut remotes = Vec::new();
            for entry in
                fs::read_dir(config_dir(&repo)?).map_err(|e| format!("list remotes: {e}"))?
            {
                let entry = entry.map_err(|e| format!("list remotes: {e}"))?;
                let filename = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF-8 remote metadata name")?;
                if filename.ends_with(".tracking.bin") {
                    continue;
                }
                let remote = filename
                    .strip_suffix(".bin")
                    .ok_or("unexpected file in remote configuration")?;
                remotes.push(load(&repo, remote)?);
            }
            remotes.sort_by(|a, b| a.name.cmp(&b.name));
            pretty(&remotes)
        }
        ["remote", root, "add", remote_name, url] => {
            name(remote_name)?;
            parse_url(url)?;
            let repo = Repository::open(root)?;
            let _lock = config_lock(&repo)?;
            let path = config_dir(&repo)?.join(format!("{remote_name}.bin"));
            if path.try_exists().map_err(|e| e.to_string())? || fs::symlink_metadata(&path).is_ok()
            {
                return Err("remote already exists".into());
            }
            let remote = Remote {
                version: 1,
                name: (*remote_name).to_owned(),
                url: (*url).to_owned(),
                allow_insecure_http: allow_http,
            };
            atomic_record(&repo, &path, &remote)?;
            pretty(&remote)
        }
        ["remote", root, "show", remote_name] => {
            let repo = Repository::open(root)?;
            let _lock = config_lock(&repo)?;
            let remote = load(&repo, remote_name)?;
            pretty(
                &serde_json::json!({"remote": remote, "tracking": optional_tracking(&repo, &remote)?}),
            )
        }
        ["remote", root, "remove", remote_name] => {
            let repo = Repository::open(root)?;
            let _lock = config_lock(&repo)?;
            let remote = load(&repo, remote_name)?;
            optional_tracking(&repo, &remote)?;
            repo.upstreams()?;
            let directory = config_dir(&repo)?;
            let cleared = repo.remove_upstream_remote(remote_name)?;
            fs::remove_file(directory.join(format!("{remote_name}.bin"))).map_err(|e| {
                format!("upstream bindings were cleared; remove remote configuration failed: {e}")
            })?;
            let tracking = directory.join(format!("{remote_name}.tracking.bin"));
            match fs::remove_file(tracking) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("upstream bindings and remote configuration were removed; remove tracking failed: {e}")),
            }
            Ok(format!(
                "removed remote {remote_name}; cleared {cleared} upstream bindings"
            ))
        }
        ["remote", root, "rename", old, new] => {
            name(new)?;
            let repo = Repository::open(root)?;
            let _lock = config_lock(&repo)?;
            let mut remote = load(&repo, old)?;
            let mut tracking = optional_tracking(&repo, &remote)?;
            repo.upstreams()?;
            let directory = config_dir(&repo)?;
            let destination = directory.join(format!("{new}.bin"));
            if fs::symlink_metadata(&destination).is_ok()
                || fs::symlink_metadata(directory.join(format!("{new}.tracking.bin"))).is_ok()
            {
                return Err("destination remote already exists".into());
            }
            remote.name = (*new).to_owned();
            atomic_record(&repo, &destination, &remote).map_err(|error| format!("creating the new remote configuration failed or its durability is uncertain; old remote and upstream bindings remain: {error}"))?;
            if let Some(tracking) = tracking.as_mut() {
                tracking.remote = (*new).to_owned();
                atomic_record(
                    &repo,
                    &directory.join(format!("{new}.tracking.bin")),
                    tracking,
                ).map_err(|error| format!("new remote configuration was created; copying tracking failed, old remote and upstream bindings remain: {error}"))?;
            }
            repo.rename_upstream_remote(old, new).map_err(|error| format!("new remote configuration and available tracking were created; upstream rename failed, old remote remains; inspect upstream bindings before retrying: {error}"))?;
            fs::remove_file(directory.join(format!("{old}.bin")))
                .map_err(|e| format!("new remote and upstream bindings were published; removing old remote configuration failed: {e}"))?;
            if tracking.is_some() {
                fs::remove_file(directory.join(format!("{old}.tracking.bin")))
                    .map_err(|e| format!("new remote and upstream bindings were published and old configuration removed; removing old tracking failed: {e}"))?;
            }
            pretty(&remote)
        }
        ["fetch", root] | ["fetch", root, _] => {
            let repo = Repository::open(root)?;
            let remote = load(&repo, args.get(2).copied().unwrap_or("origin"))?;
            pretty(&fetch(&repo, &remote, allow_http)?)
        }
        ["pull", root, rest @ ..] => {
            let message_position = rest
                .iter()
                .position(|arg| matches!(*arg, "-m" | "--message"));
            let (positionals, message) = match message_position {
                Some(position) if position <= 2 && rest.len() == position + 2 => {
                    (&rest[..position], rest[position + 1])
                }
                None if ff_only && rest.len() <= 2 => (rest, ""),
                _ => return Err(USAGE.into()),
            };
            if let Some(option) = positionals
                .iter()
                .find(|argument| argument.starts_with('-'))
            {
                return Err(format!("unsupported pull option: {option}"));
            }
            let repo = Repository::open(root)?;
            // Bind both current branch and tip before network I/O. A concurrent
            // switch or receive must be rejected rather than merging the fetched
            // history into an independently selected later branch.
            let (before, remote, branch) = {
                let _lock = config_lock(&repo)?;
                let (before, upstreams) = repo.state_with_upstreams()?;
                let upstream = upstreams.get(&before.head);
                let remote_name = positionals.first().copied().unwrap_or_else(|| {
                    upstream.map_or("origin", |binding| binding.remote.as_str())
                });
                let branch = positionals
                    .get(1)
                    .copied()
                    .unwrap_or_else(|| {
                        upstream
                            .filter(|binding| binding.remote == remote_name)
                            .map_or(before.head.as_str(), |binding| binding.branch.as_str())
                    })
                    .to_owned();
                branch_name(&branch)?;
                let remote = load(&repo, remote_name)?;
                (before, remote, branch)
            };
            let state = fetch(&repo, &remote, allow_http)?;
            let tip = state.branches.get(&branch).ok_or_else(|| {
                if positionals.len() == 2 {
                    format!("remote branch {branch:?} has no commit")
                } else {
                    format!("selected remote has no branch {branch:?} for the current branch or its upstream; specify BRANCH explicitly")
                }
            })?;
            let expected_head = before.tip().map(String::as_str);
            let outcome = if ff_only {
                repo.merge_ff_only_on_branch(tip, message, &before.head, expected_head)?
            } else if no_ff {
                repo.merge_no_ff_on_branch(tip, message, &before.head, expected_head)?
            } else {
                repo.merge_on_branch(tip, message, &before.head, expected_head)?
            };
            pretty(&outcome)
        }
        ["push", root, rest @ ..] => push_command(root, rest, allow_http),
        ["clone", url, destination] if is_remote_url(url) => {
            let endpoint = parse_url(url)?;
            let remote = Remote {
                version: 1,
                name: "origin".into(),
                url: (*url).to_owned(),
                allow_insecure_http: allow_http,
            };
            let (response, payload) = fetch_payload(&remote, allow_http, None)?;
            let state = response
                .state
                .ok_or("clone response lacks repository state")?;
            if payload.is_empty() {
                return Err("clone response lacks its verified bundle".into());
            }
            if Repository::bundle_state(&payload)? != state {
                return Err("remote advertisement does not match cloned bundle".into());
            }
            // Verification and advertisement comparison precede initialization.
            let repo = Repository::clone_bundle_bytes(&payload, destination)?;
            let _lock = config_lock(&repo)?;
            let directory = config_dir(&repo)?;
            atomic_record(&repo, &directory.join("origin.bin"), &remote)?;
            let initial_upstream = state
                .branches
                .get(&state.head)
                .map(|tip| (state.head.clone(), tip.clone()));
            atomic_record(
                &repo,
                &directory.join("origin.tracking.bin"),
                &Tracking {
                    version: 1,
                    remote: "origin".into(),
                    url: (*url).to_owned(),
                    state,
                },
            )?;
            if let Some((branch, tip)) = initial_upstream {
                repo.set_upstream_checked(&branch, "origin", &branch, &tip, None)
                    .map_err(|error| format!("clone and origin tracking were created; configuring the initial upstream failed: {error}"))?;
            }
            Ok(format!(
                "cloned {} into {}",
                endpoint.repository(),
                repo.root.display()
            ))
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_timeout_is_bounded_and_defaults_to_five_minutes() {
        assert_eq!(rpc_timeout(None).unwrap(), Duration::from_secs(300));
        for seconds in [1, 30, 300, 3600] {
            assert_eq!(
                rpc_timeout(Some(&seconds.to_string())).unwrap(),
                Duration::from_secs(seconds)
            );
        }
        for invalid in [
            "",
            "0",
            "3601",
            "-1",
            " 30",
            "30 ",
            "1.5",
            "never",
            "18446744073709551616",
        ] {
            assert!(rpc_timeout(Some(invalid)).unwrap_err().contains("1..=3600"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn process_exit_does_not_disable_the_pipe_deadline() {
        // A finite child inherits both output pipes after its parent exits. The
        // caller must return a deadline error instead of waiting for pipe EOF
        // and then reporting the immediate parent's zero exit as success.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 2 & exit 0"]);
        let error = subprocess_deadline(
            command,
            Vec::new(),
            "fixed pipe fixture",
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(error.contains("deadline"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn an_exited_transport_without_descendants_still_returns_exact_bytes() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf fixed-response"]);
        let response = subprocess_deadline(
            command,
            Vec::new(),
            "fixed response fixture",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(response, b"fixed-response");
    }

    #[test]
    fn urls_have_one_unambiguous_repository_and_safe_ssh_arguments() {
        for url in [
            "http://127.0.0.1:1234/repo",
            "https://example.org/repo",
            "http://host/a/b",
            "ssh://user@host/srv/repos//team/project",
            "http://[::1]:1234/repo",
            "ssh://user@host:2222/tmp/repos/repo",
            "ssh://user@[::1]:2222/tmp/repos/repo",
        ] {
            assert!(parse_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://user:password@host/repo",
            "http://host/repo?token=x",
            "http://host/a//b",
            "http://host/a/../b",
            "ssh://-oProxyCommand=evil/tmp/repo",
            "ssh://host/tmp/../repo",
            "ssh://host//repo",
            "ssh://host/tmp/repo;id",
            "ssh://host:0/tmp/repo",
            "ssh://user@host:65536/tmp/repo",
            "ssh://user@host/tmp/%72epo",
            "ssh://user@host/tmp/repo\n",
            "http://host/repo#x",
        ] {
            assert!(parse_url(url).is_err(), "{url}");
        }
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
