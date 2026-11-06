//! Native binary attestations using installed OpenSSH/GnuPG implementations.
//! Verification uses an isolated public keyring, never the user's trustdb.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: usize = 1024 * 1024;
const NAMESPACE: &str = "commit@wasm-git";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SignatureFormat {
    Native,
    Ssh,
    Openpgp,
}
impl SignatureFormat {
    pub(super) fn version(self) -> u32 {
        match self {
            Self::Native => 1,
            Self::Ssh => 2,
            Self::Openpgp => 3,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExternalSignature {
    pub public: Vec<u8>,
    pub signature: Vec<u8>,
}

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> RepositoryResult<Self> {
        let path = std::env::temp_dir().join(unique_name("uwgit-signature"));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|e| format!("create signature workspace: {e}"))?;
        Ok(Self(path))
    }
    fn put(&self, name: &str, bytes: &[u8]) -> RepositoryResult<PathBuf> {
        let path = self.0.join(name);
        write_new_file(&path, bytes)?;
        Ok(path)
    }
    fn run(&self, mut command: Command, input: &[u8]) -> RepositoryResult<Vec<u8>> {
        let name = unique_name("command");
        let stdin = self.put(&format!("{name}.in"), input)?;
        let stdout = self.put(&format!("{name}.out"), &[])?;
        let stderr = self.put(&format!("{name}.err"), &[])?;
        command
            .current_dir(&self.0)
            .env("LC_ALL", "C")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .stdin(Stdio::from(File::open(stdin).map_err(|e| e.to_string())?))
            .stdout(Stdio::from(
                OpenOptions::new()
                    .write(true)
                    .open(&stdout)
                    .map_err(|e| e.to_string())?,
            ))
            .stderr(Stdio::from(
                OpenOptions::new()
                    .write(true)
                    .open(&stderr)
                    .map_err(|e| e.to_string())?,
            ));
        let mut child = command
            .spawn()
            .map_err(|e| format!("start signature tool: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = loop {
            let bounded = [&stdout, &stderr]
                .iter()
                .all(|p| fs::metadata(p).is_ok_and(|m| m.len() <= LIMIT as u64));
            if !bounded || Instant::now() >= deadline {
                break Err("signature tool exceeded output or 30-second time limit".into());
            }
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break Ok(()),
                Ok(Some(_)) => {
                    break Err(format!(
                        "signature tool rejected the operation: {}",
                        String::from_utf8_lossy(&read_regular(&stderr)?)
                    ))
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => break Err(e.to_string()),
            }
        };
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result?;
        let bytes = read_regular(&stdout)?;
        if bytes.len() > LIMIT {
            return Err("signature output exceeds 1 MiB".into());
        }
        Ok(bytes)
    }
    fn gpg(&self) -> Command {
        let mut command = Command::new("gpg");
        command
            .args([
                "--no-options",
                "--batch",
                "--no-tty",
                "--no-autostart",
                "--no-auto-key-retrieve",
                "--auto-key-locate",
                "clear",
                "--homedir",
            ])
            .arg(&self.0);
        command
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn decode_ssh_public(bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
    let text = std::str::from_utf8(bytes).map_err(|_| "SSH public key must be UTF-8")?;
    let mut fields = text.split_whitespace();
    let algorithm = fields.next().ok_or("missing SSH key algorithm")?;
    let blob = STANDARD
        .decode(fields.next().ok_or("missing SSH public key")?)
        .map_err(|e| e.to_string())?;
    if ssh_algorithm(&blob)? != algorithm {
        return Err("SSH public key algorithm mismatch".into());
    }
    Ok(blob)
}
fn ssh_algorithm(bytes: &[u8]) -> RepositoryResult<&str> {
    let size = u32::from_be_bytes(
        bytes
            .get(..4)
            .ok_or("truncated SSH public key")?
            .try_into()
            .unwrap(),
    ) as usize;
    let algorithm = std::str::from_utf8(
        bytes
            .get(4..4usize.checked_add(size).ok_or("SSH size overflow")?)
            .ok_or("truncated SSH algorithm")?,
    )
    .map_err(|_| "invalid SSH algorithm")?;
    if algorithm.is_empty()
        || algorithm.len() > 128
        || !algorithm
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-@._".contains(&b))
    {
        return Err("invalid SSH public key algorithm".into());
    }
    Ok(algorithm)
}
fn ssh_public_text(bytes: &[u8]) -> RepositoryResult<String> {
    Ok(format!(
        "{} {}",
        ssh_algorithm(bytes)?,
        STANDARD.encode(bytes)
    ))
}
fn armor(bytes: &[u8]) -> Vec<u8> {
    format!(
        "-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----\n",
        STANDARD.encode(bytes)
    )
    .into_bytes()
}
fn unarmor(bytes: &[u8]) -> RepositoryResult<Vec<u8>> {
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid SSH signature armor")?;
    let body = text
        .trim()
        .strip_prefix("-----BEGIN SSH SIGNATURE-----")
        .and_then(|s| s.strip_suffix("-----END SSH SIGNATURE-----"))
        .ok_or("invalid SSH signature armor")?;
    let bytes = STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|e| e.to_string())?;
    if !bytes.starts_with(b"SSHSIG") {
        return Err("invalid SSH signature magic".into());
    }
    Ok(bytes)
}
fn pgp_fingerprint(work: &Workspace, public: &[u8]) -> RepositoryResult<String> {
    let file = work.put(&unique_name("public.pgp"), public)?;
    let mut command = work.gpg();
    command
        .args(["--with-colons", "--import-options", "show-only", "--import"])
        .arg(file);
    let output =
        String::from_utf8(work.run(command, &[])?).map_err(|_| "invalid GPG key metadata")?;
    let mut keys = 0;
    let mut fingerprint = None;
    for line in output.lines() {
        let f: Vec<_> = line.split(':').collect();
        match f.first().copied() {
            Some("sec" | "ssb") => {
                return Err("private OpenPGP material is not accepted as a public key".into())
            }
            Some("pub") => keys += 1,
            Some("fpr") if fingerprint.is_none() => fingerprint = f.get(9).map(|s| s.to_string()),
            _ => (),
        }
    }
    let fingerprint = fingerprint.ok_or("GPG key has no primary fingerprint")?;
    if keys != 1
        || !matches!(fingerprint.len(), 40 | 64)
        || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("one OpenPGP public certificate with a full fingerprint is required".into());
    }
    Ok(fingerprint.to_ascii_uppercase())
}
pub(super) fn key_id(version: u32, public: &[u8]) -> RepositoryResult<[u8; 32]> {
    if public.is_empty() || public.len() > LIMIT {
        return Err("external public key exceeds bounds".into());
    }
    let mut digest = Sha256::new();
    digest.update(b"uwgit.external.author.key.v1\0");
    digest.update(version.to_be_bytes());
    match version {
        2 => {
            ssh_algorithm(public)?;
            digest.update(public);
        }
        3 => digest.update(pgp_fingerprint(&Workspace::new()?, public)?),
        _ => return Err("unsupported external signature version".into()),
    }
    Ok(digest.finalize().into())
}
pub(super) fn import_public(format: SignatureFormat, path: &Path) -> RepositoryResult<Vec<u8>> {
    if fs::symlink_metadata(path).map_err(|e| e.to_string())?.len() > LIMIT as u64 {
        return Err("public key exceeds 1 MiB".into());
    }
    let bytes = read_regular(path)?;
    if bytes.len() > LIMIT {
        return Err("public key exceeds 1 MiB".into());
    }
    match format {
        SignatureFormat::Ssh => decode_ssh_public(&bytes),
        SignatureFormat::Openpgp => {
            let work = Workspace::new()?;
            pgp_fingerprint(&work, &bytes)?;
            let file = work.put("key.pgp", &bytes)?;
            let mut command = work.gpg();
            command
                .args(["--import-options", "import-export", "--import"])
                .arg(file);
            work.run(command, &[])
        }
        SignatureFormat::Native => Err("native public keys use the native importer".into()),
    }
}
pub(super) fn public_for_signing(format: SignatureFormat, key: &Path) -> RepositoryResult<Vec<u8>> {
    let work = Workspace::new()?;
    match format {
        SignatureFormat::Ssh => {
            let sidecar = PathBuf::from(format!("{}.pub", key.display()));
            if key.extension().is_some_and(|s| s == "pub") {
                return import_public(format, key);
            }
            if sidecar.exists() {
                return import_public(format, &sidecar);
            }
            let mut command = Command::new("ssh-keygen");
            command
                .args(["-y", "-f"])
                .arg(fs::canonicalize(key).map_err(|e| e.to_string())?);
            decode_ssh_public(&work.run(command, &[])?)
        }
        SignatureFormat::Openpgp => {
            let key = key
                .to_str()
                .ok_or("OpenPGP key requires a full fingerprint")?;
            let fingerprint = key.strip_suffix('!').unwrap_or(key);
            if !matches!(fingerprint.len(), 40 | 64)
                || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("OpenPGP signing requires a full 40/64-digit key fingerprint, optionally suffixed by '!'".into());
            }
            let mut command = Command::new("gpg");
            command.args([
                "--no-options",
                "--batch",
                "--no-tty",
                "--export-options",
                "export-minimal",
                "--export",
                key,
            ]);
            let bytes = work.run(command, &[])?;
            pgp_fingerprint(&work, &bytes)?;
            Ok(bytes)
        }
        SignatureFormat::Native => Err("native signing does not use an external tool".into()),
    }
}
pub(super) fn sign(
    format: SignatureFormat,
    key: &Path,
    public: Vec<u8>,
    message: &[u8],
) -> RepositoryResult<ExternalSignature> {
    let work = Workspace::new()?;
    let signature = match format {
        SignatureFormat::Ssh => {
            let mut command = Command::new("ssh-keygen");
            command
                .args(["-Y", "sign", "-n", NAMESPACE, "-f"])
                .arg(fs::canonicalize(key).map_err(|e| e.to_string())?);
            unarmor(&work.run(command, message)?)?
        }
        SignatureFormat::Openpgp => {
            let mut command = Command::new("gpg");
            command
                .args([
                    "--no-options",
                    "--batch",
                    "--no-tty",
                    "--pinentry-mode",
                    "error",
                    "--detach-sign",
                    "--local-user",
                ])
                .arg(key)
                .args(["--output", "-"]);
            work.run(command, message)?
        }
        SignatureFormat::Native => return Err("expected external signature format".into()),
    };
    Ok(ExternalSignature { public, signature })
}
pub(super) fn decode(signature: &AuthorSignature) -> RepositoryResult<ExternalSignature> {
    if signature.signature.len() > 2 * LIMIT {
        return Err("external signature envelope exceeds 2 MiB".into());
    }
    let envelope: ExternalSignature = crate::binary::decode(&signature.signature)?;
    if envelope.public.is_empty()
        || envelope.public.len() > LIMIT
        || envelope.signature.is_empty()
        || envelope.signature.len() > LIMIT
    {
        return Err("external key/signature sizes are invalid".into());
    }
    if signature.version == 2 {
        ssh_algorithm(&envelope.public)?;
        if !envelope.signature.starts_with(b"SSHSIG") {
            return Err("invalid SSH signature".into());
        }
    }
    Ok(envelope)
}
pub(super) fn verify(
    signature: &AuthorSignature,
    message: &[u8],
    trusted_public: Option<&[u8]>,
) -> RepositoryResult<()> {
    let envelope = decode(signature)?;
    let public = trusted_public.unwrap_or(&envelope.public);
    if key_id(signature.version, public)? != signature.public_key {
        return Err("external signing key identity mismatch".into());
    }
    let work = Workspace::new()?;
    match signature.version {
        2 => {
            let allowed = work.put(
                "allowed-signers",
                format!(
                    "uwgit-author namespaces=\"{NAMESPACE}\" {}\n",
                    ssh_public_text(public)?
                )
                .as_bytes(),
            )?;
            let sig = work.put("signature", &armor(&envelope.signature))?;
            let mut command = Command::new("ssh-keygen");
            command
                .args(["-Y", "verify", "-n", NAMESPACE, "-I", "uwgit-author", "-f"])
                .arg(allowed)
                .arg("-s")
                .arg(sig);
            work.run(command, message)?;
        }
        3 => {
            let fingerprint = pgp_fingerprint(&work, public)?;
            let public_file = work.put("public.pgp", public)?;
            let mut command = work.gpg();
            command.arg("--import").arg(public_file);
            work.run(command, &[])?;
            let sig = work.put("signature.pgp", &envelope.signature)?;
            let data = work.put("message", message)?;
            let mut command = work.gpg();
            command
                .args(["--status-fd=1", "--verify"])
                .arg(sig)
                .arg(data);
            let output =
                String::from_utf8(work.run(command, &[])?).map_err(|_| "invalid GPG status")?;
            let mut valid = 0;
            for line in output.lines().filter_map(|s| s.strip_prefix("[GNUPG:] ")) {
                let fields: Vec<_> = line.split_whitespace().collect();
                match fields.first().copied() {
                    Some("VALIDSIG") => {
                        if fields.len() < 10 {
                            return Err("invalid GPG VALIDSIG record".into());
                        }
                        let primary = fields.get(10).unwrap_or(&fields[1]);
                        if *primary != fingerprint {
                            return Err(
                                "OpenPGP signer does not match the selected primary key".into()
                            );
                        }
                        valid += 1;
                    }
                    Some(
                        "BADSIG" | "ERRSIG" | "REVKEYSIG" | "EXPKEYSIG" | "EXPSIG" | "KEYREVOKED"
                        | "KEYEXPIRED" | "SIGEXPIRED",
                    ) => {
                        return Err(
                            "OpenPGP signature or key is invalid, expired, or revoked".into()
                        )
                    }
                    _ => (),
                }
            }
            if valid != 1 {
                return Err("expected exactly one valid OpenPGP signature".into());
            }
        }
        _ => return Err("unsupported external signature".into()),
    }
    Ok(())
}
