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

const USAGE: &str = "server --root DIR [--listen 127.0.0.1:9419] [--token-file FILE | --policy FILE] [--allow-write | --read-only] [--allow-force] [--signed-authors BINARY_TRUST_FILE] [--pre-receive PROGRAM] [--update PROGRAM] [--post-receive PROGRAM] [--post-update PROGRAM] [--max-connections N]; server --stdio --root DIR [--policy FILE --principal NAME] [--read-only] [--allow-force]; server compile-policy INPUT_JSON NEW_BINARY_FILE; server audit-log BINARY_LOG";
const MAX_HEADER: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);

#[path = "server_policy.rs"]
mod policy;
#[path = "server_receive.rs"]
mod receive;

#[derive(Clone)]
struct Settings {
    root: PathBuf,
    token: Option<Vec<u8>>,
    write: bool,
    force: bool,
    policy: Option<Arc<policy::Policy>>,
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
    repository_name(name)?;
    let mut parent = settings.root.clone();
    let components: Vec<_> = name.split('/').collect();
    for component in &components[..components.len() - 1] {
        namespace_directory(&parent)?;
        let next = parent.join(component);
        if create {
            match fs::create_dir(&next) {
                Ok(()) => fs::File::open(&parent)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| e.to_string())?,
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
    match fs::symlink_metadata(path.join(".uwgit")) {
        Ok(_) => Err("server namespaces must not be nested inside a repository".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn repository_path(settings: &Settings, name: &str) -> Result<PathBuf, String> {
    let parent = repository_parent(settings, name, false)?;
    real_path(&parent.join(name.rsplit('/').next().unwrap()), true)
}

fn authorize(
    settings: &Settings,
    repository: &str,
    operation: &Operation,
    payload: &[u8],
) -> Result<(), String> {
    let check = |reference: Option<&str>, force: bool| -> Result<(), String> {
        if reference.is_some() && !settings.write {
            return Err("server is read-only".into());
        }
        if force && !settings.force {
            return Err("server does not allow forced updates".into());
        }
        if let Some(policy) = &settings.policy {
            policy.authorize(&settings.actor, repository, reference, force, false)?;
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
        Operation::ApplyUpload { operation, .. } => {
            check(Some("*"), false)?;
            // Batch operands arrive in the verified upload. Reauthorize that
            // exact payload before applying it in process_locked.
            if matches!(
                **operation,
                Operation::PublishBatch {}
                    | Operation::PublishOptions {}
                    | Operation::CheckBatch {}
                    | Operation::PublishPartial { .. }
            ) {
                Ok(())
            } else {
                authorize(settings, repository, operation, &[])
            }
        }
        Operation::UploadSlice { .. } | Operation::CancelUpload { .. } => check(Some("*"), false),
        Operation::Fork { destination } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            repository_name(destination)?;
            if let Some(policy) = &settings.policy {
                policy.authorize(&settings.actor, destination, None, false, true)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn process(
    settings: &Settings,
    request: Request,
    payload: &[u8],
) -> Result<(Response, Vec<u8>), String> {
    repository_name(&request.repository)?;
    authorize(settings, &request.repository, &request.operation, payload)?;
    let root = repository_path(settings, &request.repository)?;
    let repository = Repository::open(root)?;
    // OS-owned locks survive neither a crash nor an SSH process exit. Queue
    // competing service operations instead of leaking local fail-fast locks to
    // network readers. Local writer locks and ref leases remain authoritative.
    let _gate = repository.service_lock(TIMEOUT)?;
    process_locked(settings, request, payload)
}

fn process_locked(
    settings: &Settings,
    request: Request,
    payload: &[u8],
) -> Result<(Response, Vec<u8>), String> {
    if request.version != 1 {
        return Err("unsupported RPC version".into());
    }
    repository_name(&request.repository)?;
    let root = repository_path(settings, &request.repository)?;
    let repository = Repository::open(root)?;
    let mut response = Response {
        version: 1,
        ok: true,
        publication_unknown: false,
        error: None,
        state: None,
        tip: None,
    };
    let bytes = match request.operation {
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
        Operation::UploadSlice {
            transfer,
            total,
            offset,
        } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            repository.upload_slice(&transfer, total, offset, payload)?;
            Vec::new()
        }
        Operation::CancelUpload { transfer } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            if !payload.is_empty() {
                return Err("cancel upload requires an empty payload".into());
            }
            repository.remove_upload(&transfer)?;
            Vec::new()
        }
        Operation::ApplyUpload {
            transfer,
            operation,
        } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            if !payload.is_empty()
                || !matches!(
                    &*operation,
                    Operation::Push { .. }
                        | Operation::UpdateRef { .. }
                        | Operation::PublishBatch {}
                        | Operation::PublishOptions {}
                        | Operation::CheckBatch {}
                        | Operation::PublishPartial { .. }
                )
            {
                return Err("uploaded payload can only apply one reference mutation".into());
            }
            let bytes = match repository.read_upload(&transfer) {
                Ok(bytes) => bytes,
                Err(error) => {
                    // Incomplete uploads remain resumable. A complete upload
                    // with a bad digest can never succeed and must be removed.
                    if error.contains("checksum mismatch") {
                        repository.remove_upload(&transfer)?;
                    }
                    return Err(error);
                }
            };
            let result =
                authorize(settings, &request.repository, &operation, &bytes).and_then(|()| {
                    process_locked(
                        settings,
                        Request {
                            version: request.version,
                            repository: request.repository,
                            operation: *operation,
                        },
                        &bytes,
                    )
                });
            if let Err(error) = repository.remove_upload(&transfer) {
                eprintln!("upload cleanup: {error}");
            }
            return result;
        }
        Operation::Fork { destination } => {
            if !settings.write {
                return Err("server is read-only".into());
            }
            if !payload.is_empty() {
                return Err("fork requires an empty payload".into());
            }
            repository_name(&destination)?;
            match receive::fork(settings, &request.repository, &repository, &destination) {
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
                &repository,
                &completed,
                &updates,
                &[],
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
                &repository,
                payload,
                &updates,
                &[],
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
                &repository,
                &batch,
                &options,
                dry_run,
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
                repository.check_receive_batch(&batch.bundle, &batch.updates, |bundle| {
                    if let Some(path) = &settings.signed_authors {
                        repository.verify_receive_authors(bundle, &batch.updates, path)?;
                    }
                    Ok(())
                })
            } else {
                receive::publish(
                    settings,
                    &request.repository,
                    &repository,
                    &batch.bundle,
                    &batch.updates,
                    &options,
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

fn rpc(settings: &Settings, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let outcome = protocol::read_complete_packet::<Request>(bytes)
        .and_then(|(request, payload)| process(settings, request, &payload));
    let (response, payload) = outcome.unwrap_or_else(|error| {
        (
            Response {
                version: 1,
                ok: false,
                publication_unknown: false,
                error: Some(error),
                state: None,
                tip: None,
            },
            Vec::new(),
        )
    });
    response_packet(&response, &payload)
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
        Some("POST /v1/uwgit HTTP/1.1" | "POST /v1/uwgit HTTP/1.0")
    ) {
        return Err("expected POST /v1/uwgit HTTP/1.1".into());
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

/// One wall-clock I/O budget covers the entire connection. Per-read socket
/// timeouts alone allow a client making tiny progress to retain a worker forever.
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
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
    if let Some(policy) = &settings.policy {
        let token = headers
            .get("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        let Some(actor) = policy.authenticate(token.as_bytes()) else {
            return http_response(
                stream,
                "401 Unauthorized",
                b"authentication required",
                false,
            );
        };
        authenticated.actor = actor;
    }
    let settings = &authenticated;
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
    let bytes = rpc(settings, &body)?;
    http_response(stream, "200 OK", &bytes, true)
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
        if command == "compile-policy" {
            return policy::compile(Path::new(input), Path::new(output));
        }
        if command == "audit-log" {
            return Err("audit-log accepts only one path".into());
        }
    }
    if let [_, command, path] = args {
        if command == "audit-log" {
            return receive::read_audit(Path::new(path));
        }
    }
    let mut policy_file = None;
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
    let mut write = false;
    let mut force = false;
    let mut max_connections = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--stdio" => stdio = true,
            "--read-only" => read_only = true,
            "--allow-write" => write = true,
            "--allow-force" => force = true,
            flag @ ("--root" | "--listen" | "--token-file" | "--max-connections" | "--policy"
            | "--principal" | "--pre-receive" | "--post-receive" | "--update"
            | "--post-update" | "--signed-authors") => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("missing {flag} value; {USAGE}"))?;
                match flag {
                    "--root" => root = Some(PathBuf::from(value)),
                    "--policy" => policy_file = Some(PathBuf::from(value)),
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
    let policy = policy_file
        .map(|p| policy::Policy::load(&p).map(Arc::new))
        .transpose()?;
    if policy.is_some() && token.is_some() {
        return Err("choose --policy or --token-file".into());
    }
    if principal.is_some() && (!stdio || policy.is_none()) {
        return Err("--principal requires SSH stdio and --policy".into());
    }
    if stdio && policy.is_some() && principal.is_none() {
        return Err("SSH policy requires a trusted forced-command --principal".into());
    }
    if let Some(name) = &principal {
        if !policy.as_ref().unwrap().principal_exists(name) {
            return Err("unknown SSH principal".into());
        }
    }
    if !stdio && write && token.is_none() && policy.is_none() {
        return Err("HTTP --allow-write requires --token-file or --policy".into());
    }
    let settings = Arc::new(Settings {
        root,
        token,
        write: !read_only && (stdio || write),
        force,
        policy,
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
        let result = process(&settings, request, &payload);
        let (response, payload) = result.unwrap_or_else(|error| {
            (
                Response {
                    version: 1,
                    ok: false,
                    publication_unknown: false,
                    error: Some(error),
                    state: None,
                    tip: None,
                },
                Vec::new(),
            )
        });
        let mut output = std::io::stdout().lock();
        let bytes = response_packet(&response, &payload)?;
        output
            .write_all(&bytes)
            .map_err(|e| format!("write stdio response: {e}"))?;
        output.flush().map_err(|e| e.to_string())?;
        return Ok(String::new());
    }
    let listener = TcpListener::bind(&listen).map_err(|e| format!("listen: {e}"))?;
    println!(
        "{}",
        serde_json::json!({"listen":listener.local_addr().map_err(|e| e.to_string())?.to_string(),"read_only":!write,"protocol":1})
    );
    std::io::stdout().flush().map_err(|e| e.to_string())?;
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
                    eprintln!("server connection: {error}");
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
mod tests {
    use super::*;

    #[test]
    fn http_framing_rejects_ambiguity_and_oversized_headers() {
        for request in [
            "POST /v1/uwgit HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "POST /v1/uwgit HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST /v1/uwgit HTTP/1.1\r\nHost : x\r\n\r\n",
            "POST /v1/uwgit HTTP/1.1\r\n\r\n",
            "GET /v1/uwgit HTTP/1.1\r\nHost: x\r\n\r\n",
        ] {
            assert!(read_headers(&mut request.as_bytes()).is_err());
        }
        assert!(read_headers(&mut vec![b'x'; MAX_HEADER + 1].as_slice()).is_err());
        assert!(read_headers(
            &mut b"POST /v1/uwgit HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n".as_slice()
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
    fn rejected_expect_sends_response_and_fin_before_waiting_for_a_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let settings = Settings {
            root: std::env::temp_dir(),
            token: Some(b"required-token".to_vec()),
            write: false,
            force: false,
            policy: None,
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
        client.write_all(b"POST /v1/uwgit HTTP/1.1\r\nHost: x\r\nContent-Length: 4096\r\nExpect: 100-continue\r\n\r\n").unwrap();
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
            "uwgit-token-boundary-{}-{}",
            std::process::id(),
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
