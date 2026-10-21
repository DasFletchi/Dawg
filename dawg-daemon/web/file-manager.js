const TEXT_LIMIT = 1024 * 1024;
const UPLOAD_LIMIT = 2 * 1024 * 1024 * 1024;
const STOPPED = new Set(["Stopped", "Crashed"]);
const BINARY_EXTENSIONS = /\.(?:jar|zip|gz|tgz|xz|7z|rar|zst|png|jpe?g|gif|webp|ico|pdf|class|mca|mcr|dat|db|sqlite|woff2?|ttf|mp[34]|ogg|wav|exe|so|dll)$/i;

function node(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(attributes)) {
    if (key === "className") element.className = value;
    else if (key === "text") element.textContent = value;
    else if (key.startsWith("on")) element.addEventListener(key.slice(2).toLowerCase(), value);
    else if (value !== undefined && value !== null && value !== false) {
      if (key in element && !key.startsWith("aria-")) element[key] = value;
      else element.setAttribute(key, value === true ? "" : String(value));
    }
  }
  for (const child of children) {
    if (child !== undefined && child !== null) element.append(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return element;
}

function button(label, action, className = "button") {
  return node("button", { type: "button", className, text: label, onclick: action });
}

function sizeLabel(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value < 1024) return `${value} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let amount = value / 1024;
  let unit = 0;
  while (amount >= 1024 && unit < units.length - 1) { amount /= 1024; unit += 1; }
  return `${amount.toFixed(amount < 10 ? 1 : 0)} ${units[unit]}`;
}

function modifiedLabel(value) {
  if (!Number.isFinite(Number(value)) || value === null) return "—";
  const date = new Date(Number(value));
  return Number.isNaN(date.getTime()) ? "—" : new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "short" }).format(date);
}

function basename(path) { return path.split("/").filter(Boolean).at(-1) || ""; }
function parentPath(path) { return path.split("/").filter(Boolean).slice(0, -1).join("/"); }
function childPath(path, name) { return path ? `${path}/${name}` : name; }
function validName(name) { return !!name && name !== "." && name !== ".." && !/[\\/\0]/.test(name); }
function validPath(path) {
  return !!path && !path.startsWith("/") && !path.includes("\\") && !path.includes("\0")
    && path.split("/").every(component => component && component !== "." && component !== "..");
}

/** A stateful, server-scoped file browser. Snapshots update permissions without
 * replacing the editor or discarding its input. All HTTP paths stay same-origin. */
export function createFileManager({ request, notify, onUnauthorized }) {
  let state = freshState();

  function freshState() {
    return {
      server: null, id: null, path: "", entries: [], writable: false,
      host: null, root: null, busy: false, loading: false, editor: null,
      upload: null, epoch: 0, listSequence: 0, editorSequence: 0, refs: null,
    };
  }

  function endpoint(suffix = "", path) {
    const base = `/api/servers/${encodeURIComponent(state.id)}/files${suffix}`;
    return path === undefined ? base : `${base}?path=${encodeURIComponent(path)}`;
  }

  function stopped() { return !!state.server && STOPPED.has(state.server.status) && !state.server.pid; }
  function writable() { return state.writable && stopped() && !state.busy && !state.loading; }

  function fail(error) {
    const message = error instanceof Error ? error.message : String(error);
    if (state.refs) { state.refs.error.textContent = message; state.refs.error.hidden = false; }
    notify(message, true);
  }

  function clearError() {
    if (state.refs) { state.refs.error.textContent = ""; state.refs.error.hidden = true; }
  }

  function updatePermissions() {
    if (!state.root) return;
    for (const control of state.root.querySelectorAll("[data-write]")) control.disabled = !writable();
    state.refs.reload.disabled = state.busy || state.loading;
    state.refs.up.disabled = state.busy || !state.path;
    state.refs.list.setAttribute("aria-busy", String(state.loading));
    state.refs.access.textContent = !stopped()
      ? "Browse and download files while the server is running. Stop it to make changes."
      : state.loading ? "Loading files…"
        : !state.writable ? "This directory is read only."
          : "Changes are available while the server is stopped.";
    if (state.editor) {
      state.refs.editorText.readOnly = !writable();
      state.refs.editorSave.disabled = !writable() || !state.editor.dirty;
      state.refs.editorClose.disabled = state.busy;
      state.refs.editorChanged.textContent = state.editor.dirty ? "Unsaved changes" : "";
    }
  }

  function writeButton(label, action, className = "button") {
    const result = button(label, action, className);
    result.setAttribute("data-write", "");
    return result;
  }

  function build() {
    const error = node("p", { className: "notice error", role: "alert", hidden: true });
    const access = node("p", { className: "muted" });
    const breadcrumbs = node("nav", { className: "breadcrumbs", "aria-label": "File path" });
    const up = button("Up", () => navigate(parentPath(state.path)));
    const reload = button("Reload", () => loadDirectory(state.path));
    const input = node("input", { type: "file", multiple: true, hidden: true, "aria-label": "Files to upload" });
    input.addEventListener("change", () => {
      const files = Array.from(input.files || []);
      input.value = "";
      if (files.length) void uploadFiles(files);
    });
    const upload = writeButton("Upload files", () => input.click());
    const newFolder = writeButton("New folder", createDirectory);
    const tableHead = node("thead", {}, node("tr", {},
      ...["Name", "Type", "Size", "Modified", "Actions"].map(value => node("th", { scope: "col", className: `file-${value.toLowerCase()}${value === "Name" || value === "Actions" ? "-cell" : ""}`, text: value }))));
    const tbody = node("tbody");
    const table = node("table", { className: "file-table" }, tableHead, tbody);
    const empty = node("p", { className: "empty muted", text: "Loading files…", role: "status" });
    const list = node("div", { className: "table-wrap" }, table, empty);
    const progressText = node("span", { "aria-live": "polite" });
    const progressBar = node("progress", { max: 100, value: 0, "aria-label": "File upload progress" });
    const cancel = button("Cancel upload", cancelUpload);
    const progress = node("div", { className: "progress", hidden: true }, progressText, progressBar, cancel);
    const editorHost = node("section", { className: "file-editor", hidden: true, "aria-label": "Text file editor" });
    const root = node("div", { className: "file-browser" },
      node("div", { className: "toolbar file-actions" }, up, reload, newFolder, upload, input),
      breadcrumbs, access, error, progress, list, editorHost);
    root.addEventListener("dragover", event => { event.preventDefault(); });
    root.addEventListener("drop", event => {
      event.preventDefault();
      const files = Array.from(event.dataTransfer?.files || []);
      if (files.length) void uploadFiles(files);
    });
    state.root = root;
    state.refs = { error, access, breadcrumbs, up, reload, tbody, table, empty, list, progress, progressText, progressBar, editorHost };
    renderBreadcrumbs();
    updatePermissions();
  }

  function mount(host, server) {
    if (!server?.config?.id) { host.replaceChildren(node("p", { className: "muted", text: "Select a server to browse its files." })); return false; }
    const different = state.id !== server.config.id;
    if (different && state.id && !leave()) return false;
    if (different) {
      const epoch = state.epoch + 1;
      state = freshState(); state.epoch = epoch;
      state.id = server.config.id; state.server = server; state.host = host;
      build();
    } else { state.server = server; state.host = host; }
    if (!host.contains(state.root)) host.replaceChildren(state.root);
    updatePermissions();
    if (different) void loadDirectory("");
    return true;
  }

  function setServer(server) {
    if (!state.id || server?.config?.id !== state.id) return;
    const wasStopped = stopped();
    state.server = server;
    if (!stopped() && state.upload) cancelUpload();
    updatePermissions();
    if (wasStopped !== stopped() && !state.busy) void loadDirectory(state.path);
  }

  function leave() {
    if (state.busy && !state.upload) {
      notify("Wait for the file operation to finish before leaving.", true);
      return false;
    }
    if (state.upload) {
      if (!window.confirm("Cancel the current upload and leave the file browser?")) return false;
      cancelUpload();
    }
    if (state.editor?.dirty) {
      if (!window.confirm(`Discard unsaved changes to ${basename(state.editor.path)}?`)) return false;
      state.editor.text = state.editor.original;
      state.editor.dirty = false;
      if (state.refs?.editorText) state.refs.editorText.value = state.editor.original;
      updatePermissions();
    }
    return true;
  }

  function reset() {
    cancelUpload();
    const epoch = state.epoch + 1;
    state.host?.replaceChildren();
    state = freshState(); state.epoch = epoch;
  }

  function renderBreadcrumbs() {
    const crumbs = state.refs.breadcrumbs;
    crumbs.replaceChildren(button("Server files", () => navigate(""), "button quiet"));
    let path = "";
    for (const component of state.path.split("/").filter(Boolean)) {
      path = childPath(path, component);
      const target = path;
      crumbs.append(node("span", { className: "muted", text: "/", "aria-hidden": "true" }),
        button(component, () => navigate(target), "button quiet"));
    }
  }

  async function navigate(path) {
    if (state.busy || (state.editor?.dirty && !leave())) return;
    state.editorSequence += 1;
    state.editor = null;
    state.refs.editorHost.hidden = true;
    state.refs.editorHost.replaceChildren();
    await loadDirectory(path);
  }

  async function loadDirectory(path) {
    if (!state.id || state.busy) return;
    const epoch = state.epoch;
    const sequence = ++state.listSequence;
    state.loading = true;
    clearError(); updatePermissions();
    try {
      const response = await request(endpoint("", path));
      const result = await response.json();
      if (epoch !== state.epoch || sequence !== state.listSequence) return;
      if (!Array.isArray(result.entries) || typeof result.path !== "string") throw new Error("The server returned an invalid file listing.");
      state.path = result.path;
      state.entries = result.entries.slice().sort((a, b) => {
        const aDirectory = a.kind === "directory" ? 0 : 1;
        const bDirectory = b.kind === "directory" ? 0 : 1;
        return aDirectory - bDirectory || String(a.name).localeCompare(String(b.name), undefined, { sensitivity: "base" });
      });
      state.writable = result.writable === true;
      renderBreadcrumbs(); renderRows();
    } catch (error) {
      if (epoch === state.epoch && sequence === state.listSequence) fail(error);
    } finally {
      if (epoch === state.epoch && sequence === state.listSequence) { state.loading = false; updatePermissions(); }
    }
  }

  function renderRows() {
    state.refs.tbody.replaceChildren();
    state.refs.empty.hidden = state.entries.length > 0;
    state.refs.empty.textContent = "This directory is empty. Use Upload files or New folder to add content.";
    for (const entry of state.entries) {
      if (typeof entry.name !== "string" || typeof entry.path !== "string") continue;
      const isDirectory = entry.kind === "directory";
      const isFile = entry.kind === "file";
      const name = button(entry.name, () => isDirectory ? navigate(entry.path) : openEditor(entry), "button quiet file-name");
      name.disabled = !isDirectory && !isFile;
      name.title = entry.path;
      const actions = node("div", { className: "file-actions" });
      if (isFile) {
        actions.append(node("a", { className: "button quiet", href: endpoint("/content", entry.path), download: entry.name, text: "Download", "aria-label": `Download ${entry.name}` }));
      }
      if (isFile || isDirectory) {
        const rename = writeButton("Rename", () => renameEntry(entry), "button quiet");
        rename.setAttribute("aria-label", `Rename ${entry.name}`);
        const remove = writeButton("Delete", () => deleteEntry(entry), "button quiet danger");
        remove.setAttribute("aria-label", `Delete ${entry.name}`);
        actions.append(rename, remove);
      }
      state.refs.tbody.append(node("tr", {},
        node("td", { className: "file-name-cell" }, name),
        node("td", { className: "muted file-type", text: isDirectory ? "Directory" : isFile ? "File" : "Symbolic link" }),
        node("td", { className: "muted file-size", text: isFile ? sizeLabel(entry.size) : "—" }),
        node("td", { className: "muted file-modified", text: modifiedLabel(entry.modified_ms) }),
        node("td", { className: "file-actions-cell" }, actions)));
    }
    updatePermissions();
  }

  async function mutate(operation, success) {
    if (!writable()) { fail(new Error("Stop the server before changing files.")); return false; }
    const epoch = state.epoch;
    state.busy = true; clearError(); updatePermissions();
    try {
      await operation();
      if (epoch !== state.epoch) return false;
      notify(success);
      return true;
    } catch (error) {
      if (epoch === state.epoch) fail(error);
      return false;
    } finally {
      if (epoch === state.epoch) { state.busy = false; updatePermissions(); }
    }
  }

  async function createDirectory() {
    if (!writable()) return;
    const name = window.prompt("New folder name");
    if (name === null) return;
    if (!validName(name)) { fail(new Error("Use a folder name without slashes, and do not use . or ...")); return; }
    const path = childPath(state.path, name);
    const success = await mutate(() => request(endpoint("/directory"), { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ path }) }), `Folder ${name} created.`);
    if (success) await loadDirectory(state.path);
  }

  async function renameEntry(entry) {
    if (!writable()) return;
    const path = window.prompt("New path, relative to the server's files", entry.path);
    if (path === null || path === entry.path) return;
    if (!validPath(path)) { fail(new Error("Use a relative path without empty, . or .. components.")); return; }
    const success = await mutate(() => request(endpoint("/rename"), { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ from: entry.path, to: path }) }), `${entry.name} renamed.`);
    if (success) {
      if (state.editor && (state.editor.path === entry.path || state.editor.path.startsWith(`${entry.path}/`))) {
        state.editor.path = path + state.editor.path.slice(entry.path.length);
        renderEditor();
      }
      await loadDirectory(state.path);
    }
  }

  async function deleteEntry(entry) {
    if (!writable()) return;
    const containsEditor = state.editor && (state.editor.path === entry.path || state.editor.path.startsWith(`${entry.path}/`));
    let message = entry.kind === "directory"
      ? `Delete the directory ${entry.name} and its contents? This cannot be undone.`
      : `Delete ${entry.name}? This cannot be undone.`;
    if (containsEditor && state.editor.dirty) message += ` Unsaved changes to ${basename(state.editor.path)} will also be discarded.`;
    if (!window.confirm(message)) return;
    const success = await mutate(() => request(endpoint("", entry.path), { method: "DELETE" }), `${entry.name} deleted.`);
    if (success) {
      if (state.editor && (state.editor.path === entry.path || state.editor.path.startsWith(`${entry.path}/`))) {
        state.editor = null; state.refs.editorHost.hidden = true; state.refs.editorHost.replaceChildren();
      }
      await loadDirectory(state.path);
    }
  }

  async function limitedText(response) {
    const length = Number(response.headers.get("content-length"));
    if (Number.isFinite(length) && length > TEXT_LIMIT) { await response.body?.cancel(); throw new Error("The editor opens text files up to 1 MiB. Download this file instead."); }
    const reader = response.body?.getReader();
    if (!reader) return { text: "", bom: false };
    const chunks = []; let total = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        total += value.byteLength;
        if (total > TEXT_LIMIT) { await reader.cancel(); throw new Error("The editor opens text files up to 1 MiB. Download this file instead."); }
        chunks.push(value);
      }
    } finally { reader.releaseLock(); }
    const bytes = new Uint8Array(total); let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    if (bytes.includes(0)) throw new Error("This is a binary file. Download it instead of opening it in the text editor.");
    try {
      return { text: new TextDecoder("utf-8", { fatal: true }).decode(bytes), bom: bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf };
    } catch { throw new Error("This file is not UTF-8 text. Download it to edit it with a suitable tool."); }
  }

  async function openEditor(entry) {
    if (state.busy) return;
    if (state.editor?.path === entry.path) { state.refs.editorText.focus(); return; }
    if (state.editor?.dirty && !leave()) return;
    if (Number(entry.size) > TEXT_LIMIT || BINARY_EXTENSIONS.test(entry.name)) {
      fail(new Error("Open a UTF-8 text file up to 1 MiB, or use Download for this file.")); return;
    }
    const epoch = state.epoch;
    const sequence = ++state.editorSequence;
    clearError();
    try {
      const response = await request(endpoint("/content", entry.path));
      const result = await limitedText(response);
      if (epoch !== state.epoch || sequence !== state.editorSequence) return;
      state.editor = { path: entry.path, text: result.text.replace(/\r\n/g, "\n"), original: result.text.replace(/\r\n/g, "\n"), dirty: false, crlf: result.text.includes("\r\n"), bom: result.bom };
      renderEditor();
      state.refs.editorText.focus();
    } catch (error) { if (epoch === state.epoch && sequence === state.editorSequence) fail(error); }
  }

  function renderEditor() {
    const editor = state.editor;
    if (!editor) return;
    const heading = node("h3", { text: editor.path });
    const changed = node("span", { className: "muted", "aria-live": "polite" });
    const save = writeButton("Save file", saveEditor, "button primary");
    const close = button("Close editor", closeEditor);
    const area = node("textarea", { className: "file-editor-content", rows: 16, spellcheck: false, wrap: "off", "aria-label": `Contents of ${editor.path}`, value: editor.text });
    area.addEventListener("input", () => {
      if (!state.editor || !writable()) return;
      state.editor.text = area.value;
      state.editor.dirty = area.value !== state.editor.original;
      updatePermissions();
    });
    area.addEventListener("keydown", event => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
        event.preventDefault(); if (writable() && state.editor?.dirty) void saveEditor();
      }
    });
    const download = node("a", { className: "button quiet", href: endpoint("/content", editor.path), download: basename(editor.path), text: "Download" });
    state.refs.editorHost.replaceChildren(node("div", { className: "toolbar" }, heading, changed, save, download, close),
      node("p", { className: "muted", text: "UTF-8 text, up to 1 MiB. Changes are saved to the server's files." }), area);
    state.refs.editorHost.hidden = false;
    Object.assign(state.refs, { editorText: area, editorSave: save, editorClose: close, editorChanged: changed });
    updatePermissions();
  }

  async function saveEditor() {
    const editor = state.editor;
    if (!editor?.dirty || !writable()) return;
    let body = editor.text;
    if (editor.crlf) body = body.replace(/\n/g, "\r\n");
    if (editor.bom) body = `\ufeff${body}`;
    if (new TextEncoder().encode(body).byteLength > TEXT_LIMIT) { fail(new Error("Text files saved by the editor must be at most 1 MiB.")); return; }
    const epoch = state.epoch;
    const success = await mutate(() => request(endpoint("/content", editor.path), { method: "PUT", headers: { "Content-Type": "text/plain; charset=utf-8" }, body }), `${basename(editor.path)} saved.`);
    if (success && epoch === state.epoch && state.editor === editor) {
      editor.original = editor.text; editor.dirty = false; updatePermissions();
      await loadDirectory(state.path);
    }
  }

  function closeEditor() {
    if (state.busy || (state.editor?.dirty && !leave())) return;
    state.editorSequence += 1; state.editor = null;
    state.refs.editorHost.replaceChildren(); state.refs.editorHost.hidden = true;
  }

  function cancelUpload() {
    if (!state.upload) return;
    state.upload.cancelled = true;
    state.upload.xhr?.abort();
  }

  function uploadOne(file, path, upload) {
    return new Promise((resolve, reject) => {
      if (upload.cancelled) { reject(new Error("Upload cancelled.")); return; }
      const xhr = new XMLHttpRequest(); upload.xhr = xhr;
      xhr.open("PUT", endpoint("/content", path));
      xhr.withCredentials = true;
      xhr.setRequestHeader("X-Dawg-Request", "1");
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      xhr.upload.addEventListener("progress", event => {
        if (state.upload !== upload) return;
        const percent = event.lengthComputable ? Math.round(event.loaded / event.total * 100) : 0;
        state.refs.progressBar.value = percent;
        state.refs.progressText.textContent = `Uploading ${file.name} (${upload.index + 1}/${upload.total}): ${sizeLabel(event.loaded)}${event.lengthComputable ? ` of ${sizeLabel(event.total)}` : ""}`;
      });
      xhr.addEventListener("load", () => {
        if (xhr.status >= 200 && xhr.status < 300) { resolve(); return; }
        if (xhr.status === 401) { onUnauthorized(); reject(new Error("Your session expired. Sign in to continue.")); return; }
        let message = `Upload failed (${xhr.status}).`;
        try { const result = JSON.parse(xhr.responseText); message = result.error || result.Error || message; }
        catch { if (xhr.responseText.trim()) message = xhr.responseText.slice(0, 500); }
        reject(new Error(message));
      });
      xhr.addEventListener("error", () => reject(new Error("The upload connection failed. Reload the directory before retrying.")));
      xhr.addEventListener("abort", () => reject(new Error("Upload cancelled.")));
      xhr.send(file);
    });
  }

  async function uploadFiles(files) {
    if (!writable()) { fail(new Error("Stop the server before uploading files.")); return; }
    for (const file of files) {
      if (!validName(file.name)) { fail(new Error(`The file name ${file.name} is not supported.`)); return; }
      if (file.size > UPLOAD_LIMIT) { fail(new Error(`${file.name} exceeds the 2 GiB upload limit.`)); return; }
      if (state.entries.some(entry => entry.name === file.name && entry.kind !== "file")) { fail(new Error(`A directory or symbolic link named ${file.name} already exists.`)); return; }
    }
    const existing = files.filter(file => state.entries.some(entry => entry.name === file.name));
    if (existing.length) {
      let message = `Replace ${existing.length} existing file${existing.length === 1 ? "" : "s"}? This cannot be undone.`;
      if (state.editor?.dirty && existing.some(file => childPath(state.path, file.name) === state.editor.path)) {
        message += ` Unsaved changes to ${basename(state.editor.path)} will also be discarded after it is replaced.`;
      }
      if (!window.confirm(message)) return;
    }
    const epoch = state.epoch;
    const destination = state.path;
    const upload = { xhr: null, cancelled: false, index: 0, total: files.length };
    state.upload = upload; state.busy = true;
    state.refs.progress.hidden = false; clearError(); updatePermissions();
    let completed = 0;
    let failure = null;
    const completedPaths = new Set();
    try {
      for (const [index, file] of files.entries()) {
        upload.index = index;
        state.refs.progressBar.value = 0;
        state.refs.progressText.textContent = `Uploading ${file.name} (${index + 1}/${files.length})…`;
        await uploadOne(file, childPath(destination, file.name), upload);
        completed += 1;
        completedPaths.add(childPath(destination, file.name));
      }
      if (epoch === state.epoch) notify(`${completed} file${completed === 1 ? "" : "s"} uploaded.`);
    } catch (error) {
      if (epoch === state.epoch) {
        if (upload.cancelled) notify(`Upload cancelled. ${completed} file${completed === 1 ? "" : "s"} completed.`);
        else failure = error;
      }
    } finally {
      if (epoch === state.epoch && state.upload === upload) {
        state.upload = null; state.busy = false; state.refs.progress.hidden = true;
        updatePermissions(); await loadDirectory(destination);
        if (state.editor && completedPaths.has(state.editor.path)) {
          state.editor = null; state.refs.editorHost.hidden = true; state.refs.editorHost.replaceChildren();
        }
        if (failure) fail(failure);
      }
    }
  }

  function hasUnsavedChanges() { return !!state.editor?.dirty || state.busy; }

  return { mount, setServer, leave, reset, hasUnsavedChanges };
}
