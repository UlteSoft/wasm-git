//! Durable binary receive audit and administrator-controlled quarantine hooks.
use super::*;
use crate::protocol::RefUpdate;
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
struct AuditRecord {
    version: u32,
    transaction: String,
    unix_millis: u128,
    actor: String,
    repository: String,
    updates: Vec<RefUpdate>,
    outcome: String,
    push_options: Vec<String>,
    rejections: Vec<Option<String>>,
}
fn audit(
    settings: &Settings,
    repo: &str,
    updates: &[RefUpdate],
    transaction: &str,
    outcome: &str,
    options: &[String],
) -> Result<(), String> {
    audit_with_rejections(settings, repo, updates, transaction, outcome, options, &[])
}

fn audit_with_rejections(
    settings: &Settings,
    repo: &str,
    updates: &[RefUpdate],
    transaction: &str,
    outcome: &str,
    options: &[String],
    rejections: &[Option<String>],
) -> Result<(), String> {
    let record = AuditRecord {
        version: 3,
        transaction: transaction.into(),
        unix_millis: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
        actor: settings.actor.clone(),
        repository: repo.into(),
        updates: updates.to_vec(),
        outcome: outcome.into(),
        push_options: options.to_vec(),
        rejections: rejections.to_vec(),
    };
    let bytes = crate::binary::encode(&record)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("audit record exceeds 4 MiB".into());
    }
    let path = settings.root.join("receive-audit.bin");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err("invalid audit file".into());
        }
    }
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    // LockFileEx requires GENERIC_READ or GENERIC_WRITE; Windows append-only
    // access supplies neither. Keep append semantics while admitting the lock.
    #[cfg(windows)]
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(e) => return Err(format!("audit lock: {e}")),
        }
    }
    file.write_all(&(bytes.len() as u32).to_be_bytes())
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())
}

pub(super) fn read_audit(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(real_path(path, false)?).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut consumed = 0;
    loop {
        let mut length = [0; 4];
        let n = file.read(&mut length).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if n != 4 {
            return Err("incomplete audit tail; preserve file for recovery".into());
        }
        let length = u32::from_be_bytes(length) as usize;
        consumed += length;
        if length > 4 * 1024 * 1024 || consumed > 64 * 1024 * 1024 {
            return Err("audit query exceeds bounds; rotate the log first".into());
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        let record: AuditRecord = crate::binary::decode(&bytes)?;
        if record.version != 3 {
            return Err("unsupported audit record".into());
        }
        rows.push(record);
    }
    serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())
}

struct Quarantine(PathBuf);
impl Drop for Quarantine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct HookInput {
    _quarantine: Quarantine,
    bundle: PathBuf,
    updates: PathBuf,
}
impl HookInput {
    fn new(repo: &Repository, updates: &[RefUpdate], bundle: &[u8]) -> Result<Self, String> {
        let directory = repo.root.join(".uwgit/tmp").join(format!(
            "receive-{}-{}",
            crate::platform_fs::process_id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&directory).map_err(|e| e.to_string())?;
        let quarantine = Quarantine(directory);
        let bundle_path = quarantine.0.join("bundle.bin");
        let updates_path = quarantine.0.join("updates.bin");
        fs::write(&bundle_path, bundle).map_err(|e| e.to_string())?;
        fs::write(&updates_path, crate::binary::encode(updates)?).map_err(|e| e.to_string())?;
        Ok(Self {
            _quarantine: quarantine,
            bundle: bundle_path,
            updates: updates_path,
        })
    }
}
struct HookContext<'a> {
    settings: &'a Settings,
    repo: &'a Repository,
    input: &'a HookInput,
    options: &'a [String],
    transaction: &'a str,
    deadline: Instant,
}
fn reference(update: &RefUpdate) -> String {
    format!(
        "refs/{}/{}",
        if matches!(update.namespace, RefNamespace::Branch) {
            "heads"
        } else {
            "tags"
        },
        update.name
    )
}
fn hook(
    path: &Path,
    context: &HookContext<'_>,
    phase: &str,
    args: &[String],
) -> Result<(), String> {
    if Instant::now() >= context.deadline {
        return Err(format!("{phase} hook exceeded the receive hook deadline"));
    }
    let mut command = Command::new(path);
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|k| k.starts_with("UWGIT_PUSH_OPTION_"))
        {
            command.env_remove(key);
        }
    }
    for (index, value) in context.options.iter().enumerate() {
        command.env(format!("UWGIT_PUSH_OPTION_{index}"), value);
    }
    let mut child = command
        .args(args)
        .current_dir(&context.repo.root)
        .env("UWGIT_ACTOR", &context.settings.actor)
        .env("UWGIT_REPOSITORY", &context.repo.root)
        .env("UWGIT_QUARANTINE_BUNDLE", &context.input.bundle)
        .env("UWGIT_UPDATES_FILE", &context.input.updates)
        .env("UWGIT_PUSH_OPTION_COUNT", context.options.len().to_string())
        .env("UWGIT_RECEIVE_TRANSACTION", context.transaction)
        .env("UWGIT_HOOK", phase)
        .stdin(Stdio::from(
            fs::File::open(&context.input.updates).map_err(|e| e.to_string())?,
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start {phase} hook: {e}"))?;
    let deadline = context.deadline;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("{phase} hook rejected the update ({status})")),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{phase} hook exceeded 15 second deadline"));
            }
        }
    }
}

pub(super) fn publish(
    settings: &Settings,
    name: &str,
    repo: &Repository,
    bytes: &[u8],
    updates: &[RefUpdate],
    options: &[String],
) -> Result<(), ReceiveError> {
    let transaction = format!(
        "{}-{}-{}",
        crate::platform_fs::process_id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    audit(settings, name, updates, &transaction, "prepared", options).map_err(|message| {
        ReceiveError {
            message,
            publication_unknown: false,
        }
    })?;
    let result = repo.receive_batch_checked(bytes, updates, |bundle| {
        if let Some(path) = &settings.signed_authors {
            repo.verify_receive_authors(bundle, updates, path)?;
        }
        if settings.pre_receive.is_some() || settings.update_hook.is_some() {
            let input = HookInput::new(repo, updates, bundle)?;
            let context = HookContext {
                settings,
                repo,
                input: &input,
                options,
                transaction: &transaction,
                deadline: Instant::now() + TIMEOUT,
            };
            if let Some(path) = &settings.pre_receive {
                hook(path, &context, "pre-receive", &[])?;
            }
            if let Some(path) = &settings.update_hook {
                let zero = "0".repeat(64);
                for update in updates {
                    hook(
                        path,
                        &context,
                        "update",
                        &[
                            reference(update),
                            update.expected.as_ref().unwrap_or(&zero).clone(),
                            update.new.as_ref().unwrap_or(&zero).clone(),
                        ],
                    )?;
                }
            }
        }
        Ok(())
    });
    let outcome = match &result {
        Ok(()) => "accepted",
        Err(e) if e.publication_unknown => "unknown",
        Err(_) => "rejected",
    };
    if let Err(error) = audit(settings, name, updates, &transaction, outcome, options) {
        eprintln!("receive {transaction} {outcome}; audit completion failed: {error}");
    }
    if result.is_ok() {
        notify_received(settings, name, repo, updates, options, &transaction);
    }
    result
}

fn notify_received(
    settings: &Settings,
    name: &str,
    repo: &Repository,
    updates: &[RefUpdate],
    options: &[String],
    transaction: &str,
) {
    if !updates.is_empty() && (settings.post_receive.is_some() || settings.post_update.is_some()) {
        // Notification failures cannot revoke an acknowledged publication.
        let input = HookInput::new(repo, updates, &[]);
        let deadline = Instant::now() + TIMEOUT;
        for (phase, path) in [
            ("post-receive", &settings.post_receive),
            ("post-update", &settings.post_update),
        ] {
            if let Some(path) = path {
                let outcome = match &input {
                    Ok(input) => {
                        let context = HookContext {
                            settings,
                            repo,
                            input,
                            options,
                            transaction,
                            deadline,
                        };
                        let args = if phase == "post-update" {
                            updates.iter().map(reference).collect()
                        } else {
                            Vec::new()
                        };
                        hook(path, &context, phase, &args)
                    }
                    Err(error) => Err(error.clone()),
                };
                if let Err(error) = outcome {
                    eprintln!("published receive {transaction}; {phase} hook failed: {error}");
                    let _ = audit(
                        settings,
                        name,
                        updates,
                        transaction,
                        &format!("{phase}-failed"),
                        options,
                    );
                }
            }
        }
    }
}

pub(super) fn publish_selective(
    settings: &Settings,
    name: &str,
    repo: &Repository,
    batch: &protocol::BatchPublication,
    options: &[String],
    dry_run: bool,
) -> Result<Vec<Option<String>>, ReceiveError> {
    let transaction = format!(
        "receive-{}-{}-{}",
        crate::platform_fs::process_id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let updates = &batch.updates;
    if !dry_run {
        audit(settings, name, updates, &transaction, "prepared", options).map_err(|message| {
            ReceiveError {
                message,
                publication_unknown: false,
            }
        })?;
    }
    let result =
        repo.receive_batch_selective(&batch.bundle, updates, dry_run, |bundle, reasons| {
            let mut rejections = reasons.to_vec();
            for (update, rejection) in updates.iter().zip(&mut rejections) {
                if rejection.is_none() {
                    *rejection = authorize(
                        settings,
                        name,
                        &Operation::UpdateRef {
                            namespace: update.namespace.clone(),
                            name: update.name.clone(),
                            expected: update.expected.clone(),
                            new: update.new.clone(),
                            force: update.force,
                        },
                        &[],
                    )
                    .err();
                }
            }
            if let Some(path) = &settings.signed_authors {
                let candidates: Vec<_> = updates
                    .iter()
                    .zip(&rejections)
                    .filter(|(_, r)| r.is_none())
                    .map(|(u, _)| u.clone())
                    .collect();
                if !candidates.is_empty() {
                    repo.verify_receive_authors(bundle, &candidates, path)?;
                }
            }
            if !dry_run && (settings.pre_receive.is_some() || settings.update_hook.is_some()) {
                let input = HookInput::new(repo, updates, bundle)?;
                let context = HookContext {
                    settings,
                    repo,
                    input: &input,
                    options,
                    transaction: &transaction,
                    deadline: Instant::now() + TIMEOUT,
                };
                if let Some(path) = &settings.pre_receive {
                    hook(path, &context, "pre-receive", &[])?;
                }
                if let Some(path) = &settings.update_hook {
                    let zero = "0".repeat(64);
                    for (update, rejection) in updates.iter().zip(&mut rejections) {
                        if rejection.is_none() {
                            *rejection = hook(
                                path,
                                &context,
                                "update",
                                &[
                                    reference(update),
                                    update.expected.as_ref().unwrap_or(&zero).clone(),
                                    update.new.as_ref().unwrap_or(&zero).clone(),
                                ],
                            )
                            .err();
                        }
                    }
                }
            }
            Ok(rejections)
        });
    if !dry_run {
        let outcome = match &result {
            Ok(reasons) if reasons.iter().all(Option::is_none) => "accepted",
            Ok(reasons) if reasons.iter().all(Option::is_some) => "rejected",
            Ok(_) => "partial",
            Err(e) if e.publication_unknown => "unknown",
            Err(_) => "rejected",
        };
        let reasons = result.as_ref().map_or(&[][..], |r| r.as_slice());
        if let Err(error) = audit_with_rejections(
            settings,
            name,
            updates,
            &transaction,
            outcome,
            options,
            reasons,
        ) {
            eprintln!("receive {transaction} {outcome}; audit completion failed: {error}");
        }
        if let Ok(reasons) = &result {
            let accepted: Vec<_> = updates
                .iter()
                .zip(reasons)
                .filter(|(_, r)| r.is_none())
                .map(|(u, _)| u.clone())
                .collect();
            notify_received(settings, name, repo, &accepted, options, &transaction);
        }
    }
    result
}

pub(super) fn fork(
    settings: &Settings,
    source_name: &str,
    source: &Repository,
    destination: &str,
) -> Result<(), ReceiveError> {
    let transaction = format!(
        "fork-{}-{}-{}",
        crate::platform_fs::process_id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let staging = settings.root.join(format!(".{transaction}"));
    let target = settings.root.join(destination);
    let mut published = false;
    let result = (|| {
        audit(
            settings,
            source_name,
            &[],
            &transaction,
            &format!("fork-prepared:{destination}"),
            &[],
        )?;
        let parent = repository_parent(settings, destination, true)?;
        if target.try_exists().map_err(|e| e.to_string())? {
            return Err("fork destination already exists".into());
        }
        // Own the container before installing any cleanup guard. A collision
        // must never delete a pre-existing staging directory.
        fs::create_dir(&staging).map_err(|e| e.to_string())?;
        let _cleanup = Quarantine(staging.clone());
        let checkout = staging.join("repository");
        Repository::fork_local(source.root_path(), &checkout)?;
        // Reserve the name without replacing an existing repository. A client
        // cannot open this empty reservation before the atomic directory rename.
        fs::create_dir(&target).map_err(|e| e.to_string())?;
        if let Err(error) = fs::rename(&checkout, &target) {
            let _ = fs::remove_dir(&target);
            return Err(error.to_string());
        }
        published = true;
        crate::output::sync_directory(&parent)?;
        Ok(())
    })();
    let outcome = if result.is_ok() {
        "fork-accepted"
    } else if published {
        "fork-unknown"
    } else {
        "fork-rejected"
    };
    if let Err(error) = audit(
        settings,
        source_name,
        &[],
        &transaction,
        &format!("{outcome}:{destination}"),
        &[],
    ) {
        eprintln!("fork audit: {error}");
    }
    result.map_err(|message| ReceiveError {
        message,
        publication_unknown: published,
    })
}
