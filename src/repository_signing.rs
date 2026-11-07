//! Domain-separated Ed25519 author attestations over canonical native commits.
use super::*;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
#[path = "repository_external_signatures.rs"]
mod external;
pub use external::SignatureFormat;
const DOMAIN: &[u8] = b"wasm-git.author.commit.v1\0";
const KEY_MAGIC: &[u8] = b"WGTKEY01";
const PUBLIC_MAGIC: &[u8] = b"WGTPUB01";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorSignature {
    pub version: u32,
    pub author: String,
    pub timestamp: u64,
    pub public_key: [u8; 32],
    pub signature: Vec<u8>,
}
pub struct SigningIdentity {
    pub author: String,
    pub key: PathBuf,
    pub timestamp: u64,
}
#[derive(Serialize, Deserialize)]
struct TrustedAuthors {
    version: u32,
    authors: BTreeMap<String, BTreeSet<[u8; 32]>>,
    external_keys: BTreeMap<[u8; 32], Vec<u8>>,
}
fn author(name: &str) -> RepositoryResult<()> {
    if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
        Err("invalid author identity".into())
    } else {
        Ok(())
    }
}
fn payload(commit: &Commit, signature: &AuthorSignature) -> RepositoryResult<Vec<u8>> {
    author(&signature.author)?;
    if !matches!(signature.version, 1..=3) {
        return Err("unsupported author signature version".into());
    }
    let mut unsigned = commit.clone();
    unsigned.signature = None;
    unsigned.shallow = false;
    let mut w = crate::binary::Writer(if signature.version == 1 {
        DOMAIN.to_vec()
    } else {
        b"wasm-git.author.commit.v2\0".to_vec()
    });
    if signature.version != 1 {
        w.u32(signature.version);
    }
    w.string(&signature.author)?;
    w.u64(signature.timestamp);
    w.0.extend_from_slice(&signature.public_key);
    w.bytes(&encode_commit(&unsigned)?)?;
    Ok(w.0)
}
pub(super) fn validate(commit: &Commit) -> RepositoryResult<()> {
    if let Some(signature) = &commit.signature {
        author(&signature.author)?;
        if matches!(signature.version, 2 | 3) {
            // Reading immutable history must not execute programs or become
            // impossible when an external key later expires. Explicit verify
            // and trusted receive perform cryptographic/policy verification.
            external::decode(signature)?;
            return Ok(());
        }
        let key = VerifyingKey::from_bytes(&signature.public_key)
            .map_err(|e| format!("invalid author key: {e}"))?;
        let sig = Signature::from_slice(&signature.signature)
            .map_err(|e| format!("invalid author signature: {e}"))?;
        key.verify_strict(&payload(commit, signature)?, &sig)
            .map_err(|e| format!("author signature verification failed: {e}"))?;
    }
    Ok(())
}
fn verify_crypto(commit: &Commit, public: Option<&[u8]>) -> RepositoryResult<()> {
    validate(commit)?;
    if let Some(signature) = commit.signature.as_ref().filter(|s| s.version != 1) {
        external::verify(signature, &payload(commit, signature)?, public)?;
    }
    Ok(())
}
pub(super) fn sign(
    commit: &mut Commit,
    identity: &SigningIdentity,
    format: SignatureFormat,
) -> RepositoryResult<()> {
    author(&identity.author)?;
    if format != SignatureFormat::Native {
        let public = external::public_for_signing(format, &identity.key)?;
        let mut signature = AuthorSignature {
            version: format.version(),
            author: identity.author.clone(),
            timestamp: identity.timestamp,
            public_key: external::key_id(format.version(), &public)?,
            signature: Vec::new(),
        };
        let signed = external::sign(format, &identity.key, public, &payload(commit, &signature)?)?;
        signature.signature = crate::binary::encode(&signed)?;
        commit.signature = Some(signature);
        return verify_crypto(commit, None);
    }
    let bytes = zeroize::Zeroizing::new(read_regular(&identity.key)?);
    let seed: &[u8; 32] = bytes
        .strip_prefix(KEY_MAGIC)
        .ok_or("invalid signing key format")?
        .try_into()
        .map_err(|_| "invalid signing key length")?;
    let key = SigningKey::from_bytes(seed);
    let mut signature = AuthorSignature {
        version: 1,
        author: identity.author.clone(),
        timestamp: identity.timestamp,
        public_key: key.verifying_key().to_bytes(),
        signature: Vec::new(),
    };
    signature.signature = key.sign(&payload(commit, &signature)?).to_bytes().to_vec();
    commit.signature = Some(signature);
    validate(commit)
}
pub fn generate_key(path: &Path, public: &Path) -> RepositoryResult<()> {
    if path == public || fs::symlink_metadata(path).is_ok() || fs::symlink_metadata(public).is_ok()
    {
        return Err("key output files must be distinct and new".into());
    }
    let mut seed = zeroize::Zeroizing::new([0; 32]);
    getrandom::fill(&mut *seed).map_err(|e| e.to_string())?;
    let key = SigningKey::from_bytes(&seed);
    let mut raw = zeroize::Zeroizing::new(KEY_MAGIC.to_vec());
    raw.extend_from_slice(&*seed);
    seed.fill(0);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    file.write_all(&raw)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    raw.fill(0);
    let mut public_bytes = PUBLIC_MAGIC.to_vec();
    public_bytes.extend_from_slice(key.verifying_key().as_bytes());
    write_new_file(public, &public_bytes)
}
pub fn trust_key(path: &Path, name: &str, public: &Path) -> RepositoryResult<()> {
    trust_key_format(path, name, public, SignatureFormat::Native)
}
fn trust_key_format(
    path: &Path,
    name: &str,
    public: &Path,
    format: SignatureFormat,
) -> RepositoryResult<()> {
    author(name)?;
    if format != SignatureFormat::Native {
        let bytes = external::import_public(format, public)?;
        let id = external::key_id(format.version(), &bytes)?;
        return edit_trust(path, |trust| {
            trust.authors.entry(name.into()).or_default().insert(id);
            trust.external_keys.insert(id, bytes);
            Ok(())
        });
    }
    let bytes = read_regular(public)?;
    let key: [u8; 32] = bytes
        .strip_prefix(PUBLIC_MAGIC)
        .ok_or("invalid public key format")?
        .try_into()
        .map_err(|_| "invalid public key length")?;
    let parsed = VerifyingKey::from_bytes(&key).map_err(|e| e.to_string())?;
    if parsed.is_weak() {
        return Err("weak public key".into());
    }
    edit_trust(path, |trust| {
        trust.authors.entry(name.into()).or_default().insert(key);
        Ok(())
    })
}
fn edit_trust(
    path: &Path,
    edit: impl FnOnce(&mut TrustedAuthors) -> RepositoryResult<()>,
) -> RepositoryResult<()> {
    // A separate lock prevents concurrent administrator edits from losing keys.
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    let guard = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock)
        .map_err(|e| e.to_string())?;
    let guard = WriteLock {
        path: lock.clone(),
        _file: guard,
    };
    let mut trusted = match fs::symlink_metadata(path) {
        Ok(_) => crate::binary::decode::<TrustedAuthors>(&read_regular(path)?)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TrustedAuthors {
            version: 2,
            authors: BTreeMap::new(),
            external_keys: BTreeMap::new(),
        },
        Err(e) => return Err(e.to_string()),
    };
    if trusted.version != 2 {
        return Err("unsupported trust store".into());
    }
    edit(&mut trusted)?;
    let active: BTreeSet<_> = trusted
        .authors
        .values()
        .flat_map(|keys| keys.iter().copied())
        .collect();
    trusted.external_keys.retain(|key, _| active.contains(key));
    let temp = path.with_extension(unique_name("trust"));
    let result = (|| {
        write_new_file(&temp, &crate::binary::encode(&trusted)?)?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        sync_directory(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
    })();
    drop(guard);
    let _ = fs::remove_file(temp);
    result
}
fn trusted(commit: &Commit, trust: &TrustedAuthors) -> RepositoryResult<()> {
    validate(commit)?;
    let signature = commit
        .signature
        .as_ref()
        .ok_or("commit has no author signature")?;
    if trust.version != 2
        || !trust
            .authors
            .get(&signature.author)
            .is_some_and(|keys| keys.contains(&signature.public_key))
    {
        return Err("author/key is not trusted".into());
    }
    if signature.version != 1 {
        let public = trust
            .external_keys
            .get(&signature.public_key)
            .ok_or("trusted external public key is missing")?;
        verify_crypto(commit, Some(public))?;
    }
    Ok(())
}
impl Repository {
    pub fn commit_signed(
        &self,
        bytes: &[u8],
        message: &str,
        identity: &SigningIdentity,
    ) -> RepositoryResult<String> {
        self.commit_signed_format(bytes, message, identity, SignatureFormat::Native)
    }
    pub fn commit_signed_format(
        &self,
        bytes: &[u8],
        message: &str,
        identity: &SigningIdentity,
        format: SignatureFormat,
    ) -> RepositoryResult<String> {
        validate_wasm_header(bytes)?;
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        if let Some(Object::Commit(c)) = snapshot
            .manifest
            .tip()
            .and_then(|id| snapshot.objects.get(id))
        {
            require_legacy_commit(c)?;
        }
        let wasm_bytes = encode_object(WASM_KIND, bytes)?;
        let wasm = object_id(&wasm_bytes);
        let mut commit = Commit {
            shallow: false,
            signature: None,
            format: 1,
            tree: None,
            legacy_path: None,
            wasm: wasm.clone(),
            parents: snapshot.manifest.tip().cloned().into_iter().collect(),
            message: message.into(),
        };
        sign(&mut commit, identity, format)?;
        let raw = encode_object(COMMIT_KIND, &encode_commit(&commit)?)?;
        let id = object_id(&raw);
        self.install_encoded_object(&wasm, &wasm_bytes)?;
        self.install_encoded_object(&id, &raw)?;
        self.write_ref(&snapshot.manifest.head, &id)?;
        Ok(id)
    }
    /// Sign the selected current tip as a new immutable object with the same
    /// content and parents. Old unsigned objects remain available for audit.
    pub fn sign_head(
        &self,
        expected: &str,
        identity: &SigningIdentity,
    ) -> RepositoryResult<String> {
        self.sign_head_format(expected, identity, SignatureFormat::Native)
    }
    pub fn sign_head_format(
        &self,
        expected: &str,
        identity: &SigningIdentity,
        format: SignatureFormat,
    ) -> RepositoryResult<String> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        if snapshot.manifest.tip().map(String::as_str) != Some(expected) {
            return Err("HEAD changed before signing".into());
        }
        let Some(Object::Commit(old)) = snapshot.objects.get(expected) else {
            return Err("HEAD is not a commit".into());
        };
        if old.shallow {
            return Err("deepen this boundary before replacing its commit".into());
        }
        let mut commit = old.clone();
        sign(&mut commit, identity, format)?;
        let id = self.store_object(COMMIT_KIND, &encode_commit(&commit)?)?;
        self.write_ref(&snapshot.manifest.head, &id)?;
        Ok(id)
    }
    pub fn verify_author(&self, revision: &str, trust: Option<&Path>) -> RepositoryResult<Value> {
        let info = self.read_commit(revision)?;
        validate(&info.commit)?;
        let signature = info.commit.signature.as_ref().ok_or("commit is unsigned")?;
        if let Some(path) = trust {
            trusted(&info.commit, &crate::binary::decode(&read_regular(path)?)?)?;
        } else {
            verify_crypto(&info.commit, None)?;
        }
        Ok(
            json!({"commit":info.id,"cryptographically_valid":true,"trusted":trust.is_some(),"format":match signature.version { 1 => "native", 2 => "ssh", _ => "openpgp" },"author":signature.author,"timestamp":signature.timestamp,"public_key":crate::binary::hex(&signature.public_key)}),
        )
    }
    #[cfg(feature = "server")]
    pub(crate) fn verify_receive_authors(
        &self,
        bundle: &[u8],
        updates: &[crate::protocol::RefUpdate],
        path: &Path,
    ) -> RepositoryResult<()> {
        let current = self.snapshot()?;
        let published = reachable_objects(&current.manifest, &current.objects)?;
        let mut objects = current.objects;
        if !bundle.is_empty() {
            objects.extend(decode_bundle(bundle)?.objects);
        }
        // Ref-only transactions can expose previously dangling commits. Check
        // the newly published closure, including when no objects are uploaded;
        // deletions and references to already public history need no new sig.
        let roots = updates.iter().filter_map(|update| update.new.clone());
        let reachable = reachable_from_roots(roots, &objects)?;
        let trust: TrustedAuthors = crate::binary::decode(&read_regular(path)?)?;
        if trust.version != 2 {
            return Err("unsupported trust store".into());
        }
        for id in reachable.difference(&published) {
            if let Some(Object::Commit(commit)) = objects.get(id) {
                trusted(commit, &trust)?;
            }
        }
        Ok(())
    }
}

#[derive(clap::Parser)]
struct Args {
    #[command(subcommand)]
    command: SigningCommand,
}
#[derive(clap::Subcommand)]
enum SigningCommand {
    Keygen {
        secret: PathBuf,
        public: PathBuf,
    },
    TrustAuthor {
        trust: PathBuf,
        author: String,
        public: PathBuf,
        #[arg(long, value_enum, default_value = "native")]
        format: SignatureFormat,
    },
    UntrustAuthor {
        trust: PathBuf,
        author: String,
        #[arg(long)]
        public: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "native")]
        format: SignatureFormat,
    },
    CommitSigned {
        repository: PathBuf,
        wasm: PathBuf,
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        author: String,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        timestamp: Option<u64>,
        #[arg(long, value_enum, default_value = "native")]
        format: SignatureFormat,
    },
    SignCommit {
        repository: PathBuf,
        expected: String,
        #[arg(long)]
        author: String,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        timestamp: Option<u64>,
        #[arg(long, value_enum, default_value = "native")]
        format: SignatureFormat,
    },
    VerifyCommit {
        repository: PathBuf,
        revision: String,
        #[arg(long)]
        trust: Option<PathBuf>,
    },
}
pub fn cli(args: &[String]) -> RepositoryResult<String> {
    use clap::Parser;
    let args = match Args::try_parse_from(
        std::iter::once("wasm-git".to_owned()).chain(args.iter().cloned()),
    ) {
        Ok(v) => v,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => return Ok(e.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_secs())
            .map_err(|e| e.to_string())
    };
    match args.command {
        SigningCommand::Keygen { secret, public } => {
            generate_key(&secret, &public)?;
            Ok("created private/public author keys".into())
        }
        SigningCommand::TrustAuthor {
            trust,
            author,
            public,
            format,
        } => {
            if format == SignatureFormat::Native {
                trust_key(&trust, &author, &public)?;
            } else {
                trust_key_format(&trust, &author, &public, format)?;
            }
            Ok("trusted author key".into())
        }
        SigningCommand::UntrustAuthor {
            trust,
            author: name,
            public,
            format,
        } => {
            author(&name)?;
            let key = public
                .as_ref()
                .map(|path| -> RepositoryResult<[u8; 32]> {
                    if format != SignatureFormat::Native {
                        return external::key_id(
                            format.version(),
                            &external::import_public(format, path)?,
                        );
                    }
                    let bytes = read_regular(path)?;
                    bytes
                        .strip_prefix(PUBLIC_MAGIC)
                        .ok_or("invalid public key format")?
                        .try_into()
                        .map_err(|_| "invalid public key length".into())
                })
                .transpose()?;
            edit_trust(&trust, |trust| {
                if let Some(key) = key {
                    if let Some(keys) = trust.authors.get_mut(&name) {
                        keys.remove(&key);
                        if keys.is_empty() {
                            trust.authors.remove(&name);
                        }
                    }
                } else {
                    trust.authors.remove(&name);
                }
                Ok(())
            })?;
            Ok("revoked author trust".into())
        }
        SigningCommand::CommitSigned {
            repository,
            wasm,
            message,
            author,
            key,
            timestamp,
            format,
        } => Repository::open(repository)?.commit_signed_format(
            &read_regular(&wasm)?,
            &message,
            &SigningIdentity {
                author,
                key,
                timestamp: timestamp.map_or_else(now, Ok)?,
            },
            format,
        ),
        SigningCommand::SignCommit {
            repository,
            expected,
            author,
            key,
            timestamp,
            format,
        } => Repository::open(repository)?.sign_head_format(
            &expected,
            &SigningIdentity {
                author,
                key,
                timestamp: timestamp.map_or_else(now, Ok)?,
            },
            format,
        ),
        SigningCommand::VerifyCommit {
            repository,
            revision,
            trust,
        } => serde_json::to_string_pretty(
            &Repository::open(repository)?.verify_author(&revision, trust.as_deref())?,
        )
        .map_err(|e| e.to_string()),
    }
}
