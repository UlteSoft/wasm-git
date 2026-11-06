# Git Workflow Parity and Remaining Work

**Review date:** 2026-11-06 · [简体中文](GIT_PARITY.zh_CN.md) · [Library](README.eu_US.md)

## Scope

uwgit retains its native Wasm objects, binary control records, and network protocol.
The comparison concerns usable version-control and hosting workflows, rather than
Git object/wire compatibility. A command name alone does not establish parity:
publication, rejection, recovery, and original-byte preservation must work.
**The implementation does not yet cover every Git command, option, or server facility.**

## Implemented hosting contracts

- Authenticated HTTP/SSH, binary principal/repository/write-ref policy, read-only
  operation, exact old-value leases, fast-forward enforcement and force grants.
- Native Ed25519, standard SSHSIG and OpenPGP commit signing, exact author trust,
  current-certificate revocation enforcement and signed receive/merge admission.
- Durable cross-fork PRs with binary compressed records, review/check gates,
  protected branches, original-byte section/function diffs, fast-forward/merge/
  squash publication, exact snapshot leases, GC retention and lost-ack recovery.
  See [WasmHub integration](WASMHUB.eu_US.md).
- Atomic multi-ref publication and ordinary batches with individual results.
  One negotiated bundle is decoded per batch. Accepted refs share one durable
  decision; graph/system errors and pre-receive rejection stop the request.
- Pre-receive, update, post-receive, post-update, bounded literal push options,
  binary audit with per-ref reasons, and request-digest receipts. Lost or corrupt
  acknowledgements leave all sent updates uncertain.
- Same-name, explicit and multiple refspecs; empty-source deletion; `--tags`,
  `--all`, native branch/tag `--mirror`, `--force`, `+` refspecs, and explicit
  `--force-with-lease`. Force and mirror still use exact advertised leases.
  `--all -u` saves upstreams after confirmation. Dry runs do not publish or run hooks.
- Full-name wildcard push mappings, negative destination selections, matching-branch
  `:`/`+:` and mapped `--prune`, with exclusions protecting mapped destinations.
  Multi-ref `-u` binds confirmed source branches to their actual destinations.
- `team/project` repositories, scoped namespace grants and shared-pack forks.
  HTTP and explicit-root SSH address the same names. Fork refs remain independent.
- Complete/shallow/filtered clone and fetch, deepening/unshallowing, verified lazy
  hydration, paged refs, bounded uploads, negotiated deltas and replicated
  owner-pinned object pools.

See [server administration](SERVER_ADMINISTRATION.eu_US.md). Comparison references:
[Git receive-pack](https://git-scm.com/docs/git-receive-pack),
[push](https://git-scm.com/docs/git-push), and
[namespaces](https://git-scm.com/docs/gitnamespaces).

## Implemented history retention

Timestamped binary reflogs provide selected-log views, date/ordinal revisions,
list/exists/write/delete, age/unreachable expiry, dry runs and chain rewriting.
Expiry validates selected worktree logs before writing and refuses pending history
recovery. It preserves refs and working files; per-log atomic replacement can
leave explicitly reported partial progress on I/O failure. Maintenance provides
90-day general and 30-day unreachable retention. Explicit expiry followed by GC
releases abandoned history; ordinary GC alone does not expire logs.

Zstandard packs, indexes, bounded deltas, cross-pack dependencies, shared fork
bases, repack and explicit unreachable pruning preserve original IDs and bytes.
See the [user manual](USER_MANUAL.eu_US.md),
[Git reflog](https://git-scm.com/docs/git-reflog) and
[Git GC](https://git-scm.com/docs/git-gc).

## Remaining implementation gaps

These items are **not implemented**, not claims of completion:

- SSH CA/KRL signing policy, X.509 and signed tags; hosted account/UI/CI execution,
  issues/releases, inline review threads, CODEOWNERS, merge queues and webhook
  delivery/retry. PR records and check admission do not provide these services.
- Arbitrary refs such as `refs/pull/*`, general symbolic aliases, and namespace
  virtualization inside one physical repository. Repository path namespaces
  are distinct from reference namespaces.
- Hidden-ref advertisement with matching object-access restrictions. Reads are
  repository-level; current ref patterns authorize writes.
- Push certificates, proc-receive delegation, reference-transaction hooks, the
  full configurable receive/upload policy surface, and mutable-ref multi-node
  consensus. Existing author signatures and object pools are distinct capabilities.
- All fetch mappings, remote helpers and remote archives. Push patterns require
  full refnames; mirror covers native branches/tags only.
- Annotated tags; general unsigned author/committer metadata and associated date
  queries; interactive and merge-topology-preserving rebase; all merge strategies;
  dirty-checkout carry-forward; complete sparse-checkout and stash workflows.
- Reflog `--updateref`/`drop`/`--stale-fix`, automatic object-age pruning, external
  FSMonitor/split-index, and the full revision/pathspec/formatting option surface.
- Native equivalents of Git email/text-patch, arbitrary file-mode, submodule and
  full import/export workflows.

## Evidence and format boundary

[PR tests](../tests/server_pulls.rs), [standard signature tests](../tests/external_signatures.rs),
[hosting tests](../tests/server_hosting_cli.rs), [transport tests](../tests/remote_cli.rs),
[maintenance tests](../tests/maintenance_cli.rs), and
[reference recovery tests](../src/repository_ref_transactions.rs) cover real
HTTP/stdio, mixed acceptance, exact leases, hook order, quarantine rejection,
dry-run preservation, shared forks, expiry/GC and interrupted publication.
Finite tests do not prove complete Git parity or universal source-intent recovery.

Local tests use project `.tmp/` through `TMPDIR` and the 64 GiB cgroup. Run logs,
profiles and reference checkouts are excluded from publication. Repository marker
`UWGRPO\0\x03` and audit schema 3 reject superseded pre-release records; immutable
object and pack encodings are unchanged. See the [format reference](BINARY_FORMAT.eu_US.md).
