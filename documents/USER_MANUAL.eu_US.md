# uwgit User Manual

**Edition:** 2026-11-06 · [简体中文](USER_MANUAL.zh_CN.md) · [Library](README.eu_US.md)

## 1. Conventions and prerequisites

Examples use the `wasm-git` client from a client-enabled build. `DIR` is an uwgit
repository directory, not its `.uwgit` metadata subdirectory. File arguments for
standalone audit refer to ordinary files; staged tree paths are relative to `DIR`.
`REV` is a supported revision expression, such as a branch, tag, full commit ID,
`HEAD`, or `HEAD~1`. Names such as `TARGET_ID` and `REVIEWED_HEAD_ID` are placeholders
for the exact IDs returned by your operation.

Do not assume that every Git option has an uwgit equivalent. Run the particular
command with `--help` to inspect its accepted arguments. Scripts should check the
exit status, inspect structured results, and retain diagnostics; a JSON field
that reports incomplete interpretation is not the same as command failure.

Build instructions are in the [project overview](../README.eu_US.md). Put the
chosen executable on `PATH`, or replace `wasm-git` below with its full path. Place
scratch repositories, reports, exported modules, and test output under `.tmp/`
when working in this source checkout.

## 2. Inspect one module

```sh
wasm-git inspect app.wasm --json
wasm-git inspect app.wasm --standard core3 --json
wasm-git features --json
wasm-git features threads --standard core3 --json
```

The physical audit describes the original header, sections, nested known regions,
offsets, diagnostics, and validation result. Standard profiles are `core1`,
`core2`, `core3`, and `all-known`; the default is `all-known`. Profile selection
changes eligibility reporting, not original bytes or the length of recognized
immediates. A valid module under one profile can fail another.

Storage acceptance is broader than complete semantic interpretation. A Wasm
header with an unknown version/layer can retain an opaque body. Known malformed
encodings are diagnosed. A custom payload that does not match a community
convention remains raw evidence rather than being silently discarded.

## 3. Compare and review evidence

```sh
wasm-git diff old.wasm new.wasm
wasm-git diff old.wasm new.wasm --format json
wasm-git diff old.wasm new.wasm --compact --format json
wasm-git diff old.wasm new.wasm --short
wasm-git --threads 1 diff old.wasm new.wasm --compact --format json
```

Use full JSON for tooling that needs complete region trees and positional
explanations. Compact JSON preserves exact byte edits, validation, diagnostics,
function changes, and non-supported/deprecated feature evidence while recording
what was omitted. Short output is a summary only.

Review a report in this order:

1. Check module digests and input origins to confirm the intended files/revisions.
2. Check physical change status and validation diagnostics.
3. Inspect byte ranges and old/new byte evidence for each relevant change.
4. Examine function/block/instruction alignment and dependency bindings.
5. Check ambiguity, search-completeness, and fallback indicators before assigning
   a unique function identity or describing an edit as minimal.
6. Use debug annotations as supporting compiler metadata, not independent proof
   that a source location or symbol is trustworthy.

`exact_minimal=false` means the byte search used a bounded fallback; it does not
mean restoration is approximate. The script still retains the complete changed
bytes. An unchanged numeric call operand may bind to a different import; consult
call-binding and export-dependency evidence rather than only opcode text.

## 4. Select sections

```sh
wasm-git diff old.wasm new.wasm --section code
wasm-git diff old.wasm new.wasm --section code --section data --format json
wasm-git diff old.wasm new.wasm --section custom
wasm-git diff old.wasm new.wasm --section custom:producers
wasm-git diff old.wasm new.wasm --section id:200 --section ordinal:3
```

Selectors form a union. Core names include `type`, `import`, `function`, `table`,
`memory`, `global`, `export`, `start`, `element`, `code`, `data`, `data_count`, and
`tag`. `custom:NAME` matches the exact UTF-8 name, not a glob. `id:N` accepts a
section ID from 0 to 255. `ordinal:N` selects the zero-based physical ordinal
independently on each side. `opaque` and `all` are also accepted.

Component outer selectors include `core_module`, `core_instance`, `core_type`,
`component`, `component_instance`, `alias`, `component_type`, `canonical`,
`component_start`, `component_import`, and `component_export`. Nested contents
remain raw payload rather than recursively inferred Core correspondences.

Check `selection_complete` and `unmatched_selectors`. An incomplete name or
boundary scan cannot prove that an unobserved section is absent. Distinguish
`content_changed`, `payload_changed`, and `position_changed`. The header is not
part of any section. A selected report has `whole_module_patch=false` and does
not calculate cross-section dependencies. Use an unfiltered diff and a complete
patch when reviewing an entire update.

## 5. Map debug information to the original binary

```sh
wasm-git debug-map app.wasm --offset 173 --json
wasm-git debug-map app.wasm --code-offset 12 --json
wasm-git debug-map app.wasm --symbol calculate --json
wasm-git debug-map app.wasm --function 3 --json
wasm-git debug-map app.wasm --source /source/module.cpp --line 70 --json
```

Use one selector per query. `--offset` is an absolute file byte offset;
`--code-offset` uses the code-section coordinate convention reported by the
mapping. Function indices include imports when a global index is established.
Source paths and symbol names are exact recorded names, so path remapping during
compilation changes the query string.

The result can contain line ranges, inline call sites, declaration information,
supported variable locations, and original instruction spans. Check the module
SHA-256, coordinate basis, status, diagnostics, and range interpretation. Missing
line tables, unsupported expressions, and conflicting sections are not resolved
by executing Wasm or fetching an external debug URL. Full diff automatically
attaches available mappings; absent mappings do not suppress byte evidence.

## 6. Create and apply a binary patch

```sh
wasm-git patch old.wasm new.wasm --output update.uwgp
wasm-git apply old.wasm update.uwgp --check
wasm-git apply old.wasm update.uwgp --output restored.wasm
```

The source must match the patch's exact expected bytes. A valid target is
published as a new file only after verification; existing destinations are not
overwritten. `--check` verifies and audits without writing a target. A reverse
patch is created by swapping the old/new inputs in `patch`.

Committed input can be read from one repository snapshot:

```sh
wasm-git patch main topic --repo ./history --file modules/app.wasm --output update.uwgp
wasm-git apply main update.uwgp --repo ./history --file modules/app.wasm --check
```

`--file` selects a path in a named tree. Patch creation/application does not
commit or update references. JSON reports are review output; the `.uwgp` file
contains binary restoration data.

## 7. Start a single-module history

```sh
wasm-git init ./history
wasm-git commit ./history old.wasm -m 'Initial module'
wasm-git commit ./history new.wasm -m 'Change calculation'
wasm-git log ./history HEAD
wasm-git inspect HEAD --repo ./history --json
wasm-git diff HEAD~1 HEAD --repo ./history --format json
wasm-git checkout ./history HEAD restored.wasm
wasm-git fsck ./history
```

A positional module commit records one original binary. Export checkout requires
a new output path. Keep this form distinct from branch checkout and path restore.
A repository can also represent named trees; explicit path information is needed
when assigning a previously unnamed module to a tree.

## 8. Track a module tree

```sh
wasm-git init ./work
mkdir -p ./work/modules
cp app.wasm ./work/modules/app.wasm
cp library.wasm ./work/modules/library.wasm
wasm-git add ./work modules/app.wasm modules/library.wasm
wasm-git status ./work
wasm-git diff ./work --cached --format json
wasm-git commit ./work -m 'Track application and library'
wasm-git ls-tree ./work HEAD --json
wasm-git show ./work HEAD --file modules/app.wasm --output exported.wasm
```

The index is the proposed complete tree. A commit without a positional module
records that index. Empty trees, additions, and deletions are legitimate history.
Directories are implicit, and rename evidence is represented as removal plus
addition rather than an invented identity.

`add`, `rm`, `mv`, `restore`, and path checkout use repository-relative path
selection. Exact paths, directory prefixes, and supported pathspec forms have
explicit matching rules. A missing match or file/directory obstruction can reject
a whole planned operation before writes. Untracked files are protected where
workspace restoration would otherwise overwrite them.

```sh
wasm-git diff ./work --format json
wasm-git diff ./work --cached --section code
wasm-git restore ./work --staged -- modules/app.wasm
wasm-git restore ./work --source HEAD --staged --worktree
```

The first diff compares worktree and index; `--cached` compares index and HEAD.
Unstaging preserves working bytes. Restoring tracked worktree content discards
those selected local changes; inspect status and preserve needed bytes first.

## 9. Branches, tags, and workspace navigation

```sh
wasm-git branch ./work topic HEAD
wasm-git switch ./work topic
wasm-git branch ./work -vv
wasm-git tag ./work release-candidate
wasm-git switch ./work main
wasm-git checkout ./work -b experiment HEAD
wasm-git checkout ./work HEAD -- modules/app.wasm
```

Branch checkout and `switch` move the active branch subject to workspace and
worktree protections. Path checkout restores selected paths. The three-positional
argument export form writes a new file and has a separate purpose. Detached HEAD
and orphan branches are supported through explicit switch options; do not infer
that every Git checkout option is accepted.

Linked worktrees share objects and references but have independent HEAD/index
state. `worktree add/list/remove/move/lock/unlock/prune/repair` manage them; branch
occupancy prevents conflicting writes from another worktree. A fork is instead
an independent repository with shared immutable data.

For scripted reference changes, `update-ref --stdin` supports atomic batches and
`--batch-updates` reports individual semantic rejections. Two mutually conflicting
new names are both rejected, independent of input order. Symbolic HEAD updates
require `--no-deref`. A zero old object ID means the selected reference must not
exist: under `--no-deref`, an unborn symbolic HEAD still exists and fails that
lease. Use `symref-update HEAD NEW_TARGET ref OLD_TARGET` to retarget it with an
exact symbolic-target lease. Without `--no-deref`, `verify HEAD ZERO_ID` checks
the resolved branch's absence instead. Pointer-only transactions preserve the
index and working files.

## 10. Merge and resolve conflicts

```sh
wasm-git merge-base ./work main topic
wasm-git merge ./work topic -m 'Integrate topic'
wasm-git merge ./work topic --ff-only
```

Automatic merge is conservative. Independent function changes and supported
instruction changes may compose; overlapping or unproven transformations return
conflict evidence. The result is not a proof of behavioral correctness.
`--ff-only` refuses divergent history; `--no-ff` requests a merge commit where
supported; `--squash` prepares an index without publishing a merge parent.

For a recorded conflict workflow:

```sh
wasm-git merge ./work topic -m 'Integrate topic' --record-conflict
# Review conflict reports, edit module files, and stage the complete resolution.
wasm-git add ./work modules/app.wasm
wasm-git diff ./work --cached --format json
wasm-git merge ./work --continue --resolved-index
```

Review the entire index, including non-conflicting paths. Recorded workspace
operations prepare stage 0 and conflict stages where applicable. An unresolved
index cannot be published as a tree. `--abort` restores the recorded checkpoint
under its safety checks; `--quit` discards sequencing state while retaining current
files. Their effects are not interchangeable.

A stateless, explicitly reviewed resolution binds immutable IDs:

```sh
wasm-git merge ./work TARGET_ID -m 'Reviewed resolution' \
  --resolved-index --expect-head REVIEWED_HEAD_ID --expect-branch main
```

The command uses the existing complete index and checks the expected HEAD. It
must not be used to assume that unstaged files or an incomplete index constitute
a reviewed merge result. External `mergetool` exports are review artifacts, not
internal repository state.

## 11. Replay, undo, and history investigation

```sh
wasm-git cherry-pick ./work CHANGE_ID --record-conflict
wasm-git revert ./work CHANGE_ID --record-conflict
wasm-git rebase ./work main --record-conflict
wasm-git reflog ./work
wasm-git trace ./work HEAD --file modules/app.wasm --range 40:48
wasm-git blame ./work HEAD --file modules/app.wasm --range 40:48
```

Pick/revert support frozen multi-target queues and explicit continuation, skip,
abort, and quit operations. A merge commit requires a selected `--mainline` for
replay. Rebase supports linear replay, not interactive editing or general
merge-topology preservation. Pending state records immutable source IDs and
recovery checkpoints; concurrent changes may require reinspection rather than
blind continuation.

`reset --soft` moves the branch while retaining index and working files;
`--mixed` also resets the index; `--hard` restores tracked working content as
well. Hard reset is destructive to selected tracked changes. Stash preserves
supported tracked state, not every untracked file. Revert adds a new reverse
commit and preserves the old history.

Trace and blame report byte-range ancestry candidates and frontier boundaries
under explicit budgets. Repeated bytes do not establish a unique origin. Other
inspection commands include `rev-list`, `show-ref`, `for-each-ref`, `shortlog`,
`range-diff`, `describe`, `archive`, and non-checkout `bisect`. They use uwgit's
actual object and history model; consult command help for each option subset.

Reference events include UTC Unix timestamps. `reflog DIR` retains the shared
event view; `reflog DIR show [REF]` shows a selected log newest first (default
`HEAD`). `list` lists recorded names and `exists REF` uses exit status 0/1.
`rev-parse DIR 'HEAD@{2026-11-01T00:00:00Z}'` selects the most recent recorded
event at or before that time. Numeric selectors remain ordinals. Dates use UTC;
`@UNIX_SECONDS`, `now`, `yesterday`, and numeric relative ages such as
`7.days.ago` are also accepted. Expired or absent evidence is reported, not guessed.

## 12. Clone, fetch, push, and offline exchange

```sh
wasm-git clone ./history ./peer
wasm-git bundle ./peer peer.uwgb
wasm-git unbundle ./history peer.uwgb incoming
wasm-git remote ./peer add origin https://host.example/project
wasm-git fetch ./peer origin
wasm-git push ./peer origin main:main
wasm-git push ./peer origin --tags
```

Unbundle updates the explicitly selected branch, subject to validation and
fast-forward rules. Fetch installs verified objects and tracking state without
moving local branches or changing the workspace. Pull adds integration and may
conflict. A full clone preserves advertised branches and tags; tree files can
then be materialized with `restore DIR --source HEAD --staged --worktree`.

HTTP credentials are supplied through `UWGIT_REMOTE_TOKEN` and are not saved in
remote configuration. Use HTTPS off loopback; insecure remote HTTP requires
explicit opt-in. SSH URLs use the installed remote server and system account
policy. Server administration is documented separately.

A non-fast-forward push requires server permission and an explicit force request:
`--force-with-lease OLD_ID`, `--force`/`-f`, `+SOURCE:DESTINATION`, or `--mirror`.
Every form retains an exact compare-and-swap lease from the advertisement;
`--force-with-lease` additionally checks the caller's supplied old ID.
`push DIR origin main` publishes the same named branch; `:topic` deletes a ref.
`--all` publishes local branches, `--tags` local tags, and `--mirror` synchronizes
the native branch/tag sets, including deletions and forced replacements. Deleting
remote HEAD remains prohibited. `--all -u` saves upstreams only after confirmation.

Push mappings also accept a single `*` in both full source/destination refs,
source-only patterns (the destination is identical), and negative destination patterns
prefixed with `^`. Quote patterns so the shell does not expand them. `:` pushes
branches already present on both sides; `+:` additionally permits forced updates.
`--prune` removes absent sources only inside the selected destination mapping.
Negative push patterns match the full destination name after mapping, as in Git;
excluded destinations are also protected from pruning. With the default single-branch push it does not delete unrelated branches;
`--all --prune` selects all branches. `--branches` is an alias for `--all`.
Mapped `-u` updates the original source branch's upstream after acknowledgement;
one source cannot bind to multiple destinations in the same request.

Multiple refspecs without `--atomic` return per-reference results from one batch;
`--atomic` makes rejection of one update prevent the entire publication.
`--dry-run` previews server validation without object/ref installation or hooks;
hook approval can still reject the later real push. Repeat `-o VALUE` to pass
literal bounded push options to administrator hooks. For example:

```sh
wasm-git push ./peer origin main:main topic:review --atomic -o review=ready
wasm-git push ./peer origin --mirror --dry-run
wasm-git push ./peer origin 'refs/heads/release/*:refs/heads/public/*' '^refs/heads/public/private/*' --prune --dry-run
```

After an ambiguous network failure, query remote refs before assuming the request
failed or retrying with a changed lease.

## 13. Shallow history and partial content

```sh
wasm-git clone SOURCE ./partial --depth 10 --branch main --filter blob:none
wasm-git clone-status ./partial
wasm-git fetch ./partial --deepen 20
wasm-git fetch ./partial --unshallow
wasm-git fetch ./partial --filter all
wasm-git hydrate ./partial
```

`SOURCE` can be a local repository or supported HTTP(S)/SSH URL. Shallow boundaries
retain real parent IDs. Deepening extends from the prior boundary; unshallowing
can obtain only history the source actually has. `blob:none` omits Wasm content;
`blob:limit=N` retains content up to N bytes; `path:PATH[,PATH]` retains selected
paths or directory prefixes; `all` requests full content.

Promised content is reported separately by `fsck`. Object-dependent operations
hydrate required modules and verify them before use; read-only graph traversal
does not indiscriminately download everything. `promisor DIR SOURCE_A SOURCE_B`
configures ordered fallback sources. Missing unpromised objects remain corruption,
not an implicit promise.

## 14. Sign and verify commits

```sh
wasm-git keygen author.key author.pub
wasm-git trust-author trusted-authors.bin 'Alice <alice@example.org>' author.pub
wasm-git commit-signed ./history app.wasm -m 'Reviewed change' \
  --author 'Alice <alice@example.org>' --key author.key
wasm-git verify-commit ./history HEAD --trust trusted-authors.bin
wasm-git sign-commit ./work EXPECTED_HEAD_ID \
  --author 'Alice <alice@example.org>' --key author.key
```

Keys use uwgit's binary Ed25519 format, not SSH or OpenPGP key encoding. Keep
private keys outside source control. Verification distinguishes cryptographic
validity from the author/public-key binding in the supplied trust table. A signed
timestamp is the signer's declaration, not independently trusted time.
`sign-commit` creates a new immutable signed commit and requires the expected
HEAD, including for tree commits; the previous object is not modified in place.
`untrust-author` can remove one key or all keys for an author.

## 15. Compression, forks, and object pools

```sh
wasm-git gc ./history -n
wasm-git gc ./history
wasm-git repack ./history
wasm-git fork ./history ./fork
wasm-git fsck ./fork
wasm-git gc ./history --prune=now
```

Dry-run GC produces a plan. Ordinary GC/repack compress history and retain
objects; explicit pruning removes only objects outside retention roots. A truly
pruned object cannot be restored without another retained copy or backup.
Shared base packs and cross-pack deltas preserve original object IDs. A local
fork requires a new destination and a compatible same-filesystem hard-link path;
a failed fork must be inspected before the destination is reused.

Reflogs retain abandoned history until explicitly expired. Preview and then apply
the same retention policy before pruning objects:

```sh
wasm-git reflog ./history expire --all --expire=90.days.ago --expire-unreachable=30.days.ago -n
wasm-git reflog ./history expire --all --expire=90.days.ago --expire-unreachable=30.days.ago
wasm-git gc ./history --prune=now
```

Expiry is strictly older than the cutoff. `all` removes entries regardless of
age; `never` disables that criterion. Unreachable expiry follows the selected
branch/tag tip; HEAD uses the set of current local/tracking tips and detached
worktree heads. Both old and new endpoints must be reachable. `--all` includes
registered worktree HEAD logs; `--single-worktree` limits private HEAD-log processing.
All selected logs are validated before the first rewrite. Pending history recovery
in any registered worktree blocks mutation. Files are replaced atomically per log;
an I/O failure can leave a safe, explicitly reported partial expiry, with refs and
working bytes unchanged. Linked worktrees must remain available for validation.

`--rewrite` reconnects retained old/new chains for selected logs only.
`reflog DIR delete 'main@{1}' --rewrite [-n]` removes one entry without moving
the branch. `write REF OLD_ID NEW_ID MESSAGE` appends a validated manual record;
use full reference names and full native commit IDs (64 zeroes for absence).
`maintenance DIR run --task=reflog-expire` applies the 90/30-day policy; default
maintenance runs verify, expiry, then GC. Ordinary `gc` alone does not expire logs.

```sh
wasm-git pool attach ./history https://node-a.example/pool https://node-b.example/pool
wasm-git pool sync ./history
wasm-git pool status ./history
wasm-git pool sync ./history --evict
wasm-git pool release ./history
```

Use pools only after reading the administration manual. Eviction waits for all
configured replicas to confirm owner pins. Release hydrates required local
content before removing the owner's remote pins. These are storage-management
operations, not a substitute for independent backups or reference consensus.

## 16. Troubleshooting and interpretation limits

- **Existing destination:** choose a new patch/export/clone/fork path. Do not
  remove a useful file merely to satisfy a command.
- **Busy or stale repository lock:** check all writers and linked worktrees.
  Remove a persistent writer/index lock only after confirming no operation owns
  it. Do not delete an active object-pool lock inode.
- **Unknown grammar or disabled feature:** inspect the raw audit, selected
  standard, and feature coverage; changing a display profile does not implement
  missing semantics.
- **Promised object unavailable:** restore access to an authorized source or add
  an explicit fallback; do not substitute an empty module.
- **Lease or workspace mismatch:** fetch/query current state and review again.
  A stale expected ID is evidence that the review no longer binds current state.
- **Incomplete trace or alignment:** examine the reported frontier and budget.
  Exact byte restoration and complete explanatory correspondence are independent.

For deployment limits and backup recovery, see
[server administration](SERVER_ADMINISTRATION.eu_US.md). For exact storage
framing and version boundaries, see [binary formats](BINARY_FORMAT.eu_US.md).
