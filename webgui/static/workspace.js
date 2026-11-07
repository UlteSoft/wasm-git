"use strict";
// Project identity is carried on every request. Paths are resolved and validated
// by the launcher; changing a form cannot redirect a pending repository action.
class StaleProject extends Error {}
let projectOperation = "open", folderParent = "", projectDrafts = new Map();
async function loadProjects() {
  state.projects = (await api("/api/projects")).projects; renderProjects();
}
function renderProjects() {
  const select = clear($("project-switch")), placeholder = el("option", tr("Choose a project")); placeholder.value = ""; select.append(placeholder);
  const opened = clear($("opened-projects")), recent = clear($("recent-projects")), search = val("project-search").toLowerCase();
  for (const project of state.projects || []) {
    if (project.opened) { const option = el("option", project.name); option.value = project.id; option.disabled = !project.available; select.append(option); }
    if (!`${project.name} ${project.path}`.toLowerCase().includes(search)) continue;
    const card = el("article", "", project.opened ? "project-card" : "recent-row"), icon = el("span", project.name.slice(0, 1).toUpperCase(), "project-avatar");
    const body = el("div", "", "project-card-body");
    const open = button(project.name, () => openProject(project), "project-name"); open.disabled = !project.available;
    body.append(open, el("p", project.path, "project-path"));
    if (!project.available) body.append(badge(tr("Folder unavailable"), "warning"));
    const actions = el("div", "", "mini-actions");
    if (project.opened) actions.append(button(tr("Close"), () => closeProject(project.id)));
    else actions.append(button(tr("Remove from recent"), async () => { await api("/api/projects", {operation: "forget", id: project.id}); await loadProjects(); }));
    card.append(icon, body, actions); (project.opened ? opened : recent).append(card);
  }
  select.value = state.project || "";
  if (!opened.childNodes.length) opened.append(el("div", tr("Your opened projects will appear here."), "empty project-empty"));
  if (!recent.childNodes.length) recent.append(el("p", tr("No recent projects."), "muted"));
}
async function openProject(project) {
  const result = await api("/api/projects", {operation: "reopen", id: project.id});
  state.projects = result.projects; await selectProject(result.project.id);
}
async function openPath(path) {
  const result = await api("/api/projects", {operation: "open", path});
  state.projects = result.projects; await selectProject(result.project.id);
}
async function closeProject(id) {
  await api("/api/projects", {operation: "close", id}); await loadProjects();
  if (state.project === id) await selectProject("");
}
async function selectProject(id) {
  if (state.project) projectDrafts.set(state.project, {message: val("commit-message"), amend: $("commit-amend").checked});
  const project = state.projects?.find(p => p.id === id && p.opened && p.available);
  state.generation++; state.project = project?.id || ""; state.repo = project?.path || ""; state.status = null; state.history = []; state.commit = null;
  sessionSet("wasm-git-project", state.project);
  if (state.poll) clearTimeout(state.poll); state.job = null; state.revealOutput = false;
  $("command-streams").hidden = true;
  for (const name of ["commits", "commit-detail", "diff-result", "module-result", "debug-result", "action-result", "tool-result", "command-stdout", "command-stderr", "command-help", "pending", "refs", "overview-stats", "working-files"]) clear($(name));
  for (const name of ["compare-file", "module-file", "debug-query", "debug-sidecar", "stdin-file", "stdin-text", "command-template", "capability-search"]) $(name).value = "";
  clear($("environment-fields")); clear($("capability-fields")); $("capability-form").hidden = true;
  $("command-cwd").value = state.repo; showArgs(["version", "--build-options"]);
  $("history-revision").value = $("module-revision").value = $("compare-new").value = "HEAD"; $("compare-old").value = "HEAD^";
  for (const name of ["run-command", "cancel-command", "remove-job", "download-stdout", "download-stderr", "reveal-output"]) $(name).disabled = name === "run-command" ? !project && state.session.mode !== "server" : true;
  $("stdin-form").hidden = true; setText($("command-status"), "");
  $("commit-form").querySelector("button[type=submit]").disabled = true;
  $("commit-message").value = projectDrafts.get(id)?.message || ""; $("commit-amend").checked = projectDrafts.get(id)?.amend || false;
  setText($("current-project-label"), project?.name || tr("Projects")); setText($("repo-title"), project?.name || tr("Changes")); $("current-branch").textContent = "";
  $("close-project").hidden = !project;
  document.querySelectorAll('.nav:not([data-tab="supervision"])').forEach(node => node.disabled = !project && !(state.session.mode === "server" && node.dataset.tab === "commands"));
  const preview = clear($("change-preview"), "card preview-empty"); preview.append(el("span", "⇄", "preview-glyph"), el("h2", tr("Select a change")), el("p", tr("Select a file to review its functions, instructions and exact byte changes.")));
  renderProjects(); tab(project ? "overview" : "projects");
  if (project) await refresh();
}
function projectDialog(operation) {
  projectOperation = operation;
  const title = {open: "Open project", create: "Create project", clone: "Clone project"}[operation];
  setText($("project-dialog-title"), tr(title)); setText($("submit-project"), tr(title));
  setText($("project-dialog-description"), tr(operation === "open" ? "Choose a folder containing .wasm-git." : "Choose a parent folder and a new project name."));
  $("project-name-label").hidden = operation === "open"; $("project-name").required = operation !== "open";
  $("project-url-label").hidden = operation !== "clone"; $("project-url").required = operation === "clone";
  $("project-url").value = ""; $("project-name").value = ""; $("folder-path").value = ""; $("folder-browser").hidden = true; $("project-dialog-error").hidden = true;
  $("project-dialog").showModal(); $("folder-path").focus();
}
async function browseFolder(path = val("folder-path")) {
  const result = await api("/api/folders", {path});
  $("folder-path").value = result.path; $("folder-current").textContent = result.path; folderParent = result.parent;
  $("folder-browser").hidden = false; const entries = clear($("folder-entries"));
  for (const folder of result.entries) {
    const row = button("", () => browseFolder(folder.path), "folder-row");
    row.append(el("span", folder.repository ? "◈" : "▱"), el("span", folder.name)); if (folder.repository) row.append(badge("wasm-git", "good")); entries.append(row);
  }
  setText($("folder-note"), tr(result.limited ? "Folder list limited to 300 entries. Enter a path directly to reach another folder." : result.repository ? "This folder is a wasm-git project." : "Choose this folder or open a subfolder."));
}
function initializeWorkspace() {
  $("project-home").onclick = () => { tab("projects"); task(loadProjects); };
  document.querySelector(".brand").onclick = event => { event.preventDefault(); tab("projects"); };
  for (const operation of ["open", "create", "clone"]) $(operation + "-project").onclick = () => projectDialog(operation);
  $("project-switch").onchange = () => task(() => selectProject(val("project-switch")));
  $("project-search").oninput = renderProjects;
  $("close-project").onclick = () => task(() => closeProject(state.project));
  $("cancel-project").onclick = () => $("project-dialog").close();
  $("browse-folder").onclick = () => task(() => browseFolder()); $("folder-up").onclick = () => task(() => browseFolder(folderParent));
  $("project-form").onsubmit = async event => {
    event.preventDefault(); const control = $("submit-project"); control.disabled = true; $("project-dialog-error").hidden = true;
    try {
      const name = val("project-name").trim();
      if (projectOperation !== "open" && (!name || name === "." || name === ".." || /[\\/]/.test(name))) throw new Error("Use a project name without path separators.");
      const path = val("folder-path").replace(/[\\/]+$/, "");
      const result = await api("/api/projects", {operation: projectOperation, path: val("folder-path"), destination: `${path}/${name}`, url: val("project-url")});
      $("project-url").value = ""; $("project-dialog").close(); state.projects = result.projects; await selectProject(result.project.id);
      if (result.warning) notice(result.warning);
    } catch (error) { setText($("project-dialog-error"), i18n.error(error.message)); $("project-dialog-error").hidden = false; }
    finally { control.disabled = false; }
  };
}
