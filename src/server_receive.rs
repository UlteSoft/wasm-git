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
}
fn audit(
    settings: &Settings,
    repo: &str,
    updates: &[RefUpdate],
    transaction: &str,
    outcome: &str,
) -> Result<(), String> {
    let record = AuditRecord {
        version: 1,
        transaction: transaction.into(),
        unix_millis: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
        actor: settings.actor.clone(),
        repository: repo.into(),
        updates: updates.to_vec(),
        outcome: outcome.into(),
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
        if record.version != 1 {
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
fn hook(
    path: &Path,
    settings: &Settings,
    repo: &Repository,
    updates: &[RefUpdate],
    bundle: &[u8],
) -> Result<(), String> {
    let directory = repo.root.join(".uwgit/tmp").join(format!(
        "receive-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let quarantine = Quarantine(directory);
    let bundle_path = quarantine.0.join("bundle.bin");
    fs::write(&bundle_path, bundle).map_err(|e| e.to_string())?;
    let updates_path = quarantine.0.join("updates.bin");
    fs::write(&updates_path, crate::binary::encode(updates)?).map_err(|e| e.to_string())?;
    let mut child = Command::new(path)
        .current_dir(&repo.root)
        .env("UWGIT_ACTOR", &settings.actor)
        .env("UWGIT_REPOSITORY", &repo.root)
        .env("UWGIT_QUARANTINE_BUNDLE", &bundle_path)
        .env("UWGIT_UPDATES_FILE", &updates_path)
        .stdin(Stdio::from(
            fs::File::open(&updates_path).map_err(|e| e.to_string())?,
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start receive hook: {e}"))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("receive hook rejected the update ({status})")),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("receive hook exceeded 15 second deadline".into());
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
) -> Result<(), ReceiveError> {
    let transaction = format!(
        "{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    audit(settings, name, updates, &transaction, "prepared").map_err(|message| ReceiveError {
        message,
        publication_unknown: false,
    })?;
    let result = repo.receive_batch_checked(bytes, updates, |bundle| {
        if let Some(path) = &settings.signed_authors {
            repo.verify_receive_authors(bundle, updates, path)?;
        }
        if let Some(path) = &settings.pre_receive {
            hook(path, settings, repo, updates, bundle)?;
        }
        Ok(())
    });
    let outcome = match &result {
        Ok(()) => "accepted",
        Err(e) if e.publication_unknown => "unknown",
        Err(_) => "rejected",
    };
    if let Err(error) = audit(settings, name, updates, &transaction, outcome) {
        eprintln!("receive {transaction} {outcome}; audit completion failed: {error}");
    }
    if result.is_ok() {
        if let Some(path) = &settings.post_receive {
            // Notification cannot turn an already published transaction into a
            // rejection. Its failure is visible in stderr and a durable event.
            if let Err(error) = hook(path, settings, repo, updates, &[]) {
                eprintln!("published receive {transaction}; post-receive hook failed: {error}");
                let _ = audit(settings, name, updates, &transaction, "post-receive-failed");
            }
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
        std::process::id(),
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
        )?;
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
        fs::File::open(&settings.root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
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
    ) {
        eprintln!("fork audit: {error}");
    }
    result.map_err(|message| ReceiveError {
        message,
        publication_unknown: published,
    })
}
