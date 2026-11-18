//! Bounded HTTP RPC and SSH-invoked stdio service. Never executes Wasm.

use crate::protocol::{self, Operation, RefNamespace, Request, Response};
use crate::repository::{ReceiveError, Repository};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "server --root DIR [--listen 127.0.0.1:9419] [--token-file FILE | --policy BOOTSTRAP_FILE] [--require-operation-credentials] [--allow-write | --read-only] [--allow-force] [--signed-authors BINARY_TRUST_FILE] [--protection BINARY_PROTECTION_FILE] [--merge-author NAME --merge-signing-key KEY --merge-signing-format native|ssh|openpgp] [--pre-receive PROGRAM] [--update PROGRAM] [--post-receive PROGRAM] [--post-update PROGRAM] [--max-connections N]; server --stdio --root DIR [--policy BOOTSTRAP_FILE --principal NAME] [--read-only] [--allow-force]; server policy-set ROOT REQUEST_JSON; server policy-query ROOT REQUEST_JSON; server policy-migrate ROOT REQUEST_JSON; server operation-credential-issue ROOT REQUEST_JSON; server operation-credential-query ROOT REQUEST_JSON; server policy-status ROOT; server compile-policy INPUT_JSON NEW_BINARY_FILE; server compile-protection INPUT_JSON NEW_BINARY_FILE; server audit-log BINARY_LOG; server operation-recover ROOT REQUEST_JSON; server operation-context ROOT REQUEST_JSON; server audit-query ROOT REQUEST_JSON; server capacity-query ROOT REQUEST_JSON";
const MAX_HEADER: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

#[path = "server_admin_identity.rs"]
mod admin_identity;
#[path = "server_policy.rs"]
mod policy;
#[path = "server_pulls.rs"]
mod pulls;
#[path = "server_receive.rs"]
mod receive;
#[path = "server_recovery.rs"]
mod recovery;

#[derive(Clone)]
struct Settings {
    protection: Option<Arc<pulls::Protection>>,
    merge_signer: Option<pulls::MergeSigner>,
    root: PathBuf,
    token: Option<Vec<u8>>,
    write: bool,
    force: bool,
    policy_enabled: bool,
    policy: Option<Arc<protocol::ServerPolicy>>,
    require_operation_credentials: bool,
    operation_credential: Option<protocol::OperationCredentialReceipt>,
    actor: String,
    pre_receive: Option<PathBuf>,
    update_hook: Option<PathBuf>,
    signed_authors: Option<PathBuf>,
    post_receive: Option<PathBuf>,
    post_update: Option<PathBuf>,
}

/// Refuse symbolic links in every existing path component, including root.
fn real_path(path: &Path, directory: bool) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut prefix = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => return Err("server paths must not contain '..'".into()),
            Component::CurDir => continue,
            _ => prefix.push(component),
        }
        let metadata = fs::symlink_metadata(&prefix).map_err(|e| format!("server path: {e}"))?;
        if metadata.file_type().is_symlink() {
            return Err("server paths must not contain symbolic links".into());
        }
    }
    let metadata = fs::symlink_metadata(&prefix).map_err(|e| e.to_string())?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err("server path has the wrong file type".into());
    }
    Ok(prefix)
}

fn repository_name(name: &str) -> Result<(), String> {
    protocol::repository_name(name)
}

// Namespace directories may contain repositories, but a repository's working
// files must never become an alternative server root or a fork destination.
fn repository_parent(settings: &Settings, name: &str, create: bool) -> Result<PathBuf, String> {
    repository_parent_in(&settings.root, name, create)
}

fn repository_parent_in(root: &Path, name: &str, create: bool) -> Result<PathBuf, String> {
    repository_name(name)?;
    let mut parent = root.to_path_buf();
    let components: Vec<_> = name.split('/').collect();
    for component in &components[..components.len() - 1] {
        namespace_directory(&parent)?;
        let next = parent.join(component);
        if create {
            match fs::create_dir(&next) {
                Ok(()) => crate::output::sync_directory(&parent)?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error.to_string()),
            }
        }
        parent = real_path(&next, true)?;
    }
    namespace_directory(&parent)?;
    Ok(parent)
}

fn namespace_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path.join(".wasm-git")) {
        Ok(_) => Err("server namespaces must not be nested inside a repository".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn repository_path(settings: &Settings, name: &str) -> Result<PathBuf, String> {
    repository_path_in(&settings.root, name)
}

fn repository_path_in(root: &Path, name: &str) -> Result<PathBuf, String> {
    let parent = repository_parent_in(root, name, false)?;
    real_path(&parent.join(name.rsplit('/').next().unwrap()), true)
}

fn authorize(
    settings: &Settings,
    repository: &str,
    operation: &Operation,
    payload: &[u8],
) -> Result<(), String> {
    let mut limit = credential_limit(settings);
    let mut check = |reference: Option<&str>, force: bool| -> Result<(), String> {
        if reference.is_some_and(|r| {
            settings
                .protection
                .as_ref()
                .is_some_and(|p| p.protected(repository, r))
        }) {
            return Err("protected branch requires an approved pull request merge".into());
        }
        if reference.is_some() && !settings.write {
            return Err("server is read-only".into());
        }
        if force && !settings.force {
            return Err("server does not allow forced updates".into());
        }
        if let Some(policy) = &settings.policy {
            limit = limit.intersect(policy.authorization_limit(
                &settings.actor,
                repository,
                reference,
                force,
                false,
            )?);
        }
        Ok(())
    };
    check(None, false)?;
    match operation {
        Operation::Push { branch, force, .. } => {
            check(Some(&format!("refs/heads/{branch}")), *force)
        }
        Operation::UpdateRef {
            namespace,
            name,
            force,
            ..
        } => check(
            Some(&format!(
                "refs/{}/{name}",
                if matches!(namespace, RefNamespace::Branch) {
                    "heads"
                } else {
                    "tags"
                }
            )),
            *force,
        ),
        Operation::PublishBatch {} | Operation::PublishOptions {} | Operation::CheckBatch {} => {
            check(Some("*"), false)?;
            let (batch, _) = protocol::publication_payload(operation, payload)?;
            if batch.updates.is_empty() || batch.updates.len() > 4096 {
                return Err("receive requires 1..=4096 updates".into());
            }
            for u in batch.updates {
                check(
                    Some(&format!(
                        "refs/{}/{}",
                        if matches!(u.namespace, RefNamespace::Branch) {
                            "heads"
                        } else {
                            "tags"
                        },
                        u.name
                    )),
                    u.force,
                )?;
            }
            Ok(())
        }
        Operation::PublishPartial { .. } => {
            check(Some("*"), false)?;
            let (batch, _) = protocol::publication_payload(operation, payload)?;
            if batch.updates.is_empty() || batch.updates.len() > 4096 {
                return Err("receive requires 1..=4096 updates".into());
            }
            // Individual grants and force rules produce per-ref rejections
            // under the same repository lock as lease validation.
            Ok(())
        }
        Operation::ApplyUpload { .. }
        | Operation::UploadSlice { .. }
        | Operation::CancelUpload { .. } => Err("legacy unscoped uploads are disabled".into()),
        Operation::ScopedUploadSlice { binding, .. }
        | Operation::ApplyScopedUpload { binding, .. }
        | Operation::CancelScopedUpload { binding, .. } => {
            binding.validate()?;
            for target in &binding.targets {
                check(Some(&target.reference), target.force)?;
            }
            Ok(())
        }
        Operation::QueryScopedUpload { binding, .. } => binding.validate(),
        Operation::QueryChunk { chunk } => chunk.validate_scope(repository, &settings.actor),
        Operation::UploadChunk { chunk } => {
            chunk.validate_scope(repository, &settings.actor)?;
            for target in &chunk.scope.binding.targets {
                check(Some(&target.reference), target.force)?;
            }
            Ok(())
        }
        Operation::ReleaseRetention { command } => {
            if matches!(
                command,
                protocol::RetentionCommand::Prepare { .. }
                    | protocol::RetentionCommand::Promote { .. }
                    | protocol::RetentionCommand::Close { .. }
            ) && !settings.write
            {
                return Err("server is read-only".into());
            }
            limit = limit.intersect(
                settings
                    .policy
                    .as_ref()
                    .ok_or("release retention requires an explicit principal policy")?
                    .retention_limit(&settings.actor, repository)?,
            );
            Ok(())
        }
        Operation::Fork { destination } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            repository_name(destination)?;
            if let Some(policy) = &settings.policy {
                limit = limit.intersect(policy.authorization_limit(
                    &settings.actor,
                    destination,
                    None,
                    false,
                    true,
                )?);
            }
            Ok(())
        }
        _ => Ok(()),
    }?;
    limit.require_current()
}

fn credential_limit(settings: &Settings) -> policy::AuthorizationLimit {
    settings
        .operation_credential
        .as_ref()
        .map_or_else(policy::AuthorizationLimit::unbounded, |receipt| {
            policy::AuthorizationLimit::until(receipt.expires_at)
        })
}

// Pull merges have already passed branch protection through their own review
// path, so the final check here revalidates grants without rerouting protection.
fn authorize_updates(
    settings: &Settings,
    repository: &str,
    updates: &[protocol::RefUpdate],
    reasons: &[Option<String>],
) -> Result<(), String> {
    if updates.len() != reasons.len() {
        return Err("reference authorization decisions do not match updates".into());
    }
    let mut limit = credential_limit(settings);
    if let Some(policy) = &settings.policy {
        limit = limit.intersect(policy.authorization_limit(
            &settings.actor,
            repository,
            None,
            false,
            false,
        )?);
        for (update, rejection) in updates.iter().zip(reasons) {
            if rejection.is_none() {
                let namespace = if matches!(update.namespace, RefNamespace::Branch) {
                    "heads"
                } else {
                    "tags"
                };
                limit = limit.intersect(policy.authorization_limit(
                    &settings.actor,
                    repository,
                    Some(&format!("refs/{namespace}/{}", update.name)),
                    update.force,
                    false,
                )?);
            }
        }
    }
    limit.require_current()
}

fn process(
    settings: &Settings,
    request: Request,
    payload: &[u8],
) -> Result<(Response, Vec<u8>), String> {
    if settings.require_operation_credentials && settings.operation_credential.is_none() {
        return Err("operation credential required".into());
    }
    if settings.require_operation_credentials
        && matches!(&request.operation, Operation::ScopedUploadSlice { .. })
    {
        return Err("strict uploads require an original-operation chunk request".into());
    }
    if let Some(credential) = &settings.operation_credential {
        if credential.actor != settings.actor {
            return Err("operation credential actor mismatch".into());
        }
        credential.validate_rpc(&request, payload)?;
    }
    if let Operation::UploadChunk { chunk } | Operation::QueryChunk { chunk } = &request.operation {
        chunk.validate_scope(&request.repository, &settings.actor)?;
        if settings.operation_credential.is_none() {
            return Err("chunk operations require an operation credential".into());
        }
        if let Operation::UploadChunk { .. } = &request.operation {
            chunk.validate_payload(payload)?;
        } else if !payload.is_empty() {
            return Err("chunk query requires an empty payload".into());
        }
    }
    repository_name(&request.repository)?;
    authorize(settings, &request.repository, &request.operation, payload)?;
    if let Operation::UploadChunk { chunk } | Operation::QueryChunk { chunk } = &request.operation {
        policy::validate_original_chunk(&settings.root, chunk)?;
    }
    let attempt = match (
        &settings.operation_credential,
        recovery::observation_scope(&request.operation),
    ) {
        (Some(credential), Some(scope)) if credential.effect_key.is_some() => Some((
            credential.operation_id.clone(),
            policy::audit_start(&settings.root, credential, &request, payload)?,
            scope,
        )),
        _ => None,
    };
    let mut observed = protocol::AuditEvidenceObservation::Unknown;
    let result = (|| {
        let root = repository_path(settings, &request.repository)?;
        let expected = if attempt.is_some() {
            match &request.operation {
                Operation::UploadChunk { chunk } => Some(chunk.scope.clone()),
                Operation::ScopedUploadSlice {
                    session, binding, ..
                }
                | Operation::ApplyScopedUpload { session, binding }
                | Operation::CancelScopedUpload { session, binding } => {
                    Some(protocol::ScopedExecution {
                        actor: settings.actor.clone(),
                        repository: request.repository.clone(),
                        session: session.clone(),
                        binding: binding.clone(),
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        let repository = if settings.require_operation_credentials {
            Repository::open_for_recovery(root, expected.as_ref())?
        } else {
            Repository::open(root)?
        };
        // Root authorization gate -> service gate -> repository writer. An
        // audit attempt precedes even journal recovery performed on open.
        let gate = repository.service_lock(TIMEOUT)?;
        authorize(settings, &request.repository, &request.operation, payload)?;
        let original = request.clone();
        let result = process_locked(settings, request, payload, &gate, None, &repository);
        if attempt.is_some()
            && result
                .as_ref()
                .is_ok_and(|(response, _)| response.ok && !response.publication_unknown)
        {
            match recovery::observe_native(&repository, &gate, &settings.actor, &original) {
                Ok(evidence) => observed = evidence,
                Err(cause) => {
                    crate::cli_error(&format!("original operation result observation: {cause}"));
                    return Ok((Response { version: 1, ok: false, publication_unknown: true,
                        error: Some("native result observation is unavailable; query the original operation".into()), state: None, tip: None }, Vec::new()));
                }
            }
        }
        result
    })();
    if let Some((operation_id, attempt_id, scope)) = attempt {
        // A rejected retry does not prove the original operation failed.
        let observation = protocol::AuditObservation {
            version: 1,
            scope,
            evidence: observed,
        };
        if let Err(cause) = policy::audit_observe(
            &settings.root,
            &protocol::AuditTarget::Operation {
                operation_id,
                attempt_id,
            },
            &observation,
        ) {
            crate::cli_error(&format!("original operation audit observation: {cause}"));
            return Ok((Response { version: 1, ok: false, publication_unknown: true,
                error: Some("operation observation is unconfirmed; query or recover the original operation".into()), state: None, tip: None }, Vec::new()));
        }
    }
    result
}

fn process_locked(
    settings: &Settings,
    request: Request,
    payload: &[u8],
    gate: &crate::repository::ServiceGuard,
    scope: Option<&crate::repository::ScopedExecution>,
    repository: &Repository,
) -> Result<(Response, Vec<u8>), String> {
    if request.version != 1 {
        return Err("unsupported RPC version".into());
    }
    repository_name(&request.repository)?;
    let original_operation = request.operation.clone();
    let before_publish = || authorize(settings, &request.repository, &original_operation, payload);
    let mut response = Response {
        version: 1,
        ok: true,
        publication_unknown: false,
        error: None,
        state: None,
        tip: None,
    };
    let bytes = match request.operation {
        Operation::UploadChunk { chunk } | Operation::QueryChunk { chunk } => {
            let writes = matches!(&original_operation, Operation::UploadChunk { .. });
            let result = if writes {
                repository.scoped_chunk_write(gate, &chunk, payload, before_publish)
            } else {
                repository.scoped_chunk_query(gate, &chunk)
            };
            match result {
                Ok(status) => {
                    let encoded = status.validate_for(&chunk).and_then(|()| {
                        if writes
                            && !matches!(status.state, protocol::ScopedChunkState::Accepted { .. })
                        {
                            return Err("chunk acknowledgement has no accepted decision".into());
                        }
                        crate::binary::encode(&status)
                    });
                    match encoded {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            return Ok((
                                Response {
                                    version: 1,
                                    ok: false,
                                    publication_unknown: writes,
                                    error: Some(error),
                                    state: None,
                                    tip: None,
                                },
                                Vec::new(),
                            ))
                        }
                    }
                }
                Err(error) => {
                    return Ok((
                        Response {
                            version: 1,
                            ok: false,
                            publication_unknown: error.outcome_unknown,
                            error: Some(error.message),
                            state: None,
                            tip: None,
                        },
                        Vec::new(),
                    ))
                }
            }
        }
        Operation::ReleaseRetention { command } => {
            if !payload.is_empty() {
                return Err("release retention requires an empty payload".into());
            }
            match repository.release_retention_checked(
                &request.repository,
                &settings.actor,
                command,
                before_publish,
            ) {
                Ok(reply) => crate::binary::encode(&reply)?,
                Err(error) => {
                    return Ok((
                        Response {
                            version: 1,
                            ok: false,
                            publication_unknown: error.outcome_unknown,
                            error: Some(error.message),
                            state: None,
                            tip: None,
                        },
                        Vec::new(),
                    ))
                }
            }
        }
        Operation::ScopedUploadSlice {
            session,
            binding,
            offset,
        } => {
            if let Err(error) = repository.scoped_upload_slice_checked(
                &request.repository,
                &settings.actor,
                &session,
                &binding,
                offset,
                payload,
                before_publish,
            ) {
                return Ok((
                    Response {
                        version: 1,
                        ok: false,
                        publication_unknown: error.outcome_unknown,
                        error: Some(error.message),
                        state: None,
                        tip: None,
                    },
                    Vec::new(),
                ));
            }
            Vec::new()
        }
        Operation::QueryScopedUpload { session, binding } => {
            if !payload.is_empty() {
                return Err("upload query requires an empty payload".into());
            }
            let status = if settings.require_operation_credentials {
                repository.scoped_upload_observe(
                    gate,
                    &request.repository,
                    &settings.actor,
                    &session,
                    &binding,
                )?
            } else {
                repository.scoped_upload_status(
                    gate,
                    &request.repository,
                    &settings.actor,
                    &session,
                    &binding,
                )?
            };
            crate::binary::encode(&status)?
        }
        Operation::CancelScopedUpload { session, binding } => {
            if !payload.is_empty() {
                return Err("upload cancellation requires an empty payload".into());
            }
            if let Err(error) = repository.scoped_upload_cancel_checked(
                &request.repository,
                &settings.actor,
                &session,
                &binding,
                before_publish,
            ) {
                return Ok((
                    Response {
                        version: 1,
                        ok: false,
                        publication_unknown: error.outcome_unknown,
                        error: Some(error.message),
                        state: None,
                        tip: None,
                    },
                    Vec::new(),
                ));
            }
            Vec::new()
        }
        Operation::ApplyScopedUpload { session, binding } => {
            if !payload.is_empty() {
                return Err("upload application requires an empty payload".into());
            }
            let status = if settings.require_operation_credentials {
                repository.scoped_upload_observe(
                    gate,
                    &request.repository,
                    &settings.actor,
                    &session,
                    &binding,
                )?
            } else {
                repository.scoped_upload_status(
                    gate,
                    &request.repository,
                    &settings.actor,
                    &session,
                    &binding,
                )?
            };
            match status.state {
                Some(protocol::ScopedUploadState::Finished { response, payload }) => {
                    return Ok((response, payload))
                }
                Some(protocol::ScopedUploadState::FinishedV4 { receipt }) => {
                    return Ok((receipt.response, receipt.payload))
                }
                Some(
                    protocol::ScopedUploadState::Applying
                    | protocol::ScopedUploadState::ApplyingV4 { .. },
                ) => {
                    return Ok((
                        Response {
                            version: 1,
                            ok: false,
                            publication_unknown: true,
                            error: Some(
                                "scoped upload outcome is unknown; recover the original operation"
                                    .into(),
                            ),
                            state: None,
                            tip: None,
                        },
                        Vec::new(),
                    ))
                }
                Some(protocol::ScopedUploadState::Canceled) => {
                    return Err("scoped upload is canceled".into())
                }
                Some(protocol::ScopedUploadState::Aborted { .. }) => {
                    return Err("scoped upload was aborted before its native decision".into())
                }
                _ => (),
            }
            let bytes = repository.scoped_upload_read(
                &request.repository,
                &settings.actor,
                &session,
                &binding,
            )?;
            authorize(settings, &request.repository, &binding.operation, &bytes)?;
            if let Err(error) = repository.scoped_upload_applying_checked(
                gate,
                &request.repository,
                &settings.actor,
                &session,
                &binding,
                before_publish,
            ) {
                return Ok((
                    Response {
                        version: 1,
                        ok: false,
                        publication_unknown: error.outcome_unknown,
                        error: Some(error.message),
                        state: None,
                        tip: None,
                    },
                    Vec::new(),
                ));
            }
            let execution = crate::repository::ScopedExecution {
                actor: settings.actor.clone(),
                repository: request.repository.clone(),
                session: session.clone(),
                binding: binding.clone(),
            };
            let result = process_locked(
                settings,
                Request {
                    version: request.version,
                    repository: request.repository,
                    operation: *binding.operation.clone(),
                },
                &bytes,
                gate,
                Some(&execution),
                repository,
            );
            // An error after entering application may follow a committed ref
            // update; retain an unknown outcome rather than claiming failure.
            let (reply, _reply_bytes) = result.unwrap_or_else(|error| {
                (
                    Response {
                        version: 1,
                        ok: false,
                        publication_unknown: true,
                        error: Some(error),
                        state: None,
                        tip: None,
                    },
                    Vec::new(),
                )
            });
            let durable = if reply.ok {
                match repository.scoped_upload_status(
                    gate,
                    &execution.repository,
                    &settings.actor,
                    &session,
                    &binding,
                ) {
                    Ok(protocol::ScopedUploadStatus {
                        state: Some(protocol::ScopedUploadState::Finished { response, payload }),
                        ..
                    }) => Ok((response, payload)),
                    Ok(protocol::ScopedUploadStatus {
                        state: Some(protocol::ScopedUploadState::FinishedV4 { receipt }),
                        ..
                    }) => Ok((receipt.response, receipt.payload)),
                    Ok(_) => Err("scoped success has no durable receipt".into()),
                    Err(error) => Err(error),
                }
            } else {
                repository.scoped_record_failure(gate, &execution, reply)
            };
            match durable {
                Ok(result) => {
                    if let Err(error) = repository.scoped_upload_cleanup(&session) {
                        crate::cli_error(&format!("scoped upload cleanup: {error}"));
                    }
                    return Ok(result);
                }
                Err(error) => {
                    return Ok((
                        Response {
                            version: 1,
                            ok: false,
                            publication_unknown: true,
                            error: Some(format!(
                            "scoped upload outcome is unknown; query the original session: {error}"
                        )),
                            state: None,
                            tip: None,
                        },
                        Vec::new(),
                    ))
                }
            }
        }
        Operation::PullRequest {} => {
            let command: crate::pull::PullCommand = crate::binary::decode(payload)?;
            let mutates = command.mutates();
            match pulls::run(settings, &request.repository, repository, command) {
                Ok(reply) => {
                    use sha2::Digest;
                    response.tip = Some(format!("{:x}", sha2::Sha256::digest(payload)));
                    crate::binary::encode(&reply)?
                }
                Err(error) => {
                    return Ok((
                        receive_error_response(ReceiveError {
                            publication_unknown: mutates
                                && (error.contains("published")
                                    || error.contains("outcome is unknown")
                                    || error.contains("recovery required")),
                            message: error,
                        }),
                        Vec::new(),
                    ))
                }
            }
        }
        Operation::ObjectSlice { id, offset } => {
            if !payload.is_empty() {
                return Err("object slice requires empty payload".into());
            }
            let raw = repository.reachable_object(&id)?;
            let offset = usize::try_from(offset).map_err(|_| "object offset exceeds platform")?;
            if offset >= raw.len() {
                return Err("object offset outside envelope".into());
            }
            let mut bytes = (raw.len() as u64).to_be_bytes().to_vec();
            bytes.extend_from_slice(&crate::binary::oid(&id)?);
            bytes.extend_from_slice(
                &raw[offset
                    ..raw
                        .len()
                        .min(offset.saturating_add(protocol::TRANSFER_CHUNK))],
            );
            bytes
        }
        Operation::Advertise {} => {
            if !payload.is_empty() {
                return Err("advertise requires an empty payload".into());
            }
            response.state = Some(repository.advertised_state()?);
            Vec::new()
        }
        Operation::RefsPage { after, expected } => {
            if !payload.is_empty() {
                return Err("ref page requires an empty payload".into());
            }
            let mut state = repository.advertised_state()?;
            let generation = format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(crate::binary::encode(&state)?)
            );
            if expected.as_ref().is_some_and(|e| e != &generation) {
                return Err("references changed during pagination; restart advertisement".into());
            }
            let mut refs: Vec<_> = state
                .branches
                .iter()
                .map(|(n, id)| (format!("refs/heads/{n}"), id.clone()))
                .chain(
                    state
                        .tags
                        .iter()
                        .map(|(n, id)| (format!("refs/tags/{n}"), id.clone())),
                )
                .collect();
            refs.sort();
            refs.retain(|(n, _)| after.as_ref().is_none_or(|a| n > a));
            let next = (refs.len() > 256).then(|| refs[255].0.clone());
            state.branches.clear();
            state.tags.clear();
            for (name, id) in refs.into_iter().take(256) {
                if let Some(name) = name.strip_prefix("refs/heads/") {
                    state.branches.insert(name.into(), id);
                } else {
                    state
                        .tags
                        .insert(name.strip_prefix("refs/tags/").unwrap().into(), id);
                }
            }
            crate::binary::encode(&protocol::RefPage {
                generation,
                state,
                next,
            })?
        }
        Operation::FetchSlice { offset, expected } => {
            let options = (!payload.is_empty())
                .then(|| crate::binary::decode::<crate::repository::FetchOptions>(payload))
                .transpose()?;
            let (state, bytes) =
                repository.download_slice_options(expected.as_deref(), offset, options.as_ref())?;
            response.state = state;
            bytes
        }
        Operation::UploadSlice { .. }
        | Operation::ApplyUpload { .. }
        | Operation::CancelUpload { .. } => {
            return Err("legacy unscoped uploads are disabled".into())
        }
        Operation::Fork { destination } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            if !payload.is_empty() {
                return Err("fork requires an empty payload".into());
            }
            repository_name(&destination)?;
            match receive::fork(settings, &request.repository, repository, &destination) {
                Ok(()) => response.tip = Some(format!("fork-created:{destination}")),
                Err(error) => return Ok((receive_error_response(error), Vec::new())),
            }
            Vec::new()
        }
        Operation::Fetch {} => {
            let options = (!payload.is_empty())
                .then(|| crate::binary::decode::<crate::repository::FetchOptions>(payload))
                .transpose()?;
            let (state, bytes) = repository.network_fetch(options.as_ref())?;
            if options.is_some() {
                bytes
            } else {
                response.state = Some(state);
                Repository::empty_network_bundle(&bytes)?
            }
        }
        Operation::Push {
            branch,
            expected,
            force,
        } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            if force && !settings.force {
                return Err("server does not allow forced updates".into());
            }
            let completed = repository.complete_network_bundle(payload)?;
            let tip = Repository::bundle_state(&completed)?
                .tip()
                .cloned()
                .ok_or("bundle has no commit at its HEAD branch")?;
            let updates = [protocol::RefUpdate {
                namespace: RefNamespace::Branch,
                name: branch,
                expected,
                new: Some(tip.clone()),
                force,
            }];
            match receive::publish(
                settings,
                &request.repository,
                repository,
                &completed,
                &updates,
                &[],
                scope,
            ) {
                Ok(()) => response.tip = Some(tip),
                Err(error) => return Ok((receive_error_response(error), Vec::new())),
            }
            // The successful receive already identifies the exact published tip.
            // A second, independently locked state read could fail after commit
            // or describe a later writer, falsely reporting this push as failed.
            Vec::new()
        }
        Operation::UpdateRef {
            namespace,
            name,
            expected,
            new,
            force,
        } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            if force && !settings.force {
                return Err("server does not allow forced updates".into());
            }
            let updates = [protocol::RefUpdate {
                namespace,
                name,
                expected,
                new: new.clone(),
                force,
            }];
            match receive::publish(
                settings,
                &request.repository,
                repository,
                payload,
                &updates,
                &[],
                scope,
            ) {
                Ok(()) => response.tip = new,
                Err(error) => return Ok((receive_error_response(error), Vec::new())),
            }
            Vec::new()
        }
        operation @ Operation::PublishPartial { dry_run } => {
            let (batch, options) = protocol::publication_payload(&operation, payload)?;
            match receive::publish_selective(
                settings,
                &request.repository,
                repository,
                &batch,
                &options,
                dry_run,
                scope,
            ) {
                Ok(rejections) => {
                    response.tip = Some(format!(
                        "{:x}",
                        <sha2::Sha256 as sha2::Digest>::digest(payload)
                    ));
                    crate::binary::encode(&protocol::PartialReceipt { rejections })?
                }
                Err(error) => return Ok((receive_error_response(error), Vec::new())),
            }
        }
        operation @ (Operation::PublishBatch {}
        | Operation::PublishOptions {}
        | Operation::CheckBatch {}) => {
            let (batch, options) = protocol::publication_payload(&operation, payload)?;
            let result = if matches!(operation, Operation::CheckBatch {}) {
                repository.check_receive_batch_scoped(
                    &batch.bundle,
                    &batch.updates,
                    scope,
                    |bundle| {
                        if let Some(path) = &settings.signed_authors {
                            repository.verify_receive_authors(bundle, &batch.updates, path)?;
                        }
                        Ok(())
                    },
                )
            } else {
                receive::publish(
                    settings,
                    &request.repository,
                    repository,
                    &batch.bundle,
                    &batch.updates,
                    &options,
                    scope,
                )
            };
            match result {
                Ok(()) => {
                    response.tip = Some(format!(
                        "{:x}",
                        <sha2::Sha256 as sha2::Digest>::digest(payload)
                    ))
                }
                Err(error) => return Ok((receive_error_response(error), Vec::new())),
            }
            Vec::new()
        }
    };
    Ok((response, bytes))
}

fn receive_error_response(error: ReceiveError) -> Response {
    Response {
        version: 1,
        ok: false,
        publication_unknown: error.publication_unknown,
        error: Some(error.message),
        state: None,
        tip: None,
    }
}

fn response_packet(response: &Response, payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    if let Err(error) = protocol::write_packet(&mut bytes, response, payload) {
        // Oversized advertisements/transfers are application errors too. Keep
        // the bounded error frame readable rather than silently close a socket.
        bytes.clear();
        let response = Response {
            version: 1,
            ok: false,
            publication_unknown: response.publication_unknown
                || (response.ok && response.tip.is_some()),
            error: Some(format!("cannot encode RPC response: {error}")),
            state: None,
            tip: None,
        };
        protocol::write_packet(&mut bytes, &response, &[])?;
    }
    Ok(bytes)
}

struct PreparedReply {
    request: Option<(Request, Vec<u8>)>,
    response: Response,
    bytes: Vec<u8>,
}

impl PreparedReply {
    fn new(
        settings: &Settings,
        request: Result<(Request, Vec<u8>), String>,
    ) -> Result<Self, String> {
        let (request, outcome) = match request {
            Ok((request, payload)) => {
                let outcome = process(settings, request.clone(), &payload);
                (Some((request, payload)), outcome)
            }
            Err(error) => (None, Err(error)),
        };
        let (response, payload) = outcome.unwrap_or_else(|error| {
            (
                Response {
                    version: 1,
                    ok: false,
                    publication_unknown: settings.require_operation_credentials
                        && settings
                            .operation_credential
                            .as_ref()
                            .is_some_and(|c| c.effect_key.is_some()),
                    error: Some(error),
                    state: None,
                    tip: None,
                },
                Vec::new(),
            )
        });
        let bytes = response_packet(&response, &payload)?;
        Ok(Self {
            request,
            response,
            bytes,
        })
    }

    /// This is the last check before the first protected output write. Once
    /// that write begins, the single already-authorized transfer may finish.
    fn for_delivery(self, settings: &Settings) -> Result<Vec<u8>, String> {
        if let Some((request, payload)) = &self.request {
            if let Err(cause) =
                authorize(settings, &request.repository, &request.operation, payload)
            {
                crate::cli_error(&format!("authorization before delivery: {cause}"));
                let mutates = match &request.operation {
                    Operation::Advertise {}
                    | Operation::Fetch {}
                    | Operation::FetchSlice { .. }
                    | Operation::RefsPage { .. }
                    | Operation::ObjectSlice { .. }
                    | Operation::QueryScopedUpload { .. }
                    | Operation::CheckBatch {}
                    | Operation::QueryChunk { .. }
                    | Operation::PublishPartial { dry_run: true } => false,
                    Operation::ReleaseRetention { command } => !matches!(
                        command,
                        protocol::RetentionCommand::Query { .. }
                            | protocol::RetentionCommand::Read { .. }
                    ),
                    Operation::PullRequest {} => {
                        crate::binary::decode::<crate::pull::PullCommand>(payload)
                            .is_ok_and(|p| p.mutates())
                    }
                    _ => true,
                };
                return response_packet(&Response { version: 1, ok: false,
                    publication_unknown: self.response.publication_unknown || (self.response.ok && mutates),
                    error: Some("authorization unavailable before delivery; query the original operation with current authorization".into()),
                    state: None, tip: None }, &[]);
            }
        }
        Ok(self.bytes)
    }
}

fn equal_secret(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for (index, byte) in right.iter().enumerate() {
        difference |= usize::from(*byte ^ left.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn read_headers<R: Read>(reader: &mut R) -> Result<BTreeMap<String, String>, String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() == MAX_HEADER {
            return Err("HTTP headers exceed 16 KiB".into());
        }
        let mut byte = [0];
        reader
            .read_exact(&mut byte)
            .map_err(|e| format!("read HTTP headers: {e}"))?;
        bytes.push(byte[0]);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "HTTP headers must be ASCII")?;
    if !text.is_ascii() {
        return Err("HTTP headers must be ASCII".into());
    }
    let mut lines = text[..text.len() - 4].split("\r\n");
    if !matches!(
        lines.next(),
        Some("POST /v1/wasm-git HTTP/1.1" | "POST /v1/wasm-git HTTP/1.0")
    ) {
        return Err("expected POST /v1/wasm-git HTTP/1.1".into());
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("invalid HTTP header")?;
        if name.is_empty()
            || !name.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
        {
            return Err("invalid HTTP header name".into());
        }
        if value.bytes().any(|b| b < 32 && b != b'\t' || b == 127) {
            return Err("invalid HTTP header value".into());
        }
        if headers
            .insert(
                name.to_ascii_lowercase(),
                value.trim_matches([' ', '\t']).to_owned(),
            )
            .is_some()
        {
            return Err("duplicate HTTP header".into());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Transfer-Encoding is not supported; use Content-Length".into());
    }
    if headers.get("host").is_none_or(|h| h.is_empty()) {
        return Err("HTTP Host is required".into());
    }
    Ok(headers)
}

/// One I/O budget covers the connection, excluding processing after the complete
/// request is buffered. Tiny reads never renew a slow peer's remaining budget.
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn exclude_processing(&mut self, elapsed: Duration) -> io::Result<()> {
        self.deadline = self.deadline.checked_add(elapsed).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP processing duration overflow",
            )
        })?;
        Ok(())
    }

    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "HTTP connection deadline exceeded")
            })
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.stream.flush()
    }
}

fn http_response(
    stream: &mut impl Write,
    status: &str,
    bytes: &[u8],
    binary: bool,
) -> Result<(), String> {
    let content_type = if binary {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n", bytes.len())
        .and_then(|()| stream.write_all(bytes))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("write HTTP response: {e}"))
}

fn connection(stream: TcpStream, settings: &Settings, deadline: Instant) -> Result<(), String> {
    let mut stream = DeadlineStream { stream, deadline };
    let result = http_request(&mut stream, settings);
    finish_http(&mut stream);
    result
}

// RFC 9112 section 9.6: closing with unread request bytes can reset TCP and
// discard the response at the client. Send the response and FIN first, then
// discard pending input without parsing it or extending the connection budget.
// A peer waiting for an Expect rejection can see the complete response and EOF
// without transmitting a body. Excess input is bounded in both bytes and time.
fn finish_http(stream: &mut DeadlineStream) -> usize {
    if stream.stream.shutdown(Shutdown::Write).is_err() {
        return 0;
    }
    let mut buffer = [0u8; 16 * 1024];
    let mut consumed = 0;
    let limit = protocol::MAX_PACKET_BYTES + MAX_HEADER;
    while consumed < limit {
        let length = buffer.len().min(limit - consumed);
        match stream.read(&mut buffer[..length]) {
            Ok(0) => break,
            Ok(length) => consumed += length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    consumed
}

fn http_authentication(
    stream: &mut impl Write,
    result: Result<Option<policy::Authentication>, String>,
) -> Result<Option<policy::Authentication>, String> {
    match result {
        Ok(Some(authenticated)) => Ok(Some(authenticated)),
        Ok(None) => {
            http_response(
                stream,
                "401 Unauthorized",
                b"authentication required",
                false,
            )?;
            Ok(None)
        }
        Err(cause) => {
            crate::cli_error(&format!("operation credential authentication: {cause}"));
            http_response(
                stream,
                "503 Service Unavailable",
                b"authorization unavailable",
                false,
            )?;
            Ok(None)
        }
    }
}

fn http_request(stream: &mut DeadlineStream, settings: &Settings) -> Result<(), String> {
    let headers = match read_headers(stream) {
        Ok(headers) => headers,
        Err(error) => return http_response(stream, "400 Bad Request", error.as_bytes(), false),
    };
    if let Some(token) = &settings.token {
        let supplied = headers
            .get("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        if !equal_secret(supplied.as_bytes(), token) {
            return http_response(
                stream,
                "401 Unauthorized",
                b"authentication required",
                false,
            );
        }
    }
    let mut authenticated = settings.clone();
    let initial = match policy::request_guard(&settings.root, settings.policy_enabled) {
        Ok(guard) => guard,
        Err(_) => {
            return http_response(
                stream,
                "503 Service Unavailable",
                b"authorization unavailable",
                false,
            )
        }
    };
    if initial.operation_credentials
        && (!settings.require_operation_credentials || !initial.chunk_receipts)
    {
        return http_response(
            stream,
            "503 Service Unavailable",
            b"this root requires the strict credential entry point",
            false,
        );
    }
    if let Some(policy) = &initial.policy {
        let token = headers
            .get("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        let Some((actor, credential)) = http_authentication(
            stream,
            policy::authenticate(
                &settings.root,
                policy,
                initial.generation,
                token.as_bytes(),
                settings.require_operation_credentials,
            ),
        )?
        else {
            return Ok(());
        };
        authenticated.actor = actor;
        authenticated.operation_credential = credential;
    }
    drop(initial);
    let Some(length_text) = headers.get("content-length") else {
        return http_response(
            stream,
            "411 Length Required",
            b"Content-Length is required",
            false,
        );
    };
    if length_text.is_empty() || !length_text.bytes().all(|b| b.is_ascii_digit()) {
        return http_response(stream, "400 Bad Request", b"invalid Content-Length", false);
    }
    let Some(length) = length_text
        .parse::<usize>()
        .ok()
        .filter(|n| *n <= protocol::MAX_PACKET_BYTES)
    else {
        return http_response(
            stream,
            "413 Content Too Large",
            b"Content-Length must not exceed 64 MiB",
            false,
        );
    };
    if let Some(expect) = headers.get("expect") {
        if !expect.eq_ignore_ascii_case("100-continue") {
            return http_response(
                stream,
                "417 Expectation Failed",
                b"unsupported Expect",
                false,
            );
        }
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|e| e.to_string())?;
    }
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|e| format!("allocate HTTP body: {e}"))?;
    body.resize(length, 0);
    if let Err(error) = stream.read_exact(&mut body) {
        return http_response(
            stream,
            "400 Bad Request",
            format!("incomplete HTTP body: {error}").as_bytes(),
            false,
        );
    }
    // Authentication before reading the body is only an early rejection. The
    // actual request uses a fresh snapshot and retains the gate through output.
    let guard = match policy::request_guard(&settings.root, settings.policy_enabled) {
        Ok(guard) => guard,
        Err(_) => {
            return http_response(
                stream,
                "503 Service Unavailable",
                b"authorization unavailable",
                false,
            )
        }
    };
    if guard.operation_credentials
        && (!settings.require_operation_credentials || !guard.chunk_receipts)
    {
        return http_response(
            stream,
            "503 Service Unavailable",
            b"this root requires the strict credential entry point",
            false,
        );
    }
    authenticated.policy = guard.policy.clone();
    if let Some(policy) = &authenticated.policy {
        let token = headers
            .get("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        let Some((actor, credential)) = http_authentication(
            stream,
            policy::authenticate(
                &settings.root,
                policy,
                guard.generation,
                token.as_bytes(),
                settings.require_operation_credentials,
            ),
        )?
        else {
            return Ok(());
        };
        authenticated.actor = actor;
        authenticated.operation_credential = credential;
    }
    let settings = &authenticated;
    let started = Instant::now();
    let reply = PreparedReply::new(settings, protocol::read_complete_packet::<Request>(&body));
    // Verification/fsync may exceed the network budget on a slow host. A
    // completed publication still needs its acknowledgement; only the time
    // spent processing this fully buffered request is excluded from I/O time.
    stream
        .exclude_processing(started.elapsed())
        .map_err(|e| e.to_string())?;
    let bytes = reply?.for_delivery(settings)?;
    let result = http_response(stream, "200 OK", &bytes, true);
    drop(guard);
    result
}

fn read_token_file(path: &Path) -> Result<Vec<u8>, String> {
    let path = real_path(path, false)?;
    // The optional single LF is framing, outside the 1..4096 token bytes.
    if fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .len()
        > 4097
    {
        return Err("token file exceeds 4097 bytes including its optional final newline".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(4098)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    if bytes.is_empty() || bytes.len() > 4096 || bytes.iter().any(|byte| !(33..=126).contains(byte))
    {
        return Err("token must contain 1..4096 printable ASCII bytes without whitespace".into());
    }
    Ok(bytes)
}

pub fn run(args: &[String]) -> Result<String, String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return Ok(USAGE.into());
    }
    if let [_, command, input, output] = args {
        if matches!(
            command.as_str(),
            "policy-set"
                | "policy-query"
                | "policy-migrate"
                | "operation-credential-issue"
                | "operation-credential-query"
                | "operation-recover"
                | "audit-query"
                | "operation-context"
                | "capacity-query"
        ) {
            let root = real_path(Path::new(input), true)?;
            let request_path = real_path(Path::new(output), false)?;
            if fs::metadata(&request_path)
                .map_err(|e| e.to_string())?
                .len()
                > 16 * 1024 * 1024
            {
                return Err("policy update request exceeds 16 MiB".into());
            }
            let bytes = fs::read(request_path).map_err(|e| e.to_string())?;
            protocol::validate_node_safe_json(&bytes)?;
            if command == "operation-recover" {
                let request: protocol::OperationRecoveryRequest =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                return serde_json::to_string(&recovery::run(&root, &request)?)
                    .map_err(|e| e.to_string());
            }
            if command == "audit-query" {
                admin_identity::authorize_recovery_root(&root)?;
                let request: protocol::AuditQueryRequest =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                let reply = policy::audit_query(&root, &request)?;
                reply.validate_for(&request)?;
                return serde_json::to_string(&reply).map_err(|e| e.to_string());
            }
            if command == "operation-context" {
                admin_identity::authorize_recovery_root(&root)?;
                let request: protocol::OperationCredentialQuery =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                request.validate()?;
                let reply = protocol::OperationContextReply {
                    version: 1,
                    operation_id: request.operation_id.clone(),
                    original: policy::operation_context(&root, &request.operation_id)?,
                };
                reply.validate_for(&request)?;
                return serde_json::to_string(&reply).map_err(|e| e.to_string());
            }
            if command == "capacity-query" {
                admin_identity::authorize_recovery_root(&root)?;
                let request: protocol::CapacityQueryRequest =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                request.validate()?;
                let _guard = policy::request_guard(&root, true)?;
                let repository = Repository::open_for_recovery(
                    repository_path_in(&root, &request.repository)?,
                    None,
                )?;
                let capacity = repository.capacity_status()?;
                let reply = protocol::CapacityQueryReply {
                    version: 1,
                    repository: request.repository.clone(),
                    capacity,
                };
                reply.validate_for(&request)?;
                return serde_json::to_string(&reply).map_err(|e| e.to_string());
            }
            if command == "operation-credential-issue" {
                let request: protocol::OperationCredentialRequest =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                return serde_json::to_string(&policy::issue_credential(&root, &request)?)
                    .map_err(|e| e.to_string());
            }
            if command == "operation-credential-query" {
                let request: protocol::OperationCredentialQuery =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                return serde_json::to_string(&policy::query_credential(&root, &request)?)
                    .map_err(|e| e.to_string());
            }
            if command == "policy-set" {
                let request: protocol::PolicyUpdateRequest =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                return serde_json::to_string(&policy::set(&root, &request)?)
                    .map_err(|e| e.to_string());
            }
            if command == "policy-migrate" {
                let request: protocol::PolicyMigrationRequest =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                return serde_json::to_string(&policy::migrate(&root, &request)?)
                    .map_err(|e| e.to_string());
            }
            let request: protocol::PolicyQueryRequest =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            return serde_json::to_string(&policy::query(&root, &request)?)
                .map_err(|e| e.to_string());
        }
        if command == "compile-policy" {
            return policy::compile(Path::new(input), Path::new(output));
        }
        if command == "compile-protection" {
            return pulls::compile(Path::new(input), Path::new(output));
        }
        if command == "audit-log" {
            return Err("audit-log accepts only one path".into());
        }
    }
    if let [_, command, path] = args {
        if command == "policy-status" {
            let root = real_path(Path::new(path), true)?;
            return serde_json::to_string(&policy::status(&root)?).map_err(|e| e.to_string());
        }
        if command == "audit-log" {
            return receive::read_audit(Path::new(path));
        }
    }
    let mut policy_file = None;
    let mut protection_file = None;
    let mut merge_key = None;
    let mut merge_author = None;
    let mut merge_format = crate::repository::SignatureFormat::Native;
    let mut principal = None;
    let mut read_only = false;
    let mut pre_receive = None;
    let mut update_hook = None;
    let mut signed_authors = None;
    let mut post_receive = None;
    let mut post_update = None;
    let mut root = None;
    let mut listen = "127.0.0.1:9419".to_owned();
    let mut listen_selected = false;
    let mut token_file = None;
    let mut stdio = false;
    let mut require_operation_credentials = false;
    let mut write = false;
    let mut force = false;
    let mut max_connections = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--stdio" => stdio = true,
            "--require-operation-credentials" => require_operation_credentials = true,
            "--read-only" => read_only = true,
            "--allow-write" => write = true,
            "--allow-force" => force = true,
            flag @ ("--root"
            | "--listen"
            | "--token-file"
            | "--max-connections"
            | "--policy"
            | "--principal"
            | "--pre-receive"
            | "--post-receive"
            | "--update"
            | "--post-update"
            | "--signed-authors"
            | "--protection"
            | "--merge-signing-key"
            | "--merge-author"
            | "--merge-signing-format") => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("missing {flag} value; {USAGE}"))?;
                match flag {
                    "--root" => root = Some(PathBuf::from(value)),
                    "--policy" => policy_file = Some(PathBuf::from(value)),
                    "--protection" => protection_file = Some(PathBuf::from(value)),
                    "--merge-signing-key" => merge_key = Some(PathBuf::from(value)),
                    "--merge-author" => merge_author = Some(value.clone()),
                    "--merge-signing-format" => {
                        merge_format = match value.as_str() {
                            "native" => crate::repository::SignatureFormat::Native,
                            "ssh" => crate::repository::SignatureFormat::Ssh,
                            "openpgp" => crate::repository::SignatureFormat::Openpgp,
                            _ => return Err("unknown merge signing format".into()),
                        }
                    }
                    "--principal" => principal = Some(value.clone()),
                    "--signed-authors" => {
                        signed_authors = Some(real_path(Path::new(value), false)?)
                    }
                    "--pre-receive" => pre_receive = Some(real_path(Path::new(value), false)?),
                    "--update" => update_hook = Some(real_path(Path::new(value), false)?),
                    "--post-receive" => post_receive = Some(real_path(Path::new(value), false)?),
                    "--post-update" => post_update = Some(real_path(Path::new(value), false)?),
                    "--listen" => {
                        listen = value.clone();
                        listen_selected = true;
                    }
                    "--token-file" => token_file = Some(PathBuf::from(value)),
                    _ => {
                        max_connections = Some(
                            value
                                .parse::<usize>()
                                .ok()
                                .filter(|n| *n > 0)
                                .ok_or("invalid maximum connections")?,
                        )
                    }
                }
            }
            _ => return Err(format!("unknown server argument; {USAGE}")),
        }
        index += 1;
    }
    if stdio && (listen_selected || max_connections.is_some()) {
        return Err("--listen and --max-connections are HTTP-only options".into());
    }
    let root = real_path(
        &root.ok_or_else(|| format!("--root is required; {USAGE}"))?,
        true,
    )?;
    let token = token_file.map(|path| read_token_file(&path)).transpose()?;
    if stdio && token.is_some() {
        return Err("stdio uses SSH authentication; token-file is for HTTP".into());
    }
    if read_only && (write || force) {
        return Err("--read-only conflicts with write/force flags".into());
    }
    let policy_enabled = policy_file.is_some();
    if require_operation_credentials && (!policy_enabled || stdio) {
        return Err("operation credentials require an HTTP policy entry point".into());
    }
    if policy_enabled && token.is_some() {
        return Err("choose --policy or --token-file".into());
    }
    if principal.is_some() && (!stdio || !policy_enabled) {
        return Err("--principal requires SSH stdio and --policy".into());
    }
    if stdio && policy_enabled && principal.is_none() {
        return Err("SSH policy requires a trusted forced-command --principal".into());
    }
    if let Some(path) = &policy_file {
        policy::bootstrap(&root, path)?;
    }
    {
        let guard = policy::request_guard(&root, policy_enabled)?;
        if guard.operation_credentials && (!require_operation_credentials || !guard.chunk_receipts)
        {
            return Err("this root requires the strict credential entry point".into());
        }
        if let Some(name) = &principal {
            if !guard
                .policy
                .as_ref()
                .is_some_and(|policy| policy.principal_exists(name))
            {
                return Err("unknown SSH principal".into());
            }
        }
    }
    if !stdio && write && token.is_none() && !policy_enabled {
        return Err("HTTP --allow-write requires --token-file or --policy".into());
    }
    let settings = Arc::new(Settings {
        protection: protection_file
            .map(|p| pulls::Protection::load(&p).map(Arc::new))
            .transpose()?,
        merge_signer: match (merge_author, merge_key) {
            (Some(author), Some(key)) => Some(pulls::MergeSigner {
                author,
                key: if merge_format == crate::repository::SignatureFormat::Openpgp {
                    key
                } else {
                    real_path(&key, false)?
                },
                format: merge_format,
            }),
            (None, None) => None,
            _ => {
                return Err(
                    "merge signing requires both --merge-author and --merge-signing-key".into(),
                )
            }
        },
        root,
        token,
        write: !read_only && (stdio || write),
        force,
        policy_enabled,
        policy: None,
        require_operation_credentials,
        operation_credential: None,
        actor: principal.unwrap_or_else(|| if stdio { "ssh".into() } else { "token".into() }),
        pre_receive,
        update_hook,
        signed_authors,
        post_receive,
        post_update,
    });
    if stdio {
        let mut input = std::io::stdin().lock();
        let (request, payload) = protocol::read_packet::<_, Request>(&mut input)?;
        let mut extra = [0];
        if input.read(&mut extra).map_err(|e| e.to_string())? != 0 {
            return Err("trailing bytes after stdio request".into());
        }
        let guard = policy::request_guard(&settings.root, settings.policy_enabled)?;
        if guard.operation_credentials {
            return Err("this root requires the strict credential entry point".into());
        }
        let mut authenticated = (*settings).clone();
        authenticated.policy = guard.policy.clone();
        if let Some(policy) = &authenticated.policy {
            if !policy.principal_exists(&authenticated.actor) {
                return Err("unknown SSH principal".into());
            }
        }
        let reply = PreparedReply::new(&authenticated, Ok((request, payload)))?;
        let mut output = std::io::stdout().lock();
        let bytes = reply.for_delivery(&authenticated)?;
        output
            .write_all(&bytes)
            .map_err(|e| format!("write stdio response: {e}"))?;
        output.flush().map_err(|e| e.to_string())?;
        drop(guard);
        return Ok(String::new());
    }
    let listener = TcpListener::bind(&listen).map_err(|e| format!("listen: {e}"))?;
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "{}",
            serde_json::json!({"listen":listener.local_addr().map_err(|e| e.to_string())?.to_string(),"read_only":!write,"protocol":1})
        )
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("write server readiness: {error}"))?;
    }
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for (accepted, incoming) in listener.incoming().enumerate() {
        let stream = incoming.map_err(|e| format!("accept: {e}"))?;
        let deadline = Instant::now() + TIMEOUT;
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                workers
                    .swap_remove(index)
                    .join()
                    .map_err(|_| "server worker panicked")?;
            } else {
                index += 1;
            }
        }
        if workers.len() >= 16 {
            // Never wait for the oldest peer in the accept loop. A saturated
            // service rejects promptly; the peer can retry with backoff.
            stream.set_nonblocking(true).map_err(|e| e.to_string())?;
            let _ = http_response(
                &mut &stream,
                "503 Service Unavailable",
                b"server busy; retry",
                false,
            );
            let _ = stream.shutdown(Shutdown::Both);
        } else {
            let settings = Arc::clone(&settings);
            workers.push(std::thread::spawn(move || {
                if let Err(error) = connection(stream, &settings, deadline) {
                    crate::cli_error(&format!("server connection: {error}"));
                }
            }));
        }
        if max_connections.is_some_and(|maximum| accepted >= maximum - 1) {
            break;
        }
    }
    for worker in workers {
        worker.join().map_err(|_| "server worker panicked")?;
    }
    Ok(String::new())
}

#[cfg(test)]
pub(crate) fn test_authorized_process(
    root: &Path,
    actor: &str,
    request: Request,
    payload: &[u8],
) -> Result<(Response, Vec<u8>), String> {
    let root = real_path(root, true)?;
    let guard = policy::request_guard(&root, true)?;
    let settings = Settings {
        protection: None,
        merge_signer: None,
        root,
        token: None,
        write: true,
        force: true,
        policy_enabled: true,
        policy: guard.policy.clone(),
        require_operation_credentials: false,
        operation_credential: None,
        actor: actor.into(),
        pre_receive: None,
        update_hook: None,
        signed_authors: None,
        post_receive: None,
        post_update: None,
    };
    let result = process(&settings, request, payload);
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_store_failure_is_unavailable_not_invalid_credentials() {
        let absent = std::env::temp_dir().join(format!(
            "wasm-git-absent-credential-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!absent.exists());
        let policy = protocol::ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: Vec::new(),
        };
        let failure = policy::authenticate(&absent, &policy, 0, b"token", true);
        assert!(failure.is_err());
        let mut bytes = Vec::new();
        assert!(http_authentication(&mut bytes, failure).unwrap().is_none());
        assert!(bytes.starts_with(b"HTTP/1.1 503 Service Unavailable"));
        assert!(!String::from_utf8_lossy(&bytes).contains("store is not initialized"));
        bytes.clear();
        assert!(http_authentication(&mut bytes, Ok(None)).unwrap().is_none());
        assert!(bytes.starts_with(b"HTTP/1.1 401 Unauthorized"));
    }

    #[test]
    fn first_delivery_removes_expired_data_without_reclassifying_committed_or_unknown_writes() {
        let policy = protocol::ServerPolicy {
            version: protocol::SERVER_POLICY_VERSION,
            principals: vec![protocol::PolicyPrincipal {
                name: "alice".into(),
                token_sha256: None,
                grants: vec![protocol::PolicyGrant {
                    repository: "project".into(),
                    read: true,
                    write: vec!["refs/heads/*".into()],
                    force: false,
                    create: false,
                    release_retention: true,
                    expires_at: None,
                }],
            }],
        };
        let mut settings = Settings {
            protection: None,
            merge_signer: None,
            root: PathBuf::new(),
            token: None,
            write: true,
            force: false,
            policy_enabled: true,
            policy: Some(Arc::new(policy)),
            require_operation_credentials: true,
            operation_credential: None,
            actor: "alice".into(),
            pre_receive: None,
            update_hook: None,
            signed_authors: None,
            post_receive: None,
            post_update: None,
        };
        for (operation, mutation) in [
            (Operation::Fetch {}, false),
            (
                Operation::ReleaseRetention {
                    command: protocol::RetentionCommand::Close {
                        operation: "a".repeat(64),
                    },
                },
                true,
            ),
        ] {
            let request = Request {
                version: 1,
                repository: "project".into(),
                operation,
            };
            let issuance = protocol::OperationCredentialRequest {
                version: 1,
                operation_id: "b".repeat(64),
                policy_generation: 1,
                actor: "alice".into(),
                token_sha256: "c".repeat(64),
                request: request.clone(),
                payload_sha256: format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest([])),
                expires_at: protocol::NODE_SAFE_INTEGER_MAX,
            };
            settings.operation_credential = Some(issuance.receipt().unwrap());
            let ready = |ok, unknown| {
                let response = Response {
                    version: 1,
                    ok,
                    publication_unknown: unknown,
                    error: None,
                    state: None,
                    tip: Some("private-tip".into()),
                };
                PreparedReply {
                    request: Some((request.clone(), Vec::new())),
                    bytes: response_packet(&response, b"private-bytes").unwrap(),
                    response,
                }
            };
            let allowed = ready(true, false).for_delivery(&settings).unwrap();
            assert_eq!(
                protocol::read_complete_packet::<Response>(&allowed)
                    .unwrap()
                    .1,
                b"private-bytes"
            );
            // The result was prepared while valid, but no output has started.
            settings.operation_credential.as_mut().unwrap().expires_at = 1;
            for (ok, unknown) in [(true, false), (false, true), (false, false)] {
                let (response, payload) = protocol::read_complete_packet::<Response>(
                    &ready(ok, unknown).for_delivery(&settings).unwrap(),
                )
                .unwrap();
                assert!(!response.ok);
                assert!(payload.is_empty());
                assert!(response.tip.is_none());
                assert_eq!(response.publication_unknown, unknown || (ok && mutation));
            }
        }
    }

    #[test]
    fn http_framing_rejects_ambiguity_and_oversized_headers() {
        for request in [
            "POST /v1/wasm-git HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "POST /v1/wasm-git HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST /v1/wasm-git HTTP/1.1\r\nHost : x\r\n\r\n",
            "POST /v1/wasm-git HTTP/1.1\r\n\r\n",
            "GET /v1/wasm-git HTTP/1.1\r\nHost: x\r\n\r\n",
        ] {
            assert!(read_headers(&mut request.as_bytes()).is_err());
        }
        assert!(read_headers(&mut vec![b'x'; MAX_HEADER + 1].as_slice()).is_err());
        assert!(read_headers(
            &mut b"POST /v1/wasm-git HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n".as_slice()
        )
        .is_ok());
    }

    #[test]
    fn typed_receive_uncertainty_survives_rpc_encoding() {
        for publication_unknown in [false, true] {
            let response = receive_error_response(ReceiveError {
                message: "a typed receive outcome".into(),
                publication_unknown,
            });
            let bytes = response_packet(&response, &[]).unwrap();
            let (decoded, payload): (Response, _) = protocol::read_complete_packet(&bytes).unwrap();
            assert!(!decoded.ok);
            assert_eq!(decoded.publication_unknown, publication_unknown);
            assert_eq!(decoded.error.as_deref(), Some("a typed receive outcome"));
            assert!(payload.is_empty());
        }
    }

    #[test]
    fn oversized_response_is_a_readable_error_frame() {
        let response = Response {
            version: 1,
            ok: true,
            publication_unknown: false,
            error: None,
            state: None,
            tip: Some("x".repeat(65_536)),
        };
        let bytes = response_packet(&response, &[]).unwrap();
        let (response, payload): (Response, _) = protocol::read_complete_packet(&bytes).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("metadata length"));
        assert!(response.tip.is_none());
        assert!(payload.is_empty());
    }

    #[test]
    fn connection_deadline_does_not_reset_when_reads_make_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.write_all(b"xy").unwrap();
        let mut reader = DeadlineStream {
            stream: server,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
        // Moving the fixed deadline into the past avoids a sleeping test and
        // proves buffered progress cannot renew the connection's total budget.
        reader.deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            reader.write(b"late").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn slow_processing_preserves_the_remaining_io_budget_for_publication_receipts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut writer = DeadlineStream {
            stream: server,
            deadline: Instant::now().checked_sub(Duration::from_secs(30)).unwrap(),
        };
        assert_eq!(
            writer.write(b"receipt").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        // Model 35 seconds of server processing without a sleeping test. Five
        // seconds of I/O remain; processing does not grant a fresh 15 seconds.
        writer.exclude_processing(Duration::from_secs(35)).unwrap();
        assert!(writer.remaining().unwrap() <= Duration::from_secs(5));
        writer.write_all(b"receipt").unwrap();
        let mut bytes = [0; 7];
        client.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"receipt");
    }

    #[test]
    fn rejected_expect_sends_response_and_fin_before_waiting_for_a_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let settings = Settings {
            protection: None,
            merge_signer: None,
            root: std::env::temp_dir(),
            token: Some(b"required-token".to_vec()),
            write: false,
            force: false,
            policy_enabled: false,
            policy: None,
            require_operation_credentials: false,
            operation_credential: None,
            actor: "test".into(),
            pre_receive: None,
            update_hook: None,
            signed_authors: None,
            post_receive: None,
            post_update: None,
        };
        let worker = std::thread::spawn(move || {
            connection(server, &settings, Instant::now() + Duration::from_secs(10))
        });
        client.write_all(b"POST /v1/wasm-git HTTP/1.1\r\nHost: x\r\nContent-Length: 4096\r\nExpect: 100-continue\r\n\r\n").unwrap();
        // No body and no write shutdown: receiving the final response and EOF
        // must not depend on sending bytes that the rejected client will omit.
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
        assert!(response.ends_with(b"\r\n\r\nauthentication required"));
        assert!(!response.windows(12).any(|part| part == b"100 Continue"));
        client.shutdown(Shutdown::Write).unwrap();
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn graceful_close_cannot_drain_queued_input_after_the_original_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.write_all(b"queued input").unwrap();
        let mut stream = DeadlineStream {
            stream: server,
            deadline: Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        };
        assert_eq!(finish_http(&mut stream), 0);
        // The raw descriptor still has the bytes: graceful teardown did not
        // reset the fixed deadline even when input was immediately readable.
        stream
            .stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut pending = [0u8; 12];
        stream.stream.read_exact(&mut pending).unwrap();
        assert_eq!(&pending, b"queued input");
    }

    #[test]
    fn repository_names_and_tokens_are_exact() {
        for name in [
            "", "..", ".hidden", "../x", "x//y", "x/../y", "/x", "x/", "%2e", "a\\b",
        ] {
            assert!(repository_name(name).is_err());
        }
        assert!(repository_name("wasm-project.1").is_ok());
        assert!(repository_name("team/wasm-project.1").is_ok());
        assert!(repository_name(&["a"; 17].join("/")).is_err());
        let component = "x".repeat(128);
        assert!(repository_name(&[component.as_str(); 4].join("/")).is_err());
        assert!(equal_secret(b"secret", b"secret"));
        assert!(!equal_secret(b"secre", b"secret"));
        assert!(!equal_secret(b"secretX", b"secret"));
    }

    #[test]
    fn token_length_excludes_one_optional_final_line_feed() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "wasm-git-token-boundary-{}-{}",
            crate::platform_fs::process_id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let fixture = Fixture(path);
        for length in [1, 4095, 4096] {
            for newline in [false, true] {
                let expected = vec![b'T'; length];
                let mut file = expected.clone();
                if newline {
                    file.push(b'\n');
                }
                fs::write(&fixture.0, file).unwrap();
                assert_eq!(read_token_file(&fixture.0).unwrap(), expected);
            }
        }
        for bytes in [
            Vec::new(),
            b"\n".to_vec(),
            b"token\n\n".to_vec(),
            b"token\r\n".to_vec(),
            b"token\0".to_vec(),
            vec![0xff],
            vec![b'T'; 4097],
            [vec![b'T'; 4097], b"\n".to_vec()].concat(),
            [vec![b'T'; 4096], b"\n\n".to_vec()].concat(),
        ] {
            fs::write(&fixture.0, bytes).unwrap();
            assert!(read_token_file(&fixture.0).is_err());
        }
    }
}
