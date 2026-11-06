//! Timestamped native reflogs and explicit retention without rewriting objects.
use super::*;

pub(super) fn reflog_timestamp() -> RepositoryResult<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| format!("reflog clock: {e}"))
}

#[derive(Clone, Copy, Debug)]
pub enum ReflogCutoff {
    Never,
    All,
    Before(u64),
}
impl ReflogCutoff {
    pub fn parse(value: &str) -> RepositoryResult<Self> {
        let now = reflog_timestamp()?;
        match value {
            "never" => return Ok(Self::Never),
            "all" => return Ok(Self::All),
            "now" => return Ok(Self::Before(now)),
            "yesterday" => return Ok(Self::Before(now.saturating_sub(86_400))),
            _ => (),
        }
        if let Some(seconds) = value.strip_prefix('@') {
            return seconds
                .parse()
                .map(Self::Before)
                .map_err(|_| "invalid Unix timestamp".into());
        }
        if let Some(date) = utc_date(value) {
            return date.map(Self::Before);
        }
        let normalized = value.replace('.', " ");
        let fields: Vec<_> = normalized.split_whitespace().collect();
        if let [count, unit, "ago"] = fields.as_slice() {
            let seconds = match unit.trim_end_matches('s') {
                "second" => 1,
                "minute" => 60,
                "hour" => 3600,
                "day" => 86_400,
                "week" => 604_800,
                _ => return Err("unsupported reflog age unit".into()),
            };
            let age = count
                .parse::<u64>()
                .ok()
                .and_then(|n| n.checked_mul(seconds))
                .ok_or("invalid or overflowing reflog age")?;
            return Ok(Self::Before(now.saturating_sub(age)));
        }
        Err("expected all, never, now, @UNIX_SECONDS, UTC date, or N.days.ago (seconds/minutes/hours/weeks also accepted)".into())
    }
    fn expires(self, seconds: u64) -> bool {
        match self {
            Self::Never => false,
            Self::All => true,
            Self::Before(limit) => seconds < limit,
        }
    }
}

fn utc_date(value: &str) -> Option<RepositoryResult<u64>> {
    let b = value.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    Some((|| {
        if !matches!(b.len(), 10 | 20) || !value.is_ascii() {
            return Err("invalid UTC date".into());
        }
        let number = |a: usize, z: usize| -> RepositoryResult<u64> {
            if !b[a..z].iter().all(u8::is_ascii_digit) {
                return Err("invalid UTC date field".into());
            }
            value[a..z]
                .parse()
                .map_err(|_| "invalid UTC date field".into())
        };
        let year = number(0, 4)?;
        let month = number(5, 7)?;
        let day = number(8, 10)?;
        let leap =
            |y: u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
        let months = [
            31,
            if leap(year) { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if !(1970..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || day == 0
            || day > months[(month - 1) as usize]
        {
            return Err("UTC date is out of range".into());
        }
        let days: u64 = (1970..year)
            .map(|y| if leap(y) { 366 } else { 365 })
            .sum::<u64>()
            + months[..(month - 1) as usize].iter().sum::<u64>()
            + day
            - 1;
        let time = if b.len() == 20 {
            if b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
                return Err("timestamps require YYYY-MM-DDTHH:MM:SSZ".into());
            }
            let (h, m, s) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
            if h > 23 || m > 59 || s > 59 {
                return Err("UTC time is out of range".into());
            }
            h * 3600 + m * 60 + s
        } else {
            0
        };
        Ok(days * 86_400 + time)
    })())
}

pub struct ReflogExpire {
    pub references: Vec<String>,
    pub all: bool,
    pub single_worktree: bool,
    pub expire: ReflogCutoff,
    pub expire_unreachable: ReflogCutoff,
    pub dry_run: bool,
    pub rewrite: bool,
}

fn reference(snapshot: &Snapshot, value: &str) -> RepositoryResult<String> {
    if value == "HEAD" {
        return Ok(value.into());
    }
    if let Some(name) = value
        .strip_prefix("refs/heads/")
        .or_else(|| value.strip_prefix("refs/tags/"))
    {
        validate_ref_name(name)?;
        return Ok(value.into());
    }
    validate_ref_name(value)?;
    match (
        snapshot.manifest.refs.contains_key(value),
        snapshot.manifest.tags.contains_key(value),
    ) {
        (true, true) => Err("ambiguous reflog reference; qualify its namespace".into()),
        (false, true) => Ok(format!("refs/tags/{value}")),
        _ => Ok(format!("refs/heads/{value}")),
    }
}

fn rewrite_chain(entries: &mut [ReflogEntry], selected: &BTreeSet<String>) {
    let mut previous: BTreeMap<String, Option<String>> = BTreeMap::new();
    for entry in entries {
        if selected.contains(&entry.reference) {
            entry.old = previous.get(&entry.reference).cloned().flatten();
        }
        previous.insert(entry.reference.clone(), entry.new.clone());
    }
}

impl Repository {
    // A replay can still need its original publication record to finish an
    // interrupted operation. The shared writer lock serializes this check
    // against recording a new pending operation in every registered worktree.
    fn reflog_mutation_origins(&self) -> RepositoryResult<Vec<Repository>> {
        let origins = linked_worktrees::reflog_origins(self)?;
        for origin in &origins {
            match fs::symlink_metadata(origin.metadata.join("pending.bin")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.to_string()),
                Ok(_) => {
                    return Err(
                        "finish or abort the pending history operation before changing reflogs"
                            .into(),
                    )
                }
            }
        }
        Ok(origins)
    }

    pub fn reflog_names(&self) -> RepositoryResult<BTreeSet<String>> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        Ok(snapshot
            .reflog
            .into_iter()
            .filter(|e| e.reference != "HEAD")
            .chain(snapshot.head_reflog)
            .map(|e| e.reference)
            .collect())
    }

    pub fn write_reflog_entry(
        &self,
        name: &str,
        old: Option<String>,
        new: Option<String>,
        message: &str,
    ) -> RepositoryResult<()> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        self.reflog_mutation_origins()?;
        let normalized = reference(&snapshot, name)?;
        if normalized != name {
            return Err("reflog write requires a fully qualified reference or HEAD".into());
        }
        for id in old.iter().chain(&new) {
            require_id(id)?;
            if !matches!(snapshot.objects.get(id), Some(Object::Commit(_))) {
                return Err(format!("reflog endpoint is not an existing commit: {id}"));
            }
        }
        let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if message.is_empty() {
            return Err("reflog message cannot be empty".into());
        }
        if name == "HEAD" {
            self.record_head_event(old, new, &message)
        } else {
            self.record_reflog(name, old, new, &message)
        }
    }

    pub fn reflog_entries(&self, name: &str) -> RepositoryResult<Vec<ReflogEntry>> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        let name = reference(&snapshot, name)?;
        let entries = if name == "HEAD" {
            snapshot.head_reflog
        } else {
            snapshot.reflog
        };
        Ok(entries
            .into_iter()
            .filter(|e| e.reference == name)
            .rev()
            .collect())
    }

    pub fn expire_reflogs(&self, request: &ReflogExpire) -> RepositoryResult<Value> {
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        if request.all && !request.references.is_empty() {
            return Err("--all conflicts with explicit reflog names".into());
        }
        if !request.all && request.references.is_empty() {
            return Err("select reflogs explicitly or use --all".into());
        }
        let selected: BTreeSet<_> = request
            .references
            .iter()
            .map(|r| reference(&snapshot, r))
            .collect::<RepositoryResult<_>>()?;
        let origins = self.reflog_mutation_origins()?;
        let mut roots: BTreeSet<_> = snapshot
            .manifest
            .refs
            .values()
            .chain(snapshot.manifest.tags.values())
            .chain(snapshot.tracking.values())
            .cloned()
            .collect();
        for origin in std::iter::once(self).chain(origins.iter()) {
            let identity = origin.current_branch()?;
            if valid_id(&identity) {
                if !matches!(snapshot.objects.get(&identity), Some(Object::Commit(_))) {
                    return Err("worktree HEAD references a missing commit".into());
                }
                roots.insert(identity);
            }
        }
        let mut reachability = BTreeMap::new();
        let mut jobs = vec![(self, self.common_metadata.join("reflog"), snapshot.reflog)];
        if !request.all || request.single_worktree {
            jobs.push((
                self,
                self.metadata.join("head-reflog.bin"),
                snapshot.head_reflog,
            ));
        } else {
            for origin in &origins {
                jobs.push((
                    origin,
                    origin.metadata.join("head-reflog.bin"),
                    origin.read_head_reflog()?,
                ));
            }
        }
        let mut plans = Vec::new();
        let mut removed = Vec::new();
        for (origin, path, entries) in jobs {
            let mut kept = Vec::new();
            let mut rewritten = BTreeSet::new();
            let before = entries.len();
            for entry in entries {
                for id in entry.old.iter().chain(&entry.new) {
                    if !matches!(snapshot.objects.get(id), Some(Object::Commit(_))) {
                        return Err(format!(
                            "reflog references missing or non-commit object {id}"
                        ));
                    }
                }
                let chosen = request.all || selected.contains(&entry.reference);
                if chosen {
                    rewritten.insert(entry.reference.clone());
                }
                let expired = chosen && request.expire.expires(entry.unix_seconds);
                // Recent entries need no graph walk. Equal tips share a cached
                // closure, even when thousands of names point to that commit.
                let unreachable =
                    if chosen && !expired && request.expire_unreachable.expires(entry.unix_seconds)
                    {
                        let tip = entry
                            .reference
                            .strip_prefix("refs/heads/")
                            .and_then(|n| snapshot.manifest.refs.get(n))
                            .or_else(|| {
                                entry
                                    .reference
                                    .strip_prefix("refs/tags/")
                                    .and_then(|n| snapshot.manifest.tags.get(n))
                            });
                        let key = if entry.reference == "HEAD" {
                            "HEAD"
                        } else {
                            tip.map_or("", String::as_str)
                        };
                        if !reachability.contains_key(key) {
                            let selected_roots: Vec<String> = if key == "HEAD" {
                                roots.iter().cloned().collect()
                            } else {
                                tip.cloned().into_iter().collect()
                            };
                            reachability.insert(
                                key.to_owned(),
                                reachable_from_roots(selected_roots, &snapshot.objects)?,
                            );
                        }
                        let reachable = &reachability[key];
                        entry
                            .old
                            .iter()
                            .chain(&entry.new)
                            .any(|id| !reachable.contains(id))
                    } else {
                        false
                    };
                if expired || unreachable {
                    removed.push(json!({"log":path,"entry":entry}));
                } else {
                    kept.push(entry);
                }
            }
            if kept.len() != before {
                if request.rewrite {
                    rewrite_chain(&mut kept, &rewritten);
                }
                plans.push((origin, path, crate::binary::encode(&kept)?));
            }
        }
        if !request.dry_run {
            for (origin, path, bytes) in &plans {
                origin.atomic_write(path,bytes).map_err(|e| format!("reflog expiry may be partly complete; references and working files were preserved: {e}"))?;
            }
        }
        Ok(
            json!({"dry_run":request.dry_run,"expired":removed.len(),"logs_changed":plans.len(),"removed":removed,"references_changed":false}),
        )
    }

    pub fn delete_reflog_entry(
        &self,
        selector: &str,
        dry_run: bool,
        rewrite: bool,
    ) -> RepositoryResult<Value> {
        let (name, ordinal) = selector
            .strip_suffix('}')
            .and_then(|s| s.rsplit_once("@{"))
            .ok_or("reflog delete requires REF@{N}")?;
        let ordinal = ordinal
            .parse::<usize>()
            .map_err(|_| "invalid reflog ordinal")?;
        let _lock = self.lock()?;
        let snapshot = self.snapshot()?;
        self.reflog_mutation_origins()?;
        let name = reference(&snapshot, if name.is_empty() { "HEAD" } else { name })?;
        let (path, mut entries) = if name == "HEAD" {
            (self.metadata.join("head-reflog.bin"), snapshot.head_reflog)
        } else {
            (self.common_metadata.join("reflog"), snapshot.reflog)
        };
        let index = entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| e.reference == name)
            .nth(ordinal)
            .map(|(i, _)| i)
            .ok_or("reflog ordinal is unavailable")?;
        let removed = entries.remove(index);
        if rewrite {
            rewrite_chain(&mut entries, &BTreeSet::from([name]));
        }
        if !dry_run {
            self.atomic_write(&path, &crate::binary::encode(&entries)?)?;
        }
        Ok(json!({"dry_run":dry_run,"removed":removed,"references_changed":false}))
    }
}
