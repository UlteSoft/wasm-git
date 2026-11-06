//! Hosting authorization configuration is compiled once from an administrator's
//! external JSON input. The runtime file and audit records are native binary.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    version: u32,
    principals: Vec<Principal>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Principal {
    name: String,
    token_sha256: Option<String>,
    grants: Vec<Grant>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    repository: String,
    read: bool,
    write: Vec<String>,
    force: bool,
    create: bool,
}

fn matches(pattern: &str, value: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == value, |prefix| value.starts_with(prefix))
}

impl Policy {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.principals.len() > 4096 {
            return Err("invalid policy version or principal count".into());
        }
        let mut names = std::collections::BTreeSet::new();
        let mut tokens = std::collections::BTreeSet::new();
        for principal in &self.principals {
            protocol::repository_component(&principal.name)?;
            if !names.insert(&principal.name) {
                return Err("duplicate policy principal".into());
            }
            if let Some(hash) = &principal.token_sha256 {
                crate::binary::oid(hash)?;
                if !tokens.insert(hash) {
                    return Err("duplicate policy credential".into());
                }
            }
            for grant in &principal.grants {
                let prefix = grant
                    .repository
                    .strip_suffix('*')
                    .unwrap_or(&grant.repository);
                if grant.repository != "*" {
                    protocol::repository_name(prefix.strip_suffix('/').unwrap_or(prefix))?;
                }
                for reference in &grant.write {
                    let prefix = reference.strip_suffix('*').unwrap_or(reference);
                    if !(prefix.starts_with("refs/heads/") || prefix.starts_with("refs/tags/"))
                        || prefix.contains('*')
                        || prefix.contains("..")
                        || prefix.bytes().any(|b| b <= 32 || b >= 127)
                    {
                        return Err("policy write rules require refs/heads/ or refs/tags/ prefixes and an optional final '*'".into());
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let path = real_path(path, false)?;
        if fs::metadata(&path).map_err(|e| e.to_string())?.len() > 16 * 1024 * 1024 {
            return Err("policy exceeds 16 MiB".into());
        }
        let policy: Self = crate::binary::decode(&fs::read(path).map_err(|e| e.to_string())?)?;
        policy.validate()?;
        Ok(policy)
    }
    pub(super) fn authenticate(&self, token: &[u8]) -> Option<String> {
        let hash = format!("{:x}", Sha256::digest(token));
        let mut name = None;
        for principal in &self.principals {
            if principal
                .token_sha256
                .as_ref()
                .is_some_and(|expected| equal_secret(hash.as_bytes(), expected.as_bytes()))
            {
                name = Some(principal.name.clone());
            }
        }
        name
    }
    pub(super) fn principal_exists(&self, name: &str) -> bool {
        self.principals.iter().any(|p| p.name == name)
    }
    pub(super) fn authorize(
        &self,
        actor: &str,
        repo: &str,
        reference: Option<&str>,
        force: bool,
        create: bool,
    ) -> Result<(), String> {
        let allowed = self
            .principals
            .iter()
            .filter(|p| p.name == actor)
            .flat_map(|p| &p.grants)
            .any(|g| {
                matches(&g.repository, repo)
                    && if create {
                        g.create
                    } else {
                        g.read
                            && (!force || g.force)
                            && reference.is_none_or(|r| {
                                if r == "*" {
                                    !g.write.is_empty()
                                } else {
                                    g.write.iter().any(|p| matches(p, r))
                                }
                            })
                    }
            });
        if allowed {
            Ok(())
        } else {
            Err("authorization denied for this repository or reference".into())
        }
    }
}

pub(super) fn compile(input: &Path, output: &Path) -> Result<String, String> {
    let input = real_path(input, false)?;
    if fs::metadata(&input).map_err(|e| e.to_string())?.len() > 16 * 1024 * 1024 {
        return Err("policy input exceeds 16 MiB".into());
    }
    let policy: Policy = serde_json::from_slice(&fs::read(input).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    policy.validate()?;
    let bytes = crate::binary::encode(&policy)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(format!("compiled {} principals", policy.principals.len()))
}
