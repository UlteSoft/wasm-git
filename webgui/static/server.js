"use strict";
const serverOptions = [
  ["transport", "Transport", ["HTTP", "stdio (SSH wrapper / binary protocol)"]],
  ["listen", "Listen address", "127.0.0.1:9419"], ["access", "Access mode", ["read-only", "allow-write"]],
  ["max-connections", "Connection limit (optional)", ""],
  ["token-file", "HTTP token file", ""], ["policy", "Binary access policy", ""],
  ["principal", "SSH principal (stdio only)", ""], ["allow-force", "Allow forced ref updates", false],
  ["signed-authors", "Binary author trust file", ""], ["protection", "Binary branch protection", ""],
  ["merge-author", "Merge author name", ""], ["merge-signing-key", "Merge signing key", ""],
  ["merge-signing-format", "Merge signature format", ["native", "ssh", "openpgp"]],
  ["pre-receive", "Pre-receive hook", ""], ["update", "Per-ref update hook", ""],
  ["post-receive", "Post-receive hook", ""], ["post-update", "Post-update hook", ""]
];
function serverArguments() {
  const args = ["--root", val("server-root")];
  const stdio = val("server-option-transport") !== "HTTP";
  if (stdio) args.push("--stdio");
  for (const [name] of serverOptions) {
    if (name === "transport") continue;
    const input = $("server-option-" + name);
    if (name === "access") { args.push("--" + input.value); continue; }
    if ((stdio && ["listen", "max-connections", "token-file"].includes(name)) || (!stdio && name === "principal")) continue;
    if (input.type === "checkbox") { if (input.checked) args.push("--" + name); }
    else if (input.value) args.push("--" + name, input.value);
  }
  return args;
}
function initializeServer() {
  if (state.session.mode !== "server") return;
  $("server-nav").hidden = false; $("command-target-label").hidden = false;
  setText(document.querySelector("title"), tr("wasm-git · Server supervision"));
  setText(document.querySelector(".brand small"), tr("SERVER"));
  $("server-root").value = state.session.root;
  for (const [i, [name, label, value]] of serverOptions.entries()) {
    const wrapper = el("label", tr(label)); const input = el(Array.isArray(value) ? "select" : "input"); input.id = "server-option-" + name;
    if (Array.isArray(value)) for (const item of value) { const option = el("option", tr(item)); option.value = item; input.append(option); }
    else if (typeof value === "boolean") { input.type = "checkbox"; input.checked = value; wrapper.className = "checkbox"; }
    else input.value = value;
    wrapper.append(input); $(i < 4 ? "server-launch-fields" : "server-advanced-fields").append(wrapper);
  }
  $("server-option-transport").onchange = () => {
    const stdio = val("server-option-transport") !== "HTTP";
    for (const name of ["listen", "max-connections", "token-file"]) $("server-option-" + name).parentElement.hidden = stdio;
    $("server-option-principal").parentElement.hidden = !stdio;
  };
  $("server-option-transport").onchange();
  onForm("server-root-form", loadSupervision); $("server-refresh").onclick = () => task(loadSupervision);
  onForm("server-launch-form", async () => {
    const args = serverArguments();
    if (!await confirmAction("Start this server?", "The native service will bind the address and load the access, protection, trust and hook configuration below. Settings are read at startup; restart to apply changes.", {root: val("server-root"), listen: val("server-option-listen"), access: val("server-option-access"), transport: val("server-option-transport")})) return;
    const job = await api("/api/jobs/start", {target: "service", args, cwd: val("server-root"), keep_stdin: val("server-option-transport") !== "HTTP"});
    sessionSet("wasm-git-service", job.id); await loadSupervision();
  });
  $("probe-server").onclick = () => task(async () => report($("probe-result"), (await api("/api/supervision", {operation: "probe", address: val("server-option-listen")})).result, "Listener probe"));
  for (const command of ["compile-policy", "compile-protection"]) $(command).onclick = () => { tab("commands"); showCapability(capabilityCatalog.find(c => c.command === command)); };
  onForm("server-audit-form", async () => {
    const result = (await api("/api/supervision", {operation: "audit", file: val("audit-path")})).result;
    const node = clear($("audit-result"), ""); const records = Array.isArray(result) ? result : result.records || result.events;
    if (Array.isArray(records)) {
      node.append(el("h3", tr("{count} receive records", {count: records.length})));
      for (const record of records.slice(-100).reverse()) node.append(details(`${record.operation || record.event || "receive"} · ${record.repository || record.repo || ""} · ${record.status || record.outcome || "recorded"}`, record));
      if (records.length > 100) node.append(el("p", tr("Showing the latest 100 records. Download all records below."), "muted"));
    }

  });
  tab("supervision"); task(loadSupervision);
}
async function loadSupervision() {
  if (state.session?.mode !== "server") return;
  const value = (await api("/api/supervision", {root: val("server-root")})).result;
  const running = value.services.filter(job => job.running);
  stats($("server-stats"), [["Repositories", value.repositories.length, "Under the selected server root"], ["Managed services", running.length, "Processes launched by this GUI"], ["Audit logs", value.audit_logs.length, "Binary receive records"], ["Access", tr("Local GUI"), "Loopback with a per-session token"]]);
  const services = clear($("server-services"));
  for (const job of value.services) {
    const card = el("div", "", "card controls"); const heading = el("div", "", "card-heading"); heading.append(el("h2", job.running ? tr("Managed server is running") : tr("Server exited · {code}", {code: job.exit_code})), badge(`${job.seconds}s`, job.running ? "good" : "")); card.append(heading);
    card.append(el("p", tr("Service output remains hidden until you choose to inspect it."), "muted")); const actions = el("div", "", "mini-actions");
    actions.append(button(tr("View live output"), () => { $("command-target").value = "service"; followJob(job.id); }, "secondary"));
    if (job.running) actions.append(button(tr("Stop this server"), async () => {
      if (await confirmAction("Stop the managed server?", "Active requests may be interrupted. Check receive-audit and repository state before retrying an interrupted write.", {service: job.id})) { await api("/api/jobs/cancel", {id: job.id}); await new Promise(resolve => setTimeout(resolve, 250)); await loadSupervision(); }
    }, "secondary")); card.append(actions); services.append(card);
  }
  if (!value.services.length) services.append(el("p", tr("No service has been launched by this GUI. Existing services are not adopted or stopped."), "muted"));
  const repos = clear($("server-repositories"), "card"); const heading = el("div", "", "card-heading"); heading.append(el("h2", tr("Hosted repositories")), el("span", value.root, "hash")); repos.append(heading);
  for (const repo of value.repositories) {
    const row = el("div", "", "tree-file"); row.append(el("strong", repo.name));
    const actions = el("div", "", "mini-actions"); actions.append(button(tr("Browse"), async () => { await openPath(repo.path); }), button(tr("Integrity"), async () => { await openPath(repo.path); await act("fsck", {}, false, $("supervision-result")); tab("supervision"); }), button(tr("Storage"), async () => { await openPath(repo.path); showArgs(["count-objects", repo.path, "-v"]); $("command-target").value = "client"; tab("commands"); })); row.append(actions); repos.append(row);
  }
  if (!value.repositories.length) repos.append(el("div", tr("No .wasm-git repositories found under this root."), "empty"));
  if (value.omitted_repositories) repos.append(el("p", tr("{count} additional repositories omitted from this view.", {count: value.omitted_repositories}), "muted"));
  if (value.unscanned_directories) repos.append(el("p", tr("{count} directories were not scanned due to depth or inventory limits.", {count: value.unscanned_directories}), "muted"));
  dataList("audit-files", value.audit_logs); if (!val("audit-path")) $("audit-path").value = value.audit_logs[0] || `${value.root}/receive-audit.bin`;
}
