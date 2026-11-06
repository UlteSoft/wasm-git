//! Authenticated PR workflows and branch protection over the native RPC.
use super::*;
use crate::pull::*;
use crate::repository::{SignatureFormat, SigningIdentity};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub(super) struct MergeSigner {
    pub author: String,
    pub key: PathBuf,
    pub format: SignatureFormat,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Protection {
    version: u32,
    rules: Vec<Rule>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    repository: String,
    branches: Vec<String>,
    required_reviews: u16,
    required_checks: Vec<String>,
    #[serde(default)]
    check_issuers: BTreeMap<String, Vec<String>>,
    #[serde(default = "methods")]
    methods: Vec<MergeMethod>,
}
fn methods() -> Vec<MergeMethod> {
    vec![
        MergeMethod::FastForward,
        MergeMethod::Merge,
        MergeMethod::Squash,
    ]
}
fn matches(pattern: &str, value: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == value, |p| value.starts_with(p))
}
impl Protection {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.rules.len() > 4096 {
            return Err("invalid protection version or rule count".into());
        }
        for r in &self.rules {
            if r.repository != "*" {
                let prefix = pattern_prefix(&r.repository);
                protocol::repository_name(prefix)?;
            }
            if r.branches.is_empty()
                || r.required_reviews > 64
                || r.required_checks.len() > 64
                || r.methods.is_empty()
            {
                return Err("invalid protection rule bounds".into());
            }
            for branch in &r.branches {
                let name = branch
                    .strip_prefix("refs/heads/")
                    .ok_or("protection branches require refs/heads/ names")?;
                if name != "*" {
                    crate::repository::validate_ref_name(pattern_prefix(name))?;
                }
            }
            let mut contexts = BTreeSet::new();
            for context in &r.required_checks {
                if context.is_empty()
                    || context.len() > 128
                    || context.chars().any(char::is_control)
                    || !contexts.insert(context)
                {
                    return Err("invalid or duplicate check context".into());
                }
            }
            for (context, issuers) in &r.check_issuers {
                if !contexts.contains(context) || issuers.is_empty() {
                    return Err(
                        "check issuers must name a required check and at least one principal"
                            .into(),
                    );
                }
                for actor in issuers {
                    protocol::repository_component(actor)?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let path = real_path(path, false)?;
        if fs::metadata(&path).map_err(|e| e.to_string())?.len() > 16 * 1024 * 1024 {
            return Err("protection file exceeds 16 MiB".into());
        }
        let value: Self = crate::binary::decode(&fs::read(path).map_err(|e| e.to_string())?)?;
        value.validate()?;
        Ok(value)
    }
    fn rules<'a>(&'a self, repo: &'a str, reference: &'a str) -> impl Iterator<Item = &'a Rule> {
        self.rules.iter().filter(move |r| {
            matches(&r.repository, repo) && r.branches.iter().any(|p| matches(p, reference))
        })
    }
    pub(super) fn protected(&self, repo: &str, reference: &str) -> bool {
        self.rules(repo, reference).next().is_some()
    }
}
fn pattern_prefix(pattern: &str) -> &str {
    pattern
        .strip_suffix('*')
        .map_or(pattern, |prefix| prefix.strip_suffix('/').unwrap_or(prefix))
}
pub(super) fn compile(input: &Path, output: &Path) -> Result<String, String> {
    let input = real_path(input, false)?;
    if fs::metadata(&input).map_err(|e| e.to_string())?.len() > 16 * 1024 * 1024 {
        return Err("protection input exceeds 16 MiB".into());
    }
    let value: Protection = serde_json::from_slice(&fs::read(input).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    value.validate()?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output).map_err(|e| e.to_string())?;
    file.write_all(&crate::binary::encode(&value)?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "compiled {} branch protection rules",
        value.rules.len()
    ))
}
fn can_write(settings: &Settings, actor: &str, repo: &str, branch: &str) -> Result<(), String> {
    if !settings.write {
        return Err("server is read-only".into());
    }
    if let Some(policy) = &settings.policy {
        policy.authorize(
            actor,
            repo,
            Some(&format!("refs/heads/{branch}")),
            false,
            false,
        )?;
    }
    Ok(())
}
fn owner_or_writer(settings: &Settings, repo: &str, p: &PullRecord) -> Result<(), String> {
    if p.author == settings.actor {
        Ok(())
    } else {
        can_write(settings, &settings.actor, repo, &p.base_branch)
    }
}
fn source(
    settings: &Settings,
    target: &Repository,
    name: &str,
    branch: &str,
) -> Result<(String, Vec<u8>), String> {
    protocol::repository_name(name)?;
    crate::repository::validate_ref_name(branch)?;
    can_write(settings, &settings.actor, name, branch)?;
    let repo = Repository::open(repository_path(settings, name)?)?;
    let state = repo.state()?;
    let head = state
        .branches
        .get(branch)
        .ok_or("source branch does not exist")?
        .clone();
    let current = target.state()?;
    let have = current
        .branches
        .values()
        .chain(current.tags.values())
        .cloned()
        .collect::<Vec<_>>();
    let (_, bundle) = repo.network_bundle(std::slice::from_ref(&head), &have)?;
    Ok((head, bundle))
}
fn approve(
    settings: &Settings,
    name: &str,
    p: &PullRecord,
    method: MergeMethod,
) -> Result<(), String> {
    let reference = format!("refs/heads/{}", p.base_branch);
    let rules: Vec<_> = settings
        .protection
        .as_ref()
        .map(|v| v.rules(name, &reference).collect())
        .unwrap_or_default();
    let mut reviews = BTreeMap::new();
    for e in p
        .events
        .iter()
        .filter(|e| e.head == p.head && e.base == p.base)
    {
        if let PullEventKind::Review { decision, .. } = e.kind {
            if decision != ReviewDecision::Comment
                && can_write(settings, &e.actor, name, &p.base_branch).is_ok()
            {
                reviews.insert(e.actor.as_str(), decision);
            }
        }
    }
    if reviews
        .values()
        .any(|v| *v == ReviewDecision::RequestChanges)
    {
        return Err("a current reviewer requests changes".into());
    }
    let approved = reviews
        .iter()
        .filter(|(actor, v)| **actor != p.author && **v == ReviewDecision::Approve)
        .count();
    for r in rules {
        if !r.methods.contains(&method) {
            return Err("merge method is disabled by branch protection".into());
        }
        if approved < r.required_reviews as usize {
            return Err(format!(
                "branch protection requires {} current non-author approvals",
                r.required_reviews
            ));
        }
        for context in &r.required_checks {
            let check = p.events.iter().rev().find(|e| {
                e.head == p.head
                    && e.base == p.base
                    && matches!(&e.kind,PullEventKind::Check{context:c,..} if c==context)
                    && r.check_issuers
                        .get(context)
                        .is_none_or(|allowed| allowed.contains(&e.actor))
                    && can_write(settings, &e.actor, name, &p.base_branch).is_ok()
            });
            if !check.is_some_and(|e| {
                matches!(
                    e.kind,
                    PullEventKind::Check {
                        state: CheckState::Success,
                        ..
                    }
                )
            }) {
                return Err(format!(
                    "required check is missing or unsuccessful: {context}"
                ));
            }
        }
    }
    Ok(())
}
fn reply(p: PullRecord) -> PullReply {
    PullReply {
        records: vec![p],
        ..Default::default()
    }
}
pub(super) fn run(
    settings: &Settings,
    name: &str,
    repo: &Repository,
    command: PullCommand,
) -> Result<PullReply, String> {
    if command.mutates() && !settings.write {
        return Err("server is read-only".into());
    }
    match command {
        PullCommand::List { after, limit } => repo.pulls(after, limit),
        PullCommand::Show { number } => Ok(reply(repo.pull(number)?)),
        PullCommand::Create {
            source_repository,
            source_branch,
            base_branch,
            title,
            body,
            draft,
        } => {
            crate::repository::validate_ref_name(&base_branch)?;
            if source_repository == name && source_branch == base_branch {
                return Err("pull source and base branches must differ".into());
            }
            let (head, bundle) = source(settings, repo, &source_repository, &source_branch)?;
            let p = PullRecord {
                schema: 1,
                number: 0,
                version: 0,
                source_repository,
                source_branch,
                base_branch,
                author: settings.actor.clone(),
                title,
                body,
                state: if draft {
                    PullState::Draft
                } else {
                    PullState::Open
                },
                base: String::new(),
                head,
                revisions: vec![],
                events: vec![],
                merge: None,
            };
            Ok(reply(repo.create_pull(p, &bundle)?))
        }
        PullCommand::Files {
            number,
            version,
            against,
        } => repo.pull_files(number, version, against),
        PullCommand::File {
            number,
            version,
            path,
            old,
            offset,
            against,
        } => Ok(PullReply {
            chunk: Some(repo.pull_file(number, version, &path, old, offset, against)?),
            ..Default::default()
        }),
        PullCommand::Merge {
            number,
            version,
            head,
            base,
            method,
            message,
        } => {
            let current = repo.pull(number)?;
            can_write(settings, &settings.actor, name, &current.base_branch)?;
            let identity = settings.merge_signer.as_ref().map(|s| SigningIdentity {
                author: s.author.clone(),
                key: s.key.clone(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
            let signing = identity
                .as_ref()
                .zip(settings.merge_signer.as_ref().map(|s| s.format));
            let (prepared, bundle) = repo.prepare_pull_merge(
                number,
                version,
                PullMergeOptions {
                    method,
                    message: &message,
                    actor: &settings.actor,
                    signing,
                },
                |p| {
                    if p.head != head || p.base != base {
                        return Err(
                            "merge does not match the explicitly reviewed base/head IDs".into()
                        );
                    }
                    approve(settings, name, p, method)
                },
            )?;
            let updates = [protocol::RefUpdate {
                namespace: RefNamespace::Branch,
                name: prepared.base_branch.clone(),
                expected: Some(prepared.base.clone()),
                new: Some(prepared.merge.as_ref().unwrap().commit.clone()),
                force: false,
            }];
            if let Err(error) = receive::publish(settings, name, repo, &bundle, &updates, &[]) {
                if error.publication_unknown {
                    return Err(format!(
                        "pull merge publication outcome is unknown: {}; use pr recover",
                        error.message
                    ));
                }
                repo.recover_pull_merge(number, prepared.version)
                    .map_err(|e| format!("pull merge rejected; metadata recovery required: {e}"))?;
                return Err(error.message);
            }
            Ok(reply(
                repo.recover_pull_merge(number, prepared.version)
                    .map_err(|e| {
                        format!("pull merge was published; completion record needs recovery: {e}")
                    })?,
            ))
        }
        PullCommand::Recover { number, version } => {
            let p = repo.pull(number)?;
            can_write(settings, &settings.actor, name, &p.base_branch)?;
            Ok(reply(repo.recover_pull_merge(number, version)?))
        }
        command => {
            let number = command.number().ok_or("missing pull number")?;
            let current = repo.pull(number)?;
            let refresh = if matches!(command, PullCommand::Update { refresh: true, .. }) {
                owner_or_writer(settings, name, &current)?;
                Some(source(
                    settings,
                    repo,
                    &current.source_repository,
                    &current.source_branch,
                )?)
            } else {
                None
            };
            let version = match &command {
                PullCommand::Update { version, .. }
                | PullCommand::Close { version, .. }
                | PullCommand::Reopen { version, .. }
                | PullCommand::Ready { version, .. }
                | PullCommand::Draft { version, .. }
                | PullCommand::Comment { version, .. }
                | PullCommand::Review { version, .. }
                | PullCommand::Check { version, .. } => *version,
                _ => return Err("invalid pull mutation".into()),
            };
            let record = repo.edit_pull(
                number,
                version,
                |p| {
                    Ok(match command {
                        PullCommand::Update { title, body, .. } => {
                            owner_or_writer(settings, name, p)?;
                            if let Some(title) = title {
                                p.title = title;
                            }
                            if let Some(body) = body {
                                p.body = body;
                            }
                            PullEventKind::Updated {
                                title: p.title.clone(),
                                body: p.body.clone(),
                            }
                        }
                        PullCommand::Close { .. } => {
                            owner_or_writer(settings, name, p)?;
                            if !matches!(p.state, PullState::Open | PullState::Draft) {
                                return Err("pull request is not open".into());
                            }
                            p.state = PullState::Closed;
                            PullEventKind::Closed
                        }
                        PullCommand::Reopen { .. } => {
                            owner_or_writer(settings, name, p)?;
                            if p.state != PullState::Closed {
                                return Err("pull request is not closed".into());
                            }
                            p.state = PullState::Open;
                            PullEventKind::Reopened
                        }
                        PullCommand::Ready { .. } => {
                            owner_or_writer(settings, name, p)?;
                            if p.state != PullState::Draft {
                                return Err("pull request is not a draft".into());
                            }
                            p.state = PullState::Open;
                            PullEventKind::Ready
                        }
                        PullCommand::Draft { .. } => {
                            owner_or_writer(settings, name, p)?;
                            if p.state != PullState::Open {
                                return Err("pull request is not open".into());
                            }
                            p.state = PullState::Draft;
                            PullEventKind::Draft
                        }
                        PullCommand::Comment { body, .. } => PullEventKind::Comment { body },
                        PullCommand::Review { decision, body, .. } => {
                            can_write(settings, &settings.actor, name, &p.base_branch)?;
                            if p.state != PullState::Open {
                                return Err("only open pull requests can be reviewed".into());
                            }
                            if decision == ReviewDecision::Approve && p.author == settings.actor {
                                return Err("pull authors cannot approve their own request".into());
                            }
                            PullEventKind::Review { decision, body }
                        }
                        PullCommand::Check {
                            context,
                            state,
                            description,
                            ..
                        } => {
                            can_write(settings, &settings.actor, name, &p.base_branch)?;
                            let reference = format!("refs/heads/{}", p.base_branch);
                            if let Some(protection) = &settings.protection {
                                for r in protection.rules(name, &reference) {
                                    if r.check_issuers
                                        .get(&context)
                                        .is_some_and(|allowed| !allowed.contains(&settings.actor))
                                    {
                                        return Err(
                                            "principal is not an allowed issuer for this check"
                                                .into(),
                                        );
                                    }
                                }
                            }
                            PullEventKind::Check {
                                context,
                                state,
                                description,
                            }
                        }
                        _ => return Err("invalid pull metadata operation".into()),
                    })
                },
                &settings.actor,
                refresh.as_ref().map(|(h, b)| (h.as_str(), b.as_slice())),
            )?;
            Ok(reply(record))
        }
    }
}
