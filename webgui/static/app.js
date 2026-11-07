"use strict";
const $ = id => document.getElementById(id);
const state = {repo: "", session: null, status: null, history: [], commit: null, busy: 0, generation: 0, job: null, offsets: [0, 0], poll: null};
let token = new URLSearchParams(location.hash.slice(1)).get("token") || sessionStorage.getItem("wasm-git-token") || "";
if (token) { sessionStorage.setItem("wasm-git-token", token); history.replaceState(null, "", location.pathname); }
const el = (tag, text = "", cls = "") => { const node = document.createElement(tag); setText(node, text); if (cls) node.className = cls; return node; };
const button = (text, fn, cls = "quiet") => { const node = el("button", text, cls); node.type = "button"; node.addEventListener("click", () => task(fn, node)); return node; };
const clear = (node, cls) => { node.replaceChildren(); if (cls !== undefined) node.className = cls; return node; };
const val = id => $(id).value;
const pretty = data => typeof data === "string" ? data : JSON.stringify(data, null, 2);
const shorten = value => value ? value.slice(0, 12) : "—";
const bytes = n => n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KiB` : `${(n / 1048576).toFixed(1)} MiB`;
const badge = (text, cls = text) => el("span", typeof text === "string" && i18n.has(text) ? tr(text) : text || "—", `badge ${cls}`);
function notice(message = "") { setText($("notice"), message ? i18n.error(message) : ""); $("notice").hidden = !message; }
async function api(path, data) {
  state.busy++; busy();
  try {
    const response = await fetch(path, {method: data === undefined ? "GET" : "POST", cache: "no-store",
      headers: {"X-Wasm-Git-Token": token, "Content-Type": "application/json"}, body: data === undefined ? undefined : JSON.stringify(data)});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
    return result;
  } finally { state.busy--; busy(); }
}
function busy() { setText($("connection"), tr(state.busy ? "Running client…" : "Local connection")); $("connection").classList.toggle("busy", state.busy > 0); }
async function task(fn, control) {
  if (control) control.disabled = true;
  notice();
  try { return await fn(); } catch (error) { notice(error.message || String(error)); }
  finally { if (control) control.disabled = false; }
}
function onForm(id, fn) { $(id).addEventListener("submit", e => { e.preventDefault(); task(fn, e.submitter); }); }
async function query(operation, fields = {}) { return (await api("/api/query", {repository: state.repo, operation, ...fields})).result; }
function tab(name) {
  document.querySelectorAll(".panel").forEach(node => node.hidden = node.id !== name);
  document.querySelectorAll(".nav").forEach(node => node.classList.toggle("active", node.dataset.tab === name));
  if (name === "history" && !state.history.length && state.repo && state.status?.head) task(loadHistory);
}
document.querySelectorAll(".nav").forEach(node => node.addEventListener("click", () => { tab(node.dataset.tab); if (node.dataset.tab === "supervision") task(loadSupervision); }));
function details(title, value, opened = false) {
  const node = el("details"); node.open = opened; node.append(el("summary", title));
  let loaded = false;
  const load = () => { if (!loaded && node.open) { node.append(el("pre", pretty(value), "json")); loaded = true; } };
  node.addEventListener("toggle", load); load(); return node;
}
function download(data, name, type = "application/json") {
  const blob = data instanceof Blob ? data : new Blob([typeof data === "string" ? data : JSON.stringify(data, null, 2)], {type});
  const link = document.createElement("a"); link.href = URL.createObjectURL(blob); link.download = name; link.click(); setTimeout(() => URL.revokeObjectURL(link.href), 30000);
}
function report(node, value, title = "Client result") {
  clear(node, "card receipt"); const heading = el("div", "", "result-heading");
  heading.append(el("h2", tr(title)), button(tr("Download JSON"), () => download(value, "wasm-git-result.json")));
  node.append(heading, details(tr("Complete client response"), value, true));
}
function stats(node, items) {
  clear(node);
  for (const [label, value, note] of items) { const card = el("div", "", "stat"); card.append(el("div", tr(label), "stat-label"), el("div", value, "stat-value"), el("div", tr(note), "stat-note")); node.append(card); }
}
function dataList(id, values) { const target = clear($(id)); for (const value of values) { const option = el("option"); option.value = value; target.append(option); } }
async function refresh() {
  const generation = ++state.generation, repository = state.repo;
  const data = await query("overview");
  if (generation !== state.generation || repository !== state.repo) return;
  state.status = data.status; state.repo = data.repository; $("repository").value = state.repo;
  const status = data.status; const files = status.files || [];
  $("repo-title").textContent = state.repo.split(/[\\/]/).filter(Boolean).pop() || "Workspace overview";
  const staged = files.filter(f => f.staged !== "clean").length;
  const modified = files.filter(f => f.worktree !== "clean").length;
  stats($("overview-stats"), [["Current branch", status.detached ? tr("Detached HEAD") : status.branch, shorten(status.head)], ["Tracked files", files.length, "Wasm modules in the index"], ["Staged changes", staged, "Ready for your next commit"], ["Working changes", modified + (status.untracked || []).length, tr("{count} untracked files", {count: (status.untracked || []).length})]]);
  const refs = clear($("refs"));
  for (const ref of data.refs.refs || []) {
    const name = ref.name.replace(/^refs\/(heads|tags|remotes)\//, "");
    const item = button(`${ref.name.startsWith("refs/tags/") ? "◇" : "⑂"}  ${name}`, async () => { $("history-revision").value = ref.name; tab("history"); await loadHistory(); }, `ref ${ref.current ? "current" : ""}`);
    item.title = `${ref.name}\n${ref.id}`; refs.append(item);
  }
  if (!refs.childNodes.length) refs.append(el("p", tr("No committed references yet."), "muted"));
  dataList("revisions", ["HEAD", "HEAD^", ...(data.refs.refs || []).map(r => r.name)]);
  const rows = files.concat((status.untracked || []).map(path => ({path, staged: "untracked", worktree: "untracked"})));
  dataList("files", rows.map(f => f.path));
  renderWorking(rows);
  const pending = clear($("pending"));
  if (status.pending) {
    const card = el("div", "", "card controls"); card.append(el("h2", tr("Pending {operation}", {operation: status.pending.operation || tr("operation")})), details(tr("Conflict / recovery state"), status.pending, true));
    const actions = el("div", "", "mini-actions");
    for (const action of ["continue", "abort", "skip"]) actions.append(button(tr(action), () => act(action, {pending_operation: status.pending.operation}, true), "secondary"));
    card.append(actions); pending.append(card);
  }
  $("commit-form").hidden = status.legacy_head_unmapped === true;
  setText($("client-info"), tr("Client: {path}", {path: state.session?.client || ""}));
  $("command-cwd").value = state.repo;
  return data;
}
function renderWorking(rows) {
  const node = clear($("working-files"), "");
  if (!rows.length) { node.append(el("div", tr("No tracked or untracked Wasm files. Create files in the worktree, then refresh."), "empty")); return; }
  const table = el("table", "", "file-table"); const head = el("thead"); const hr = el("tr");
  for (const label of ["File", "Index", "Worktree", "Actions"]) hr.append(el("th", tr(label))); head.append(hr); table.append(head); const body = el("tbody");
  for (const file of rows) {
    const row = el("tr"); const name = el("td", file.path, "file-path"); const staged = el("td"); staged.append(badge(file.staged)); const work = el("td"); work.append(badge(file.worktree));
    const actions = el("td"); const group = el("div", "", "file-actions");
    if (file.worktree !== "clean") group.append(button(tr("Stage"), () => act("stage", {file: file.path})));
    if (!["clean", "untracked"].includes(file.staged)) group.append(button(tr("Unstage"), () => act("unstage", {file: file.path})));
    if (file.worktree === "unmerged") { group.append(button(tr("Ours"), () => act("ours", {file: file.path}, true)), button(tr("Theirs"), () => act("theirs", {file: file.path}, true))); }
    if (file.worktree !== "untracked") group.append(button(tr("Diff"), () => compareFile(file.path, file.worktree !== "clean" ? "worktree" : "staged")));
    if (!["deleted", "unmerged"].includes(file.worktree)) group.append(button(tr("View"), () => viewFile(file.path, "HEAD", "worktree")));
    if (["modified", "deleted"].includes(file.worktree)) group.append(button(tr("Discard"), () => act("restore", {file: file.path}, true)));
    actions.append(group); row.append(name, staged, work, actions); body.append(row);
  }
  table.append(body); node.append(table);
}
onForm("repository-form", async () => {
  state.repo = val("repository"); state.history = []; state.commit = null; setText(clear($("commits"), "empty"), tr("Load a revision to see history."));
  setText(clear($("commit-detail"), "card empty"), tr("Select a commit."));
  for (const id of ["diff-result", "module-result", "debug-result"]) setText(clear($(id), "result empty"), tr("Choose inputs in this repository."));
  tab("overview"); await refresh();
});
$("refresh").onclick = () => task(refresh, $("refresh"));
$("worktree-diff").onclick = () => task(() => compareFile("", "worktree"));
$("stage-all").onclick = () => task(() => act("stage-all"), $("stage-all"));
onForm("commit-form", async () => { const receipt = await act("commit", {message: val("commit-message"), amend: $("commit-amend").checked}, $("commit-amend").checked); if (receipt?.exit_code === 0) $("commit-message").value = ""; });
function confirmAction(title, description, data) {
  setText($("action-dialog-title"), tr(title)); setText($("action-dialog-description"), tr(description)); $("action-dialog-detail").textContent = pretty(data);
  const dialog = $("action-dialog"); dialog.showModal();
  return new Promise(resolve => {
    const close = value => { dialog.close(); dialog.oncancel = null; resolve(value); };
    $("confirm-action").onclick = () => close(true); $("cancel-action").onclick = () => close(false); dialog.oncancel = e => { e.preventDefault(); close(false); };
  });
}
async function act(action, fields = {}, risky = false, target = $("action-result")) {
  if (risky && !await confirmAction(tr("Apply {action}?", {action}), "Review the exact selection below. This operation can replace files or change history. The native client will enforce its own repository checks.", {repository: state.repo, action, ...fields})) return;
  const value = (await api("/api/action", {repository: state.repo, action, ...fields, confirmed: risky})).result;
  renderReceipt(target, value);
  if (value.repository) { state.repo = value.repository; $("repository").value = state.repo; }
  if (state.repo) await refresh();
  state.history = [];
  return value;
}
function renderReceipt(node, value) {
  clear(node, `card receipt ${value.exit_code === 0 ? "success" : "failed"}`);
  node.append(el("h2", value.exit_code === 0 ? tr("Client completed successfully") : value.exit_code === 2 ? tr("Operation stopped for review") : tr("Client exited with code {code}", {code: value.exit_code})));
  if (value.stderr) node.append(el("pre", value.stderr, "client-text"));
  node.append(details(tr("Client result"), value.output, true), details(tr("Executed arguments"), value.command));
}
onForm("history-form", () => loadHistory());
$("more-history").onclick = () => task(() => loadHistory(true), $("more-history"));
async function loadHistory(append = false) {
  const result = await query("history", {revision: val("history-revision"), offset: append ? state.history.length : 0});
  state.history = append ? state.history.concat(result.commits) : result.commits;
  const list = clear($("commits"), "");
  for (const commit of state.history) {
    const item = button("", () => selectCommit(commit), `commit ${state.commit?.id === commit.id ? "selected" : ""}`);
    item.append(el("span", commit.message.split("\n")[0] || tr("(no message)"), "commit-title"), el("small", tr("{id} · parents: {count}{boundary}", {id: shorten(commit.id), count: commit.parents.length, boundary: commit.shallow ? tr(" · shallow boundary") : ""})));
    item.dataset.commit = commit.id; list.append(item);
  }
  if (!state.history.length) list.append(el("div", tr("No commits in this revision."), "empty"));
  $("more-history").hidden = state.history.length >= result.total;
  if (!append && state.history.length) await selectCommit(state.history[0]);
}
async function selectCommit(commit) {
  state.commit = commit; document.querySelectorAll(".commit").forEach(node => node.classList.toggle("selected", node.dataset.commit === commit.id));
  const node = clear($("commit-detail"), "card"); const content = el("div", "", "commit-detail-body");
  content.append(el("p", commit.id, "hash"), el("h2", commit.message, "commit-message"));
  const facts = el("div", "", "facts"); facts.append(badge(tr(commit.signature ? "Author signature present" : "Unsigned"), "")); if (commit.shallow) facts.append(badge("Shallow boundary", "ambiguous")); content.append(facts);
  for (const parent of commit.parents) { const row = el("div", "", "parent-row"); row.append(el("span", tr("Parent {id}", {id: shorten(parent)}), "hash"), button(tr("Compare →"), () => compareRevisions(parent, commit.id))); content.append(row); }
  if (!commit.parents.length) content.append(el("p", tr("Root commit — browse its initial files below."), "muted"));
  const actions = el("div", "", "mini-actions"); actions.append(button(tr("Cherry-pick"), () => toolSelect("cherry-pick", {revision: commit.id})), button(tr("Revert"), () => toolSelect("revert", {revision: commit.id})), button(tr("Tag"), () => toolSelect("tag-create", {revision: commit.id})), button(tr("Reset…"), () => toolSelect("reset", {revision: commit.id})));
  content.append(actions, details(tr("Commit metadata"), commit)); node.append(content);
  const tree = await query("tree", {revision: commit.id}); if (state.commit?.id !== commit.id) return;
  const list = el("div", "", "file-tree");
  for (const file of tree.entries || []) { const row = el("div", "", "tree-file"); row.append(button(`▧  ${file.path}`, () => viewFile(file.path, commit.id), "quiet file-path"), el("small", bytes(file.byte_len))); list.append(row); }
  if (!tree.named) list.append(button(tr("Inspect unnamed legacy module"), () => viewFile("", commit.id)));
  if (tree.named && !tree.entries.length) list.append(el("p", tr("This commit contains an empty file tree."), "empty"));
  node.append(list);
}
function compareMode() { const revisions = val("compare-mode") === "revisions"; $("old-label").hidden = $("new-label").hidden = !revisions; }
$("compare-mode").onchange = compareMode;
async function compareFile(file, mode) { $("compare-mode").value = mode; $("compare-file").value = file; compareMode(); tab("compare"); await runDiff(); }
async function compareRevisions(old, current) { $("compare-mode").value = "revisions"; $("compare-old").value = old; $("compare-new").value = current; $("compare-file").value = ""; compareMode(); tab("compare"); await runDiff(); }
onForm("compare-form", runDiff);
async function runDiff() {
  const result = await query("diff", {mode: val("compare-mode"), old: val("compare-old"), new: val("compare-new"), file: val("compare-file"), section: val("compare-section"), representation: val("compare-view")});
  const node = clear($("diff-result"), "result"); const heading = el("div", "", "result-heading");
  heading.append(el("h2", tr("Change review")), button(tr("Download report"), () => download(result, typeof result === "string" ? "wasm-git-diff.txt" : "wasm-git-diff.json"))); node.append(heading);
  if (typeof result === "string") { node.append(el("pre", result, "client-text")); return; }
  if (result.files) {
    const changed = result.files.filter(f => f.status !== "unchanged");
    const nav = el("div", "", "diff-nav"); node.append(nav);
    for (const file of changed) {
      const card = diffCard(file.diff || {audit: file.audit, content: file.content}, file.path, file.status);
      nav.append(button(file.path, () => card.scrollIntoView({behavior: "smooth", block: "start"}), "secondary")); node.append(card);
    }
    if (!changed.length) node.append(el("div", tr("No differences in the selected files."), "card empty"));
  } else node.append(diffCard(result, val("compare-file") || tr("Module comparison")));
  node.append(details(tr("Complete audit report"), result));
}
function diffCard(data, path, status) {
  const card = el("article", "", "card"); const heading = el("div", "", "card-heading"); heading.append(el("h2", path, "file-path"));
  if (status) heading.append(badge(status)); card.append(heading);
  if (data.summary) { const facts = el("div", "", "facts symbols"); for (const [key, count] of Object.entries(data.summary)) if (count) facts.append(el("span", tr("{count} {label}", {count, label: tr(key.replaceAll("_", " "))}), "fact")); card.append(facts); }
  if (data.function_analysis?.available === false) card.append(el("p", tr("Typed function analysis unavailable: {reason}. Exact byte evidence follows.", {reason: data.function_analysis.reason}), "source-warning"));
  if (data.function_matching?.similarity_search_complete === false) card.append(el("p", tr("Function similarity search is incomplete. Review unclassified functions in the full report."), "source-warning"));
  for (const change of data.changes || []) {
    const block = el("div", "", "change"); const f = change.function;
    block.append(el("h3", f ? tr("{symbol} · function {before} → {after}", {symbol: f.new_symbol || f.old_symbol || f.new_id, before: f.old_source_index, after: f.new_source_index}) : tr("{kind}: {id}", {kind: tr(change.kind.replaceAll("_", " ")), id: change.id || ""})));
    if (f) {
      const evidence = (data.function_matches || []).find(m => m.old_source_index === f.old_source_index && m.new_source_index === f.new_source_index);
      if (evidence?.ambiguous) block.append(el("p", tr("Ambiguous function association — identity is not proven."), "source-warning"));
      const ops = f.operators || [];
      for (const op of ops.slice(0, 250)) { const row = el("div", "", "operator"); const a = op.old || (op.kind === "delete" ? op.operator : null); const b = op.new || (op.kind === "insert" ? op.operator : null); row.append(el("pre", a ? `− @${a.offset}  ${a.text}` : "", "before"), el("pre", b ? `+ @${b.offset}  ${b.text}` : "", "after")); block.append(row); }
      if (ops.length > 250) block.append(el("p", tr("Showing 250 of {count} instruction edits. The full report contains every edit.", {count: ops.length}), "muted"));
      if (f.source_annotations?.length) block.append(details(tr("Source and symbol annotations (compiler metadata)"), f.source_annotations));
      block.append(details(tr("Block alignment & function evidence"), f));
    }
    card.append(block);
  }
  const audit = data.audit;
  if (audit) {
    const block = el("div", "", "change"); block.append(el("h3", tr("Exact byte changes")));
    const edits = audit.edits || [];
    for (const edit of edits.slice(0, 200)) {
      const row = el("div", "", "operator");
      row.append(el("pre", `− [${edit.old_span?.start}..${edit.old_span?.end})\n${edit.old_hex || "∅"}`, "before"), el("pre", `+ [${edit.new_span?.start}..${edit.new_span?.end})\n${edit.new_hex || "∅"}`, "after")); block.append(row);
    }
    if (edits.length > 200) block.append(el("p", tr("Showing 200 of {count} byte edits. Download the complete report for every edit.", {count: edits.length}), "muted"));
    if (audit.custom_changes?.length) block.append(details(tr("Custom-section changes"), audit.custom_changes, true));
    block.append(details(tr("Byte audit, diagnostics & decoding scope"), audit)); card.append(block);
  }
  const tail = el("div", "", "report-details"); tail.append(details(tr("Full file result"), data)); card.append(tail); return card;
}
function moduleFields() { return {source: val("module-source"), revision: val("module-revision"), file: val("module-file")}; }
$("module-source").onchange = () => $("module-revision-label").hidden = val("module-source") === "worktree";
async function viewFile(file, revision = "HEAD", source = "revision") { $("module-file").value = file; $("module-revision").value = revision; $("module-source").value = source; $("module-source").onchange(); tab("module"); await inspectModule(); }
onForm("module-form", inspectModule);
async function inspectModule() {
  const data = await query("inspect", moduleFields()); const node = clear($("module-result"), "result");
  const heading = el("div", "", "result-heading"); heading.append(el("h2", val("module-file") || tr("Module")), button(tr("Download inspection"), () => download(data, "wasm-git-inspect.json"))); node.append(heading);
  const card = el("div", "", "card controls"); const facts = el("div", "", "facts");
  facts.append(el("span", bytes(data.audit?.byte_len || 0), "fact")); for (const [name, n] of Object.entries(data.summary || {})) facts.append(el("span", tr("{count} {label}", {count: n, label: tr(name.replaceAll("_", " "))}), "fact")); card.append(facts);
  for (const section of data.audit?.sections || []) { const detail = el("details"); const summary = el("summary", "", "section-row"); summary.append(el("span", section.path), el("code", `[${section.span.start}..${section.span.end})`), badge(section.status)); detail.append(summary); let loaded = false; detail.ontoggle = () => { if (detail.open && !loaded) { detail.append(el("pre", pretty(section), "json")); loaded = true; } }; card.append(detail); }
  if (data.function_analysis?.available === false) card.append(el("p", data.function_analysis.reason, "source-warning"));
  node.append(card, details(tr("Exports & imports"), {exports: data.module?.exports, imports: data.module?.imports}), details(tr("Functions & debug symbols"), {functions: data.module?.functions, debug_info: data.debug_info}), details(tr("Complete inspection"), data));
}
$("debug-kind").onchange = () => $("debug-query").disabled = val("debug-kind") === "all";
onForm("debug-form", async () => { const value = await query("debug", {...moduleFields(), query_kind: val("debug-kind"), query: val("debug-query"), sidecar: val("debug-sidecar")}); report($("debug-result"), value, "Debug mappings"); });

const tools = {
  "init": ["Create repository", [["destination", "New directory", ""]]],
  "clone": ["Clone repository", [["url", "Source URL or local path", ""], ["destination", "New directory", ""]]],
  "branch-create": ["Create branch", [["name", "Branch name", ""], ["revision", "Starting revision", "HEAD"]]],
  "switch": ["Switch branch", [["name", "Branch name", "main"]]],
  "branch-delete": ["Delete merged branch", [["name", "Branch name", ""]]],
  "tag-create": ["Create tag", [["name", "Tag name", ""], ["revision", "Revision", "HEAD"]]],
  "tag-delete": ["Delete tag", [["name", "Tag name", ""]], true],
  "remote-add": ["Add remote", [["name", "Remote name", "origin"], ["url", "URL or local repository", ""]]],
  "remote-remove": ["Remove remote", [["name", "Remote name", "origin"]]],
  "fetch": ["Fetch remote", [["remote", "Remote", "origin"]]],
  "pull": ["Pull (fast-forward only)", [["remote", "Remote", "origin"], ["branch", "Remote branch", "main"]]],
  "push": ["Push & set upstream", [["remote", "Remote", "origin"], ["branch", "Branch", "main"]]],
  "merge": ["Merge revision", [["revision", "Target revision", ""], ["message", "Merge message", ""]]],
  "rebase": ["Rebase onto upstream", [["revision", "Upstream revision", ""]]],
  "cherry-pick": ["Cherry-pick commit", [["revision", "Commit", ""]]],
  "revert": ["Revert commit", [["revision", "Commit", ""]]],
  "reset": ["Reset history", [["revision", "Target revision", "HEAD"], ["mode", "Mode", ["mixed", "soft", "hard"]]], true],
  "stash-save": ["Save tracked changes to stash", [["message", "Stash message", ""]]],
  "stash-apply": ["Apply stash", [["id", "Stash ID", ""]]],
  "stash-pop": ["Pop stash", [["id", "Stash ID", ""]], true],
  "stash-drop": ["Drop stash", [["id", "Stash ID", ""]], true],
  "gc": ["Compress & collect history", []],
  "fsck": ["Verify repository integrity", []]
};
for (const [key, item] of Object.entries(tools)) { const option = el("option", tr(item[0])); option.value = key; $("tool-action").append(option); }
function toolFields(values = {}) {
  const spec = tools[val("tool-action")]; const fields = clear($("tool-fields"));
  for (const [key, label, value] of spec[1]) { const wrapper = el("label", tr(label)); const input = el(Array.isArray(value) ? "select" : key === "message" ? "textarea" : "input"); if (Array.isArray(value)) for (const item of value) { const option = el("option", tr(item)); option.value = item; input.append(option); } else input.value = value; if (key in values) input.value = values[key]; input.dataset.key = key; wrapper.append(input); fields.append(wrapper); }
  setText($("tool-description"), tr(spec[2] ? "This operation changes history or removes saved data. You will review the selection before applying it." : ["fetch", "pull", "push"].includes(val("tool-action")) ? "Uses your existing client credentials and SSH agent. Authentication prompts requiring a terminal should be prepared in your local shell first." : "Runs the native client and displays its complete receipt. More options are available in the command desk."));
}
function toolSelect(action, values = {}) { $("tool-action").value = action; toolFields(values); tab("tools"); }
$("tool-action").onchange = () => toolFields(); toolFields();
onForm("tool-form", async () => { const values = {}; $("tool-fields").querySelectorAll("[data-key]").forEach(input => values[input.dataset.key] = input.value); await act(val("tool-action"), values, !!tools[val("tool-action")][2], $("tool-result")); });
for (const kind of ["remotes", "stashes", "reflog"]) $("list-" + kind).onclick = () => task(async () => report($("tool-result"), await query(kind), kind));

let decoders = [new TextDecoder(), new TextDecoder()];
function argsValue() {
  const value = val("command-args"); const args = $("args-json").checked ? JSON.parse(value) : value.split(/\r?\n/).filter(v => v !== "");
  if (!Array.isArray(args) || !args.length || args.some(v => typeof v !== "string")) throw new Error("Enter one argument per line, or a JSON array of strings."); return args;
}
function showArgs(args) { $("args-json").checked = true; $("command-args").value = JSON.stringify(args, null, 2); }
onForm("command-form", async () => {
  const environment = val("command-env").trim() ? JSON.parse(val("command-env")) : {};
  const args = argsValue();
  const job = await api("/api/jobs/start", {args, cwd: val("command-cwd"), environment, stdin_file: val("stdin-file"), keep_stdin: $("keep-stdin").checked, target: $("command-target")?.value || "client"});
  $("command-env").value = ""; followJob(job.id);
});
function followJob(id) {
  if (state.poll) clearTimeout(state.poll); state.job = id; state.offsets = [0, 0]; decoders = [new TextDecoder(), new TextDecoder()];
  sessionStorage.setItem("wasm-git-job", id); clear($("command-stdout")); clear($("command-stderr")); tab("commands"); pollJob(); task(loadJobs);
}
async function loadJobs() {
  const result = await api("/api/jobs"); const select = clear($("recent-jobs"));
  const empty = el("option", tr(result.jobs.length ? "Select a command to inspect its output." : "No commands in this session.")); empty.value = ""; select.append(empty);
  for (const job of result.jobs.reverse()) {
    const option = el("option", tr("{state} · {target} · {command}", {state: tr(job.running ? "Running" : "Finished"), target: job.target, command: job.args.join(" ")}));
    option.value = job.id; select.append(option);
  }
  select.value = state.job || "";
}
$("recent-jobs").onchange = () => { if (val("recent-jobs")) followJob(val("recent-jobs")); };
$("jobs-refresh").onclick = () => task(loadJobs);
async function pollJob() {
  const id = state.job; if (!id) return;
  try {
    const job = await api("/api/jobs/poll", {id, stdout: state.offsets[0], stderr: state.offsets[1]}); if (id !== state.job) return;
    for (const [i, key] of ["stdout", "stderr"].entries()) {
      const data = Uint8Array.from(atob(job[key].base64), c => c.charCodeAt(0)); const output = $("command-" + key);
      if (data.length) { const text = output.textContent + decoders[i].decode(data, {stream: true}); output.textContent = text.length > 2000000 ? "[Earlier display text omitted; download exact bytes.]\n" + text.slice(-1900000) : text; output.scrollTop = output.scrollHeight; }
      state.offsets[i] = job[key].offset; $(key + "-size").textContent = bytes(job.sizes[i]);
    }
    setText($("command-status"), tr("{state} · {seconds}s{exit}{reason}", {state: tr(job.running ? "Running" : "Finished"), seconds: job.seconds, exit: job.exit_code !== null ? tr(" · exit {code}", {code: job.exit_code}) : "", reason: job.reason ? tr(" · {reason}", {reason: tr(job.reason)}) : ""}));
    $("cancel-command").disabled = !job.running; $("remove-job").disabled = job.running;
    $("stdin-form").hidden = job.stdin_closed || !job.running;
    $("stdin-form").querySelectorAll("button,input,textarea").forEach(node => node.disabled = job.stdin_busy);
    $("download-stdout").disabled = $("download-stderr").disabled = false;
    if (job.running || state.offsets.some((n, i) => n < job.sizes[i])) state.poll = setTimeout(pollJob, 400);
    else { for (const [i, key] of ["stdout", "stderr"].entries()) $("command-" + key).append(document.createTextNode(decoders[i].decode())); await loadJobs(); }
  } catch (error) { notice(error.message); $("run-command").disabled = false; }
}
$("cancel-command").onclick = () => task(async () => { await api("/api/jobs/cancel", {id: state.job}); });
$("remove-job").onclick = () => task(async () => { await api("/api/jobs/remove", {id: state.job}); state.job = null; sessionStorage.removeItem("wasm-git-job"); setText($("command-status"), tr("Output removed.")); for (const key of ["stdout", "stderr"]) { clear($("command-" + key)); $("download-" + key).disabled = true; } $("remove-job").disabled = true; await loadJobs(); });
for (const key of ["stdout", "stderr"]) $("download-" + key).onclick = () => task(async () => {
  const response = await fetch(`/api/jobs/${state.job}/${key}`, {headers: {"X-Wasm-Git-Token": token}}); if (!response.ok) throw new Error("Output download failed."); download(await response.blob(), `${key}.bin`, "application/octet-stream");
});
async function sendInput(data, eof = false) { if (data.length > 16000) throw new Error("Send at most 16,000 bytes per text message; use a local stdin file for larger input."); await api("/api/jobs/input", {id: state.job, base64: btoa(String.fromCharCode(...data)), eof}); }
onForm("stdin-form", async () => { await sendInput(new TextEncoder().encode(val("stdin-text"))); $("stdin-text").value = ""; });
$("stdin-eof").onclick = () => task(() => sendInput(new Uint8Array(), true));
$("stdin-upload").onchange = () => task(async () => {
  const file = $("stdin-upload").files[0]; if (!file) return;
  for (let at = 0; at < file.size; at += 16000) {
    const chunk = new Uint8Array(await file.slice(at, at + 16000).arrayBuffer());
    while ((await api("/api/jobs/poll", {id: state.job, stdout: state.offsets[0], stderr: state.offsets[1]})).stdin_busy) await new Promise(resolve => setTimeout(resolve, 100));
    await sendInput(chunk);
  }
  $("stdin-upload").value = "";
});
$("client-help").onclick = () => task(async () => { const help = await api($("command-target")?.value !== "client" ? "/api/server-help" : "/api/help"); report($("command-help"), `${help.output}\n${help.stderr}`, "Installed binary help"); });
$("shutdown").onclick = $("stop-header").onclick = () => $("stop-dialog").showModal();
$("cancel-stop").onclick = () => $("stop-dialog").close();
$("confirm-stop").onclick = () => task(async () => {
  await api("/api/shutdown", {}); if (state.poll) clearTimeout(state.poll); $("stop-dialog").close();
  const content = clear(document.querySelector(".content"), "content stopped"); content.append(el("h1", tr("Web GUI stopped")), el("p", tr("Your repository is on disk. Run the start command again to reopen the interface."), "muted")); setText($("connection"), tr("Stopped"));
  document.querySelectorAll("button,input").forEach(node => node.disabled = true);
});

async function boot() {
  state.session = await api("/api/session"); state.repo = state.session.repository;
  $("repository").value = state.repo; $("command-cwd").value = state.repo || state.session.cwd;
  initializeCatalog(); initializeServer(); await loadJobs();
  if (state.repo) await refresh(); else {
    setText($("repo-title"), tr("Open your workspace"));
    const welcome = clear($("working-files"), "empty"); welcome.append(el("p", tr("Enter a local wasm-git repository path above, or start with a new repository.")));
    const actions = el("div", "", "welcome-actions"); actions.append(button(tr("Create repository"), () => toolSelect("init"), "secondary"), button(tr("Clone repository"), () => toolSelect("clone"), "secondary")); welcome.append(actions); $("commit-form").hidden = true;
  }
  const lastJob = sessionStorage.getItem("wasm-git-job"); if (lastJob) { try { await api("/api/jobs/poll", {id: lastJob}); followJob(lastJob); } catch { sessionStorage.removeItem("wasm-git-job"); } }
}
task(boot);
