# wasm-git Server Administration Manual

**Edition:** 2026-11-06 · [简体中文](SERVER_ADMINISTRATION.zh_CN.md) · [Library](README.eu_US.md)

## 1. Deployment model

`wasm-git-server` serves existing wasm-git repositories beneath a configured root.
Repositories can use namespaces such as `team/project` or `team/users/fork`.
Each component is 1–128 ASCII characters, starts with an alphanumeric character,
and otherwise allows letters, digits, `-`, `_`, and `.`. A repository path is
limited to 512 bytes and 16 components. Empty components, traversal, symlinks,
and repositories nested inside another repository are refused.
The service never executes uploaded Wasm.

The server and client share object, audit, graph, transaction, and transport code.
A server-only build does not require a copied source tree:

```sh
cargo build --locked --release --no-default-features --features server
cargo build --locked --release --features server
```

The second form also provides the client used for administrative commands. Use
matching binaries for this pre-release format. Git clients cannot access this
service through the Git wire protocol. A WasmHub-style platform can use it as a
repository backend, but must supply account management, review UI, orchestration,
external TLS, monitoring, and any stronger cross-node reference coordination.

The [WasmHub integration guide](WASMHUB.eu_US.md) documents the implemented PR
backend, reviews/checks, branch protection, standard SSH/OpenPGP signing, signed
merge publication and recovery. It also lists hosting features still absent.

## 2. Initial filesystem layout

A typical installation separates executable code, repositories, and configuration:

- `/usr/local/bin/wasm-git` and `/usr/local/bin/wasm-git-server`: installed binaries.
- `/srv/wasm-git/project/`: one repository, including its `.wasm-git/` metadata.
- `/etc/wasm-git/`: authorization policy, token, and author trust configuration.
- An operator-managed backup destination independent of the live storage volume.

Run the service as a dedicated unprivileged account with write permission only
where required. Initialize repositories as that account. These commands assume
the account and directories have already been provisioned:

```sh
sudo -u wasm-git /usr/local/bin/wasm-git init /srv/wasm-git/project
sudo -u wasm-git /usr/local/bin/wasm-git fsck /srv/wasm-git/project
wasm-git-server --root /srv/wasm-git --listen 127.0.0.1:9419
```

The last command is an illustrative foreground launch and must also run under the
service account. HTTP defaults to loopback and read-only. The service root must
already exist. Source-code `.tmp/` conventions apply to development output, not
to the location of production repositories.

## 3. HTTP authentication and public HTTPS

For a single transport identity, provision a private token file and launch:

```sh
wasm-git-server --root /srv/wasm-git --listen 127.0.0.1:9419 \
  --token-file /etc/wasm-git/token --allow-write
```

The token is 1–4096 printable non-whitespace ASCII bytes; one trailing newline is
accepted in the file. Generate a high-entropy token and restrict file permissions.
When a token is configured, both reads and writes require bearer authentication.
Do not put a real token in source, a service command line, or published examples.
The client reads `WASM_GIT_REMOTE_TOKEN` from its environment and does not persist it
in remote configuration.

Expose public traffic through a TLS reverse proxy that forwards the repository
RPC route to loopback. The native listener does not terminate TLS. Preserve
request/response bodies exactly and configure body/time limits for the documented
packet/chunk protocol. The client validates HTTPS certificates, does not follow
redirects, ignores curlrc and proxy environment configuration, and creates a
private temporary header file instead of exposing the token in curl arguments.
Non-loopback cleartext HTTP authentication needs explicit `--allow-insecure-http`.

```sh
wasm-git clone https://wasm.example/project ./peer
wasm-git remote ./peer add origin https://wasm.example/project
wasm-git push ./peer origin main:main
```

Supply the environment token through the deployment's credential facility before
running these client commands. A token gives transport access; commit authorship
is a separate policy described below.

## 4. Repository and reference authorization

A policy provides multiple transport principals and scoped grants. Administrators
write an external JSON policy, then compile it to the binary runtime record:

```sh
wasm-git-server compile-policy policy.json policy.bin
wasm-git-server --root /srv/wasm-git --policy policy.bin --allow-write
```

Example external input:

```json
{
  "version": 3,
  "principals": [
    {
      "name": "release-bot",
      "token_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
      "grants": [
        {
          "repository": "project",
          "read": true,
          "write": ["refs/heads/release", "refs/tags/release-*"],
          "force": false,
          "create": false
        }
      ]
    }
  ]
}
```

Replace the 64 zeroes with the SHA-256 of the exact bearer token bytes, excluding
any token-file newline. The example is a schema illustration, not a usable shared
credential. Compilation rejects unknown fields, invalid identities, duplicate
principals or credentials, and invalid rule syntax. Output is created as a new
private file; existing files are not overwritten. Runtime storage is binary.

Repository and reference rules allow an exact match or a single trailing `*`
prefix pattern, not a general glob language. Writes are scoped to
`refs/heads/` and `refs/tags/`; reads are repository-level. Grant `create` only for
intended server-side fork destinations. Force permission also requires the
server's `--allow-force`. Every update retains an exact lease, including mirror,
`--force`, and `+SOURCE:DESTINATION` updates. `team/*` scopes an entire repository
namespace; principals and configured remote names remain single components.
HTTP write enablement is
still explicit. A compiled policy and `--token-file` are mutually exclusive.

The compiled file bootstraps a root once. Subsequent policy updates use the
root's `policy-set` CAS and durable operation receipt; `policy-query` recovers a
particular update after a lost reply. HTTP and stdio instances sharing that root
use the same authorization gate. The deployment, filesystem, identity and
natural-expiry limits are specified in [server authorization](../docs/server.md#多用户权限与原子发布).
Never let an untrusted client select an SSH principal or the server root.

Use native `operation-credential-issue` / `operation-credential-query` with the
HTTP `--require-operation-credentials` entry point to bind original operation
deadlines. First issuance atomically upgrades the authorization database;
older binaries and ordinary authentication entry points then reject that root.
Exact request binding, immutable mutation identities, query recovery, and
deployment constraints are specified in [strict operation credentials](../docs/server.md#严格操作凭据).
Strict uploads use the original-operation Chunk and QueryChunk contracts of
`scoped-upload-rpc`; ordinary Slice is not accepted by that entry point.
Accepted decisions survive staged-data cleanup, as specified in
[chunk decisions and cleanup](../docs/server.md#分片决定与清理).

On Linux, a local administrator can use `wasm-git-server --format json`
with `operation-context`, `operation-recover`, `audit-query`, and `capacity-query`
to inspect original operations, perform restricted recovery, and query audit and
repository capacity. After a principal is revoked, recovery accepts the exact
original issuance request only to inspect or close the upload or retention scope.
It does not renew authority, upload or read content, or make new publication or promotion decisions.
Recovery takes the root's exclusive authorization gate and preserves committed
results. See [local administration and restricted recovery](../docs/server.md#本地运维与受限恢复)
for identity and path requirements, JSON request files, audit evidence, and legacy credentials.

Grant `release_retention` only to the trusted principal responsible for release
decisions; repository read permission is also required. Single-token mode cannot
use release retention. The holder durably records prepare, promote, and close for
the original operation. A close tombstone rejects a late prepare, and a promoted
reference cannot be closed. Promotion and close convey the coordinator's
authoritative decision; the repository does not decide the release. Fixed reads
are limited to the selected commit and files, which must be local at preparation
and are protected from GC and pool eviction. Versioned native types are defined in
`src/protocol.rs`. Cross-language callers can use `--format json retention-rpc`,
`upload-binding`, `upload-recovery`, and `scoped-upload-rpc`, checking the process status. These
commands read native-type JSON on stdin and use `WASM_GIT_REMOTE_TOKEN` for credentials.
`--format json integration-capabilities` reports the tool, contract versions, and
authorization limits. `--format json integration-validate` validates requests,
responses, and the actual process exit code using `IntegrationValidation` in
`src/protocol.rs`. Node callers must embed the original, unparsed JSON bytes in
the validation request and use `JSON.parse` only after validation succeeds. This
entry rejects floats and integers outside JavaScript's safe range; native binary
`u64` values retain their full width.

## 5. SSH transport

```sh
wasm-git clone ssh://alice@host.example/srv/wasm-git/project ./peer
wasm-git fetch ./peer origin
```

The URL parent path becomes the service root; the last segment is the repository
name. The client starts a remote `wasm-git-server server --stdio --root ...`
request using SSH without a pseudo-terminal. Stdio normally permits writes under
the system account's filesystem authority. Use `--read-only` to prohibit writes,
or a trusted forced-command wrapper with a fixed root, compiled policy, and
`--principal NAME` to constrain a transport identity.

For a namespaced repository, use an explicit double-slash boundary between the
absolute service root and repository path:
`ssh://alice@host.example/srv/wasm-git//team/project`. HTTP needs no such boundary:
`https://wasm.example/team/project`.

`WASM_GIT_REMOTE_PROGRAM` specifies one remote executable path;
`WASM_GIT_SSH_PROGRAM` specifies one local SSH executable or wrapper. They are not
arbitrary shell command strings. SSH uses noninteractive authentication and normal
host-key checks. HTTP bearer credentials are not forwarded into the SSH process.
A forced-command wrapper must validate or replace client-requested arguments;
merely giving a principal broad shell access does not confine it to wasm-git policy.

## 6. Receive pipeline, hooks, and audit

Incoming updates are checked for transport identity, repository/reference grants,
object integrity, object-graph closure, expected old reference values, force
policy, author trust requirements, and optional pre-receive approval. Reference
batches use the shared durable transaction mechanism. `push --atomic` exposes the
all-or-nothing batch contract to the client.

Ordinary multi-reference pushes send one negotiated object bundle. The service
validates it once, runs pre-receive once, and checks each eligible reference's
update hook. Lease, permission, force-policy, and update-hook rejections are
reported per reference; accepted changes share one durable publication decision.
A global pre-receive failure, invalid object graph, or system error stops the
request. Competing newly created prefix names are both rejected. With `--atomic`,
any reference rejection rejects the entire batch before object installation.

`push --dry-run` validates permissions, leases, graph closure, and author trust
without installing objects, updating refs/upstreams, running hooks, or appending
receive audit. Hook approval is therefore not promised by a successful preview.
Large previews may create bounded temporary upload records; they do not publish
objects. A corrupt or lost receipt makes every sent update's result unknown.
Always reconcile refs before retrying.

```sh
wasm-git-server --root /srv/wasm-git --policy policy.bin --allow-write \
  --pre-receive /usr/local/libexec/wasm-git-pre-receive \
  --update /usr/local/libexec/wasm-git-update \
  --post-receive /usr/local/libexec/wasm-git-post-receive \
  --post-update /usr/local/libexec/wasm-git-post-update
wasm-git-server audit-log /srv/wasm-git/receive-audit.bin
```

Hooks are administrator-owned executables, not repository-uploaded scripts.
They run with the repository as their working directory and receive
`WASM_GIT_ACTOR`, `WASM_GIT_REPOSITORY`, `WASM_GIT_QUARANTINE_BUNDLE`, and
`WASM_GIT_UPDATES_FILE`, `WASM_GIT_RECEIVE_TRANSACTION`, and `WASM_GIT_HOOK`.
Repeated client `-o VALUE` / `--push-option VALUE` arguments become
`WASM_GIT_PUSH_OPTION_COUNT` and `WASM_GIT_PUSH_OPTION_0`, `_1`, etc. Options are literal
UTF-8 values: at most 256 and 64 KiB combined, without NUL or LF. Inherited option
variables are cleared. Options are included in the binary request receipt and audit.
Standard input contains the same binary update record
identified by the updates file. Do not parse it as Git's text hook protocol.
Pre-receive and update share one private quarantine and a 15-second total budget.
The update hook receives three arguments: full ref name, old ID, new ID; absence
uses 64 zeroes. Post-receive and post-update share a separate 15-second budget
and an empty bundle after publication. Post hooks receive only accepted updates;
post-update also receives their full ref names as arguments. Deploy your own
process isolation if hooks can spawn descendants.

A failing pre-receive hook prevents publication. A post-receive failure cannot
undo an accepted update and is reported separately. The binary audit records
principal, repository, transaction, updates, and outcomes, including prepared,
accepted, partial, rejected, unknown, per-reference reasons, and post-hook failures.
Post-update is still attempted when post-receive fails. A completion-audit write
can itself fail after publication; retain service stderr and query actual refs
when reconciling outcomes. Audit output is JSON only when explicitly rendered.
`receive-audit.bin` is supplementary evidence for ordinary or historical receives;
use the root audit index and native receipts for strict operations. `Unknown`,
`Unobserved`, or an incomplete audit observation does not prove non-execution.

## 7. Author-signature policy

```sh
wasm-git trust-author /etc/wasm-git/authors.bin 'Alice <alice@example.org>' alice.pub
wasm-git-server --root /srv/wasm-git --policy policy.bin --allow-write \
  --signed-authors /etc/wasm-git/authors.bin
```

The server verifies newly exposed commits with native Ed25519, OpenSSH SSHSIG or
OpenPGP verification and the configured exact author/key trust binding. External
formats require installed `ssh-keygen` or `gpg`. Already public ancestors may
retain earlier unsigned history. An unsigned dangling commit cannot bypass the
policy merely because its object was already stored. Ref-only publication also
undergoes the new-history check. Deletion and references to already-public history
have distinct behavior and do not require rewriting old ancestors.

Transport identity, reference authorization, author trust, and hook approval are
independent checks. Manage key rotation by adding the replacement binding before
removing the old key where policy permits. `untrust-author` can revoke one public
key or all bindings for a name. Re-signing a commit changes its ID and may require
a reviewed non-fast-forward update; it never changes bytes under the old ID.

## 8. Shared forks and packed storage

```sh
wasm-git fork /srv/wasm-git/project /srv/wasm-git/project-fork
wasm-git fork https://wasm.example/project project-fork
wasm-git repack /srv/wasm-git/project
wasm-git gc /srv/wasm-git/project
```

Local same-filesystem forks link immutable packs and maintain independent refs,
HEAD, recovery records, and pool ownership. Loose source data is packed before
sharing. Subsequent GC retains eligible shared base packs and writes private
additions separately. Cross-pack deltas can use retained shared bases; deleting
the source pathname does not remove the fork's hard links.

Do not edit a shared pack in place. Manage storage through the repository engine.
Explicit pruning may rewrite a repository's representation when retaining an
entire shared pack would keep objects outside the requested closure. `pack_bytes`
reports newly written packs, not total retained/shared disk usage. Filesystem
hard-link accounting and pool-backed promises must be considered in capacity
planning. A cross-filesystem hard-link fork fails; a normal local clone can fall
back to copying and verifying objects.

## 9. Shallow and partial clients

Clients can select depth, branch, and content filters, later deepen history or
hydrate missing Wasm. The server retains the ordinary full repository model.
A shallow client's new commits may be accepted when the receiver already owns
the missing ancestors and can validate the final complete graph. A new empty
receiver cannot publish history whose required ancestors remain unavailable.

Partial clones do not equate promised objects with verified data. The object-slice
endpoint serves authorized, exposed reachable objects, not arbitrary dangling
objects known only by hash. Use local configuration to select trusted promisor
sources; received bundles cannot install arbitrary network fetch locations.

## 10. Multi-node immutable object pools

Provision each pool node as an ordinary repository service accessible only to
appropriate backend identities. Configure source repositories explicitly:

```sh
wasm-git pool attach /srv/wasm-git/project https://node-a.example/pool https://node-b.example/pool
wasm-git pool sync /srv/wasm-git/project
wasm-git pool status /srv/wasm-git/project
wasm-git pool sync /srv/wasm-git/project --evict
```

Each owner has a random identity and pins commits through
`pool/<owner>/<commit-id>` references. Synchronization copies immutable data and
uses the normal verified, leased publication path. Every configured node must
confirm its pins before local Wasm eviction. A failed node leaves local data
available; successful replica work can be reused on retry. Reads fall back among
configured nodes and verify restored object hashes.

Forks obtain independent pins without downloading a redundant copy of pool-backed
Wasm. Synchronize new local commits before a pool-backed fork that needs those
pins. `pool release DIR` first hydrates required local content and then removes
that owner's remote pins. Partial failure is retryable. Node reflog retention
can continue preserving objects after pin removal; deletion of refs alone is not
proof that disk space has been reclaimed.

The pool repository itself is the authorization boundary. Do not expose a common
readable pool across tenants that must not read each other's objects. Replication
provides immutable-object availability, not multi-primary mutable-reference
consensus. Owner IDs, pin state, and configured sources are part of recovery.
The stable `pool.gate` inode coordinates operations; do not delete it while any
participant may hold its process lock.

## 11. Service supervision and limits

The repository includes a [systemd unit template](../contrib/wasm-git-server.service).
It uses a dedicated account, restrictive umask, filesystem protections, and a
loopback write service with a private token file. Review paths and deployment
ownership before installing it. Apply CPU/memory controls appropriate to the
workload; filesystem restrictions must allow the live root and required hook data.

The standalone server has no client-style leading `--threads` option. Control its
available CPUs through service affinity/cgroups. Native parallel operations use
the shared pool; the HTTP listener permits up to 16 active connection workers and
returns 503 when saturated. **`--max-connections N` limits total accepted
connections before the process exits; it is not the concurrency setting.** It is
primarily useful for finite tests.

Current bounds include 64 MiB per RPC packet, 64 KiB metadata, 1 MiB transfer
chunks, 4 GiB per bundle, 32 pending uploads, and an 8 GiB staged-upload reservation
budget per repository. Upload expiry is 24 hours.
Scoped uploads bind the authenticated principal, repository, target, session, and
content digest. Legacy unscoped upload RPCs are rejected. Expired staging chunks
may be uploaded again under an open session; cancellation leaves a tombstone.
A v4 receive decision and its exact reference receipt share the native journal.
Query the original session after recovery; an explicit close aborts a v4 operation
only when it has no durable decision and no in-flight worker. It cannot be replayed. Legacy `Applying`
records remain unknown and require reconciliation. Older programs cannot recover
a v4 journal; service and local maintenance commands must support that version.
Scoped upload sessions and release retentions each admit new records up to
4,096 active records and 64 MiB of active encoded record data. Tombstones and
completed or promoted terminal records remain but do not consume active capacity.
`capacity-query` reports active and total counts and encoded bytes separately;
totals exclude repository objects and chunk payloads. After a storage failure,
restore storage before querying or retrying with the original ID.
Before migrating old ledgers to per-record storage, take a consistent backup and
stop old writers and maintenance programs during a maintenance window; do not mix
versions. A first `capacity-query` can trigger migration. Preserve the migration
marker together with the new record directory.
See [session and retention capacity](../docs/server.md#会话与保留容量).
For a chunked upload through the ordinary client, supply a unique 64-hex-digit
`WASM_GIT_UPLOAD_SESSION` and an existing private `WASM_GIT_UPLOAD_RECOVERY_DIR`.
Before sending, the client persists the binding and full payload there.
`upload-recovery` validates both; use the same principal to query or resume.
The caller removes these recovery files after reconciling the outcome.

Each HTTP connection has a
15-second cumulative I/O budget. Processing after a complete request is buffered
does not consume that budget, so verification and durable storage on a slow host
do not prevent an otherwise completed publication from returning its receipt.
Tiny reads do not renew the budget. Server processing is bounded by request sizes
and concurrency, rather than cancelled at a wall-clock deadline. Hook groups have
their separate deadlines described above.

The client allows 300 seconds per HTTP/SSH RPC by default. Set
`WASM_GIT_RPC_TIMEOUT_SECONDS` to an integer from 1 through 3600 to change it. A large
transfer comprises multiple RPCs, each with its own budget. Connection setup has
a 10-second timeout; process and pipe cleanup allows five additional seconds.
A client timeout after publication remains an uncertain result: inspect the
remote refs before retrying. Audit rendering also has a cumulative size limit. These
limits are not memory-use guarantees. Full snapshot and closure verification can
load substantial history. Capacity-test representative data and concurrent users.

## 12. Backup, restore, and incident response

For a consistent filesystem backup, stop writers or use a coordinated snapshot
that captures repository objects, packs, refs, recovery/control records, and
linked-worktree metadata together. Preserve hard links if storage efficiency
matters. Back up service policy, author trust, and private credentials separately
with appropriate access control. A portable bundle covers its selected repository
history, not all operational configuration or unsaved workspace state.
Back up the root authorization SQLite database, its recovery journal and audit
index, and the retention and scoped-upload markers and records together with the
repository. Before restoring, reconcile release decisions, completed reference
writes, and the replay horizon; do not overwrite newer tombstones with an old
backup or treat unknown outcomes as absent.

A partial or pool-evicted repository is not a self-contained backup. Either hydrate
it before export or back up its required pool replicas, pins, and ownership state
as a coordinated set. Validate the backup by restoring into a new directory,
running `fsck`, and exporting selected historic modules for byte comparison.

After an interrupted operation, retain journals, incomplete audit tails, and
service diagnostics. For strict operations, recover with the exact original
request through `operation-recover`; ordinary strict reads and capacity queries
reject a pending reference journal. Other local engine workflows retain their
normal transaction recovery. Query refs before retrying an uncertain push. Do not manually
remove packs, promised-object markers, or pool pins to suppress an error.
Native writer/index/transfer lock files remain in place. Unix operations explicitly
unlock on normal completion; after a crash, every inherited descriptor still
holding the lock must close. Never unlink the versioned inodes.
Legacy and WASIp1 create-new locks require a controlled maintenance window and
confirmation that no process remains before removal. Do not switch lock backends
on a shared repository during operation.

Use `gc DIR -n` to review a collection plan. Normal GC compresses without pruning;
`--prune=now` explicitly requests reclamation outside retained roots. Recovering
truly deleted unreferenced history requires another retained copy or backup.
Rotation of receive-audit data must preserve complete records and coordinate with
writers; keep the original file when a truncated tail requires investigation.

## 13. Operational acceptance checks

Before enabling write traffic, exercise a fresh staging deployment: authenticated
read/write and denial cases; stale leases; an atomic batch with one rejected ref;
hook rejection and post-receive failure; signed and untrusted authors; fork after
GC and source removal; shallow/partial hydration; replica failure before eviction;
and backup restoration. Run tests under a resource limit and retain their exact
inputs and logs under the source checkout's `.tmp/` directory.

The service does not supply a hosting UI, automatic certificate issuance,
organization lifecycle management, automatic multi-node reference failover, or
all Git plumbing/options. Keep these responsibilities explicit when integrating
it into a larger platform.
