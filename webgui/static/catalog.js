"use strict";
// Forms are examples, never a command whitelist. The command desk accepts the
// installed binary's entire argument grammar unchanged, including future flags.
const capabilityCatalog = [];
function recipe(group, command, args, description = "", target = "client") {
  capabilityCatalog.push({group, command, args, description, target});
}
for (const [command, args] of [
  ["version", ["--build-options"]], ["features", ["--json"]], ["init", ["NEW_DIRECTORY"]],
  ["clone", ["SOURCE_OR_URL", "NEW_DIRECTORY"]], ["fork", ["@repo", "NEW_DIRECTORY"]],
  ["status", ["@repo"]], ["add", ["@repo", "--", "FILE.wasm"]], ["rm", ["@repo", "--", "FILE.wasm"]],
  ["mv", ["@repo", "--", "OLD.wasm", "NEW.wasm"]], ["commit", ["@repo", "-m", "Describe the change"]],
  ["restore", ["@repo", "--", "FILE.wasm"]], ["reset", ["@repo", "--mixed", "HEAD"]],
  ["checkout", ["@repo", "BRANCH"]], ["switch", ["@repo", "BRANCH"]], ["branch", ["@repo", "--list", "--json"]],
  ["tag", ["@repo"]], ["stash", ["@repo", "list"]], ["config", ["@repo"]], ["clean", ["@repo", "-n"]]
]) recipe("Workspace", command, args);
for (const [command, args] of [
  ["log", ["@repo", "HEAD"]], ["show", ["@repo", "HEAD"]], ["reflog", ["@repo"]],
  ["rev-list", ["@repo", "--parents", "HEAD"]], ["rev-parse", ["@repo", "HEAD"]], ["describe", ["@repo", "HEAD"]],
  ["merge", ["@repo", "REVISION", "--record-conflict", "-m", "Merge branch"]],
  ["merge-base", ["@repo", "HEAD", "REVISION"]], ["cherry-pick", ["@repo", "COMMIT"]],
  ["revert", ["@repo", "COMMIT"]], ["rebase", ["@repo", "UPSTREAM", "--record-conflict"]],
  ["bisect", ["@repo", "status"]], ["notes", ["@repo", "list"]], ["shortlog", ["@repo", "HEAD"]],
  ["range-diff", ["@repo", "OLD_BASE..OLD_TIP", "NEW_BASE..NEW_TIP"]],
  ["mergetool", ["@repo", "--output", "NEW_DIRECTORY"]], ["worktree", ["@repo", "list"]]
]) recipe("History & recovery", command, args);
for (const [command, args] of [
  ["inspect", ["HEAD", "--repo", "@repo", "--file", "FILE.wasm", "--json"]],
  ["validate", ["FILE.wasm", "--config", "CONFIG.json", "--json"]],
  ["diff", ["HEAD^", "HEAD", "--repo", "@repo", "--format", "json"]],
  ["debug-map", ["HEAD", "--repo", "@repo", "--file", "FILE.wasm", "--json"]],
  ["patch", ["OLD.wasm", "NEW.wasm", "--output", "NEW_PATCH.wgp"]],
  ["apply", ["OLD.wasm", "PATCH.wgp", "--check"]],
  ["trace", ["@repo", "HEAD", "--file", "FILE.wasm", "--range", "0:8"]],
  ["blame", ["@repo", "HEAD", "--file", "FILE.wasm"]],
  ["diff-files", ["@repo"]], ["diff-index", ["@repo", "HEAD"]], ["diff-tree", ["@repo", "HEAD^", "HEAD"]],
  ["merge --squash", ["@repo", "REVISION", "--squash", "-m", "Squashed merge"]]
]) recipe("Wasm audit", command, args);
for (const [command, args] of [
  ["remote", ["@repo", "-v"]], ["fetch", ["@repo", "origin"]], ["pull", ["@repo", "origin", "main", "--ff-only"]],
  ["push", ["@repo", "origin", "main:main"]], ["ls-remote", ["@repo", "origin", "--json"]],
  ["clone-status", ["@repo"]], ["hydrate", ["@repo"]], ["promisor", ["@repo", "SOURCE_OR_URL"]],
  ["bundle", ["@repo", "NEW_BUNDLE.wgb"]], ["unbundle", ["@repo", "BUNDLE.wgb", "BRANCH"]]
]) recipe("Remotes & transfer", command, args);
for (const [command, args] of [
  ["gc", ["@repo"]], ["repack", ["@repo"]], ["fsck", ["@repo"]], ["count-objects", ["@repo", "-v"]],
  ["maintenance", ["@repo", "run", "--task", "verify"]], ["pool", ["status", "@repo"]],
  ["archive", ["@repo", "HEAD", "--output", "NEW_ARCHIVE.tar"]]
]) recipe("Storage & integrity", command, args);
for (const [command, args] of [
  ["keygen", ["PRIVATE_KEY", "PUBLIC_KEY"]],
  ["trust-author", ["TRUST_FILE", "AUTHOR", "PUBLIC_KEY", "--format", "native"]],
  ["untrust-author", ["TRUST_FILE", "AUTHOR"]],
  ["commit-signed", ["@repo", "FILE.wasm", "-m", "Signed change", "--author", "AUTHOR", "--key", "PRIVATE_KEY", "--format", "native"]],
  ["sign-commit", ["@repo", "EXPECTED_HEAD_ID", "--author", "AUTHOR", "--key", "PRIVATE_KEY", "--format", "native"]],
  ["verify-commit", ["@repo", "HEAD", "--trust", "TRUST_FILE"]]
]) recipe("Authors & signatures", command, args, "Native, SSH and OpenPGP options are forwarded to the installed client. Use command help for key and trust-file requirements.");
for (const [action, args] of [
  ["create", ["--source-repository", "SOURCE_REPOSITORY", "--source-branch", "feature", "--base-branch", "main", "--title", "Proposal title", "--body", "Description"]],
  ["list", ["--limit", "30"]], ["show", ["1"]], ["files", ["1", "--version", "1"]],
  ["diff", ["1", "--format", "json"]], ["update", ["1", "--version", "1", "--refresh"]],
  ["comment", ["1", "--version", "1", "--body", "Review comment"]],
  ["review", ["1", "--version", "1", "--decision", "approve", "--body", "Review details"]],
  ["check", ["1", "--version", "1", "--context", "build", "--state", "success"]],
  ["ready", ["1", "--version", "1"]], ["close", ["1", "--version", "1"]],
  ["reopen", ["1", "--version", "1"]],
  ["merge", ["1", "--version", "1", "--head", "HEAD_ID", "--base", "BASE_ID", "--method", "merge", "-m", "Merge reviewed proposal"]],
  ["recover", ["1", "--version", "1"]]
]) recipe("WasmHub proposals", `pr ${action}`, ["URL", action, ...args], "Version and expected IDs bind an action to the proposal you reviewed. The server enforces permissions and protection rules.");
for (const [command, args] of [
  ["hash-object", ["@repo", "FILE.wasm"]], ["cat-file", ["@repo", "HEAD"]], ["mktree", ["@repo"]],
  ["write-tree", ["@repo"]], ["read-tree", ["@repo", "HEAD"]], ["commit-tree", ["@repo", "TREE_ID", "-m", "Commit message"]],
  ["update-ref", ["@repo", "refs/heads/BRANCH", "NEW_ID", "EXPECTED_ID"]], ["symbolic-ref", ["@repo", "HEAD"]],
  ["check-ref-format", ["--branch", "BRANCH"]], ["ls-files", ["@repo", "--json"]],
  ["ls-tree", ["@repo", "HEAD", "--json"]], ["show-ref", ["@repo", "--json"]], ["for-each-ref", ["@repo", "--json"]],
  ["update-index", ["@repo", "--refresh"]], ["checkout-index", ["@repo", "-a", "-n"]],
  ["merge-index", ["@repo", "--all", "--dry-run"]]
]) recipe("Plumbing & index", command, args, "Batch modes use standard input; enable it in the command desk. Output can be downloaded as exact binary bytes.");
for (const [command, args] of [["compile-policy", ["INPUT_JSON", "NEW_BINARY_FILE"]], ["compile-protection", ["INPUT_JSON", "NEW_BINARY_FILE"]], ["audit-log", ["BINARY_AUDIT_FILE"]]]) recipe("Server administration", command, args, "Uses the standalone server executable. Policy and protection inputs are compiled into native binary records.", "server");
recipe("Server administration", "server", ["--root", "@root", "--listen", "127.0.0.1:9419", "--read-only"], "Start a supervised native server; stop it from the supervision dashboard.", "service");

let selectedCapability;
function expandedArgs(spec) {
  const prefix = spec.target === "service" ? [] : [spec.command.split(" ")[0]];
  return prefix.concat(spec.args.map(a => a === "@repo" ? state.repo || "REPOSITORY_DIRECTORY" : a === "@root" ? val("server-root") : a));
}
function showCapability(spec) {
  selectedCapability = spec;
  $("capability-form").hidden = false; $("capability-title").textContent = spec.command; setText($("capability-description"), spec.description ? tr(spec.description) : tr("Configure {command}, then review the exact arguments before running it.", {command: spec.command}));
  const fields = clear($("capability-fields"));
  for (const [i, value] of expandedArgs(spec).entries()) {
    const label = el("label", i === 0 && spec.target !== "service" ? tr("Command") : value.startsWith("--") ? tr("Option") : tr("Argument {index}", {index: i}));
    const input = el("input"); input.value = value; input.dataset.argument = i; if (i === 0 && spec.target !== "service") input.readOnly = true; label.append(input); fields.append(label);
  }
  $("command-template").value = spec.command;
}
function filterCapabilities() {
  const group = val("capability-group"), search = val("capability-search").toLowerCase(); const node = clear($("capabilities"));
  for (const spec of capabilityCatalog) {
    if (spec.target !== "client" && state.session.mode !== "server") continue;
    if (group !== "All capabilities" && spec.group !== group) continue;
    if (!`${spec.command} ${spec.group} ${spec.description} ${i18n.translate(spec.group)} ${i18n.translate(spec.description)}`.toLowerCase().includes(search)) continue;
    const item = button("", () => showCapability(spec), "capability"); item.append(el("strong", spec.command), el("small", tr(spec.group))); node.append(item);
  }
}
function initializeCatalog() {
  const groups = ["All capabilities", ...new Set(capabilityCatalog.filter(x => x.target === "client" || state.session.mode === "server").map(x => x.group))];
  for (const group of groups) { const option = el("option", tr(group)); option.value = group; $("capability-group").append(option); }
  dataList("command-catalog", capabilityCatalog.filter(x => x.target === "client" || state.session.mode === "server").map(x => x.command));
  document.addEventListener("languagechange", filterCapabilities);
  $("capability-group").onchange = $("capability-search").oninput = filterCapabilities; filterCapabilities();
  $("use-template").onclick = () => task(() => {
    const spec = capabilityCatalog.find(x => x.command === val("command-template") && (x.target === "client" || state.session.mode === "server"));
    if (spec) showCapability(spec); else { showArgs([val("command-template"), "--help"]); }
  });
  onForm("capability-form", () => { showArgs(Array.from($("capability-fields").querySelectorAll("input"), node => node.value)); $("command-target").value = selectedCapability.target; $("command-form").scrollIntoView({behavior: "smooth"}); });
  $("capability-help").onclick = () => task(async () => {
    const args = selectedCapability.command.startsWith("pr ") ? ["pr", "--help"] : selectedCapability.target === "service" ? ["--help"] : [selectedCapability.command.split(" ")[0], "--help"];
    const job = await api("/api/jobs/start", {args, target: selectedCapability.target === "service" ? "server" : selectedCapability.target, cwd: val("command-cwd")}); followJob(job.id);
  });
}
