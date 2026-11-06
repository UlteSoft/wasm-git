# WasmHub Integration: Signing, Pull Requests and Protected Publication

**Edition:** 2026-11-06 · [简体中文](WASMHUB.zh_CN.md) · [Library](README.eu_US.md)

## 1. Backend contract

The shared client/server engine provides native Wasm repository transport,
authenticated principals, shared-storage forks, durable pull requests (PRs),
discussion, reviews, CI results, protected branches, and reviewed publication.
PR operations use the same bounded binary RPC over HTTP(S) or SSH as repository
operations. `wasm-git pr URL --help` lists the client interface. A platform can
consume the Rust `pull` types to implement its own frontend.

SSH transport authentication and SSH commit signing are independent. A transport
principal controls repository access; a commit signature authenticates canonical
commit content against an explicitly managed author trust store. PR approvals
record authenticated hosting principals and the exact reviewed commits.

Git's server transport does not itself provide GitHub PRs. This hosting layer
implements a native review workflow; it does not implement Git's wire protocol
or GitHub's REST API. A complete hosted product still needs account/key enrollment,
review UI, OAuth/SSO, CI scheduling and execution isolation, notifications, TLS,
quotas, backups and operations. Issues, releases, inline review threads, CODEOWNERS,
merge queues, rebase merges, and a webhook delivery/retry service are not implemented.
See [remaining Git workflow gaps](GIT_PARITY.eu_US.md).

## 2. Standard SSH and OpenPGP signatures

`commit-signed`, `sign-commit`, `trust-author` and `untrust-author` accept
`--format native|ssh|openpgp`. Native binary Ed25519 remains the default.
SSH uses the installed `ssh-keygen` implementation of SSHSIG with namespace
`commit@wasm-git`. OpenPGP uses the installed `gpg`, including signing subkeys
under a pinned primary certificate. No signing tools are downloaded at runtime.

```sh
# Use an existing SSH signing key; a .pub file also works with a loaded agent.
wasm-git trust-author authors.bin 'Alice' ~/.ssh/id_ed25519.pub --format ssh
wasm-git commit-signed ./repo app.wasm -m 'Audited update' \
  --author Alice --format ssh --key ~/.ssh/id_ed25519
wasm-git verify-commit ./repo HEAD --trust authors.bin

# Export one OpenPGP public certificate. Use its full primary fingerprint.
gpg --export FULL_PRIMARY_FINGERPRINT > alice.pgp
wasm-git trust-author authors.bin Alice alice.pgp --format openpgp
wasm-git commit-signed ./repo app.wasm -m 'Audited update' \
  --author Alice --format openpgp --key FULL_PRIMARY_FINGERPRINT
```

OpenPGP selectors must be full 40- or 64-hex-digit fingerprints, optionally ending
in `!` to require the selected key. Verification requires one valid detached
signature with the expected primary identity. It rejects invalid, expired or
revoked signatures/keys. Verification uses a temporary isolated public keyring,
disables network key retrieval and agent autostart, and does not inherit the
user's GPG configuration or trust database. Tool output is bounded to 1 MiB per
stream and execution to 30 seconds per invocation. Signing uses the user's keyring
and already available agent; interactive passphrase prompts are disabled.

Trust is an exact author/key binding, independent of GPG ownertrust. Import a
refreshed public certificate using `trust-author` to apply new revocations or
subkeys. The current trusted certificate overrides the older embedded certificate.
`untrust-author authors.bin Alice` removes all Alice bindings; `--public KEY
--format FORMAT` removes one. An administrator must distribute trust updates.
External SSH allowed-signers policy files, SSH CA/KRL policy, X.509 signatures,
signed tags and push certificates are not implemented.

Signatures cover the author, declared timestamp, key identity, canonical unsigned
commit, original content IDs, ordered parents and message. They use uwgit-specific
domains and cannot be substituted for Git signatures. Both external formats store
binary public material and binary signatures, with no ASCII armor in commit
objects. Private key material is never stored in the repository.

Ordinary history loading and `fsck` check external signature framing and object
integrity without executing key tools. Run `verify-commit` for cryptographic
verification; supply `--trust` for author binding. `--signed-authors authors.bin`
enforces both on newly published history, including previously stored dangling
objects and PR imports. Existing public ancestors need not be rewritten. Native
Ed25519 verification remains in-process during object validation.

Trust files now use positional schema version 2, including public certificates.
Rebuild earlier pre-release trust files with `trust-author`; no migration of the
unreleased version 1 trust store is provided. Existing immutable native signed
commits retain their encoding and identity.

## 3. Create and audit a proposal

```sh
wasm-git pr https://wasm.example/team/project create \
  --source-repository alice/fork --source-branch feature --base-branch main \
  --title 'Correct the bounds check' --body 'Review the affected function' --draft
wasm-git pr https://wasm.example/team/project list --limit 30
wasm-git pr https://wasm.example/team/project show 1
wasm-git pr https://wasm.example/team/project files 1 --version 1
wasm-git pr https://wasm.example/team/project diff 1 --format json --section code
```

Creating a PR requires target read access, source branch write access and a
writable server. It deliberately shares the captured source history with target
readers. The source branch and target branch must exist. Hosting requires complete
history and materialized content. A PR pins its base/head IDs and imports their
verified binary object graph. Later source pushes do not silently change a reviewed
proposal. Use `update NUMBER --version VERSION --refresh` to capture the new head
and current target together. Refresh requires source write access again.

The default diff compares the unique common ancestor to the proposed head, like
a three-dot review. `--against target` compares the frozen target and head directly.
`files` accepts the same option and reports its actual `diff_base`. If there is no
unique common ancestor, the default diff fails explicitly; target comparison is
still available. Merge never guesses among ambiguous ancestors.

For multiple files, select `diff NUMBER --file path/module.wasm`; legacy single
module repositories use the empty path. Content is fetched in 1 MiB binary slices,
guarded by the PR version and original object hash. The client verifies complete
bytes before invoking the regular section/function/block/instruction audit.
Added/deleted files show an inspection of their present side, with no fabricated
empty Wasm counterpart. They honor `--section` and retain exact selected byte
evidence with original spans; unfiltered JSON retains the whole present file.
`--compact` removes explanatory region trees while keeping byte evidence.
JSON is an output format only.

## 4. Discussion, review and CI

Every mutation after creation requires the currently displayed `--version`.
A concurrent edit rejects stale versions instead of overwriting another person's
work. Events retain the authenticated actor, timestamp and base/head pair;
creation and metadata-update events also retain the title and body at that time.

```sh
# Refresh show output after each mutation and use the returned version.
wasm-git pr URL ready 1 --version VERSION
wasm-git pr URL comment 1 --version VERSION --body 'Reviewed the byte evidence'
wasm-git pr URL review 1 --version VERSION --decision approve --body 'Looks correct'
wasm-git pr URL check 1 --version VERSION --context build --state success
wasm-git pr URL merge 1 --version VERSION --head HEAD_ID --base BASE_ID \
  --method merge -m 'Merge the reviewed change'
```

Readers may comment; authors and target branch writers may edit metadata,
close/reopen or switch draft/ready state. Only target branch writers can submit
reviews or check results. Self-approval is rejected. Merged PRs accept comments
but cannot be reopened or edited. Pending merges require recovery before edits.

Approval uses each eligible reviewer's latest non-comment decision for the exact
current base/head pair. Any current request-changes decision blocks merging.
Changing either commit invalidates prior reviews and checks for admission, while
preserving their historical events. Changing only title/body does not invalidate
an identical reviewed pair. Reviewer permissions are checked again at merge time.
Checks use the latest eligible event for a context and pair; pending/failure is
not success. Check reporting does not execute a job: a CI service performs that
work and authenticates when reporting its result.

## 5. Branch protection and signed merges

Compile an external administrator-supplied JSON document into binary policy:

```json
{"version":1,"rules":[{"repository":"team/*","branches":["refs/heads/main"],
"required_reviews":1,"required_checks":["build"],
"check_issuers":{"build":["ci"]},"methods":["FastForward","Merge","Squash"]}]}
```

```sh
wasm-git-server compile-protection protection-input.json protection.bin
wasm-git-server --root /srv/uwgit --policy principals.bin --allow-write \
  --protection protection.bin --signed-authors authors.bin \
  --merge-author 'Merge service' --merge-signing-format ssh \
  --merge-signing-key /etc/uwgit/merge-key
```

Repository/branch patterns accept an exact value or a final `*`. Branch patterns
use full `refs/heads/` names. All matching rules apply. Every matching rule requires
PR publication, including when the review/check counts are zero. Direct pushes,
forced pushes, ref-only updates and atomic/partial/uploaded batches cannot bypass
protection. Each accepted unprotected ref in a partial batch keeps its individual
result. Local administrators with filesystem write access remain trusted.

An allowed check issuer also needs target branch write permission. Set
`check_issuers` for required contexts so ordinary writers cannot impersonate CI.
Without that restriction, any authorized target branch writer may report a check.
Use separate policy principals for authors, reviewers and CI; a shared token has
one identity and cannot provide independent approvals.

Access and protection policies are loaded at server startup; restart the service
after replacing them. The author trust file is read for each verification/admission,
so an atomic trust update applies to subsequent operations without a restart.

Merge supports fast-forward, a two-parent semantic merge, or a one-parent squash.
The target must still equal the reviewed base. Both requested IDs, PR version,
permissions, method, reviews, checks, graph, signatures and receive hooks must pass.
Conflicts publish no partial output. Merge/squash commits can use the configured
service signing identity, which must be trusted when signed admission is enabled.
Fast-forward preserves the source commit and its signature. Squash does not publish
the source parent chain; admission checks the resulting reachable history.

## 6. Durability, retention and recovery

PR records live in shared repository metadata at `pulls/NUMBER.bin`: `UWGPR001`,
an eight-byte big-endian decoded length, and a Zstandard-compressed positional
binary record. `counter.bin` is a binary monotonically allocated number. Neither
file stores JSON. Updates use the repository writer lock and atomic durable
replacement. Failed creation can leave a number gap, never reuse an issued ID.

Records are bounded to 16 MiB decoded/encoded, 4096 events and 4096 captured
revisions. Titles are at most 256 bytes, bodies/review comments 64 KiB, check
contexts 128 bytes and descriptions 4096 bytes. Listing uses an exclusive `after`
cursor, 1–100 requested records and a 32 MiB response-data budget. It may return
fewer than requested with a continuation cursor. Byte limits count UTF-8 bytes.

All captured revisions, including closed/merged PR history, are GC roots. Deleting
a source branch or fork cannot erase the review evidence. `fsck` verifies PR record
framing and retained commit existence. PR metadata is hosting-local: clone/fork and
object bundles do not export discussions, policies or accounts. Back up the entire
repository metadata plus the external policy/trust configuration. No PR deletion
or archival command is provided; retained review history is intentionally durable.

Publication is prepared as `Merging` before the reference transaction. An exact
later reflog event proves the candidate was published. `pr recover NUMBER
--version VERSION` marks it merged after a lost acknowledgement, even if the target
has advanced again. If the target is unchanged and no publication is recorded,
recovery returns it to open; an inconsistent outcome remains explicit uncertainty.
Reflog mutation is refused while a merge needs this evidence. Post-receive hook
failure does not undo publication. Never interpret a timeout as a rollback.

## 7. Verification and upstream references

Regression tests cover real HTTP policy enforcement, review/check freshness,
cross-fork retention, original-byte diffs, semantic merge/squash, signed admission,
receive hooks, stale versions and lost-ack recovery. External signing tests use
real temporary OpenSSH/GnuPG keys, tampered commit fields, signing subkeys and
revocation. A local isolated `sshd` run additionally exercises host pinning,
forced-command principal binding, clone/push/fork and PR create/diff/merge. Tests
and artifacts run under project `.tmp/` in the configured 64 GiB cgroup.

Reference contracts: [Git signing configuration](https://git-scm.com/docs/git-config),
[OpenSSH signatures](https://man.openbsd.org/ssh-keygen),
[GnuPG input/output](https://www.gnupg.org/documentation/manuals/gnupg/GPG-Input-and-Output.html),
and [GitHub pull requests](https://docs.github.com/en/rest/pulls/pulls).
These guide equivalent workflows, not a claim of API compatibility or complete
Git/GitHub feature parity.
