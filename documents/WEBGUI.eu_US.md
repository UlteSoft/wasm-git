# Local Web GUI and Server Supervision

[简体中文](WEBGUI.zh_CN.md) · [Documentation library](README.eu_US.md)

## Architecture and requirements

`webgui/client.py` and `webgui/server.py` are separate Python 3.10+ entry points
sharing a standard-library HTTP bridge and static browser assets. The bridge consumes
[versioned binary command responses](COMMAND_PROTOCOL.eu_US.md), not terminal text. They launch the
existing native `wasm-git` and `wasm-git-server` programs with argument arrays.
No shell interprets command arguments. The GUI is source distributed; it is not
linked into either native executable and has no pip/npm runtime dependency.

The client interface reviews and modifies local repositories. The server interface
includes those capabilities and adds supervision of a selected hosting root. The
GUI does not replace the native protocol, storage engine, authentication policy,
author verification, branch protection or receive transaction implementation.

## Start, locate binaries and stop

Run these commands from the source checkout:

```sh
python3 webgui/client.py start --repo /absolute/repository --open
python3 webgui/server.py start --root /absolute/hosting-root --open
python3 webgui/client.py status
python3 webgui/server.py status
python3 webgui/client.py stop
python3 webgui/server.py stop
```

The default ports are 8737 and 8738. `--port` selects another loopback port and
must also be supplied to `status` or `stop`. `--state-dir` defaults to the source
checkout's `.tmp/webgui/`; use the same directory for lifecycle commands. `serve`
runs in the foreground and handles Ctrl-C. Without `--repo`, the browser can
create, clone or open a repository. A repository contains `.wasm-git/`.

Binary lookup checks an explicit `--client`/`--server` argument, the corresponding
`WASM_GIT_CLIENT`/`WASM_GIT_SERVER` environment variable, PATH, and local
`target`, `.tmp/target` and `.tmp/webgui-build` release/debug directories. An
invalid explicit selection fails with a diagnostic. If no executable is found,
Cargo builds the required native program into `.tmp/webgui-build/`. Cargo and a
native C toolchain must already be installed; dependencies may need downloading.
The GUI does not install a toolchain. `--no-build` prevents compilation; `--build`
rebuilds from the current checkout even when a binary exists. The server build
uses `--no-default-features --features server --bin wasm-git-server`; the client
uses the default client feature and `--bin wasm-git`.

Startup prints a URL containing a random per-session token in the fragment. Open
that complete URL. The page moves the token to tab session storage and removes
it from the address bar. `--open` asks the operating system to open the URL. The
startup log and compact binary process-state file are local to the state directory.

## English and Simplified Chinese

The top-right selector changes the entire interface between English and Simplified
Chinese without a reload. Static labels, dynamic action forms, confirmation dialogs,
command categories, audit explanations and supervision controls participate. The
first visit follows the first supported browser language; a saved preference takes
precedence and is stored per origin. Disabled browser storage does not prevent
switching the current page. Locale identifiers use the project's `eu_US` and
`zh_CN` convention; HTML language tags are `en-US` and `zh-CN`.

Editable values are never translated. Commit subjects, branch/path names, symbols,
instructions, offsets, native JSON fields and downloaded output remain verbatim.
For example, a commit titled `Overview` remains `Overview` in the Chinese UI.
Native help and diagnostics retain their original language; the GUI localizes its
own explanation around them. Translation changes do not change command arguments,
repository records, matching evidence or byte restoration.

## Projects and workspace selection

The launcher has **Open project**, **Create project** and **Clone project** actions.
A folder browser lists local directories; a path can also be entered directly.
Creating/cloning takes a parent folder and project name. Opened projects appear as
cards and in the sidebar selector; closing moves a project to recents without
removing repository files. Removing a recent entry forgets only that entry.
The private SQLite registry persists up to 100 projects across GUI sessions and
is separate from repository storage. Missing folders are marked unavailable.

Every structured query/write carries the selected project ID. The backend resolves
it through the registry and rejects closed/missing projects. Switching clears old
results, command arguments, environment inputs and stream previews. Async responses
from the previously selected project cannot update the new view. Commit drafts
are kept separately per project in page memory and disappear on reload. Closing a
project does not cancel an already submitted native operation. Use job cancellation
explicitly when needed and inspect repository state before retrying.

## Repository and audit workflow

The Changes view shows branch/HEAD state, tracked files, separate staged/worktree
file groups and pending operations. Selecting a change opens an inline audit
beside the commit form. File controls stage, unstage, inspect, compare,
discard or select a conflict side. Commit messages support multiple paragraphs.
History displays parents, signatures, shallow boundaries and the selected commit's
file tree. Repository tools cover branches/tags, remotes, fast-forward pull, push,
stash, reset, merge/replay, GC and integrity verification. Destructive structured
actions present their concrete selection for review; native checks still apply.

The comparison view supports worktree versus index, index versus HEAD and two
revisions, with optional file and section selection such as `code` or
`custom:producers`. It presents function/instruction changes and exact old/new
byte spans. Ambiguous association and incomplete typed analysis are visible.
The initial page shows up to 250 instruction edits per function and 200 byte edits
per file; Show more loads further edits. Large hex payloads expand in 4 KiB pages.
The full report also remains available through explicit original-data export. Original bytes
are the evidence; metadata and inferred source correspondence remain advisory.

The Wasm explorer inspects a committed module or a worktree file. Symbol/source
queries support symbol name, function index, file byte offset, code-payload offset,
Go virtual PC and recorded source path. External source-map/debug-Wasm sidecars
must reside inside the selected worktree. Go PCs and file offsets are distinct.

## Complete command access

The capability catalog groups command forms for history, Wasm auditing, remotes,
storage, native/SSH/OpenPGP signatures, WasmHub PRs and low-level index/object/ref
operations. A form prepares editable arguments; it does not execute automatically.
Use installed-binary help to inspect current options. Templates are starting
points, not a whitelist or a substitute for choosing actual IDs and file paths.

The command desk accepts every option recognized by the selected binary, including
options added after this GUI version. Each editable argument has its own field, excluding the executable. Empty
arguments and embedded newlines are preserved. Environment name/value rows replace
JSON editing; values are masked and cleared after launch.
The chosen working directory must exist. Environment overrides affect only the
child process and are not written to job metadata. Existing SSH agents and other
native credential mechanisms are inherited. Terminal-only password/PIN prompts
need prior local preparation; the bridge provides pipes, not a pseudo-terminal.

Standard input can come from a regular local file or remain open for interactive
batch input. Text/file uploads are sent in bounded chunks, and EOF can be explicit.
Standard output and error are captured independently as binary files. The browser initially shows only job state and byte counts. Original stdout/stderr
requires deliberate review; the optional preview is UTF-8, while explicit downloads
preserve original bytes, including NUL and non-UTF-8 values. Recent jobs can be selected again, downloaded or removed after
completion. Output is temporary for the GUI session; download it before stopping.

## Server operations

The supervision view inventories non-symlink repositories under the selected root,
with an eight-level, 10,000-visited-directory and 1,000-repository display bound.
It exposes native integrity/storage commands and receive-audit files. A loopback
TCP probe checks only whether a listener is reachable, not protocol health or
authorization. The native service can listen elsewhere if explicitly configured;
the administration GUI itself always stays on loopback.

A launch form covers HTTP or stdio transport, access mode, connection limit,
token/policy files, SSH principal, forced updates, trusted authors, protection,
merge identity/key/format and receive/update hooks. Review the selected root, transport, listener and access mode
before startup. Complete argv and credentials are not shown in service cards. One managed service can run per GUI instance, alongside client
operations and a separate server administrative command. Stop controls affect
only processes launched by that instance. Existing externally managed services
are not adopted. Policy/protection changes require restarting the native service.

Compile access/protection JSON inputs through the native server to binary files;
JSON here is a human-facing configuration input, not repository storage. The
receive-audit viewer reads native binary records and presents the latest 100
records as structured fields with known credentials removed. PR review/check/merge and signing remain
native operations in the command catalog; their authorization and expected-ID
checks are enforced by the server. See [WasmHub integration](WASMHUB.eu_US.md).

## Limits, failures and trust boundary

The HTTP bridge checks Host, Origin, fetch-site context and the session token. It
has no CORS allowance or arbitrary filesystem static route. CSP forbids external
scripts and framing. Repository/native strings are inserted as text, never HTML.
This is a powerful local administrative interface running with the launching
user's filesystem privileges; it is not intended for public multi-user hosting.

Preview queries allow two concurrent processes, with a default 120-second timeout,
32 MiB stdout limit and 256 KiB stderr limit. Oversized results fail explicitly
instead of displaying a truncated audit. Use narrower selections, `--timeout`,
or command-desk binary output for larger work. Structured writes and client jobs
are serialized within an instance; independent programs still rely on native
repository locking. Command jobs have no fixed runtime deadline and default to
256 MiB per output stream (`--job-output-mib`). Each target retains at most 64
jobs until completed output is removed.

Native exit codes and partial/conflict results remain visible. Routine receipts
show selected summary fields and sanitized diagnostics. Default API responses
omit full arguments, environment, key/signature bytes, compiler provenance and
embedded source contents. Known credential fields and recognized token/password
assignments are masked server-side; this is not merely CSS hiding. Original Wasm
byte evidence remains verbatim, so raw audit exports can contain arbitrary user
content. Those exports require an explicit confirmation. Binary encoding is not
encryption; see the [interface security boundary](COMMAND_PROTOCOL.eu_US.md). Cancellation,
timeout, output exhaustion or shutdown can interrupt a write after partial work;
inspect status, pending operations and receive audit before retrying. POSIX children
run in process groups; Windows cleanup uses the native process-tree termination
tool. Stopping the GUI stops its commands and managed service and removes temporary
output. It does not roll back completed repository operations.

## Verification

`webgui/test_webgui.py` exercises real native binaries through the HTTP bridge:
origin/token rejection, project creation/clone/recents/persistence/isolation,
versioned JSON/binary equivalence, endian fixtures and malformed-frame rejection,
credential projection, explicit exports, read-only previews, independent byte reconstruction,
staging/history, literal paths, binary batch I/O, cancellation/output limits,
policy/protection compilation, authenticated receive/audit and startup/shutdown.
`webgui/test_catalog.cjs` checks locale placeholders and command coverage against
native help. CI runs both plus JavaScript syntax checks. Generated repositories,
reports, browser captures and process state belong in `.tmp/`.

The current pre-release namespace uses `.wasm-git`, `WASM_GIT_*`, the
`/v1/wasm-git` RPC route, `WGT` format magic prefixes and `WASMGIDX` for the index.
Previous internal names and signatures are intentionally not accepted or migrated.
Use matching new client/server builds and newly initialized native repositories.

Privacy filtering is separate from translation: ordinary metadata/diagnostics mask known credentials; original exports retain unmodified evidence. A client-GUI clone checks out named files. If checkout fails after cloning, the project remains accessible with a recovery warning.
