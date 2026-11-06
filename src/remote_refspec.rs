//! Expand native push mappings before any objects or refs are published.
//! Like Git push (remote.c and t5582), negative patterns match destinations
//! after mapping, including references otherwise selected for pruning.
use super::*;

struct Pattern(String);
impl Pattern {
    fn new(value: &str) -> RepositoryResult<Self> {
        if !value.starts_with("refs/") || value.matches('*').count() > 1 {
            return Err("refspec patterns require a full refs/ name and at most one '*'".into());
        }
        // Patterns may span namespaces (refs/*), but each expanded destination
        // is subsequently validated against the repository's native refs.
        branch_name(&value[5..].replace('*', "pattern"))?;
        Ok(Self(value.into()))
    }
    fn capture<'a>(&self, reference: &'a str) -> Option<&'a str> {
        if let Some((prefix, suffix)) = self.0.split_once('*') {
            reference.strip_prefix(prefix)?.strip_suffix(suffix)
        } else {
            (reference == self.0).then_some("")
        }
    }
    fn substitute(&self, capture: &str) -> String {
        self.0.replacen('*', capture, 1)
    }
}

fn refs(state: &RepositoryState) -> BTreeSet<String> {
    state
        .branches
        .keys()
        .map(|n| format!("refs/heads/{n}"))
        .chain(state.tags.keys().map(|n| format!("refs/tags/{n}")))
        .collect()
}

pub(super) fn special(spec: &str) -> bool {
    spec.contains('*') || spec.starts_with('^') || matches!(spec, ":" | "+:")
}

pub(super) fn expand(
    specs: &[&str],
    local: &RepositoryState,
    remote: &RepositoryState,
    prune: bool,
) -> RepositoryResult<Vec<String>> {
    let local_refs = refs(local);
    let remote_refs = refs(remote);
    let mut excluded = Vec::new();
    for spec in specs.iter().filter_map(|s| s.strip_prefix('^')) {
        if spec.contains(':') || spec.starts_with('+') || valid_id(spec) {
            return Err(
                "negative push refspecs contain only a destination reference or pattern".into(),
            );
        }
        excluded.push(Pattern::new(spec)?);
    }
    let excluded = |target: &str| excluded.iter().any(|p| p.capture(target).is_some());
    let mut result = Vec::new();
    let mut append = |spec: String| -> RepositoryResult<()> {
        if result.len() == crate::repository::MAX_REFERENCE_EDITS {
            return Err("expanded push exceeds the 4096-reference transaction limit".into());
        }
        result.push(spec);
        Ok(())
    };
    let mut positive = false;
    for raw in specs.iter().filter(|s| !s.starts_with('^')) {
        positive = true;
        let (spec, force) = raw.strip_prefix('+').map_or((*raw, ""), |s| (s, "+"));
        if spec == ":" {
            for name in &remote_refs {
                if name.starts_with("refs/heads/") && !excluded(name) {
                    if local_refs.contains(name) {
                        append(format!("{force}{name}:{name}"))?;
                    } else if prune {
                        append(format!(":{name}"))?;
                    }
                }
            }
            continue;
        }
        let (source, destination) = spec.split_once(':').unwrap_or((spec, spec));
        if source.contains('*') || destination.contains('*') {
            if source.matches('*').count() != 1 || destination.matches('*').count() != 1 {
                return Err(
                    "wildcard refspecs require one '*' in both source and destination".into(),
                );
            }
            let source = Pattern::new(source)?;
            let destination = Pattern::new(destination)?;
            for name in &local_refs {
                if let Some(capture) = source.capture(name) {
                    let target = destination.substitute(capture);
                    reference(&target)?;
                    if !excluded(&target) {
                        append(format!("{force}{name}:{target}"))?;
                    }
                }
            }
            if prune {
                for target in &remote_refs {
                    if let Some(capture) = destination.capture(target) {
                        let name = source.substitute(capture);
                        if !local_refs.contains(&name) && !excluded(target) {
                            append(format!(":{target}"))?;
                        }
                    }
                }
            }
        } else {
            let dest = if spec == "HEAD" {
                &local.head
            } else {
                destination
            };
            let (ns, name) = reference(dest)?;
            let ns = if matches!(ns, RefNamespace::Branch) {
                "heads"
            } else {
                "tags"
            };
            if !excluded(&format!("refs/{ns}/{name}")) {
                append(raw.to_string())?;
            }
        }
    }
    if !positive {
        return Err("a negative refspec requires a positive source selection".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(branches: &[&str], tags: &[&str]) -> RepositoryState {
        RepositoryState {
            format: 1,
            head: "main".into(),
            branches: branches
                .iter()
                .map(|n| (n.to_string(), "a".repeat(64)))
                .collect(),
            tags: tags
                .iter()
                .map(|n| (n.to_string(), "b".repeat(64)))
                .collect(),
        }
    }

    #[test]
    fn mapped_pruning_excludes_selected_and_unrelated_destinations() {
        let local = state(
            &[
                "main",
                "release/v1",
                "release/nested/v2",
                "release/private/x",
            ],
            &[],
        );
        let remote = state(
            &["main", "public/stale", "public/private/stale", "unrelated"],
            &[],
        );
        assert_eq!(
            expand(
                &[
                    "+refs/heads/release/*:refs/heads/public/*",
                    "^refs/heads/public/private/*"
                ],
                &local,
                &remote,
                true
            )
            .unwrap(),
            [
                "+refs/heads/release/nested/v2:refs/heads/public/nested/v2",
                "+refs/heads/release/v1:refs/heads/public/v1",
                ":refs/heads/public/stale",
            ]
        );
    }

    #[test]
    fn matching_patterns_and_exact_exclusions_share_destination_semantics() {
        let local = state(&["main", "shared", "local"], &["v1"]);
        let remote = state(&["main", "shared", "remote"], &["old"]);
        assert_eq!(
            expand(&["+:", "^refs/heads/main"], &local, &remote, true).unwrap(),
            [":refs/heads/remote", "+refs/heads/shared:refs/heads/shared"]
        );
        assert_eq!(
            expand(
                &[
                    "main:other",
                    "HEAD:head-copy",
                    "refs/tags/*",
                    "^refs/heads/main"
                ],
                &local,
                &remote,
                false
            )
            .unwrap(),
            ["main:other", "HEAD:head-copy", "refs/tags/v1:refs/tags/v1"]
        );
        assert!(expand(
            &[
                "main:other",
                ":refs/heads/removed",
                "^refs/heads/other",
                "^refs/heads/removed"
            ],
            &local,
            &remote,
            true
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            expand(&["refs/*:refs/*", "^refs/heads/*"], &local, &remote, true).unwrap(),
            ["refs/tags/v1:refs/tags/v1", ":refs/tags/old"]
        );
    }

    #[test]
    fn malformed_or_unbounded_refspecs_fail_before_publication() {
        let local = state(&["main"], &[]);
        for specs in [
            vec!["^main"],
            vec!["refs/heads/*:main"],
            vec!["refs/heads/**:refs/tags/*"],
            vec!["*:refs/heads/*"],
            vec!["main", "^main:other"],
            vec!["main", "^refs/heads/../*"],
        ] {
            assert!(expand(&specs, &local, &local, true).is_err(), "{specs:?}");
        }
        let mut large = local.clone();
        for n in 0..4096 {
            large.branches.insert(format!("b{n}"), "a".repeat(64));
        }
        assert!(expand(&["refs/heads/*"], &large, &local, false)
            .unwrap_err()
            .contains("4096"));
    }
}
