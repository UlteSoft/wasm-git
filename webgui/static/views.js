"use strict";
// View components consume presentation fields, never stringify machine reports.
function fieldLabel(key) { return tr(key.replaceAll("_", " ")); }
function recordView(value, depth = 0) {
  if (value === null || value === undefined) return el("span", "—", "muted");
  if (typeof value !== "object") return el("span", typeof value === "boolean" ? tr(value ? "Yes" : "No") : String(value), "record-value");
  if (Array.isArray(value)) {
    const list = el("div", "", "record-list");
    if (!value.length) { list.append(el("span", tr("No entries"), "muted")); return list; }
    let offset = 0;
    const more = button(tr("Show more"), append, "quiet");
    function append() {
      more.remove();
      for (const item of value.slice(offset, offset + 40)) {
        const row = el("div", "", "record-item"); row.append(recordView(item, depth + 1)); list.append(row);
      }
      offset += 40; if (offset < value.length) list.append(more);
    }
    append(); return list;
  }
  const list = el("dl", "", "property-list");
  for (const [key, item] of Object.entries(value)) {
    if (item === null || item === undefined || (Array.isArray(item) && !item.length)) continue;
    const row = el("div"); row.append(el("dt", fieldLabel(key)));
    const cell = el("dd");
    if (depth > 1 && typeof item === "object") {
      const detail = el("details"); detail.append(el("summary", tr("View details")));
      let loaded = false; detail.ontoggle = () => { if (detail.open && !loaded) { loaded = true; detail.append(recordView(item, depth + 1)); } }; cell.append(detail);
    } else cell.append(recordView(item, depth + 1));
    row.append(cell); list.append(row);
  }
  return list;
}
function tableView(columns, rows) {
  const wrapper = el("div", "", "table-scroll"), table = el("table"), head = el("thead"), headings = el("tr"), body = el("tbody");
  for (const [label] of columns) headings.append(el("th", tr(label)));
  head.append(headings); table.append(head, body); wrapper.append(table);
  let offset = 0;
  const more = button(tr("Show more"), append, "quiet");
  function append() {
    more.remove();
    for (const row of rows.slice(offset, offset + 100)) {
      const line = el("tr");
      for (const [, get] of columns) { const cell = el("td"), value = get(row); cell.append(value instanceof Node ? value : el("span", value ?? "—")); line.append(cell); }
      body.append(line);
    }
    offset += 100; if (offset < rows.length) wrapper.append(more);
  }
  append(); if (!rows.length) wrapper.append(el("p", tr("No entries"), "empty")); return wrapper;
}
function spanLabel(span) { return span ? `[${span.start}..${span.end})` : "—"; }
function renderDebug(node, data) {
  clear(node, "card controls"); node.append(el("h2", tr("Symbol & source mappings")));
  const info = data.debug_info || data.annotation || data;
  node.append(el("p", tr("Compiler metadata helps locate code. Original Wasm bytes remain the evidence."), "muted"));
  const symbols = info.symbols || data.symbols || data.matches || [];
  if (Array.isArray(symbols)) node.append(tableView([["Symbol", x => x.linkage_name || x.name], ["Function", x => x.function_index], ["Source", x => x.declaration?.file || x.source?.file], ["Line", x => x.declaration?.line || x.source?.line], ["Kind", x => x.kind]], symbols));
  const lines = info.lines || info.sources || data.lines || data.ranges || [];
  if (Array.isArray(lines) && lines.length) node.append(tableView([["Source", x => x.location?.file || x.file], ["Line", x => x.location?.line || x.line], ["Function", x => x.range?.function_index], ["Byte range", x => spanLabel(x.range?.file_span || x.file_span)]], lines));
  if (info.names?.length) node.append(tableView([["Symbol", x => x.name], ["Index space", x => x.index_space], ["Index", x => x.index], ["Scope", x => x.scope_index]], info.names));
  if (info.go_lines?.length) node.append(tableView([["Source", x => x.location?.file], ["Line", x => x.location?.line], ["Function", x => x.function_index], ["Go virtual PC", x => `[${x.virtual_pc?.join("..")})`]], info.go_lines));
  if (data.annotation) node.append(recordView(Object.fromEntries(Object.entries(data.annotation).filter(([key]) => !["symbols", "sources"].includes(key)))));
  if (info.diagnostics?.length) node.append(details(tr("Diagnostics"), info.diagnostics, true));
  if (!symbols.length && !lines.length && !info.names?.length && !info.go_lines?.length) node.append(el("p", tr("No mappings match this query."), "muted"));
  // Query variants (address/name/Go PC) have their own shaped evidence.
  for (const key of ["mapping", "mappings", "functions", "locations", "diagnostics", "status"]) if (data[key]) node.append(details(fieldLabel(key), data[key]));
}
function diffView(node, result, path = "") {
  clear(node, "diff-content");
  if (result.files) {
    const files = result.files.filter(file => file.status !== "unchanged");
    for (const file of files) node.append(diffCard(file.diff || {audit: file.audit}, file.path, file.status));
    if (!files.length) node.append(el("div", tr("No differences in the selected files."), "empty"));
  } else node.append(diffCard(result, path || tr("Module comparison")));
}

function pagedItems(node, items, render, limit) {
  let offset = 0;
  const more = button(tr("Show more"), append, "quiet");
  function append() {
    more.remove();
    for (const item of items.slice(offset, offset + limit)) node.append(render(item));
    offset += limit; if (offset < items.length) node.append(more);
  }
  append();
}
function bytePane(hex, span, before) {
  const pane = el("div", "", before ? "before byte-pane" : "after byte-pane");
  pane.append(el("code", `${before ? "−" : "+"} ${spanLabel(span)}`));
  const content = el("pre"), label = el("p", "", "muted"); pane.append(content, label);
  let shown = 0;
  const more = button(tr("Show more bytes"), append, "quiet");
  function append() {
    more.remove();
    const chunk = hex.slice(shown, shown + 8192); shown += chunk.length;
    content.append(document.createTextNode((chunk.match(/.{1,64}/g) || ["∅"]).join("\n") + "\n"));
    setText(label, shown < hex.length ? tr("Showing {shown} of {total} bytes", {shown: shown / 2, total: hex.length / 2}) : "");
    if (shown < hex.length) pane.append(more);
  }
  append(); return pane;
}
function byteEdits(node, edits) {
  pagedItems(node, edits, edit => {
    const item = el("div", "", "byte-edit"), row = el("div", "", "operator");
    row.append(bytePane(edit.old_hex || "", edit.old_span, true), bytePane(edit.new_hex || "", edit.new_span, false)); item.append(row);
    if (edit.old_context?.length || edit.new_context?.length) item.append(details(tr("Byte context"), {before: edit.old_context, after: edit.new_context}));
    return item;
  }, 200);
}
