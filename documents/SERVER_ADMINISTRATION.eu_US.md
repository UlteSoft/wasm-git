# uwgit Server Administration Manual

**Edition:** 2026-11-06 · [简体中文](SERVER_ADMINISTRATION.zh_CN.md) · [Library](README.eu_US.md)

## 1. Deployment model

`wasm-git-server` serves existing uwgit repositories beneath a configured root.
Each repository is an immediate child directory. Repository names are 1–128 ASCII
characters, start with an alphanumeric character, and otherwise allow letters,
digits, `-`, `_`, and `.`. Repository names are not arbitrary filesystem paths.
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

## 2. Initial filesystem layout

A typical installation separates executable code, repositories, and configuration:

- `/usr/local/bin/wasm-git` and `/usr/local/bin/wasm-git-server`: installed binaries.
- `/srv/uwgit/project/`: one repository, including its `.uwgit/` metadata.
- `/etc/uwgit/`: authorization policy, token, and author trust configuration.
- An operator-managed backup destination independent of the live storage volume.

Run the service as a dedicated unprivileged account with write permission only
where required. Initialize repositories as that account. These commands assume
the account and directories have already been provisioned:

```sh
sudo -u uwgit /usr/local/bin/wasm-git init /srv/uwgit/project
sudo -u uwgit /usr/local/bin/wasm-git fsck /srv/uwgit/project
wasm-git-server --root /srv/uwgit --listen 127.0.0.1:9419
```

The last command is an illustrative foreground launch and must also run under the
service account. HTTP defaults to loopback and read-only. The service root must
already exist. Source-code `.tmp/` conventions apply to development output, not
to the location of production repositories.

## 3. HTTP authentication and public HTTPS

For a single transport identity, provision a private token file and launch:

```sh
wasm-git-server --root /srv/uwgit --listen 127.0.0.1:9419 \
  --token-file /etc/uwgit/token --allow-write
```

The token is 1–4096 printable non-whitespace ASCII bytes; one trailing newline is
accepted in the file. Generate a high-entropy token and restrict file permissions.
When a token is configured, both reads and writes require bearer authentication.
Do not put a real token in source, a service command line, or published examples.
The client reads `UWGIT_REMOTE_TOKEN` from its environment and does not persist it
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
wasm-git-server --root /srv/uwgit --policy policy.bin --allow-write
```

Example external input:

```json
{
  "version": 1,
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
server's `--allow-force` and the client's exact lease. HTTP write enablement is
still explicit. A compiled policy and `--token-file` are mutually exclusive.

Treat policy replacement as a controlled configuration rollout. Restart instances
to load the new policy; do not assume hot reload. Never let an untrusted client
select an SSH principal or the server filesystem root.

## 5. SSH transport

```sh
wasm-git clone ssh://alice@host.example/srv/uwgit/project ./peer
wasm-git fetch ./peer origin
```

The URL parent path becomes the service root; the last segment is the repository
name. The client starts a remote `wasm-git-server server --stdio --root ...`
request using SSH without a pseudo-terminal. Stdio normally permits writes under
the system account's filesystem authority. Use `--read-only` to prohibit writes,
or a trusted forced-command wrapper with a fixed root, compiled policy, and
`--principal NAME` to constrain a transport identity.

`UWGIT_REMOTE_PROGRAM` specifies one remote executable path;
`UWGIT_SSH_PROGRAM` specifies one local SSH executable or wrapper. They are not
arbitrary shell command strings. SSH uses noninteractive authentication and normal
host-key checks. HTTP bearer credentials are not forwarded into the SSH process.
A forced-command wrapper must validate or replace client-requested arguments;
merely giving a principal broad shell access does not confine it to uwgit policy.

## 6. Receive pipeline, hooks, and audit

Incoming updates are checked for transport identity, repository/reference grants,
object integrity, object-graph closure, expected old reference values, force
policy, author trust requirements, and optional pre-receive approval. Reference
batches use the shared durable transaction mechanism. `push --atomic` exposes the
all-or-nothing batch contract to the client.

```sh
wasm-git-server --root /srv/uwgit --policy policy.bin --allow-write \
  --pre-receive /usr/local/libexec/uwgit-pre-receive \
  --post-receive /usr/local/libexec/uwgit-post-receive
wasm-git-server audit-log /srv/uwgit/receive-audit.bin
```

Hooks are administrator-owned executables, not repository-uploaded scripts.
They run with the repository as their working directory and receive
`UWGIT_ACTOR`, `UWGIT_REPOSITORY`, `UWGIT_QUARANTINE_BUNDLE`, and
`UWGIT_UPDATES_FILE`. Standard input contains the same binary update record
identified by the updates file. Do not parse it as Git's text hook protocol.
Pre-receive receives a quarantined binary bundle; post-receive is a notification
with an empty bundle after publication. The direct hook process has a 15-second
deadline. Deploy your own process isolation if hooks can spawn descendants.

A failing pre-receive hook prevents publication. A post-receive failure cannot
undo an accepted update and is reported separately. The binary audit records
principal, repository, transaction, updates, and outcomes, including prepared,
accepted, rejected, unknown, and post-receive failures. A completion-audit write
can itself fail after publication; retain service stderr and query actual refs
when reconciling outcomes. Audit output is JSON only when explicitly rendered.

## 7. Author-signature policy

```sh
wasm-git trust-author /etc/uwgit/authors.bin 'Alice <alice@example.org>' alice.pub
wasm-git-server --root /srv/uwgit --policy policy.bin --allow-write \
  --signed-authors /etc/uwgit/authors.bin
```

The server verifies newly exposed commits with strict Ed25519 validation and the
configured exact author/public-key trust binding. Already public ancestors may
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
wasm-git fork /srv/uwgit/project /srv/uwgit/project-fork
wasm-git fork https://wasm.example/project project-fork
wasm-git repack /srv/uwgit/project
wasm-git gc /srv/uwgit/project
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
wasm-git pool attach /srv/uwgit/project https://node-a.example/pool https://node-b.example/pool
wasm-git pool sync /srv/uwgit/project
wasm-git pool status /srv/uwgit/project
wasm-git pool sync /srv/uwgit/project --evict
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
budget per repository. Upload expiry is 24 hours. Header and direct request/hook
deadlines are bounded; audit rendering also has a cumulative size limit. These
limits are not memory-use guarantees. Full snapshot and closure verification can
load substantial history. Capacity-test representative data and concurrent users.

## 12. Backup, restore, and incident response

For a consistent filesystem backup, stop writers or use a coordinated snapshot
that captures repository objects, packs, refs, recovery/control records, and
linked-worktree metadata together. Preserve hard links if storage efficiency
matters. Back up service policy, author trust, and private credentials separately
with appropriate access control. A portable bundle covers its selected repository
history, not all operational configuration or unsaved workspace state.

A partial or pool-evicted repository is not a self-contained backup. Either hydrate
it before export or back up its required pool replicas, pins, and ownership state
as a coordinated set. Validate the backup by restoring into a new directory,
running `fsck`, and exporting selected historic modules for byte comparison.

After an interrupted operation, retain journals, incomplete audit tails, and
service diagnostics. Reopen through the normal engine so durable transaction
recovery can run. Query refs before retrying an uncertain push. Do not manually
remove packs, promised-object markers, or pool pins to suppress an error.
Persistent writer/index locks require confirmation that no writer remains before
manual removal; process-held pool locks have different lifetime semantics.

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
