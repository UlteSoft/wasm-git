"use strict";
const $ = id => document.getElementById(id);
const state = {repo: "", project: "", projects: [], revealOutput: false, session: null, status: null, history: [], commit: null, busy: 0, generation: 0, job: null, offsets: [0, 0], poll: null};
function sessionGet(key) { try { return sessionStorage.getItem(key); } catch { return null; } }
function sessionSet(key, value) { try { sessionStorage.setItem(key, value); } catch { /* Per-page state remains usable. */ } }
function sessionRemove(key) { try { sessionStorage.removeItem(key); } catch { /* No persisted state. */ } }
let token = new URLSearchParams(location.hash.slice(1)).get("token") || sessionGet("wasm-git-token") || "";
if (token) { sessionSet("wasm-git-token", token); history.replaceState(null, "", location.pathname); }
const el = (tag, text = "", cls = "") => { const node = document.createElement(tag); setText(node, text); if (cls) node.className = cls; return node; };
const button = (text, fn, cls = "quiet") => { const node = el("button", text, cls); node.type = "button"; node.addEventListener("click", () => task(fn, node)); return node; };
const clear = (node, cls) => { node.replaceChildren(); if (cls !== undefined) node.className = cls; return node; };
const val = id => $(id).value;
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
  try { return await fn(); } catch (error) { if (!(error instanceof StaleProject)) notice(error.message || String(error)); }
  finally { if (control) control.disabled = false; }
}
function onForm(id, fn) { $(id).addEventListener("submit", e => { e.preventDefault(); task(fn, e.submitter); }); }
async function query(operation, fields = {}) {
  const project = state.project, generation = state.generation;
  const result = (await api("/api/query", {project, operation, ...fields})).result;
  if (project !== state.project || generation !== state.generation) throw new StaleProject();
  return result;
}
function tab(name) {
  document.querySelectorAll(".panel").forEach(node => node.hidden = node.id !== name);
  document.querySelectorAll(".nav").forEach(node => node.classList.toggle("active", node.dataset.tab === name));
  if (name === "history" && !state.history.length && state.repo && state.status?.head) task(loadHistory);
}
document.querySelectorAll(".nav").forEach(node => node.addEventListener("click", () => { tab(node.dataset.tab); if (node.dataset.tab === "supervision") task(loadSupervision); }));
function details(title, value, opened = false) {
  const node = el("details"); node.open = opened; node.append(el("summary", title));
  let loaded = false;
  const load = () => { if (!loaded && node.open) { node.append(recordView(value)); loaded = true; } };
  node.addEventListener("toggle", load); load(); return node;
}
function download(data, name, type = "application/json") {
  const blob = data instanceof Blob ? data : new Blob([typeof data === "string" ? data : JSON.stringify(data, null, 2)], {type});
  const link = document.createElement("a"); link.href = URL.createObjectURL(blob); link.download = name; link.click(); setTimeout(() => URL.revokeObjectURL(link.href), 30000);
}
function report(node, value, title = "Client result") {
  clear(node, "card receipt"); node.append(el("h2", tr(title)), typeof value === "string" ? el("pre", value, "client-text") : recordView(value));
}
async function exportReport(operation, fields) {
  const project = state.project;
  if (!await confirmAction("Export original report?", "Original data may include local paths, compiler metadata or sensitive values. Only export to a trusted destination.", {project: state.projects.find(p => p.id === project)?.name, operation})) return;
  const result = await api("/api/export", {project, operation, ...fields, confirmed: true});
  download(result.result, `wasm-git-${operation}.json`);
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
  state.status = data.status;
  $("commit-form").querySelector("button[type=submit]").disabled = false;
  const preview = clear($("change-preview"), "card preview-empty"); preview.append(el("span", "⇄", "preview-glyph"), el("h2", tr("Select a change")), el("p", tr("Select a file to review its functions, instructions and exact byte changes.")));
  $("current-branch").textContent = data.status.branch || String(tr("Detached HEAD"));
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
  setText($("client-info"), tr("Original bytes are the audit evidence. Compiler names and source mappings are advisory."));
  $("command-cwd").value = state.repo;
  return data;
}
function renderWorking(rows) {
  const node = clear($("working-files"), "file-groups");
  for (const [title, mode, files] of [["Staged changes", "staged", rows.filter(f => !["clean", "untracked"].includes(f.staged))], ["Working changes", "worktree", rows.filter(f => f.worktree !== "clean")]]) {
    const group = el("div", "", "file-group"), heading = el("div", "", "file-group-heading"); heading.append(el("h3", tr(title)), el("span", String(files.length), "count")); group.append(heading);
    for (const file of files) {
      const row = el("div", "", "change-file");
      const select = button("", async () => {
        document.querySelectorAll(".change-file").forEach(n => n.classList.remove("selected")); row.classList.add("selected");
        if (file.worktree === "untracked") { await viewFile(file.path, "HEAD", "worktree"); return; }
        const result = await query("diff", {mode, file: file.path}); diffView($("change-preview"), result, file.path);
      }, "file-select"); select.append(el("span", "▧", "file-icon"), el("span", file.path, "file-path"), badge(mode === "staged" ? file.staged : file.worktree));
      row.append(select, button(tr(mode === "staged" ? "Unstage" : "Stage"), () => act(mode === "staged" ? "unstage" : "stage", {file: file.path}), "stage-control"));
      if (file.worktree === "unmerged") row.append(button(tr("Ours"), () => act("ours", {file: file.path}, true)), button(tr("Theirs"), () => act("theirs", {file: file.path}, true)));
      if (mode === "worktree" && ["modified", "deleted"].includes(file.worktree)) row.append(button(tr("Discard"), () => act("restore", {file: file.path}, true), "discard-control"));
      group.append(row);
    }
    if (!files.length) group.append(el("p", tr(mode === "staged" ? "Nothing staged yet." : "Working tree is clean."), "group-empty"));
    node.append(group);
  }
  const all = el("details", "", "all-files"); all.append(el("summary", tr("Project files")));
  for (const file of rows.filter(f => f.worktree !== "deleted")) all.append(button(file.path, () => viewFile(file.path, "HEAD", "worktree"), "project-file-link"));
  node.append(all);
}
$("refresh").onclick = () => task(refresh, $("refresh"));
$("worktree-diff").onclick = () => task(() => compareFile("", "worktree"));
$("stage-all").onclick = () => task(() => act("stage-all"), $("stage-all"));
onForm("commit-form", async () => { const receipt = await act("commit", {message: val("commit-message"), amend: $("commit-amend").checked}, $("commit-amend").checked); if (receipt?.exit_code === 0) $("commit-message").value = ""; });
function confirmAction(title, description, data) {
  setText($("action-dialog-title"), tr(title)); setText($("action-dialog-description"), tr(description)); clear($("action-dialog-detail")).append(recordView(data));
  const dialog = $("action-dialog"); dialog.showModal();
  return new Promise(resolve => {
    const close = value => { dialog.close(); dialog.oncancel = null; resolve(value); };
    $("confirm-action").onclick = () => close(true); $("cancel-action").onclick = () => close(false); dialog.oncancel = e => { e.preventDefault(); close(false); };
  });
}
async function act(action, fields = {}, risky = false, target = $("action-result")) {
  const project = state.project;
  if (risky && !await confirmAction(tr("Apply {action}?", {action}), "Review the selection. This operation can replace files or change history.", {project: state.projects.find(p => p.id === project)?.name, action, ...fields})) return;
  if (project !== state.project) throw new StaleProject();
  const value = (await api("/api/action", {project, action, ...fields, confirmed: risky})).result;
  if (project !== state.project) throw new StaleProject();
  renderReceipt(target, value); state.history = [];
  for (const name of ["diff-result", "module-result", "debug-result"]) clear($(name));
  await refresh(); return value;
}
function renderReceipt(node, value) {
  clear(node, `card receipt ${value.exit_code === 0 ? "success" : "failed"}`);
  node.append(el("h3", value.exit_code === 0 ? tr("Done") : value.exit_code === 2 ? tr("Operation stopped for review") : tr("Client exited with code {code}", {code: value.exit_code})));
  if (value.diagnostic) node.append(el("p", value.diagnostic, "source-warning"));
  if (Object.keys(value.summary || {}).length) node.append(recordView(value.summary));
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
  const facts = el("div", "", "facts"); facts.append(badge(tr(commit.signed ? "Author signature present" : "Unsigned"), "")); if (commit.shallow) facts.append(badge("Shallow boundary", "ambiguous")); content.append(facts);
  for (const parent of commit.parents) { const row = el("div", "", "parent-row"); row.append(el("span", tr("Parent {id}", {id: shorten(parent)}), "hash"), button(tr("Compare →"), () => compareRevisions(parent, commit.id))); content.append(row); }
  if (!commit.parents.length) content.append(el("p", tr("Root commit — browse its initial files below."), "muted"));
  const actions = el("div", "", "mini-actions"); actions.append(button(tr("Cherry-pick"), () => toolSelect("cherry-pick", {revision: commit.id})), button(tr("Revert"), () => toolSelect("revert", {revision: commit.id})), button(tr("Tag"), () => toolSelect("tag-create", {revision: commit.id})), button(tr("Reset…"), () => toolSelect("reset", {revision: commit.id})));
  content.append(actions); node.append(content);
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
  const fields = {mode: val("compare-mode"), old: val("compare-old"), new: val("compare-new"), file: val("compare-file"), section: val("compare-section")};
  const result = await query("diff", fields); const node = clear($("diff-result"), "result");
  const heading = el("div", "", "result-heading"); heading.append(el("h2", tr("Change review")), button(tr("Export original report…"), () => exportReport("diff", fields))); node.append(heading);
  const body = el("div"); diffView(body, result, fields.file); node.append(body);
}
function diffCard(data, path, status) {
  const card = el("article", "", "card"); const heading = el("div", "", "card-heading"); heading.append(el("h2", path, "file-path"));
  if (status) heading.append(badge(status)); card.append(heading);
  if (data.summary) { const facts = el("div", "", "facts symbols"); for (const [key, count] of Object.entries(data.summary)) if (count) facts.append(el("span", tr("{count} {label}", {count, label: tr(key.replaceAll("_", " "))}), "fact")); card.append(facts); }
  if (data.function_analysis?.available === false) card.append(el("p", tr("Typed function analysis unavailable: {reason}. Exact byte evidence follows.", {reason: data.function_analysis.reason}), "source-warning"));
  if (data.function_matching?.similarity_search_complete === false) card.append(el("p", tr("Function similarity search is incomplete. Review unclassified functions in the full report."), "source-warning"));
  if (data.kind === "section_diff") {
    card.append(el("p", tr("Selected sections only. Edits are scoped to each section; cross-section dependencies require a full comparison."), "source-warning"));
    if (data.selection_complete?.includes(false)) card.append(el("p", tr("Section selection is incomplete beyond an untrusted boundary."), "source-warning"));
    if (data.unmatched_selectors?.length) card.append(details(tr("Unmatched section selectors"), data.unmatched_selectors, true));
    for (const change of data.changes || []) {
      const group = el("div", "", "change"); group.append(el("h3", `${change.old?.path || "∅"} → ${change.new?.path || "∅"}`));
      if (change.ambiguous || change.identity_unproven) group.append(el("p", tr("Section association is advisory; identity is not proven."), "source-warning"));
      if (change.position_changed) group.append(el("p", tr("Section position changed."), "muted"));
      if (change.fields?.length) group.append(tableView([["Path", f => f.path], ["Before", f => f.old?.description], ["After", f => f.new?.description]], change.fields));
      byteEdits(group, change.edits || []); card.append(group);
    }
    if (!data.changes?.length) card.append(el("p", tr("No differences in the selected files."), "empty"));
    return card;
  }
  if (data.content?.hex !== undefined) {
    const group = el("div", "", "change"); group.append(el("h3", tr("Complete file bytes")), el("p", `${bytes(data.content.byte_len)} · sha256 ${data.content.sha256}`, "hash"));
    group.append(bytePane(data.content.hex, data.content.span, data.content.side === "old"));
    if (data.module?.reason) group.append(el("p", data.module.reason, "source-warning")); card.append(group);
  }
  for (const change of data.changes || []) {
    const block = el("div", "", "change"); const f = change.function;
    block.append(el("h3", f ? tr("{symbol} · function {before} → {after}", {symbol: f.new_symbol || f.old_symbol || f.new_id, before: f.old_source_index, after: f.new_source_index}) : tr("{kind}: {id}", {kind: tr((change.kind || "change").replaceAll("_", " ")), id: change.id || ""})));
    if (f) {
      const evidence = (data.function_matches || []).find(m => m.old_source_index === f.old_source_index && m.new_source_index === f.new_source_index);
      if (evidence?.ambiguous) block.append(el("p", tr("Ambiguous function association — identity is not proven."), "source-warning"));
      const ops = f.operators || [];
      pagedItems(block, ops, op => { const row = el("div", "", "operator"); const a = op.old || (op.kind === "delete" ? op.operator : null); const b = op.new || (op.kind === "insert" ? op.operator : null); row.append(el("pre", a ? `− @${a.offset}  ${a.text}` : "", "before"), el("pre", b ? `+ @${b.offset}  ${b.text}` : "", "after")); return row; }, 250);
      if (f.source_annotations?.length) block.append(details(tr("Source and symbol annotations (compiler metadata)"), f.source_annotations));
      const evidenceFields = Object.fromEntries(Object.entries(f).filter(([key]) => !["operators", "source_annotations", "old_symbol", "new_symbol", "old_id", "new_id"].includes(key)));
      block.append(details(tr("Block alignment & function evidence"), evidenceFields));
    }
    card.append(block);
  }
  const audit = data.audit;
  if (audit) {
    const block = el("div", "", "change"); block.append(el("h3", tr("Exact byte changes")));
    const edits = audit.edits || [];
    byteEdits(block, edits);
    if (audit.custom_changes?.length) block.append(details(tr("Custom-section changes"), audit.custom_changes, true));
    if (audit.semantic_changes?.length) block.append(details(tr("Section changes"), audit.semantic_changes));
    if (audit.alignment) block.append(details(tr("Alignment evidence"), audit.alignment)); card.append(block);
  }
  return card;
}
function moduleFields() { return {source: val("module-source"), revision: val("module-revision"), file: val("module-file")}; }
$("module-source").onchange = () => $("module-revision-label").hidden = val("module-source") === "worktree";
async function viewFile(file, revision = "HEAD", source = "revision") { $("module-file").value = file; $("module-revision").value = revision; $("module-source").value = source; $("module-source").onchange(); tab("module"); await inspectModule(); }
onForm("module-form", inspectModule);
async function inspectModule() {
  const fields = moduleFields(), data = await query("inspect", fields); const node = clear($("module-result"), "result");
  const heading = el("div", "", "result-heading"); heading.append(el("h2", fields.file || tr("Module")), button(tr("Export original report…"), () => exportReport("inspect", fields))); node.append(heading);
  const card = el("div", "", "card controls"), facts = el("div", "", "facts");
  facts.append(el("span", bytes(data.audit?.byte_len || 0), "fact"));
  for (const [name, count] of Object.entries(data.summary || {})) facts.append(el("span", tr("{count} {label}", {count, label: fieldLabel(name)}), "fact")); card.append(facts, el("h2", tr("Sections")));
  card.append(tableView([["Section", x => x.path], ["Byte range", x => spanLabel(x.span)], ["Payload", x => spanLabel(x.payload_span)], ["Status", x => badge(x.status)]], data.audit?.sections || []));
  if (data.function_analysis?.available === false) card.append(el("p", data.function_analysis.reason, "source-warning"));
  node.append(card);
  for (const [title, rows] of [["Exports", data.module?.exports], ["Imports", data.module?.imports]]) {
    const group = el("div", "", "card controls"); group.append(el("h2", tr(title)), tableView([["Name", x => x.name || x.id], ["Module", x => x.module], ["Kind", x => x.kind], ["Index", x => x.item_index ?? x.index ?? x.function_index]], rows || [])); node.append(group);
  }
  const functions = el("div", "", "card controls"); functions.append(el("h2", tr("Functions")), tableView([["Function", x => x.source_index], ["Symbol", x => x.symbol || (x.export_names || []).join(", ")], ["Type", x => x.type_id], ["Kind", x => x.kind]], data.module?.functions || [])); node.append(functions);
}
$("debug-kind").onchange = () => $("debug-query").disabled = val("debug-kind") === "all";
onForm("debug-form", async () => { const value = await query("debug", {...moduleFields(), query_kind: val("debug-kind"), query: val("debug-query"), sidecar: val("debug-sidecar")}); renderDebug($("debug-result"), value); });

const tools = {
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
  setText($("tool-description"), tr(spec[2] ? "This operation changes history or removes saved data. You will review the selection before applying it." : ["fetch", "pull", "push"].includes(val("tool-action")) ? "Uses your existing client credentials and SSH agent. Authentication prompts requiring a terminal should be prepared in your local shell first." : "Runs the native client and refreshes the project. More options are available in All capabilities."));
}
function toolSelect(action, values = {}) { $("tool-action").value = action; toolFields(values); tab("tools"); }
$("tool-action").onchange = () => toolFields(); toolFields();
onForm("tool-form", async () => { const values = {}; $("tool-fields").querySelectorAll("[data-key]").forEach(input => values[input.dataset.key] = input.value); await act(val("tool-action"), values, !!tools[val("tool-action")][2], $("tool-result")); });
for (const kind of ["remotes", "stashes", "reflog"]) $("list-" + kind).onclick = () => task(async () => report($("tool-result"), await query(kind), kind));

let decoders = [new TextDecoder(), new TextDecoder()];
function argumentRow(value = "") {
  const row = el("div", "", "argument-row"), input = el("textarea"); input.rows = 1; input.value = value; input.spellcheck = false; input.setAttribute("aria-label", String(tr("Argument")));
  row.append(input, button(tr("Remove"), () => row.remove(), "quiet")); $("command-arguments").append(row);
}
function argsValue() { const args = Array.from($("command-arguments").querySelectorAll("textarea"), node => node.value); if (!args.length || !args[0]) throw new Error("Choose a command first."); return args; }
function showArgs(args) { clear($("command-arguments")); for (const value of args) argumentRow(value); }
$("add-argument").onclick = () => argumentRow();
$("add-environment").onclick = () => {
  const row = el("div", "", "environment-row"), name = el("input"), value = el("input"); name.placeholder = String(tr("Variable name")); value.placeholder = String(tr("Secret value")); value.type = "password"; value.autocomplete = "new-password";
  row.append(name, value, button(tr("Remove"), () => row.remove())); $("environment-fields").append(row);
};
onForm("command-form", async () => {
  const environment = Object.create(null); for (const row of $("environment-fields").children) { const [name, value] = row.querySelectorAll("input"); if (name.value) environment[name.value] = value.value; }
  const args = argsValue(), project = state.project;
  const job = await api("/api/jobs/start", {project, args, cwd: val("command-cwd"), environment, stdin_file: val("stdin-file"), keep_stdin: $("keep-stdin").checked, target: $("command-target").value});
  clear($("environment-fields")); if (project === state.project) followJob(job.id);
});
function followJob(id) {
  if (state.poll) clearTimeout(state.poll); state.job = id; state.revealOutput = false; $("command-streams").hidden = true; $("reveal-output").disabled = false; state.offsets = [0, 0]; decoders = [new TextDecoder(), new TextDecoder()];
  sessionSet("wasm-git-job", id); clear($("command-stdout")); clear($("command-stderr")); tab("commands"); pollJob(); task(loadJobs);
}
async function loadJobs() {
  const result = await api("/api/jobs"); const select = clear($("recent-jobs"));
  const empty = el("option", tr(result.jobs.length ? "Select a command to inspect its output." : "No commands in this session.")); empty.value = ""; select.append(empty);
  for (const job of result.jobs.filter(job => !job.project || job.project === state.project).reverse()) {
    const option = el("option", tr("{state} · {target} · {command}", {state: tr(job.running ? "Running" : "Finished"), target: job.target, command: job.command}));
    option.value = job.id; select.append(option);
  }
  select.value = state.job || "";
}
$("recent-jobs").onchange = () => { if (val("recent-jobs")) followJob(val("recent-jobs")); };
$("jobs-refresh").onclick = () => task(loadJobs);
async function pollJob() {
  const id = state.job; if (!id) return;
  try {
    const job = await api("/api/jobs/poll", {id, stdout: state.offsets[0], stderr: state.offsets[1], reveal: state.revealOutput}); if (id !== state.job) return;
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
    if (job.running || (state.revealOutput && state.offsets.some((n, i) => n < job.sizes[i]))) state.poll = setTimeout(pollJob, 400);
    else { for (const [i, key] of ["stdout", "stderr"].entries()) $("command-" + key).append(document.createTextNode(decoders[i].decode())); await loadJobs(); }
  } catch (error) { notice(error.message); $("run-command").disabled = false; }
}
$("reveal-output").onclick = () => task(async () => {
  if (!state.job || !await confirmAction("Reveal original output?", "Original data may include local paths, compiler metadata or sensitive values. Only reveal it in a private workspace.", {})) return;
  state.revealOutput = true; $("command-streams").hidden = false; $("reveal-output").disabled = true; if (state.poll) clearTimeout(state.poll); await pollJob();
});
$("cancel-command").onclick = () => task(async () => { await api("/api/jobs/cancel", {id: state.job}); });
$("remove-job").onclick = () => task(async () => { await api("/api/jobs/remove", {id: state.job}); state.job = null; sessionRemove("wasm-git-job"); setText($("command-status"), tr("Output removed.")); for (const key of ["stdout", "stderr"]) { clear($("command-" + key)); $("download-" + key).disabled = true; } $("remove-job").disabled = true; await loadJobs(); });
for (const key of ["stdout", "stderr"]) $("download-" + key).onclick = () => task(async () => {
  if (!await confirmAction("Export original output?", "Original data may include local paths, compiler metadata or sensitive values. Only export to a trusted destination.", {stream: key})) return;
  const response = await fetch(`/api/jobs/${state.job}/${key}`, {headers: {"X-Wasm-Git-Token": token, "X-Wasm-Git-Export": "confirmed"}}); if (!response.ok) throw new Error("Output download failed."); download(await response.blob(), `${key}.bin`, "application/octet-stream");
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
  state.session = await api("/api/session");
  initializeCatalog(); initializeWorkspace(); await loadProjects();
  const requested = sessionGet("wasm-git-project") || state.session.initial_project || "";
  await selectProject(requested); initializeServer(); await loadJobs();
}
task(boot);
