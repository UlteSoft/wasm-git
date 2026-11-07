//! Durable PR snapshots, exact merge leases, recovery evidence and GC roots.
use super::*;
use crate::pull::*;
const MAGIC: &[u8] = b"WGTPR001";
const MAX_RECORD: usize = 16 * 1024 * 1024;

fn text(value: &str, limit: usize, empty: bool) -> RepositoryResult<()> {
    if (!empty && value.trim().is_empty()) || value.len() > limit || value.contains('\0') {
        return Err("pull request text is empty, contains NUL, or exceeds its size limit".into());
    }
    Ok(())
}
fn validate(p: &PullRecord) -> RepositoryResult<()> {
    if p.schema != 1
        || p.number == 0
        || p.version == 0
        || p.events.len() > 4096
        || p.revisions.len() > 4096
    {
        return Err("invalid pull request version, number, or history bounds".into());
    }
    crate::protocol::repository_name(&p.source_repository)?;
    validate_ref_name(&p.source_branch)?;
    validate_ref_name(&p.base_branch)?;
    text(&p.author, 128, false)?;
    text(&p.title, 256, false)?;
    text(&p.body, 65_536, true)?;
    require_id(&p.base)?;
    require_id(&p.head)?;
    if p.revisions
        .last()
        .is_none_or(|r| r.base != p.base || r.head != p.head)
    {
        return Err("pull request revision history is inconsistent".into());
    }
    for r in &p.revisions {
        require_id(&r.base)?;
        require_id(&r.head)?;
    }
    for e in &p.events {
        text(&e.actor, 128, false)?;
        require_id(&e.base)?;
        require_id(&e.head)?;
        match &e.kind {
            PullEventKind::Created { title, body } | PullEventKind::Updated { title, body } => {
                text(title, 256, false)?;
                text(body, 65_536, true)?;
            }
            PullEventKind::Comment { body } | PullEventKind::Review { body, .. } => {
                text(body, 65_536, true)?
            }
            PullEventKind::Check {
                context,
                description,
                ..
            } => {
                text(context, 128, false)?;
                text(description, 4096, true)?;
            }
            _ => (),
        }
    }
    if matches!(p.state, PullState::Merging | PullState::Merged) != p.merge.is_some() {
        return Err("pull request merge state is inconsistent".into());
    }
    if let Some(m) = &p.merge {
        require_id(&m.commit)?;
        text(&m.actor, 128, false)?;
    }
    Ok(())
}
fn event(p: &mut PullRecord, actor: &str, kind: PullEventKind) -> RepositoryResult<()> {
    p.events.push(PullEvent {
        actor: actor.into(),
        timestamp: reflog_storage::reflog_timestamp()?,
        base: p.base.clone(),
        head: p.head.clone(),
        kind,
    });
    p.version = p
        .version
        .checked_add(1)
        .ok_or("pull request version overflow")?;
    Ok(())
}
fn check_version(p: &PullRecord, version: u64) -> RepositoryResult<()> {
    if p.version != version {
        Err("pull request version changed; refresh and review before retrying".into())
    } else {
        Ok(())
    }
}
fn decode_record(path: &Path) -> RepositoryResult<PullRecord> {
    if fs::symlink_metadata(path).map_err(|e| e.to_string())?.len() > MAX_RECORD as u64 {
        return Err("pull record exceeds bounds".into());
    }
    let raw = read_regular(path)?;
    let bytes = raw
        .strip_prefix(MAGIC)
        .ok_or("invalid pull record header")?;
    let len = u64::from_be_bytes(
        bytes
            .get(..8)
            .ok_or("truncated pull length")?
            .try_into()
            .unwrap(),
    );
    if len > MAX_RECORD as u64 {
        return Err("pull record decoded size exceeds bounds".into());
    }
    let data =
        zstd::bulk::decompress(bytes.get(8..).unwrap(), len as usize).map_err(|e| e.to_string())?;
    if data.len() != len as usize {
        return Err("pull record length mismatch".into());
    }
    let p = crate::binary::decode(&data)?;
    validate(&p)?;
    Ok(p)
}

impl Repository {
    fn pulls_directory(&self, create: bool) -> RepositoryResult<PathBuf> {
        let path = self.common_metadata.join("pulls");
        if create && !path.try_exists().map_err(|e| e.to_string())? {
            fs::create_dir(&path).map_err(|e| e.to_string())?;
            sync_directory(&self.common_metadata)?;
        }
        if path.try_exists().map_err(|e| e.to_string())? {
            require_directory(&path)?;
        }
        Ok(path)
    }
    fn pull_ids(&self) -> RepositoryResult<Vec<u64>> {
        let path = self.pulls_directory(false)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid pull filename")?;
            if name == "counter.bin" {
                continue;
            }
            let id = name
                .strip_suffix(".bin")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .ok_or("invalid pull filename")?;
            if name != format!("{id}.bin") {
                return Err("noncanonical pull filename".into());
            }
            ids.push(id);
        }
        ids.sort_unstable();
        Ok(ids)
    }
    fn read_pull(&self, number: u64) -> RepositoryResult<PullRecord> {
        if number == 0 {
            return Err("pull request numbers start at 1".into());
        }
        let p = decode_record(&self.pulls_directory(false)?.join(format!("{number}.bin")))?;
        if p.number != number {
            return Err("pull record number does not match its filename".into());
        }
        Ok(p)
    }
    fn save_pull(&self, p: &PullRecord) -> RepositoryResult<()> {
        validate(p)?;
        let data = crate::binary::encode(p)?;
        if data.len() > MAX_RECORD {
            return Err("pull request history exceeds 16 MiB".into());
        }
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(data.len() as u64).to_be_bytes());
        bytes.extend(zstd::bulk::compress(&data, 3).map_err(|e| e.to_string())?);
        if bytes.len() > MAX_RECORD {
            return Err("compressed pull request exceeds 16 MiB".into());
        }
        self.atomic_write(
            &self
                .pulls_directory(true)?
                .join(format!("{}.bin", p.number)),
            &bytes,
        )
    }
    pub fn pull(&self, number: u64) -> RepositoryResult<PullRecord> {
        let _lock = self.lock()?;
        self.read_pull(number)
    }
    pub fn pulls(&self, after: u64, limit: u16) -> RepositoryResult<PullReply> {
        if !(1..=100).contains(&limit) {
            return Err("pull page size must be 1..=100".into());
        }
        let _lock = self.lock()?;
        let ids: Vec<_> = self
            .pull_ids()?
            .into_iter()
            .filter(|n| *n > after)
            .take(limit as usize + 1)
            .collect();
        let mut records = Vec::new();
        let mut bytes = 0usize;
        let mut next = None;
        for id in ids {
            let p = self.read_pull(id)?;
            let size = crate::binary::encode(&p)?.len();
            // Leave framing room below the 64 MiB packet bound, even when
            // individual records approach their 16 MiB history limit.
            if records.len() == limit as usize
                || bytes.saturating_add(size) > crate::protocol::MAX_PACKET_BYTES / 2
            {
                next = records.last().map(|p: &PullRecord| p.number);
                break;
            }
            bytes += size;
            records.push(p);
        }
        Ok(PullReply {
            records,
            next,
            ..Default::default()
        })
    }
    pub(super) fn pull_roots(
        &self,
        objects: &BTreeMap<String, Object>,
    ) -> RepositoryResult<Vec<String>> {
        let mut roots = BTreeSet::new();
        for id in self.pull_ids()? {
            let p = self.read_pull(id)?;
            for r in &p.revisions {
                roots.extend([r.base.clone(), r.head.clone()]);
            }
            if let Some(m) = &p.merge {
                // A prepared candidate may not be installed until receive.
                if p.state == PullState::Merged || objects.contains_key(&m.commit) {
                    roots.insert(m.commit.clone());
                }
            }
        }
        for root in &roots {
            if !matches!(objects.get(root), Some(Object::Commit(_))) {
                return Err(format!("pull request retains missing commit {root}"));
            }
        }
        Ok(roots.into_iter().collect())
    }
    pub(super) fn pending_pull_merge(&self) -> RepositoryResult<bool> {
        for id in self.pull_ids()? {
            if self.read_pull(id)?.state == PullState::Merging {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// Pins the proposed snapshot even if its fork/branch is later deleted.
    pub fn create_pull(&self, mut p: PullRecord, bundle: &[u8]) -> RepositoryResult<PullRecord> {
        let _lock = self.lock()?;
        let current = self.snapshot()?;
        complete(&current)?;
        let incoming = network_storage::decode_transfer(bundle, Some(&current))?;
        if !incoming.manifest.shallow.is_empty() || !incoming.manifest.promised.is_empty() {
            return Err("pull requests require complete proposed history".into());
        }
        check_collisions(&current, &incoming)?;
        let base = current
            .manifest
            .refs
            .get(&p.base_branch)
            .ok_or("pull base branch does not exist")?;
        if !matches!(
            incoming
                .objects
                .get(&p.head)
                .or_else(|| current.objects.get(&p.head)),
            Some(Object::Commit(_))
        ) {
            return Err("pull head commit is missing".into());
        }
        let directory = self.pulls_directory(true)?;
        let counter = directory.join("counter.bin");
        let prior = match fs::symlink_metadata(&counter) {
            Ok(_) => crate::binary::decode::<u64>(&read_regular(&counter)?)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.to_string()),
        };
        p.number = prior.checked_add(1).ok_or("pull request number overflow")?;
        if directory
            .join(format!("{}.bin", p.number))
            .try_exists()
            .map_err(|e| e.to_string())?
        {
            return Err("pull counter would reuse an existing number".into());
        }
        p.base = base.clone();
        p.version = 0;
        p.schema = 1;
        p.merge = None;
        p.revisions = vec![PullRevision {
            base: p.base.clone(),
            head: p.head.clone(),
        }];
        p.events.clear();
        let actor = p.author.clone();
        let kind = PullEventKind::Created {
            title: p.title.clone(),
            body: p.body.clone(),
        };
        event(&mut p, &actor, kind)?;
        validate(&p)?;
        self.install_snapshot_objects(&incoming)?;
        self.atomic_write(&counter, &crate::binary::encode(&p.number)?)?;
        self.save_pull(&p)?;
        Ok(p)
    }
    /// CAS-protected metadata edit. The caller supplies authenticated policy.
    pub fn edit_pull(
        &self,
        number: u64,
        version: u64,
        edit: impl FnOnce(&mut PullRecord) -> RepositoryResult<PullEventKind>,
        actor: &str,
        refresh: Option<(&str, &[u8])>,
    ) -> RepositoryResult<PullRecord> {
        let _lock = self.lock()?;
        let mut p = self.read_pull(number)?;
        check_version(&p, version)?;
        if p.state == PullState::Merging {
            return Err("pending-merge pull requests require recovery before editing".into());
        }
        let merged = p.state == PullState::Merged;
        let kind = edit(&mut p)?;
        if merged && (!matches!(kind, PullEventKind::Comment { .. }) || refresh.is_some()) {
            return Err("merged pull requests only accept comments".into());
        }
        if let Some((head, bundle)) = refresh {
            let current = self.snapshot()?;
            complete(&current)?;
            let incoming = network_storage::decode_transfer(bundle, Some(&current))?;
            if !incoming.manifest.shallow.is_empty() || !incoming.manifest.promised.is_empty() {
                return Err("pull refresh requires complete history".into());
            }
            check_collisions(&current, &incoming)?;
            if !matches!(
                incoming
                    .objects
                    .get(head)
                    .or_else(|| current.objects.get(head)),
                Some(Object::Commit(_))
            ) {
                return Err("pull head is missing".into());
            }
            let base = current
                .manifest
                .refs
                .get(&p.base_branch)
                .ok_or("pull base branch was deleted")?;
            p.head = head.into();
            p.base = base.clone();
            p.revisions.push(PullRevision {
                base: p.base.clone(),
                head: p.head.clone(),
            });
            self.install_snapshot_objects(&incoming)?;
        }
        event(&mut p, actor, kind)?;
        self.save_pull(&p)?;
        Ok(p)
    }
    pub fn pull_files(
        &self,
        number: u64,
        version: u64,
        against: DiffBase,
    ) -> RepositoryResult<PullReply> {
        let _lock = self.lock()?;
        let p = self.read_pull(number)?;
        check_version(&p, version)?;
        let base = diff_base(self, &p, against)?;
        let old = pull_files_map(self, &base)?;
        let new = pull_files_map(self, &p.head)?;
        let paths: BTreeSet<_> = old.keys().chain(new.keys()).collect();
        let files = paths
            .into_iter()
            .filter(|path| old.get(*path) != new.get(*path))
            .map(|path| PullFile {
                path: path.clone(),
                old: old.get(path).cloned(),
                new: new.get(path).cloned(),
            })
            .collect();
        Ok(PullReply {
            files,
            diff_base: Some(base),
            ..Default::default()
        })
    }
    pub fn pull_file(
        &self,
        number: u64,
        version: u64,
        path: &str,
        old: bool,
        offset: u64,
        against: DiffBase,
    ) -> RepositoryResult<PullChunk> {
        let _lock = self.lock()?;
        let p = self.read_pull(number)?;
        check_version(&p, version)?;
        let revision = if old {
            diff_base(self, &p, against)?
        } else {
            p.head.clone()
        };
        let files = pull_files_map(self, &revision)?;
        let id = files
            .get(path)
            .ok_or("path is not in the selected pull revision")?;
        let Object::Wasm(bytes) = pull_object(self, id)? else {
            return Err("pull file is missing or not materialized".into());
        };
        let start = usize::try_from(offset).map_err(|_| "pull file offset overflow")?;
        if start > bytes.len() {
            return Err("pull file offset exceeds content length".into());
        }
        Ok(PullChunk {
            id: id.clone(),
            total: bytes.len() as u64,
            offset,
            bytes: bytes[start
                ..bytes
                    .len()
                    .min(start.saturating_add(crate::protocol::TRANSFER_CHUNK))]
                .to_vec(),
        })
    }
    pub fn prepare_pull_merge(
        &self,
        number: u64,
        version: u64,
        options: PullMergeOptions<'_>,
        approve: impl FnOnce(&PullRecord) -> RepositoryResult<()>,
    ) -> RepositoryResult<(PullRecord, Vec<u8>)> {
        let PullMergeOptions {
            method,
            message,
            actor,
            signing,
        } = options;
        text(message, 65_536, false)?;
        let _lock = self.lock()?;
        let mut p = self.read_pull(number)?;
        check_version(&p, version)?;
        if p.state != PullState::Open {
            return Err("only ready, open pull requests can be merged".into());
        }
        let snapshot = self.snapshot()?;
        complete(&snapshot)?;
        check_expected(snapshot.manifest.refs.get(&p.base_branch), Some(&p.base))?;
        approve(&p)?;
        if is_ancestor(&p.head, &p.base, &snapshot.objects)? {
            return Err("pull head is already included in its base".into());
        }
        let mut encoded = BTreeMap::new();
        let candidate = if method == MergeMethod::FastForward {
            if !is_ancestor(&p.base, &p.head, &snapshot.objects)? {
                return Err("pull request is not a fast-forward; choose merge or squash".into());
            }
            p.head.clone()
        } else {
            let base = unique_merge_base(&p.base, &p.head, &snapshot.objects)?;
            let parents = if method == MergeMethod::Squash {
                vec![p.base.clone()]
            } else {
                vec![p.base.clone(), p.head.clone()]
            };
            let id = if [&base, &p.base, &p.head].iter().any(
                |id| matches!(snapshot.objects.get(*id),Some(Object::Commit(c)) if c.format==2),
            ) {
                let merged = merge_file_trees(&snapshot, &base, &p.base, &p.head)?;
                if merged.reports.iter().any(|r| r.status == "conflict") {
                    return Err("pull request has file/semantic conflicts; resolve in the source branch and refresh".into());
                }
                check_tree_parents(&snapshot, &parents, merged.legacy_path.as_deref())?;
                prepare_tree_commit(
                    &merged.files,
                    message,
                    &parents,
                    merged.legacy_path.as_deref(),
                    &snapshot.encoded,
                    &mut encoded,
                )?
            } else {
                let mut report = crate::merge::merge(
                    snapshot_wasm(&snapshot, &base)?,
                    snapshot_wasm(&snapshot, &p.base)?,
                    snapshot_wasm(&snapshot, &p.head)?,
                )?;
                if report.status != "clean" {
                    return Err("pull request has semantic conflicts; resolve in the source branch and refresh".into());
                }
                let wasm = stage_tree_object(
                    WASM_KIND,
                    &report.bytes.take().ok_or("missing clean merge bytes")?,
                    &snapshot.encoded,
                    &mut encoded,
                )?;
                let commit = Commit {
                    shallow: false,
                    signature: None,
                    format: 1,
                    tree: None,
                    legacy_path: None,
                    wasm,
                    parents,
                    message: message.into(),
                };
                stage_tree_object(
                    COMMIT_KIND,
                    &encode_commit(&commit)?,
                    &snapshot.encoded,
                    &mut encoded,
                )?
            };
            if let Some((identity, format)) = signing {
                let raw = encoded
                    .get(&id)
                    .or_else(|| snapshot.encoded.get(&id))
                    .ok_or("missing merge commit")?;
                let Object::Commit(mut commit) = decode_object(&id, raw)? else {
                    return Err("merge candidate is not a commit".into());
                };
                signing_storage::sign(&mut commit, identity, format)?;
                stage_tree_object(
                    COMMIT_KIND,
                    &encode_commit(&commit)?,
                    &snapshot.encoded,
                    &mut encoded,
                )?
            } else {
                id
            }
        };
        let known: BTreeSet<_> = snapshot.objects.keys().cloned().collect();
        let mut outgoing = snapshot;
        for (id, raw) in encoded {
            outgoing
                .objects
                .insert(id.clone(), decode_object(&id, &raw)?);
            outgoing.encoded.insert(id, raw);
        }
        outgoing.manifest.refs = BTreeMap::from([("merge".into(), candidate.clone())]);
        outgoing.manifest.tags.clear();
        outgoing.manifest.head = "merge".into();
        outgoing.manifest.format = manifest_format(
            &outgoing.manifest.head,
            &outgoing.manifest.tags,
            &outgoing.objects,
        );
        let keep = reachable_objects(&outgoing.manifest, &outgoing.objects)?;
        outgoing.objects.retain(|id, _| keep.contains(id));
        outgoing
            .encoded
            .retain(|id, _| keep.contains(id) && !known.contains(id));
        let bundle = network_storage::encode_transfer(self, &outgoing, &known)?;
        p.state = PullState::Merging;
        p.merge = Some(PullMerge {
            commit: candidate,
            actor: actor.into(),
            method,
            after_reflog: outgoing.reflog.last().map_or(0, |e| e.sequence),
        });
        event(&mut p, actor, PullEventKind::MergeStarted)?;
        self.save_pull(&p)?;
        Ok((p, bundle))
    }
    /// Completion is proven by a later exact reflog event, including recovery
    /// of the reference transaction journal. Lost replies never imply rollback.
    pub fn recover_pull_merge(&self, number: u64, version: u64) -> RepositoryResult<PullRecord> {
        let _lock = self.lock()?;
        let mut p = self.read_pull(number)?;
        check_version(&p, version)?;
        if p.state != PullState::Merging {
            return Ok(p);
        }
        let snapshot = self.snapshot()?;
        let m = p.merge.as_ref().ok_or("missing merge attempt")?;
        let published = snapshot.reflog.iter().any(|e| {
            e.sequence > m.after_reflog
                && e.reference == format!("refs/heads/{}", p.base_branch)
                && e.old.as_ref() == Some(&p.base)
                && e.new.as_ref() == Some(&m.commit)
        });
        let actor = m.actor.clone();
        let kind = if published {
            p.state = PullState::Merged;
            PullEventKind::Merged
        } else if snapshot.manifest.refs.get(&p.base_branch) == Some(&p.base) {
            p.state = PullState::Open;
            p.merge = None;
            PullEventKind::MergeAborted
        } else {
            return Err("merge outcome is unknown: base moved without the retained exact publication receipt".into());
        };
        event(&mut p, &actor, kind)?;
        self.save_pull(&p)?;
        Ok(p)
    }
}
fn complete(snapshot: &Snapshot) -> RepositoryResult<()> {
    if !snapshot.manifest.shallow.is_empty() || !snapshot.manifest.promised.is_empty() {
        return Err("pull request hosting requires a complete target repository".into());
    }
    Ok(())
}
fn diff_base(repo: &Repository, p: &PullRecord, against: DiffBase) -> RepositoryResult<String> {
    match against {
        DiffBase::Target => Ok(p.base.clone()),
        DiffBase::MergeBase => {
            // Ancestry needs commits only. Do not unpack every historical Wasm
            // body just to list paths or serve a single review content slice.
            let mut objects = BTreeMap::new();
            let mut pending = vec![p.base.clone(), p.head.clone()];
            let shallow = repo.partial_state()?.shallow;
            while let Some(id) = pending.pop() {
                if objects.contains_key(&id) {
                    continue;
                }
                let Object::Commit(commit) = pull_object(repo, &id)? else {
                    return Err("pull ancestry contains a non-commit".into());
                };
                if shallow.contains(&id) {
                    return Err("pull comparison requires complete ancestry".into());
                }
                pending.extend(commit.parents.iter().cloned());
                objects.insert(id, Object::Commit(commit));
            }
            unique_merge_base(&p.base, &p.head, &objects)
        }
    }
}
fn pull_files_map(repo: &Repository, id: &str) -> RepositoryResult<BTreeMap<String, String>> {
    let Object::Commit(commit) = pull_object(repo, id)? else {
        return Err("pull revision is not a commit".into());
    };
    if let Some(tree) = &commit.tree {
        let Object::Tree(tree) = pull_object(repo, tree)? else {
            return Err("missing pull tree".into());
        };
        Ok(tree.entries.clone())
    } else {
        Ok(BTreeMap::from([("".into(), commit.wasm.clone())]))
    }
}
fn pull_object(repo: &Repository, id: &str) -> RepositoryResult<Object> {
    // Retain frame/CRC/delta checks and verify the final original envelope
    // once, rather than decoding and hashing each packed result twice.
    decode_object(id, &repo.read_network_envelope(id)?)
}
