// ocmini browser client. Sessions, prompts and live updates use the OpenCode v2
// HTTP/SSE API; /web/api/fs/* backs the file tree and editor. No build step.
"use strict";
(() => {
  const $ = (selector) => document.querySelector(selector);
  const MUTATING_TOOLS = new Set(["edit", "write", "apply_patch", "patch", "multiedit", "shell", "bash"]);
  const state = {
    dir: "",
    sep: "/",
    sid: null,
    session: null,
    sessions: [],
    sessionCursor: null,
    messages: [],
    byId: new Map(),
    olderCursor: null,
    pending: new Map(),
    permissions: new Map(),
    forms: new Map(),
    busy: new Set(),
    models: [],
    agents: [],
    commands: [],
    skills: [],
    attachments: [],
    selectedSkills: [],
    selection: null,
    files: [],
    active: null,
    tree: null,
    treeSelected: null,
    scope: "all",
    showArchived: false,
    showThinking: true,
    toolView: "brief",
    theme: "system",
    promptHistory: [],
    historyIndex: -1,
    historyDraft: "",
    sandbox: null,
    sandboxChoices: null,
    sandboxPick: "",
    remote: false,
  };

  // ---------- helpers ----------
  function h(tag, props, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
      if (value == null || value === false) continue;
      if (key === "class") node.className = value;
      else if (key === "text") node.textContent = value;
      else if (key === "html") node.innerHTML = value;
      else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
      else if (key in node && typeof value !== "string") node[key] = value;
      else node.setAttribute(key, value === true ? "" : value);
    }
    for (const child of children.flat()) {
      if (child == null || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }
  const esc = (text) =>
    String(text).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

  function toast(message, error = false, onClick = null) {
    const props = {
      class: "toast" + (error ? " error" : "") + (onClick ? " clickable" : ""),
      role: onClick ? "button" : error ? "alert" : "status",
      text: onClick ? message + " — click to open" : message,
    };
    if (onClick) {
      props.tabindex = 0;
      const activateToast = () => { node.remove(); onClick(); };
      props.onclick = activateToast;
      props.onkeydown = (e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); activateToast(); }
      };
    }
    const node = h("div", props);
    $("#toasts").append(node);
    setTimeout(() => node.remove(), error ? 8000 : onClick ? 8000 : 3500);
  }
  const fail = (error) => {
    console.error(error);
    toast(error && error.message ? error.message : String(error), true);
  };

  async function api(method, path, body) {
    const init = { method, headers: {}, cache: "no-store" };
    if (body !== undefined) {
      init.headers["content-type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const response = await fetch(path, init);
    if (response.status === 204) return null;
    const type = response.headers.get("content-type") || "";
    const data = type.includes("json") ? await response.json() : await response.text();
    if (!response.ok) {
      const message = (data && (data.message || (data.data && data.data.message))) || response.statusText;
      const error = new Error(message);
      error.status = response.status;
      error.data = data;
      throw error;
    }
    return data;
  }
  const q = (params) =>
    Object.entries(params)
      .filter(([, v]) => v != null && v !== "")
      .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
      .join("&");
  const located = () => q({ "location[directory]": state.dir });
  const sessionPath = (sid, rest = "") => "/api/session/" + encodeURIComponent(sid) + rest;

  function relative(path) {
    if (!path) return "";
    const root = state.dir.endsWith(state.sep) ? state.dir : state.dir + state.sep;
    return path.startsWith(root) ? path.slice(root.length) : path;
  }
  function join(dir, name) {
    if (isAbsolute(name)) return name;
    return dir.replace(/[\\/]+$/, "") + state.sep + name;
  }
  const isAbsolute = (path) => /^([A-Za-z]:)?[\\/]/.test(path);
  // Tools accept paths relative to the session directory; the file API needs absolute ones.
  const resolve = (path) => (isAbsolute(path) ? path : join(state.dir, path));
  // Paths are truncated from the left with direction: rtl; a leading LRM keeps them in order.
  const ltr = (text) => "\u200e" + text;
  const basename = (path) => path.split(/[\\/]/).filter(Boolean).pop() || path;
  function fileUri(path) {
    const parts = path.replace(/\\/g, "/").split("/").map(encodeURIComponent).join("/");
    return "file://" + (parts.startsWith("/") ? "" : "/") + parts;
  }
  function ago(ms) {
    if (!ms) return "";
    const s = Math.max(0, (Date.now() - ms) / 1000);
    if (s < 60) return "now";
    if (s < 3600) return Math.floor(s / 60) + "m";
    if (s < 86400) return Math.floor(s / 3600) + "h";
    if (s < 86400 * 30) return Math.floor(s / 86400) + "d";
    return new Date(ms).toLocaleDateString();
  }
  function compact(n) {
    if (!n) return "0";
    return n >= 1e6 ? (n / 1e6).toFixed(1) + "M" : n >= 1e3 ? (n / 1e3).toFixed(1) + "k" : String(n);
  }
  function usage(tokens, cost) {
    if (!tokens) return "";
    const total = (tokens.input || 0) + (tokens.output || 0) + (tokens.reasoning || 0) +
      ((tokens.cache && (tokens.cache.read || 0) + (tokens.cache.write || 0)) || 0);
    if (!total && !cost) return "";
    return compact(total) + " tokens" + (cost ? " · $" + cost.toFixed(cost < 1 ? 4 : 2) : "");
  }
  const store = {
    get(key) { try { return localStorage.getItem("ocmini." + key); } catch { return null; } },
    set(key, value) { try { localStorage.setItem("ocmini." + key, value); } catch { /* private mode */ } },
  };
  const narrow = () => matchMedia("(max-width: 900px)").matches;

  // ---------- markdown ----------
  function inline(text) {
    // Code spans and links become placeholders so emphasis never edits inside them.
    const held = [];
    const hold = (html) => "\u0000" + (held.push(html) - 1) + "\u0000";
    const link = (label, url) => hold('<a href="' + url + '" target="_blank" rel="noopener noreferrer">' + label + "</a>");
    let out = esc(text)
      .replace(/`([^`\n]+)`/g, (_, code) => hold("<code>" + code + "</code>"))
      .replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, (_, label, url) => link(label, url))
      .replace(/(^|[\s(])(https?:\/\/[^\s<)\u0000]+)/g, (_, before, url) => before + link(url, url));
    out = out
      .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
      .replace(/__([^_\n]+)__/g, "<strong>$1</strong>")
      .replace(/(^|[^*\w])\*([^*\s][^*\n]*)\*(?!\w)/g, "$1<em>$2</em>")
      .replace(/(^|[^_\w])_([^_\s][^_\n]*)_(?!\w)/g, "$1<em>$2</em>")
      .replace(/~~([^~\n]+)~~/g, "<del>$1</del>");
    return out.replace(/\u0000(\d+)\u0000/g, (_, n) => held[+n]);
  }
  const LIST = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
  const blockStart = (line) => /^\s*(```|~~~|#{1,6}\s|>|([-*+]|\d+[.)])\s)/.test(line) || /^\s*(---+|\*\*\*+|___+)\s*$/.test(line);
  function markdownBlocks(source) {
    const lines = String(source).split("\n");
    const out = [];
    for (let i = 0; i < lines.length; ) {
      const line = lines[i];
      const fence = line.match(/^\s*(`{3,}|~{3,})\s*([^\s`]*)/);
      if (fence) {
        const body = [];
        for (i++; i < lines.length && !lines[i].trim().startsWith(fence[1]); i++) body.push(lines[i]);
        i++;
        out.push('<pre class="code"><div class="bar"><span>' + esc(fence[2] || "") +
          '</span><button type="button" data-copy>Copy</button></div><code>' + esc(body.join("\n")) + "</code></pre>");
        continue;
      }
      const heading = line.match(/^(#{1,6})\s+(.*)$/);
      if (heading) {
        out.push("<h" + heading[1].length + ">" + inline(heading[2]) + "</h" + heading[1].length + ">");
        i++;
        continue;
      }
      if (/^\s*(---+|\*\*\*+|___+)\s*$/.test(line)) { out.push("<hr>"); i++; continue; }
      if (/^\s*>/.test(line)) {
        const body = [];
        for (; i < lines.length && /^\s*>/.test(lines[i]); i++) body.push(lines[i].replace(/^\s*>\s?/, ""));
        out.push("<blockquote>" + markdown(body.join("\n")) + "</blockquote>");
        continue;
      }
      if (line.includes("|") && i + 1 < lines.length && /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/.test(lines[i + 1])) {
        const cells = (row) => row.trim().replace(/^\||\|$/g, "").split("|").map((c) => inline(c.trim()));
        const head = cells(line);
        const rows = [];
        for (i += 2; i < lines.length && lines[i].includes("|") && lines[i].trim(); i++) rows.push(cells(lines[i]));
        out.push("<table><thead><tr>" + head.map((c) => "<th>" + c + "</th>").join("") + "</tr></thead><tbody>" +
          rows.map((r) => "<tr>" + r.map((c) => "<td>" + c + "</td>").join("") + "</tr>").join("") + "</tbody></table>");
        continue;
      }
      const item = line.match(LIST);
      if (item) {
        const ordered = /\d/.test(item[2]);
        const items = [];
        for (; i < lines.length; i++) {
          const m = lines[i].match(LIST);
          if (m && m[1].length <= item[1].length) items.push(m[3]);
          else if (lines[i].trim() && items.length && /^\s+/.test(lines[i])) items[items.length - 1] += "\n" + lines[i].trim();
          else break;
        }
        const tag = ordered ? "ol" : "ul";
        out.push("<" + tag + ">" + items.map((t) => "<li>" + markdown(t).replace(/^<p>([\s\S]*)<\/p>$/, "$1") + "</li>").join("") + "</" + tag + ">");
        continue;
      }
      if (!line.trim()) { i++; continue; }
      const para = [];
      for (; i < lines.length && lines[i].trim() && !(para.length && blockStart(lines[i])); i++) para.push(inline(lines[i]));
      out.push("<p>" + para.join("<br>") + "</p>");
    }
    return out;
  }
  function markdown(source) {
    return markdownBlocks(source).join("");
  }
  function updateMarkdown(target, text) {
    const next = markdownBlocks(text || "");
    const prev = target._blocks;
    if (!prev || prev.length !== target.children.length) {
      target.innerHTML = next.join("");
      target._blocks = next;
      return;
    }
    let common = 0;
    while (common < prev.length && common < next.length && prev[common] === next[common]) common++;
    while (target.children.length > next.length) target.lastElementChild.remove();
    const tpl = document.createElement("template");
    for (let i = common; i < next.length; i++) {
      tpl.innerHTML = next[i];
      const fresh = tpl.content.firstElementChild;
      if (!fresh) continue;
      if (i < target.children.length) target.children[i].replaceWith(fresh);
      else target.append(fresh);
    }
    target._blocks = next;
  }

  // ---------- sessions ----------
  async function loadSessions(append = false) {
    const search = $("#session-search").value.trim();
    const params = append && state.sessionCursor
      ? { cursor: state.sessionCursor }
      : {
          directory: state.scope === "project" ? state.dir : null,
          parentID: "null",
          archived: state.showArchived ? "true" : "false",
          limit: 50,
          search,
        };
    const result = await api("GET", "/api/session?" + q(params));
    state.sessions = append ? state.sessions.concat(result.data) : result.data;
    state.sessionCursor = result.cursor && result.cursor.next;
    const current = state.session && state.sessions.find((s) => s.id === state.sid);
    if (current && current.title !== state.session.title) {
      state.session.title = current.title;
      renderHeader();
    }
    renderSessions();
  }
  let sessionsTimer = 0;
  const reloadSessions = () => {
    clearTimeout(sessionsTimer);
    sessionsTimer = setTimeout(() => loadSessions().catch(fail), 250);
  };
  const sessionDir = (s) => (s.location && s.location.directory) || "";
  function renderSessions() {
    document.querySelectorAll(".scope button[data-scope]").forEach((b) => {
      const active = b.dataset.scope === state.scope;
      b.classList.toggle("active", active);
      b.setAttribute("aria-checked", String(active));
    });
    const archivedBtn = $("#archived-toggle");
    if (archivedBtn) {
      archivedBtn.setAttribute("aria-pressed", String(state.showArchived));
      archivedBtn.classList.toggle("active", state.showArchived);
    }
    const list = $("#sessions");
    const rows = [];
    // Sessions arrive newest first, so projects are ordered by their latest activity.
    const groups = new Map();
    for (const s of state.sessions) {
      const key = state.scope === "all" ? sessionDir(s) : "";
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(s);
    }
    for (const [dir, sessions] of groups) {
      if (state.scope === "all") {
        rows.push(h("li", { class: "group" + (dir === state.dir ? " current" : ""), title: "Switch to " + dir, tabindex: 0,
          onclick: () => dir !== state.dir && setDirectory(dir).catch(fail),
          onkeydown: (e) => { if (e.key === "Enter" && dir !== state.dir) setDirectory(dir).catch(fail); } },
        basename(dir) || dir));
      }
      rows.push(...sessions.map(sessionRow));
    }
    list.replaceChildren(...rows);
    if (!state.sessions.length) {
      list.append(h("li", { class: "muted", text: state.showArchived ? "No archived sessions" : "No sessions yet" }));
    }
    $("#more-sessions").hidden = !state.sessionCursor;
  }
  function sessionRow(s) {
    const isArchived = !!(state.showArchived || (s.time && s.time.archived));
    return (
        h("li", { "data-sid": s.id, class: s.id === state.sid ? "active" : "", tabindex: 0, title: s.title, onclick: () => openSession(s.id),
          onkeydown: (e) => { if (e.key === "Enter") openSession(s.id); } },
          state.busy.has(s.id) ? h("span", { class: "dot", title: "Working" }) : null,
          h("span", { class: "name", text: s.title || s.id }),
          h("span", { class: "when", text: ago(s.time && s.time.updated) }),
          h("span", { class: "tools" },
            h("button", { type: "button", title: "Rename", "aria-label": "Rename", text: "✎",
              onclick: (e) => { e.stopPropagation(); renameSession(s); } }),
            isArchived
              ? h("button", { type: "button", title: "Unarchive", "aria-label": "Unarchive", text: "↩",
                  onclick: (e) => { e.stopPropagation(); unarchiveSession(s); } })
              : h("button", { type: "button", title: "Archive", "aria-label": "Archive", text: "⌫",
                  onclick: (e) => { e.stopPropagation(); archiveSession(s); } }),
            h("button", { type: "button", title: "Delete", "aria-label": "Delete", text: "✕",
              onclick: (e) => { e.stopPropagation(); deleteSession(s); } }))));
  }
  function updateSessionBusy(sid) {
    const row = Array.from($("#sessions").children).find((el) => el.dataset && el.dataset.sid === sid);
    if (!row) return;
    const busy = state.busy.has(sid);
    const dot = row.querySelector(":scope > .dot");
    if (busy && !dot) row.prepend(h("span", { class: "dot", title: "Working" }));
    else if (!busy && dot) dot.remove();
  }
  async function renameSession(s) {
    const title = prompt("Session title", s.title);
    if (title == null || !title.trim()) return;
    await api("PATCH", sessionPath(s.id), { title: title.trim() }).catch(fail);
    if (state.session && state.session.id === s.id) state.session.title = title.trim();
    renderHeader();
    reloadSessions();
  }
  async function archiveSession(s) {
    await api("PATCH", sessionPath(s.id), { archived: true }).catch(fail);
    if (s.id === state.sid) clearSession();
    reloadSessions();
  }
  async function unarchiveSession(s) {
    await api("PATCH", sessionPath(s.id), { archived: false }).catch(fail);
    toast("Unarchived " + (s.title || s.id));
    reloadSessions();
  }
  async function deleteSession(s) {
    if (!confirm(`Delete "${s.title}" and its subagent sessions? This cannot be undone.`)) return;
    await api("DELETE", sessionPath(s.id)).catch(fail);
    if (s.id === state.sid) clearSession();
    reloadSessions();
  }

  function clearSession() {
    stopWatching();
    state.sid = null;
    state.session = null;
    state.messages = [];
    state.byId.clear();
    state.pending.clear();
    state.permissions.clear();
    state.forms.clear();
    state.olderCursor = null;
    state.sandbox = null;
    if (location.hash) history.replaceState(null, "", location.pathname);
    renderAll();
  }

  // Events for a session arriving while its snapshot loads are replayed on top of
  // it; otherwise the snapshot would erase anything that happened in between.
  let loading = null;
  async function openSession(sid) {
    if (narrow()) $("#app").classList.remove("sidebar-open");
    showView("chat");
    const switching = sid !== state.sid;
    stopWatching();
    state.sid = sid;
    if (switching) {
      state.messages = [];
      state.byId.clear();
      state.pending.clear();
      state.permissions.clear();
      state.forms.clear();
    }
    history.replaceState(null, "", "#" + sid);
    const buffer = { sid, events: [] };
    loading = buffer;
    try {
      const [session, page, permissions, forms, inbox, active] = await Promise.all([
        api("GET", sessionPath(sid)),
        api("GET", sessionPath(sid, "/message?order=desc&limit=80")),
        api("GET", sessionPath(sid, "/permission")),
        api("GET", sessionPath(sid, "/form")),
        api("GET", sessionPath(sid, "/inbox")),
        api("GET", "/api/session/active"),
      ]);
      if (state.sid !== sid) {
        if (loading === buffer) loading = null;
        return;
      }
      state.session = session.data;
      if (session.data.location && session.data.location.directory !== state.dir) {
        await setDirectory(session.data.location.directory, true);
      }
      state.messages = [];
      state.byId.clear();
      for (const m of page.data.slice().reverse()) addMessage(m);
      syncPromptHistory();
      state.olderCursor = page.cursor && page.cursor.next;
      state.pending.clear();
      for (const item of inbox.data) state.pending.set(item.id, item);
      state.permissions = new Map(permissions.data.map((p) => [p.id, p]));
      state.forms = new Map(forms.data.map((f) => [f.id, f]));
      state.busy = new Set(Object.keys(active.data));
      state.sandbox = null;
      loadSessionSandbox(sid);
      renderAll();
      if (loading === buffer) loading = null;
      for (const event of buffer.events) handle(event);
      scrollToBottom(true);
      watchRemote(sid);
    } catch (error) {
      if (loading === buffer) loading = null;
      if (error.status === 404) {
        clearSession();
        toast("That session no longer exists", true);
      } else fail(error);
    }
  }
  async function loadOlder() {
    if (!state.olderCursor || !state.sid) return;
    const sid = state.sid;
    const page = await api("GET", sessionPath(sid, "/message?" + q({ cursor: state.olderCursor, limit: 80 })));
    if (sid !== state.sid) return;
    const box = $("#transcript");
    const height = box.scrollHeight;
    const older = page.data.slice().reverse().filter((m) => !state.byId.has(m.id));
    for (const m of older) state.byId.set(m.id, m);
    state.messages = older.concat(state.messages);
    state.olderCursor = page.cursor && page.cursor.next;
    const nodes = older.map(renderMessage).filter(Boolean);
    if (nodes.length) $("#messages").prepend(...nodes);
    $("#older").hidden = !state.olderCursor;
    $("#empty").hidden = !!(state.messages.length || state.pending.size);
    box.scrollTop += box.scrollHeight - height;
  }
  function addMessage(m) {
    if (state.byId.has(m.id)) return state.byId.get(m.id);
    state.messages.push(m);
    state.byId.set(m.id, m);
    return m;
  }

  async function ensureSession() {
    if (state.sid) return state.sid;
    const body = { location: { directory: state.dir }, agent: $("#agent").value || "build" };
    if (state.selection) body.model = state.selection;
    if (state.sandboxPick) body.sandbox = state.sandboxPick;
    const created = await api("POST", "/api/session", body);
    state.sid = created.data.id;
    state.session = created.data;
    state.sandboxPick = "";
    loadSessionSandbox(state.sid);
    history.replaceState(null, "", "#" + state.sid);
    reloadSessions();
    renderAll();
    return state.sid;
  }

  // ---------- catalogs ----------
  async function loadCatalogs() {
    const [models, agents, commands, skills] = await Promise.allSettled([
      api("GET", "/api/model?" + located()),
      api("GET", "/api/agent?" + located()),
      api("GET", "/api/command?" + located()),
      api("GET", "/api/skill?" + located()),
    ]);
    state.models = models.status === "fulfilled" ? models.value.data : [];
    if (models.status === "rejected") toast("Models: " + models.reason.message, true);
    state.agents = agents.status === "fulfilled" ? agents.value.data.filter((a) => a.mode !== "subagent" && !a.hidden) : [];
    state.commands = commands.status === "fulfilled" ? commands.value.data : [];
    state.skills = skills.status === "fulfilled" ? skills.value.data : [];
    renderSelectors();
    renderSkillPicker();
  }
  function renderSkillPicker() {
    const pick = $("#skill-pick");
    if (!pick) return;
    pick.hidden = !state.skills.length;
    pick.replaceChildren(
      h("option", { value: "", text: "+ Skill" }),
      ...state.skills.map((s) => h("option", { value: s.id, text: s.name || s.id, title: s.description || "" })));
    pick.value = "";
  }
  const modelKey = (m) => m ? m.providerID + "/" + m.id : "";
  // An existing session without a selection runs on the configured default.
  function currentModel() {
    return state.session ? state.session.model : state.selection;
  }
  function renderSelectors() {
    const agent = $("#agent");
    const selectedAgent = (state.session && state.session.agent) || agent.value || "build";
    const agents = state.agents.length ? state.agents : [{ id: "build", name: "build" }];
    agent.replaceChildren(...agents.map((a) => h("option", { value: a.id, text: a.name, title: a.description || "" })));
    agent.value = selectedAgent;
    if (agent.value !== selectedAgent) {
      agent.append(h("option", { value: selectedAgent, text: selectedAgent }));
      agent.value = selectedAgent;
    }

    const model = $("#model");
    const current = currentModel();
    const groups = new Map();
    for (const m of state.models) {
      if (!groups.has(m.providerID)) groups.set(m.providerID, []);
      groups.get(m.providerID).push(m);
    }
    model.replaceChildren(
      h("option", { value: "", text: "Default model" }),
      ...[...groups].map(([provider, list]) =>
        h("optgroup", { label: provider }, ...list.map((m) => h("option", { value: modelKey(m), text: m.name || m.id })))));
    model.value = modelKey(current);
    if (current && model.value !== modelKey(current)) {
      model.append(h("option", { value: modelKey(current), text: modelKey(current) }));
      model.value = modelKey(current);
    }
    const info = current && state.models.find((m) => modelKey(m) === modelKey(current));
    const variant = $("#variant");
    const variants = (info && info.variants) || [];
    variant.hidden = !variants.length;
    variant.replaceChildren(h("option", { value: "", text: "Default variant" }),
      ...variants.map((v) => h("option", { value: v.id, text: v.id })));
    variant.value = (current && current.variant) || "";
  }
  async function selectModel() {
    const key = $("#model").value;
    const found = state.models.find((m) => modelKey(m) === key);
    if (!found) return;
    const selection = { providerID: found.providerID, id: found.id };
    const variant = $("#variant").value;
    if (variant && (found.variants || []).some((v) => v.id === variant)) selection.variant = variant;
    state.selection = selection;
    store.set("model", JSON.stringify(selection));
    if (state.sid) {
      await api("POST", sessionPath(state.sid, "/model"), { model: selection }).catch(fail);
      if (state.session) state.session.model = selection;
    }
    renderSelectors();
  }
  async function selectAgent() {
    const agent = $("#agent").value;
    if (!state.sid) return;
    await api("POST", sessionPath(state.sid, "/agent"), { agent }).catch(fail);
    if (state.session) state.session.agent = agent;
  }

  // ---------- transcript rendering ----------
  function nearBottom() {
    const box = $("#transcript");
    return box.scrollHeight - box.scrollTop - box.clientHeight < 80;
  }
  function scrollToBottom(force) {
    const box = $("#transcript");
    if (force || stick) box.scrollTop = box.scrollHeight;
  }
  let stick = true;

  function renderAll() {
    renderHeader();
    renderSessions();
    renderSelectors();
    renderDisplayControls();
    renderMessages();
    renderRequests();
    renderStatus();
    renderSandbox();
  }
  function renderHeader() {
    $("#session-title").textContent = state.session ? state.session.title || state.session.id : "New session";
    $("#usage").textContent = state.session ? usage(state.session.tokens, state.session.cost) : "";
    const busy = !!(state.sid && state.busy.has(state.sid));
    $("#compact").disabled = $("#fork").disabled = $("#instructions").disabled = !state.sid;
    $("#move-session").disabled = !state.sid || busy;
    const parentBtn = $("#parent-session");
    if (parentBtn) parentBtn.hidden = !(state.session && state.session.parentID);
    $("#project").textContent = ltr(state.dir);
    $("#project").title = "Project: " + state.dir + " (click to change)";
    $("#empty-dir").textContent = state.dir;
    document.title = (state.session ? state.session.title + " · " : "") + "ocmini";
  }
  function renderDisplayControls() {
    $("#chat").classList.toggle("hide-thinking", !state.showThinking);
    const thinkBtn = $("#toggle-thinking");
    if (thinkBtn) {
      thinkBtn.textContent = "Thinking: " + (state.showThinking ? "on" : "off");
      thinkBtn.setAttribute("aria-pressed", String(state.showThinking));
    }
    const toolsBtn = $("#toggle-tools");
    if (toolsBtn) toolsBtn.textContent = "Tools: " + state.toolView;
  }
  function toggleThinking() {
    state.showThinking = !state.showThinking;
    store.set("thinking", state.showThinking ? "on" : "off");
    renderDisplayControls();
  }
  function cycleToolView() {
    const order = ["brief", "full", "grouped"];
    state.toolView = order[(order.indexOf(state.toolView) + 1) % order.length];
    store.set("toolView", state.toolView);
    renderDisplayControls();
    renderMessages();
  }
  function applyTheme(theme) {
    state.theme = ["system", "light", "dark"].includes(theme) ? theme : "system";
    if (state.theme === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", state.theme);
    const btn = $("#theme-toggle");
    if (btn) {
      const icon = { system: "◐", light: "☀", dark: "☾" }[state.theme];
      btn.textContent = icon;
      btn.title = "Theme: " + state.theme + " (click to switch)";
      btn.setAttribute("aria-label", "Theme: " + state.theme);
    }
  }
  function cycleTheme() {
    const order = ["system", "light", "dark"];
    const next = order[(order.indexOf(state.theme) + 1) % order.length];
    store.set("theme", next);
    applyTheme(next);
  }
  function syncPromptHistory() {
    state.promptHistory = [];
    for (const m of state.messages) {
      if (m.type === "user" && m.text && m.text.trim()) {
        const text = m.text.trim();
        if (state.promptHistory[state.promptHistory.length - 1] !== text) state.promptHistory.push(text);
      }
    }
    state.historyIndex = -1;
    state.historyDraft = "";
  }
  function renderMessages() {
    const box = $("#messages");
    box.replaceChildren(...state.messages.map(renderMessage).filter(Boolean));
    renderPending();
    $("#older").hidden = !state.olderCursor;
  }
  function renderPending() {
    const box = $("#pending");
    box.replaceChildren(...[...state.pending.values()].filter((item) => item.type !== "compaction" && item.type !== "move").map((item) => {
      const payload = item.payload || {};
      const nextDelivery = item.delivery === "queue" ? "steer" : "queue";
      return h("div", { class: "msg user pending" },
        h("div", { class: "role" }, (item.delivery === "queue" ? "Queued" : "Steer") + " · ",
          h("span", { class: "tools shown" },
            h("button", { type: "button", class: "link",
              text: nextDelivery === "steer" ? "Switch to steer" : "Switch to queue",
              onclick: () => toggleInboxDelivery(item.id, nextDelivery) }),
            h("button", { type: "button", class: "link", text: "Cancel", onclick: () => cancelInput(item.id) }))),
        h("div", { class: "bubble", text: payload.text || "" }),
        fileChips(payload.files));
    }));
    $("#empty").hidden = !!(state.messages.length || state.pending.size);
  }
  function fileChips(files) {
    if (!files || !files.length) return null;
    return h("div", { class: "files" }, ...files.map((f) => h("span", { class: "chip", text: "📎 " + (f.name || "file") })));
  }
  function renderMessage(m) {
    let node;
    switch (m.type) {
      case "user":
      case "synthetic": {
        const meta = m.metadata || {};
        const label = m.type === "synthetic" ? (meta.source === "subagent" ? "Subagent report" : "Synthetic") : "You";
        node = h("div", { class: "msg " + m.type },
          h("div", { class: "role" }, label,
            m.type === "user" ? h("span", { class: "tools" },
              h("button", { type: "button", class: "link", text: "Rewind here", title: "Remove this message and everything after it",
                onclick: () => rewind(m) }),
              h("button", { type: "button", class: "link", text: "Fork", title: "Start a new session from before this message",
                onclick: () => forkSession(m.id) })) : null),
          h("div", { class: "bubble", text: m.text || "" }),
          fileChips(m.files));
        break;
      }
      case "assistant": {
        node = h("div", { class: "msg assistant" });
        node.append(h("div", { class: "role", text: [m.agent, m.model && m.model.id].filter(Boolean).join(" · ") }));
        const body = h("div", { class: "content" });
        m.content = m.content || [];
        if (state.toolView === "grouped") {
          let run = [];
          const flushTools = () => {
            if (!run.length) return;
            body.append(renderToolGroup(run));
            run = [];
          };
          for (const block of m.content) {
            if (!block) continue;
            if (block.type === "tool") run.push(block);
            else { flushTools(); body.append(renderBlock(block)); }
          }
          flushTools();
        } else {
          m.content.forEach((block) => block && body.append(renderBlock(block)));
        }
        node.append(body);
        if (m.error) node.append(h("div", { class: "error", text: m.error.message || "Failed" }));
        const footer = m.time && m.time.completed ? usage(m.tokens, m.cost) : "";
        if (footer) node.append(h("div", { class: "footer", text: footer }));
        break;
      }
      case "info-card":
        node = h("div", { class: "info-card" },
          h("div", { class: "info-head" },
            h("h3", { text: m.title || "" }),
            h("button", { type: "button", class: "icon", "aria-label": "Dismiss", text: "✕",
              onclick: () => {
                const idx = state.messages.indexOf(m);
                if (idx !== -1) state.messages.splice(idx, 1);
                state.byId.delete(m.id);
                node.remove();
                $("#empty").hidden = !!(state.messages.length || state.pending.size);
              } })),
          m.bodyNode ? m.bodyNode.cloneNode(true) : h("div", { class: "md", html: markdown(m.text || "") }));
        break;
      case "compaction":
        node = h("div", { class: "notice" },
          m.status === "failed" ? h("span", { class: "error", text: "Compaction failed: " + ((m.error && m.error.message) || "") })
            : m.summary ? h("details", {}, h("summary", { text: "Context compacted" }), h("div", { class: "md", html: markdown(m.summary) }))
              : "Compacting context…");
        break;
      case "model-switched":
        node = h("div", { class: "notice", text: "Model: " + modelKey(m.model) });
        break;
      case "agent-switched":
        node = h("div", { class: "notice", text: "Agent: " + m.agent });
        break;
      case "notice":
        node = h("div", { class: "notice" + (m.error ? " error" : ""), text: m.text });
        break;
      default:
        return null;
    }
    m.node = node;
    return node;
  }
  function renderBlock(block) {
    let node;
    if (block.type === "text") {
      node = h("div", { class: "md" });
      updateMarkdown(node, block.text || "");
    } else if (block.type === "reasoning") {
      const md = h("div", { class: "md" });
      updateMarkdown(md, block.text || "");
      node = h("details", { class: "reasoning", open: !!block.open, ontoggle: (e) => { block.open = e.target.open; } },
        h("summary", { text: block.done === false ? "Thinking…" : "Thought" }),
        md);
    } else if (block.type === "tool") {
      node = renderTool(block);
    } else return h("span");
    block.node = node;
    return node;
  }
  function toolArg(name, input) {
    if (!input || typeof input !== "object") return "";
    const path = input.filePath || input.path || input.file_path;
    if (path) return relative(path);
    const question = Array.isArray(input.questions) && input.questions[0] && input.questions[0].question;
    const todos = Array.isArray(input.todos) && input.todos.length + " items";
    return input.command || input.pattern || input.url || input.description || input.query || question || todos || "";
  }
  function renderToolGroup(blocks) {
    const counts = new Map();
    let running = 0;
    let failed = 0;
    for (const b of blocks) {
      counts.set(b.name, (counts.get(b.name) || 0) + 1);
      const st = b.state && b.state.status;
      if (st === "running" || st === "streaming") running++;
      else if (st === "error") failed++;
    }
    const parts = [...counts].map(([name, n]) => name + " × " + n).join(", ");
    const tail = [];
    if (running) tail.push(running + " running");
    if (failed) tail.push(failed + " failed");
    const mark = failed ? "✕" : "▸";
    const summaryText = `${mark} ${blocks.length} tool call${blocks.length === 1 ? "" : "s"} · ${parts}${tail.length ? " · " + tail.join(" · ") : ""}`;
    return h("details", { class: "tool-group" },
      h("summary", { text: summaryText }),
      h("div", { class: "group-items" }, ...blocks.map((b) => renderTool(b))));
  }
  function renderTool(block) {
    const s = block.state || {};
    const status = s.status || "streaming";
    let input = s.input;
    if (typeof input === "string") { try { input = JSON.parse(input); } catch { /* still streaming */ } }
    const icon = { streaming: "…", running: "●", completed: "✓", error: "✕" }[status] || "•";
    const isOpen = block.open != null ? !!block.open : state.toolView === "full";
    const node = h("details", { class: "tool " + status, open: isOpen, ontoggle: (e) => { block.open = e.target.open; } });
    const arg = toolArg(block.name, input);
    const path = input && (input.filePath || input.path);
    const childSid = s.metadata && (s.metadata.sessionID || s.metadata.sessionId);
    const summary = h("summary", {},
      h("span", { class: "state", text: icon }),
      h("span", { class: "name", text: block.name }),
      h("span", { class: "arg", text: arg, title: arg }));
    if (childSid && (block.name === "subagent" || block.name === "task")) {
      summary.append(h("button", {
        type: "button", class: "link", text: "Open subagent ↗",
        title: "Open child session " + childSid,
        onclick: (e) => { e.preventDefault(); e.stopPropagation(); openSession(childSid); },
      }));
    }
    node.append(summary);
    // Building the body lazily keeps long transcripts cheap.
    const fill = () => {
      if (node.querySelector(".body")) return;
      const body = h("div", { class: "body" });
      if (childSid && (block.name === "subagent" || block.name === "task")) {
        body.append(h("div", {}, h("button", {
          type: "button", class: "filelink", text: "Open subagent transcript (" + childSid + ") ↗",
          onclick: () => openSession(childSid),
        })));
      }
      if (path && typeof path === "string") {
        body.append(h("div", {}, h("button", { type: "button", class: "filelink", text: "Open " + relative(path), onclick: () => openFile(resolve(path)) })));
      }
      if (input && typeof input === "object" && typeof input.oldString === "string" && typeof input.newString === "string") {
        body.append(h("div", { class: "label", text: "Change" }), diff(input.oldString, input.newString));
      } else if (input && typeof input === "object" && typeof input.content === "string" && block.name === "write") {
        body.append(h("div", { class: "label", text: "Content" }), h("pre", { text: input.content }));
      } else if (input && typeof input === "object" && typeof input.command === "string") {
        body.append(h("pre", { text: "$ " + input.command }));
      } else if (input !== undefined) {
        body.append(h("div", { class: "label", text: "Input" }), h("pre", { text: typeof input === "string" ? input : JSON.stringify(input, null, 2) }));
      }
      const output = (s.content || []).filter((c) => c.type === "text").map((c) => c.text).join("\n") ||
        (s.metadata && typeof s.metadata.output === "string" ? s.metadata.output : "");
      if (output) body.append(h("div", { class: "label", text: "Output" }), h("pre", { text: output }));
      if (s.error) body.append(h("div", { class: "error", text: s.error.message || "Tool failed" }));
      node.append(body);
    };
    if (isOpen) fill();
    node.addEventListener("toggle", () => node.open && fill());
    return node;
  }
  function diff(before, after) {
    const a = before.split("\n");
    const b = after.split("\n");
    let start = 0;
    while (start < a.length && start < b.length && a[start] === b[start]) start++;
    let endA = a.length;
    let endB = b.length;
    while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) endA--, endB--;
    const rows = [];
    for (let i = Math.max(0, start - 2); i < start; i++) rows.push(h("div", { text: "  " + a[i] }));
    for (let i = start; i < endA; i++) rows.push(h("div", { class: "del", text: "- " + a[i] }));
    for (let i = start; i < endB; i++) rows.push(h("div", { class: "add", text: "+ " + b[i] }));
    for (let i = endA; i < Math.min(a.length, endA + 2); i++) rows.push(h("div", { text: "  " + a[i] }));
    return h("div", { class: "diff" }, ...rows);
  }

  // Coalesce streamed deltas into one repaint per frame. Browsers pause frames
  // for hidden or occluded windows, so a timer bounds the delay as well.
  const dirty = new Set();
  let frame = 0;
  let frameTimer = 0;
  function touch(message, block) {
    dirty.add(block ? block : message);
    if (!frame) {
      frame = requestAnimationFrame(flush);
      frameTimer = setTimeout(flush, 120);
    }
  }
  function flush() {
    cancelAnimationFrame(frame);
    clearTimeout(frameTimer);
    frame = 0;
    stick = nearBottom();
    for (const item of dirty) {
      if (item.type === "assistant" || item.type === "notice" || item.type === "info-card" || item.type === "user" || item.type === "synthetic" || item.type === "compaction") {
        const old = item.node;
        const fresh = renderMessage(item);
        if (old && old.isConnected && fresh) old.replaceWith(fresh);
        else if (fresh) $("#messages").append(fresh);
      } else if (item.node && item.node.isConnected) {
        if (item.type === "tool") item.node.replaceWith(renderBlock(item));
        else {
          const target = item.node.classList.contains("md") ? item.node : item.node.querySelector(".md");
          if (target) updateMarkdown(target, item.text || "");
          const summary = item.node.querySelector("summary");
          if (summary) summary.textContent = item.done === false ? "Thinking…" : "Thought";
        }
      }
    }
    dirty.clear();
    renderPending();
    scrollToBottom();
  }

  // ---------- requests (permissions and forms) ----------
  function renderRequests() {
    const box = $("#requests");
    const cards = [];
    for (const p of state.permissions.values()) {
      if (p.sessionID !== state.sid) continue;
      const meta = p.metadata || {};
      const detail = meta.command || meta.filepath || meta.filePath || meta.url || "";
      cards.push(h("div", { class: "card", role: "alertdialog", "aria-label": "Permission request" },
        h("h3", { text: "Allow " + p.action + "?" }),
        (p.resources || []).length ? h("pre", { text: p.resources.join("\n") }) : null,
        detail && !(p.resources || []).includes(detail) ? h("pre", { text: String(detail) }) : null,
        h("div", { class: "buttons" },
          h("button", { type: "button", class: "primary", text: "Allow once", onclick: () => replyPermission(p, "once") }),
          (p.save || []).length ? h("button", { type: "button", text: "Always allow", title: (p.save || []).join(", "),
            onclick: () => replyPermission(p, "always") }) : null,
          h("button", { type: "button", text: "Reject", onclick: () => replyPermission(p, "reject") }),
          h("button", { type: "button", text: "Reject with feedback…", onclick: () => {
            const message = prompt("Tell the agent what to do instead");
            if (message != null) replyPermission(p, "reject", message);
          } }))));
    }
    for (const form of state.forms.values()) {
      if (form.sessionID !== state.sid) continue;
      cards.push(renderForm(form));
    }
    box.replaceChildren(...cards);
  }
  async function replyPermission(p, decision, message) {
    const body = { decision };
    if (message) body.message = message;
    try {
      await api("POST", sessionPath(p.sessionID, "/permission/" + encodeURIComponent(p.id) + "/reply"), body);
      state.permissions.delete(p.id);
      renderRequests();
    } catch (error) { fail(error); }
  }
  function renderForm(form) {
    const node = h("form", { class: "card", "aria-label": form.title || "Question" });
    node.append(h("h3", { text: form.title || "Question" }));
    for (const field of form.fields || []) {
      const set = h("fieldset", { "data-key": field.key });
      set.append(h("legend", { text: field.title || field.description || "Answer" }));
      if (field.title && field.description) set.append(h("div", { class: "md", html: markdown(field.description) }));
      const multi = field.type === "multiselect";
      for (const option of field.options || []) {
        set.append(h("label", { class: "option" },
          h("input", { type: multi ? "checkbox" : "radio", name: "f" + field.key, value: option.value }),
          h("span", {}, option.label, option.description ? h("small", { text: " — " + option.description }) : null)));
      }
      if (!multi && (field.custom || !(field.options || []).length)) {
        set.append(h("input", { type: "text", name: "c" + field.key, placeholder: "Or type your own answer",
          oninput: (e) => { if (e.target.value) set.querySelectorAll("input[type=radio]").forEach((r) => (r.checked = false)); } }));
      }
      node.append(set);
    }
    node.append(h("div", { class: "buttons" },
      h("button", { type: "submit", class: "primary", text: "Answer" }),
      h("button", { type: "button", text: "Dismiss", onclick: () => cancelForm(form) })));
    node.addEventListener("submit", async (e) => {
      e.preventDefault();
      const answer = {};
      for (const field of form.fields || []) {
        if (field.type === "multiselect") {
          answer[field.key] = [...node.querySelectorAll(`input[name="f${CSS.escape(field.key)}"]:checked`)].map((i) => i.value);
        } else {
          const custom = node.querySelector(`input[name="c${CSS.escape(field.key)}"]`);
          const picked = node.querySelector(`input[name="f${CSS.escape(field.key)}"]:checked`);
          const value = (custom && custom.value.trim()) || (picked && picked.value) || "";
          if (field.required && !value) return toast("Answer every question first", true);
          answer[field.key] = value;
        }
      }
      try {
        await api("POST", sessionPath(form.sessionID, "/form/" + encodeURIComponent(form.id) + "/reply"), { answer });
        state.forms.delete(form.id);
        renderRequests();
      } catch (error) { fail(error); }
    });
    return node;
  }
  async function cancelForm(form) {
    try {
      await api("DELETE", sessionPath(form.sessionID, "/form/" + encodeURIComponent(form.id)));
      state.forms.delete(form.id);
      renderRequests();
    } catch (error) { fail(error); }
  }

  // ---------- watching another process ----------
  // Live events cover only this server. A session run by another ocmini process
  // (such as T3's own server) is followed through its saved history instead,
  // only while it is open here and running there.
  let watchTimer = 0;
  let watchSignature = "";
  function stopWatching() {
    clearTimeout(watchTimer);
    watchTimer = 0;
    watchSignature = "";
    if (state.remote) {
      state.remote = false;
      renderStatus();
    }
  }
  async function watchRemote(sid) {
    if (sid !== state.sid) return;
    if (state.busy.has(sid)) return stopWatching();
    let status;
    try {
      status = await api("GET", "/web/api/session/" + encodeURIComponent(sid) + "/status");
    } catch {
      return;
    }
    if (sid !== state.sid) return;
    if (!(status.busy && !status.local)) {
      if (state.remote) {
        stopWatching();
        openSession(sid);
      }
      return;
    }
    if (!state.remote) {
      state.remote = true;
      renderStatus();
    } else {
      await refreshHistory(sid).catch(() => {});
    }
    watchTimer = setTimeout(() => watchRemote(sid), 2000);
  }
  async function refreshHistory(sid) {
    const page = await api("GET", sessionPath(sid, "/message?order=desc&limit=80"));
    if (sid !== state.sid) return;
    const signature = page.data.length + ":" + JSON.stringify(page.data[0] || null).length;
    if (signature === watchSignature) return;
    watchSignature = signature;
    const open = new Set();
    for (const m of state.messages) for (const b of m.content || []) if (b && b.open) open.add(b.id || b);
    state.messages = [];
    state.byId.clear();
    for (const m of page.data.slice().reverse()) {
      for (const b of m.content || []) if (b && b.id && open.has(b.id)) b.open = true;
      addMessage(m);
    }
    state.olderCursor = page.cursor && page.cursor.next;
    stick = nearBottom();
    renderMessages();
    scrollToBottom();
  }

  // ---------- composer ----------
  let started = 0;
  function renderStatus() {
    const busy = state.sid && state.busy.has(state.sid);
    $("#stop").hidden = !busy;
    $("#send").textContent = busy ? ($("#delivery").value === "queue" ? "Queue" : "Steer") : "Send";
    if (busy && !started) started = Date.now();
    if (!busy) started = 0;
    $("#status").textContent = busy ? "Working… " + Math.round((Date.now() - started) / 1000) + "s"
      : state.remote ? "Running in another ocmini process · watching" : "";
  }
  setInterval(() => { if (started) renderStatus(); }, 1000);

  function renderAttachments() {
    const fileNodes = state.attachments.map((a, i) =>
      h("span", { class: "chip" }, "📎 " + a.name,
        h("button", { type: "button", "aria-label": "Remove " + a.name, text: "✕",
          onclick: () => { state.attachments.splice(i, 1); renderAttachments(); } })));
    const skillNodes = state.selectedSkills.map((id, i) =>
      h("span", { class: "chip skill" }, "✦ " + id,
        h("button", { type: "button", "aria-label": "Remove skill " + id, text: "✕",
          onclick: () => { state.selectedSkills.splice(i, 1); renderAttachments(); } })));
    $("#attachments").replaceChildren(...skillNodes, ...fileNodes);
  }
  function addInfoCard(title, bodyNode) {
    const card = addMessage({ id: "info_" + Date.now() + "_" + Math.random().toString(36).slice(2), type: "info-card", title, bodyNode });
    touch(card);
    stick = true;
    scrollToBottom(true);
  }
  function showHelpCard() {
    const rows = [
      ["/help", "Show this commands & shortcuts card"],
      ["/context", "Inspect approximate context usage for the active session"],
      ["/compact [focus]", "Summarize older context with optional focus instructions"],
      ...state.commands.map((c) => ["/" + c.name, c.description || c.template || ""]),
      ["Enter / Shift+Enter", "Send prompt / insert newline"],
      ["Up / Down", "Recall prompt history when at the start of the composer"],
      ["Ctrl+T", "Toggle thinking blocks visibility"],
      ["Ctrl+O", "Cycle tool display (brief / full / grouped)"],
      ["Ctrl+P", "Quick open file in workspace"],
      ["Ctrl+S", "Save active editor file"],
      ["Esc", "Close suggestions or stop an active turn"],
    ];
    const dl = h("dl", {});
    for (const [term, desc] of rows) {
      dl.append(h("dt", { text: term }), h("dd", { text: desc }));
    }
    addInfoCard("Commands & Shortcuts", dl);
  }
  async function showContextCard() {
    if (!state.sid) {
      toast("Start or open a session first to inspect context", true);
      return;
    }
    const res = await api("GET", sessionPath(state.sid, "/context"));
    const items = res.data || [];
    let userCount = 0, assistantCount = 0, toolCount = 0, textChars = 0, reasoningChars = 0, toolChars = 0;
    for (const m of items) {
      if (m.type === "user" || m.type === "synthetic") {
        userCount++;
        textChars += (m.text || "").length;
      } else if (m.type === "assistant") {
        assistantCount++;
        for (const b of m.content || []) {
          if (!b) continue;
          if (b.type === "text") textChars += (b.text || "").length;
          else if (b.type === "reasoning") reasoningChars += (b.text || "").length;
          else if (b.type === "tool") {
            toolCount++;
            toolChars += JSON.stringify(b.state || {}).length;
          }
        }
      } else if (m.type === "compaction") {
        textChars += (m.summary || "").length;
      }
    }
    const est = (chars) => Math.ceil(chars / 4);
    const estTotal = est(textChars) + est(reasoningChars) + est(toolChars);
    const current = currentModel();
    const modelInfo = current && state.models.find((m) => modelKey(m) === modelKey(current));
    const ctxWindow = modelInfo && modelInfo.limit && modelInfo.limit.context ? compact(modelInfo.limit.context) + " tokens" : "unknown";
    const observed = state.session ? usage(state.session.tokens, state.session.cost) || "none yet" : "none yet";
    const dl = h("dl", {},
      h("dt", { text: "Active messages" }), h("dd", { text: `${items.length} (${userCount} user, ${assistantCount} assistant, ${toolCount} tool calls)` }),
      h("dt", { text: "Est. conversation" }), h("dd", { text: `~${compact(est(textChars))} tokens (4 bytes/tok)` }),
      h("dt", { text: "Est. reasoning" }), h("dd", { text: `~${compact(est(reasoningChars))} tokens` }),
      h("dt", { text: "Est. tool payloads" }), h("dd", { text: `~${compact(est(toolChars))} tokens` }),
      h("dt", { text: "Est. subtotal" }), h("dd", { text: `~${compact(estTotal)} tokens (excludes system prompt & tool schemas)` }),
      h("dt", { text: "Session usage" }), h("dd", { text: observed }),
      h("dt", { text: "Model window" }), h("dd", { text: ctxWindow }));
    addInfoCard("Context breakdown", dl);
  }
  async function toggleInboxDelivery(id, delivery) {
    if (!state.sid) return;
    try {
      await api("PATCH", sessionPath(state.sid, "/inbox/" + encodeURIComponent(id)), { delivery });
      const item = state.pending.get(id);
      if (item) item.delivery = delivery;
      renderPending();
    } catch (error) { fail(error); }
  }
  function readFileAsDataUrl(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result);
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
  }
  async function attachFiles(list) {
    for (const file of list) {
      try {
        state.attachments.push({ name: file.name, uri: await readFileAsDataUrl(file) });
      } catch (error) { fail(error); }
    }
    renderAttachments();
  }

  async function submit(e) {
    if (e) e.preventDefault();
    const input = $("#input");
    const text = input.value;
    if (!text.trim() && !state.attachments.length && !state.selectedSkills.length) return;
    hideSuggest();
    const slash = text.trim().match(/^\/(\S+)(?:\s+([\s\S]*))?$/);
    if (slash && slash[1] === "help") {
      input.value = "";
      autosize();
      store.set("draft", "");
      showHelpCard();
      input.focus();
      return;
    }
    if (slash && slash[1] === "context") {
      input.value = "";
      autosize();
      store.set("draft", "");
      try { await showContextCard(); } catch (error) { fail(error); }
      input.focus();
      return;
    }
    if (state.remote) {
      return toast("Another ocmini process is running this session. You can send once it finishes; your draft is kept.", true);
    }
    const send = $("#send");
    send.disabled = true;
    try {
      const sid = await ensureSession();
      const delivery = $("#delivery").value;
      if (slash && slash[1] === "compact") {
        await api("POST", sessionPath(sid, "/compact"), slash[2] ? { ocminiFocus: slash[2] } : {});
      } else if (slash && state.commands.some((c) => c.name === slash[1])) {
        await api("POST", sessionPath(sid, "/command"), { name: slash[1], text: slash[2] || "", delivery });
      } else {
        const body = { text, delivery };
        if (state.attachments.length) body.files = state.attachments.map((a) => ({ uri: a.uri, name: a.name }));
        if (state.selectedSkills.length) body.skills = state.selectedSkills.map((id) => ({ id }));
        const receipt = await api("POST", sessionPath(sid, "/prompt"), body);
        const item = receipt.data;
        if (item && !state.byId.has(item.id) && sid === state.sid && !delivered.has(item.id)) {
          state.pending.set(item.id, item);
        }
      }
      if (text.trim() && state.promptHistory[state.promptHistory.length - 1] !== text.trim()) {
        state.promptHistory.push(text.trim());
      }
      state.historyIndex = -1;
      state.historyDraft = "";
      input.value = "";
      state.attachments = [];
      state.selectedSkills = [];
      renderAttachments();
      autosize();
      store.set("draft", "");
      stick = true;
      renderPending();
      scrollToBottom(true);
    } catch (error) {
      fail(error);
    } finally {
      send.disabled = false;
      input.focus();
    }
  }
  async function cancelInput(id) {
    try {
      await api("DELETE", sessionPath(state.sid, "/inbox/" + encodeURIComponent(id)));
      state.pending.delete(id);
      renderPending();
    } catch (error) { fail(error); }
  }
  async function interrupt() {
    if (!state.sid) return;
    await api("POST", sessionPath(state.sid, "/interrupt")).catch(fail);
  }
  async function rewind(m) {
    if (state.busy.has(state.sid)) return toast("Stop the agent before rewinding", true);
    if (!confirm("Remove this message and everything after it? Files are not restored.")) return;
    try {
      await api("POST", sessionPath(state.sid, "/revert/stage"), { messageID: m.id, files: false });
      await api("POST", sessionPath(state.sid, "/revert/commit"));
      const input = $("#input");
      if (!input.value.trim()) input.value = m.text || "";
      autosize();
      await openSession(state.sid);
    } catch (error) { fail(error); }
  }
  async function forkSession(before) {
    if (!state.sid) return;
    try {
      const body = before ? { before } : {};
      const forked = await api("POST", sessionPath(state.sid, "/fork"), body);
      reloadSessions();
      await openSession(forked.data.id);
      toast("Forked into a new session");
    } catch (error) { fail(error); }
  }
  async function compactSession() {
    if (!state.sid) return;
    await api("POST", sessionPath(state.sid, "/compact"), {}).then(() => toast("Compaction requested")).catch(fail);
  }

  function autosize() {
    const input = $("#input");
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight + 2, innerHeight * 0.4) + "px";
  }
  let suggestIndex = 0;
  function suggestions() {
    const text = $("#input").value;
    const m = text.match(/^\/(\S*)$/);
    if (!m) return [];
    const builtins = [
      { name: "compact", description: "Summarize older context" },
      { name: "context", description: "Inspect approximate context usage" },
      { name: "help", description: "Show commands and keyboard shortcuts" },
    ];
    const all = builtins.concat(state.commands.filter((c) => !builtins.some((b) => b.name === c.name)));
    return all.filter((c) => c.name.startsWith(m[1]));
  }
  function renderSuggest() {
    const list = suggestions();
    const box = $("#suggest");
    const input = $("#input");
    if (!list.length) return hideSuggest();
    suggestIndex = Math.min(suggestIndex, list.length - 1);
    box.replaceChildren(...list.map((c, i) => {
      const active = i === suggestIndex;
      const optId = "suggest-opt-" + i;
      return h("li", {
        id: optId,
        role: "option",
        "aria-selected": String(active),
        class: active ? "active" : "",
        onmousedown: (e) => { e.preventDefault(); pickSuggest(c); },
      },
      h("b", { text: "/" + c.name }), h("small", { text: c.description || c.template || "" }));
    }));
    input.setAttribute("aria-activedescendant", "suggest-opt-" + suggestIndex);
    box.hidden = false;
  }
  function hideSuggest() {
    $("#suggest").hidden = true;
    $("#input").removeAttribute("aria-activedescendant");
    suggestIndex = 0;
  }
  function pickSuggest(c) {
    $("#input").value = "/" + c.name + (c.name === "help" || c.name === "context" ? "" : " ");
    hideSuggest();
    $("#input").focus();
  }

  // ---------- events ----------
  const delivered = new Set();
  let source = null;
  let retry = 0;
  function connect() {
    if (source) source.close();
    source = new EventSource("/api/event");
    const conn = $("#conn");
    source.onopen = () => {
      conn.className = "conn live";
      conn.title = "Live";
      if (retry) resync();
      retry = 0;
    };
    source.onerror = () => {
      conn.className = "conn down";
      conn.title = "Disconnected; reconnecting";
      if (source.readyState === EventSource.CLOSED) {
        retry++;
        setTimeout(connect, Math.min(30000, 1000 * 2 ** Math.min(retry, 5)));
      } else retry = Math.max(retry, 1);
    };
    source.onmessage = (e) => {
      let event;
      try { event = JSON.parse(e.data); } catch { return; }
      try { handle(event); } catch (error) { console.error(error, event); }
    };
  }
  function resync() {
    reloadSessions();
    if (state.sid) openSession(state.sid);
  }

  function assistant(data) {
    let m = state.byId.get(data.assistantMessageID);
    if (!m) {
      m = addMessage({ id: data.assistantMessageID, type: "assistant", agent: data.agent, model: data.model, content: [], time: { created: Date.now() } });
      touch(m);
    }
    return m;
  }
  function deliver(id) {
    delivered.add(id);
    if (delivered.size > 512) delivered.delete(delivered.values().next().value);
    const item = state.pending.get(id);
    if (!item) return;
    state.pending.delete(id);
    if (item.type === "compaction" || item.type === "move") return renderPending();
    const payload = item.payload || {};
    const m = addMessage({ id, type: item.type || "user", text: payload.text || "", files: payload.files, metadata: payload.metadata, time: { created: Date.now() } });
    touch(m);
  }
  function handle(event) {
    const type = event.type;
    const data = event.data || {};
    const sid = data.sessionID || (data.form && data.form.sessionID);
    if (loading && sid === loading.sid) {
      loading.events.push(event);
      return;
    }
    switch (type) {
      case "server.connected":
        return;
      case "session.created":
      case "session.forked":
      case "session.deleted":
      case "session.archived":
      case "session.unarchived":
        if (type === "session.deleted" && sid === state.sid) clearSession();
        return reloadSessions();
      case "session.location.changed":
        if (sid === state.sid) openSession(sid);
        return reloadSessions();
      case "session.renamed": {
        const s = state.sessions.find((x) => x.id === sid);
        if (s) {
          s.title = data.title;
          renderSessions();
        }
        if (state.session && sid === state.sid) {
          state.session.title = data.title;
          renderHeader();
        }
        return;
      }
      case "session.execution.started":
        state.busy.add(sid);
        updateSessionBusy(sid);
        if (sid === state.sid) { renderStatus(); renderSandbox(); }
        return;
      case "session.execution.succeeded":
      case "session.execution.failed":
      case "session.execution.interrupted":
        state.busy.delete(sid);
        updateSessionBusy(sid);
        if (sid !== state.sid) return reloadSessions();
        renderStatus();
        renderSandbox();
        if (type === "session.execution.failed" && data.error) {
          touch(addMessage({ id: event.id, type: "notice", error: true, text: data.error.message || "Execution failed" }));
        }
        refreshOpenFiles();
        return reloadSessions();
      case "session.usage.updated":
        if (sid === state.sid && state.session) {
          state.session.tokens = data.tokens;
          state.session.cost = data.cost;
          renderHeader();
        }
        return;
      case "session.model.selected":
      case "session.agent.selected":
        if (sid === state.sid && state.session) {
          if (data.model) state.session.model = data.model;
          if (data.agent) state.session.agent = data.agent;
          renderSelectors();
        }
        return;
      case "permission.asked":
        state.permissions.set(data.id, data);
        if (sid === state.sid) renderRequests();
        else toast("A session is waiting for permission", false, sid ? () => openSession(sid) : null);
        return;
      case "permission.replied":
        state.permissions.delete(data.requestID);
        return renderRequests();
      case "form.created":
        state.forms.set(data.form.id, data.form);
        if (data.form.sessionID === state.sid) renderRequests();
        else toast("A session asked a question", false, data.form.sessionID ? () => openSession(data.form.sessionID) : null);
        return;
      case "form.replied":
      case "form.cancelled":
        state.forms.delete(data.id);
        return renderRequests();
    }
    if (sid !== state.sid) return;
    switch (type) {
      case "session.inbox.enqueued":
        if (!state.byId.has(data.inboxID) && !delivered.has(data.inboxID)) {
          state.pending.set(data.inboxID, { id: data.inboxID, type: data.item.type, delivery: data.item.delivery, payload: data.item.payload });
          renderPending();
          scrollToBottom();
        }
        return;
      case "session.inbox.delivered":
        return deliver(data.inboxID);
      case "session.inbox.cancelled":
        state.pending.delete(data.inboxID);
        return renderPending();
      case "session.step.started":
        assistant(data);
        return;
      case "session.text.started":
      case "session.reasoning.started": {
        const m = assistant(data);
        const block = { type: type.includes("text") ? "text" : "reasoning", text: "", done: false };
        m.content[data.ordinal] = block;
        return touch(m);
      }
      case "session.text.delta":
      case "session.reasoning.delta":
      case "session.text.ended":
      case "session.reasoning.ended": {
        const m = assistant(data);
        let block = m.content[data.ordinal];
        if (!block) {
          block = m.content[data.ordinal] = { type: type.includes("text") ? "text" : "reasoning", text: "" };
          touch(m);
        }
        if (type.endsWith("delta")) {
          block.text += data.delta || "";
        } else {
          if (data.text != null) block.text = data.text;
          block.done = true;
        }
        return touch(m, block.node ? block : null);
      }
      case "session.tool.input.started": {
        const m = assistant(data);
        if (!m.content.some((b) => b && b.type === "tool" && b.id === data.id)) {
          m.content.push({ type: "tool", id: data.id, name: data.name, state: { status: "streaming" } });
          touch(m);
        }
        return;
      }
      case "session.tool.input.ended": {
        // The complete input arrives before approval, so a waiting call can show its target.
        const block = assistant(data).content.find((b) => b && b.type === "tool" && b.id === data.id);
        if (block && block.state.status === "streaming") {
          block.state = { status: "streaming", input: data.text };
          touch(block, null);
        }
        return;
      }
      case "session.tool.called":
      case "session.tool.progress":
      case "session.tool.success":
      case "session.tool.failed": {
        const m = assistant(data);
        let block = m.content.find((b) => b && b.type === "tool" && b.id === data.id);
        if (!block) {
          block = { type: "tool", id: data.id, name: data.name, state: {} };
          m.content.push(block);
        }
        const status = { "session.tool.called": "running", "session.tool.progress": "running",
          "session.tool.success": "completed", "session.tool.failed": "error" }[type];
        block.state = { status, input: data.input, metadata: data.metadata, content: data.content, error: data.error };
        touch(m, state.toolView === "grouped" ? null : (block.node ? block : null));
        if (type === "session.tool.success" && MUTATING_TOOLS.has(data.name)) {
          refreshOpenFiles();
          refreshTree();
        }
        return;
      }
      case "session.step.ended":
      case "session.step.failed": {
        const m = assistant(data);
        m.time = Object.assign(m.time || {}, { completed: Date.now() });
        m.tokens = data.tokens;
        m.cost = data.cost;
        m.finish = data.finish;
        if (data.error) m.error = data.error;
        if (m.node && m.node.isConnected) {
          let err = m.node.querySelector(":scope > .error");
          if (m.error) {
            if (!err) { err = h("div", { class: "error" }); m.node.append(err); }
            err.textContent = m.error.message || "Failed";
          } else if (err) err.remove();
          const footerText = m.time && m.time.completed ? usage(m.tokens, m.cost) : "";
          let footer = m.node.querySelector(":scope > .footer");
          if (footerText) {
            if (!footer) { footer = h("div", { class: "footer" }); m.node.append(footer); }
            footer.textContent = footerText;
          } else if (footer) footer.remove();
          return;
        }
        return touch(m);
      }
      case "session.retry.scheduled":
        $("#status").textContent = "Retrying (attempt " + data.attempt + "): " + ((data.error && data.error.message) || "");
        return;
      case "session.compaction.started":
        touch(addMessage({ id: event.id, type: "notice", text: "Compacting context…" }));
        return;
      case "session.compaction.ended":
      case "session.compaction.failed":
      case "session.revert.committed":
      case "session.revert.cleared":
        return openSession(state.sid);
    }
  }

  // ---------- file tree ----------
  function applyDirEntries(node, result) {
    const previous = new Map((node.children || []).map((c) => [c.path, c]));
    node.children = result.entries.map((e) => {
      const old = previous.get(e.path);
      return old && old.type === e.type ? Object.assign(old, e) : { ...e, open: false, children: null };
    });
  }
  async function loadDir(node) {
    const result = await api("GET", "/web/api/fs/list?" + q({ path: node.path }));
    applyDirEntries(node, result);
  }
  async function initTree(initial) {
    state.tree = { path: state.dir, name: basename(state.dir), type: "directory", open: true, children: null };
    state.treeSelected = state.dir;
    if (initial) applyDirEntries(state.tree, initial);
    else await loadDir(state.tree).catch(fail);
    renderTree();
  }
  let treeTimer = 0;
  function refreshTree() {
    clearTimeout(treeTimer);
    treeTimer = setTimeout(async () => {
      if (!state.tree) return;
      const walk = async (node) => {
        if (node.type !== "directory" || !node.open) return;
        await loadDir(node).catch(() => { node.children = []; });
        await Promise.all((node.children || []).map(walk));
      };
      await walk(state.tree);
      renderTree();
    }, 400);
  }
  function renderTree() {
    const tree = $("#tree");
    if (!state.tree) return tree.replaceChildren();
    tree.replaceChildren(h("ul", {}, ...(state.tree.children || []).map(renderNode)));
    if (state.tree.children && !state.tree.children.length) tree.append(h("div", { class: "muted", text: "Empty folder" }));
  }
  function renderNode(node) {
    const dir = node.type === "directory";
    const row = h("div", {
      class: "node" + (state.treeSelected === node.path ? " selected" : "") + (node.name.startsWith(".") ? " hidden-file" : ""),
      role: "treeitem", tabindex: 0, title: node.path, "aria-expanded": dir ? String(!!node.open) : null,
      onclick: () => activateNode(node),
      onkeydown: (e) => { if (e.key === "Enter") activateNode(node); },
    },
    h("span", { class: "caret", text: dir ? (node.open ? "▾" : "▸") : "" }),
    h("span", { class: "label", text: node.name + (node.link ? " ↗" : "") }),
    h("span", { class: "tools" },
      h("button", { type: "button", title: "Rename or move", "aria-label": "Rename", text: "✎",
        onclick: (e) => { e.stopPropagation(); renamePath(node); } }),
      h("button", { type: "button", title: "Delete", "aria-label": "Delete", text: "✕",
        onclick: (e) => { e.stopPropagation(); deletePath(node); } })));
    const item = h("li", {}, row);
    if (dir && node.open && node.children) item.append(h("ul", {}, ...node.children.map(renderNode)));
    return item;
  }
  async function activateNode(node) {
    state.treeSelected = node.path;
    if (node.type === "directory") {
      node.open = !node.open;
      if (node.open && !node.children) await loadDir(node).catch(fail);
      renderTree();
    } else {
      renderTree();
      openFile(node.path);
    }
  }
  function selectedFolder() {
    const find = (node) => {
      if (node.path === state.treeSelected) return node;
      for (const c of node.children || []) { const f = find(c); if (f) return f; }
      return null;
    };
    const selected = state.tree && find(state.tree);
    if (!selected) return state.dir;
    return selected.type === "directory" ? selected.path : selected.path.slice(0, selected.path.length - selected.name.length - 1);
  }
  async function newFile() {
    const folder = selectedFolder();
    const name = prompt("New file name (relative to " + (relative(folder) || ".") + ")");
    if (!name || !name.trim()) return;
    const path = join(folder, name.trim());
    try {
      await api("PUT", "/web/api/fs/write", { path, content: "", create: true });
      refreshTree();
      openFile(path);
    } catch (error) { fail(error); }
  }
  async function newFolder() {
    const folder = selectedFolder();
    const name = prompt("New folder name (relative to " + (relative(folder) || ".") + ")");
    if (!name || !name.trim()) return;
    await api("POST", "/web/api/fs/mkdir", { path: join(folder, name.trim()) }).then(refreshTree).catch(fail);
  }
  async function renamePath(node) {
    const target = prompt("Rename or move to", relative(node.path));
    if (!target || !target.trim() || target.trim() === relative(node.path)) return;
    const to = join(state.dir, target.trim());
    try {
      await api("POST", "/web/api/fs/rename", { from: node.path, to });
      for (const f of state.files) {
        if (f.path === node.path || f.path.startsWith(node.path + state.sep)) {
          f.path = to + f.path.slice(node.path.length);
          f.name = basename(f.path);
        }
      }
      renderTabs();
      refreshTree();
    } catch (error) { fail(error); }
  }
  async function deletePath(node) {
    const what = node.type === "directory" ? "the folder " + node.name + " and everything in it" : node.name;
    if (!confirm("Delete " + what + "?")) return;
    try {
      await api("POST", "/web/api/fs/delete", { path: node.path });
      state.files.filter((f) => f.path === node.path || f.path.startsWith(node.path + state.sep)).forEach((f) => { f.dirty = true; f.missing = true; });
      renderTabs();
      refreshTree();
    } catch (error) { fail(error); }
  }

  // ---------- editor ----------
  const code = () => $("#code");
  async function fetchFile(path, version) {
    const headers = version ? { "if-none-match": version } : {};
    const response = await fetch("/web/api/fs/read?" + q({ path }), { headers, cache: "no-store" });
    if (response.status === 304) return null;
    if (!response.ok) {
      const data = await response.json().catch(() => ({}));
      const error = new Error(data.message || response.statusText);
      error.status = response.status;
      error.data = data;
      throw error;
    }
    const bytes = await response.arrayBuffer();
    let text;
    let readonly = false;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch {
      text = new TextDecoder("utf-8").decode(bytes);
      readonly = true;
    }
    return { text, readonly, version: response.headers.get("x-ocmini-version") };
  }
  async function openFile(path, quiet = false) {
    showView("editor");
    let file = state.files.find((f) => f.path === path);
    if (!file) {
      try {
        const loaded = await fetchFile(path);
        file = { path, name: basename(path), text: loaded.text, saved: loaded.text, version: loaded.version,
          diskVersion: loaded.version, readonly: loaded.readonly, dirty: false, scroll: [0, 0], cursor: [0, 0] };
        if (loaded.readonly) toast(basename(path) + " is not valid UTF-8; opened read-only", true);
        state.files.push(file);
      } catch (error) {
        if (quiet) return;
        if (error.status === 415) toast(basename(path) + " looks like a binary file and cannot be edited here", true);
        else fail(error);
        return;
      }
    }
    activate(file);
  }
  function stash() {
    const file = state.active;
    if (!file) return;
    const c = code();
    file.text = c.value;
    file.scroll = [c.scrollTop, c.scrollLeft];
    file.cursor = [c.selectionStart, c.selectionEnd];
  }
  function activate(file) {
    if (state.active !== file) stash();
    state.active = file;
    const c = code();
    c.value = file.text;
    c.readOnly = !!file.readonly;
    c.setSelectionRange(file.cursor[0], file.cursor[1]);
    c.scrollTop = file.scroll[0];
    c.scrollLeft = file.scroll[1];
    lines = -1;
    renderGutter();
    renderTabs();
    updateCursor();
    store.set("files", JSON.stringify(state.files.map((f) => f.path)));
    if (!narrow() || $("#app").classList.contains("view-editor")) c.focus({ preventScroll: true });
  }
  function renderTabs() {
    const app = $("#app");
    app.classList.toggle("has-files", state.files.length > 0);
    $("#editor-empty").hidden = state.files.length > 0;
    $("#editor-body").hidden = $("#editor-status").hidden = !state.files.length;
    $("#editor-tabs").replaceChildren(...state.files.map((f) =>
      h("div", { class: "ftab" + (f === state.active ? " active" : "") + (f.dirty ? " dirty" : ""), title: f.path, tabindex: 0,
        onclick: () => activate(f), onauxclick: (e) => { if (e.button === 1) closeFile(f); },
        onkeydown: (e) => { if (e.key === "Enter") activate(f); } },
      h("span", { class: "fname", text: f.name }),
      h("button", { type: "button", "aria-label": "Close " + f.name, text: "✕", onclick: (e) => { e.stopPropagation(); closeFile(f); } }))));
    if (state.active) {
      $("#editor-path").textContent = ltr(state.active.path) + (state.active.readonly ? " (read-only)" : "") + (state.active.missing ? " (deleted)" : "");
      $("#save").disabled = !!state.active.readonly;
    }
  }
  function closeFile(file) {
    if (file.dirty && !confirm(file.name + " has unsaved changes. Close anyway?")) return;
    const index = state.files.indexOf(file);
    state.files.splice(index, 1);
    if (state.active === file) {
      state.active = null;
      const next = state.files[Math.min(index, state.files.length - 1)];
      if (next) activate(next);
      else { code().value = ""; showView("chat"); }
    }
    store.set("files", JSON.stringify(state.files.map((f) => f.path)));
    renderTabs();
  }
  let lines = -1;
  function renderGutter() {
    const c = code();
    let count = 1;
    for (let i = c.value.indexOf("\n"); i !== -1; i = c.value.indexOf("\n", i + 1)) count++;
    if (count !== lines) {
      lines = count;
      const numbers = new Array(count);
      for (let i = 0; i < count; i++) numbers[i] = i + 1;
      $("#gutter").textContent = numbers.join("\n") + "\n\n";
    }
    $("#gutter").scrollTop = c.scrollTop;
  }
  function updateCursor() {
    const c = code();
    const text = c.value;
    const pos = c.selectionStart;
    let line = 1;
    let last = -1;
    for (let i = text.indexOf("\n"); i !== -1 && i < pos; i = text.indexOf("\n", i + 1)) {
      line++;
      last = i;
    }
    $("#cursor").textContent = "Ln " + line + ", Col " + (pos - last);
  }
  function onEdit() {
    const file = state.active;
    if (!file) return;
    file.text = code().value;
    const dirty = file.text !== file.saved || !!file.missing;
    if (dirty !== file.dirty) { file.dirty = dirty; renderTabs(); }
    renderGutter();
    updateCursor();
  }
  function insert(text) {
    const c = code();
    // execCommand keeps the browser's native undo stack; fall back where it is gone.
    if (!document.execCommand || !document.execCommand("insertText", false, text)) {
      c.setRangeText(text, c.selectionStart, c.selectionEnd, "end");
      onEdit();
    }
  }
  function indentUnit() {
    const file = state.active;
    if (!file) return "  ";
    if (/^\t/m.test(file.saved) || /\.(go|mk)$|^Makefile$/.test(file.name)) return "\t";
    return "  ";
  }
  function editorKeys(e) {
    const c = code();
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      return save();
    }
    if (c.readOnly) return;
    if (e.key === "Tab" && !e.ctrlKey && !e.altKey && !e.metaKey) {
      e.preventDefault();
      const unit = indentUnit();
      const value = c.value;
      const start = value.lastIndexOf("\n", c.selectionStart - 1) + 1;
      if (e.shiftKey || c.selectionStart !== c.selectionEnd) {
        const end = c.selectionEnd;
        const block = value.slice(start, end);
        const outdent = unit === "\t" ? /^(\t| {1,4})/gm : /^(\t| {1,2})/gm;
        const changed = e.shiftKey ? block.replace(outdent, "") : block.replace(/^/gm, unit);
        c.setSelectionRange(start, end);
        insert(changed);
        c.setSelectionRange(start, start + changed.length);
      } else insert(unit);
      return;
    }
    if (e.key === "Enter" && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && !e.isComposing) {
      const value = c.value;
      const start = value.lastIndexOf("\n", c.selectionStart - 1) + 1;
      const indent = value.slice(start, c.selectionStart).match(/^[ \t]*/)[0];
      e.preventDefault();
      insert("\n" + indent);
    }
  }
  async function save(force = false) {
    const file = state.active;
    if (!file || file.readonly) return;
    stash();
    const body = { path: file.path, content: file.text };
    if (!force && !file.missing) body.version = file.version;
    try {
      const result = await api("PUT", "/web/api/fs/write", body);
      file.version = result.version;
      file.diskVersion = result.version;
      file.saved = file.text;
      file.dirty = false;
      file.missing = false;
      renderTabs();
      toast("Saved " + file.name);
      refreshTree();
    } catch (error) {
      if (error.status === 409 && error.data && error.data._tag === "FileChangedError") {
        if (confirm(file.name + " changed on disk since you opened it (perhaps by the agent).\n\nOK overwrites it with your version. Cancel keeps the disk version; use Reload to see it.")) {
          return save(true);
        }
        return;
      }
      fail(error);
    }
  }
  function applyLoadedFile(file, loaded) {
    const c = code();
    const keep = file === state.active ? [c.scrollTop, c.selectionStart] : null;
    Object.assign(file, {
      text: loaded.text,
      saved: loaded.text,
      version: loaded.version,
      diskVersion: loaded.version,
      readonly: loaded.readonly,
      dirty: false,
      missing: false,
    });
    if (keep) {
      c.value = file.text;
      c.scrollTop = keep[0];
      c.setSelectionRange(Math.min(keep[1], file.text.length), Math.min(keep[1], file.text.length));
      lines = -1;
      renderGutter();
      updateCursor();
    }
    renderTabs();
  }
  async function reloadFile(file = state.active, ask = true) {
    if (!file) return;
    if (ask && file.dirty && !confirm("Discard your changes to " + file.name + "?")) return;
    try {
      const loaded = await fetchFile(file.path);
      applyLoadedFile(file, loaded);
    } catch (error) {
      if (error.status === 404) { file.missing = true; file.dirty = true; file.diskVersion = null; renderTabs(); } else fail(error);
    }
  }
  // After agent edits, refresh clean buffers; a conditional read costs one stat when unchanged.
  let refreshTimer = 0;
  function refreshOpenFiles() {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(async () => {
      stash();
      for (const file of state.files) {
        try {
          const loaded = await fetchFile(file.path, file.diskVersion || file.version);
          if (!loaded) continue;
          file.diskVersion = loaded.version;
          if (file.dirty) {
            toast(file.name + " changed on disk; saving will ask before overwriting");
            continue;
          }
          applyLoadedFile(file, loaded);
        } catch (error) {
          if (error.status === 404 && !file.missing) { file.missing = true; file.dirty = true; file.diskVersion = null; renderTabs(); }
        }
      }
    }, 300);
  }

  // ---------- project directory ----------
  function recentProjects() {
    try { return JSON.parse(store.get("projects") || "[]"); } catch { return []; }
  }
  async function setDirectory(dir, keepSession) {
    const listing = await api("GET", "/web/api/fs/list?" + q({ path: dir }));
    const changed = listing.path !== state.dir;
    state.dir = listing.path;
    store.set("dir", state.dir);
    store.set("projects", JSON.stringify([state.dir].concat(recentProjects().filter((p) => p !== state.dir)).slice(0, 20)));
    if (!keepSession) clearSession();
    renderHeader();
    const sessions = changed || state.scope === "project" ? loadSessions().catch(fail) : (renderSessions(), null);
    await Promise.all([sessions, loadCatalogs().catch(fail), loadSandboxChoices(), initTree(listing)]);
  }
  function changeDirectory() {
    const known = [...new Set([state.dir].concat(state.sessions.map(sessionDir), recentProjects()))].filter(Boolean);
    const input = h("input", { type: "text", value: state.dir, "aria-label": "Project directory", spellcheck: "false" });
    const go = (dir) => {
      if (!dir || !dir.trim()) return;
      $("#modal").close();
      setDirectory(dir.trim()).catch(fail);
    };
    modal("Open project", h("div", {},
      h("form", { class: "field", onsubmit: (e) => { e.preventDefault(); go(input.value); } },
        h("label", { text: "Directory" }), input,
        h("div", { class: "hint", text: "An absolute path on the server. Its opencode.json and sessions apply." })),
      h("div", { class: "buttons" }, h("button", { type: "button", class: "primary", text: "Open", onclick: () => go(input.value) })),
      h("h3", { text: "Known projects" }),
      h("ul", { class: "projects" }, ...known.map((dir) =>
        h("li", { class: dir === state.dir ? "current" : "", tabindex: 0, text: dir,
          onclick: () => go(dir), onkeydown: (e) => { if (e.key === "Enter") go(dir); } })))));
    input.select();
  }

  // ---------- dialogs ----------
  function modal(title, body) {
    $("#modal-title").textContent = title;
    $("#modal-body").replaceChildren(body);
    const dialog = $("#modal");
    if (!dialog.open) dialog.showModal();
  }

  // ---------- MCP panel ----------
  async function openMcpModal() {
    let list;
    try {
      const res = await api("GET", "/api/mcp?" + located());
      list = res.data || [];
    } catch (error) { return fail(error); }
    const rows = list.length ? list.map((srv) => {
      const st = (srv.status && srv.status.status) || "disabled";
      const err = srv.status && srv.status.error;
      const connected = st === "connected";
      const act = async () => {
        const op = connected ? "disconnect" : "connect";
        try {
          await api("POST", "/api/experimental/mcp/" + encodeURIComponent(srv.name) + "/" + op + "?" + located());
          toast((connected ? "Disconnected " : "Connected ") + srv.name);
          openMcpModal();
        } catch (error) { fail(error); openMcpModal(); }
      };
      return h("div", { class: "modal-row" },
        h("div", {},
          h("b", { text: srv.name }), " ",
          h("span", { class: "badge " + st, text: st }),
          err ? h("div", { class: "note", text: err }) : null),
        h("button", { type: "button", text: connected ? "Disconnect" : "Connect", onclick: act }));
    }) : [h("p", { class: "note", text: "No MCP servers configured for this directory. Add them under \"mcp\" in opencode.json." })];
    modal("MCP servers", h("div", {},
      ...rows,
      h("div", { class: "buttons" }, h("button", { type: "button", text: "Refresh", onclick: openMcpModal }))));
  }

  // ---------- session keyed instructions ----------
  async function openInstructionsModal() {
    if (!state.sid) return;
    let entries;
    try {
      const res = await api("GET", "/api/experimental/session/" + encodeURIComponent(state.sid) + "/instructions/entries");
      entries = res.data || [];
    } catch (error) { return fail(error); }
    const keyInput = h("input", { type: "text", placeholder: "e.g. style.guide", "aria-label": "Instruction key", spellcheck: "false" });
    const valInput = h("textarea", { rows: 4, placeholder: "Instruction text or JSON value", "aria-label": "Instruction value" });
    const saveEntry = async (e) => {
      if (e) e.preventDefault();
      const key = keyInput.value.trim();
      const raw = valInput.value;
      if (!key) return toast("Key is required", true);
      try {
        await api("PUT", "/api/experimental/session/" + encodeURIComponent(state.sid) + "/instructions/entries/" + encodeURIComponent(key), { value: raw });
        toast("Saved instruction: " + key);
        openInstructionsModal();
      } catch (error) { fail(error); }
    };
    const removeEntry = async (key) => {
      try {
        await api("DELETE", "/api/experimental/session/" + encodeURIComponent(state.sid) + "/instructions/entries/" + encodeURIComponent(key));
        toast("Removed instruction: " + key);
        openInstructionsModal();
      } catch (error) { fail(error); }
    };
    const listNodes = entries.length ? entries.map((entry) => {
      const text = typeof entry.value === "string" ? entry.value : JSON.stringify(entry.value, null, 2);
      return h("div", { class: "modal-row" },
        h("div", { style: "min-width:0;flex:1;" },
          h("code", { text: entry.key }),
          h("div", { class: "note", text: text.length > 160 ? text.slice(0, 160) + "…" : text })),
        h("div", { style: "display:flex;gap:4px;" },
          h("button", { type: "button", text: "Edit", onclick: () => { keyInput.value = entry.key; valInput.value = text; valInput.focus(); } }),
          h("button", { type: "button", text: "Delete", onclick: () => removeEntry(entry.key) })));
    }) : [h("p", { class: "note", text: "No session instructions set yet." })];
    modal("Session instructions", h("div", {},
      ...listNodes,
      h("h3", { text: "Add or update instruction" }),
      h("form", { onsubmit: saveEntry },
        h("div", { class: "field" }, h("label", { text: "Key" }), keyInput,
          h("div", { class: "hint", text: "Lowercase letters, digits, dots, underscores, or hyphens." })),
        h("div", { class: "field" }, h("label", { text: "Value" }), valInput),
        h("div", { class: "buttons" }, h("button", { type: "submit", class: "primary", text: "Save instruction" })))));
  }

  // ---------- move session directory ----------
  function moveSessionModal() {
    if (!state.sid) return;
    if (state.busy.has(state.sid)) return toast("Stop the session before moving it", true);
    const known = [...new Set([state.dir].concat(state.sessions.map(sessionDir), recentProjects()))].filter(Boolean);
    const input = h("input", { type: "text", value: state.dir, "aria-label": "Target directory", spellcheck: "false" });
    const go = async (dir) => {
      if (!dir || !dir.trim()) return;
      try {
        await api("POST", sessionPath(state.sid, "/move"), { directory: dir.trim() });
        $("#modal").close();
        toast("Session moved to " + dir.trim());
        await openSession(state.sid);
        reloadSessions();
      } catch (error) { fail(error); }
    };
    modal("Move session", h("div", {},
      h("form", { class: "field", onsubmit: (e) => { e.preventDefault(); go(input.value); } },
        h("label", { text: "Directory" }), input,
        h("div", { class: "hint", text: "Move this session to another absolute directory on the server." })),
      h("div", { class: "buttons" }, h("button", { type: "button", class: "primary", text: "Move", onclick: () => go(input.value) })),
      h("h3", { text: "Known projects" }),
      h("ul", { class: "projects" }, ...known.map((dir) =>
        h("li", { class: dir === state.dir ? "current" : "", tabindex: 0, text: dir,
          onclick: () => go(dir), onkeydown: (e) => { if (e.key === "Enter") go(dir); } })))));
    input.select();
  }

  // ---------- quick open (Ctrl+P) ----------
  function openQuickOpen() {
    let results = [];
    let selected = 0;
    let timer = 0;
    const input = h("input", { type: "search", placeholder: "Type a file name or path…", "aria-label": "Quick open file", spellcheck: "false" });
    const list = h("ul", { class: "modal-list", role: "listbox", "aria-label": "Matching files" });
    const renderList = () => {
      if (!results.length) {
        list.replaceChildren(h("li", { class: "muted", text: "No matching files" }));
        return;
      }
      selected = Math.min(selected, results.length - 1);
      list.replaceChildren(...results.map((item, i) =>
        h("li", {
          role: "option",
          "aria-selected": String(i === selected),
          class: i === selected ? "active" : "",
          text: item.relative,
          title: item.path,
          onclick: () => { $("#modal").close(); openFile(item.path); },
        })));
    };
    const search = async () => {
      try {
        const res = await api("GET", "/web/api/fs/find?" + q({ root: state.dir, query: input.value.trim(), limit: 80 }));
        results = res.entries || [];
        selected = 0;
        renderList();
      } catch (error) { fail(error); }
    };
    input.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(search, 120); });
    input.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown") { e.preventDefault(); if (results.length) { selected = (selected + 1) % results.length; renderList(); } }
      else if (e.key === "ArrowUp") { e.preventDefault(); if (results.length) { selected = (selected + results.length - 1) % results.length; renderList(); } }
      else if (e.key === "Enter" && results[selected]) { e.preventDefault(); $("#modal").close(); openFile(results[selected].path); }
    });
    modal("Quick open", h("div", {}, h("div", { class: "row", style: "padding:0 0 8px;" }, input), list));
    input.focus();
    search();
  }

  // ---------- sandbox ----------
  async function loadSandboxChoices() {
    try {
      state.sandboxChoices = await api("GET", "/web/api/sandbox?" + q({ directory: state.dir }));
    } catch (error) {
      state.sandboxChoices = null;
    }
    renderSandbox();
  }
  async function loadSessionSandbox(sid) {
    try {
      const value = await api("GET", "/web/api/session/" + encodeURIComponent(sid) + "/sandbox");
      if (sid === state.sid) state.sandbox = value;
    } catch {
      if (sid === state.sid) state.sandbox = null;
    }
    renderSandbox();
  }
  function renderSandbox() {
    const select = $("#sandbox");
    const info = state.sid ? state.sandbox : state.sandboxChoices;
    // Builds without sandbox support only offer "off"; there is nothing to choose.
    if (!info || !info.available || info.error) { select.hidden = true; return; }
    const current = state.sid ? info.preset || "" : state.sandboxPick;
    select.replaceChildren(
      h("option", { value: "", text: "Sandbox: " + info.default + " (default)" }),
      ...info.presets.map((p) => h("option", { value: p, text: "Sandbox: " + p })));
    select.value = current;
    if (select.value !== current) {
      select.append(h("option", { value: current, text: "Sandbox: " + current }));
      select.value = current;
    }
    select.hidden = false;
    const busy = state.sid && state.busy.has(state.sid);
    select.disabled = !!info.forced || (state.sid && info.root === false) || !!busy;
    select.title = info.forced ? "This server was started with --sandbox-preset " + info.forced
      : state.sid && info.root === false ? "Subagent sessions use their parent's sandbox"
        : busy ? "Stop the session to change its sandbox"
          : state.sid ? "Sandbox for this session's next turn; changing it stops its background jobs" : "Sandbox for the new session";
  }
  async function selectSandbox() {
    const preset = $("#sandbox").value;
    if (!state.sid) {
      state.sandboxPick = preset;
      return;
    }
    const choice = preset || (state.sandbox && state.sandbox.default);
    try {
      state.sandbox = await api("PUT", "/web/api/session/" + encodeURIComponent(state.sid) + "/sandbox", { preset: choice });
      toast("Sandbox: " + choice + " applies from the next turn");
    } catch (error) {
      fail(error);
    }
    renderSandbox();
  }

  // ---------- settings ----------
  const SETTING_LABELS = {
    model: "Default model",
    small_model: "Small model",
    title_model: "Title model",
    sandbox: "Default sandbox",
    "compaction.auto": "Auto compaction",
    "compaction.prune": "Prune old tool output",
  };
  async function openSettings(scope = store.get("settingsScope") || "project", initialData = null) {
    let data = initialData;
    if (!data) {
      try {
        data = await api("GET", "/web/api/config?" + q({ directory: state.dir }));
      } catch (error) { return fail(error); }
    }
    store.set("settingsScope", scope);
    const file = data.files[scope];
    const values = file.values || {};
    const effective = data.effective || {};
    const sandbox = data.sandbox || {};
    const modelOptions = (extra) => [h("option", { value: "", text: "Not set" })].concat(extra || [],
      state.models.map((m) => h("option", { value: modelKey(m), text: modelKey(m) + (m.name && m.name !== m.id ? " — " + m.name : "") })));
    const boolOptions = () => [h("option", { value: "", text: "Not set" }), h("option", { value: "true", text: "On" }), h("option", { value: "false", text: "Off" })];
    const controls = {};
    const field = (key, options, hint) => {
      const select = h("select", { id: "set-" + key.replace(".", "-"), disabled: !file.editable }, ...options);
      const raw = values[key];
      const value = raw == null ? "" : typeof raw === "object" ? (raw.preset || "") : String(raw);
      select.value = value;
      if (select.value !== value) { select.append(h("option", { value, text: value })); select.value = value; }
      controls[key] = select;
      return h("div", { class: "field" }, h("label", { for: select.id, text: SETTING_LABELS[key] }), select,
        hint ? h("div", { class: "hint", text: hint }) : null);
    };
    const eff = (v) => (v === "" || v == null ? "none" : String(v));
    const scopeButtons = h("div", { class: "row scope" }, ...["project", "global"].map((name) =>
      h("button", { type: "button", class: name === scope ? "active" : "", text: name === "project" ? "This project" : "Global",
        onclick: () => openSettings(name) })));
    const save = async () => {
      const set = {};
      for (const [key, select] of Object.entries(controls)) {
        const v = select.value;
        set[key] = v === "" ? null : key.startsWith("compaction.") ? v === "true" : v;
      }
      try {
        const updated = await api("PATCH", "/web/api/config", { scope, directory: state.dir, set });
        toast("Settings saved");
        if (updated && updated.sandbox) {
          state.sandboxChoices = updated.sandbox;
          renderSandbox();
        }
        openSettings(scope, updated);
      } catch (error) { fail(error); }
    };
    const openConfig = async () => {
      $("#modal").close();
      if (!file.exists) {
        try {
          await api("PUT", "/web/api/fs/write", { path: file.path, content: '{\n  "$schema": "https://opencode.ai/config.json"\n}\n', create: true });
        } catch (error) { if (error.status !== 409) return fail(error); }
      }
      openFile(file.path);
    };
    modal("Settings", h("div", {},
      scopeButtons,
      h("p", { class: "note" }, "Saved in ", h("code", { text: file.path }), file.exists ? "" : " (created on save)", ". ",
        scope === "project" ? "Project settings override global ones for " + state.dir + "." : "Global settings apply to every project."),
      data.error ? h("div", { class: "error", text: "Current configuration error: " + data.error }) : null,
      file.editable ? null : h("div", { class: "error", text: "This file uses JSONC comments or is not plain JSON; edit it directly to keep its formatting." }),
      field("model", modelOptions(), "In effect: " + eff(effective.model)),
      field("small_model", modelOptions(), "In effect: " + eff(effective.small_model)),
      field("title_model", modelOptions([h("option", { value: "small", text: "small (use the small model)" })]),
        "In effect: " + (effective.title_model ? effective.title_model : "first line of the prompt, no model call")),
      sandbox.available ? field("sandbox", [h("option", { value: "", text: "Not set" })].concat((sandbox.presets || []).map((p) => h("option", { value: p, text: p }))),
        "In effect for new sessions: " + eff(sandbox.default) + (sandbox.forced ? " (forced by --sandbox-preset)" : "")) : null,
      field("compaction.auto", boolOptions(), "In effect: " + (effective.compaction ? (effective.compaction.auto ? "on" : "off") : "unknown")),
      field("compaction.prune", boolOptions(), "In effect: " + (effective.compaction ? (effective.compaction.prune ? "on" : "off") : "unknown")),
      h("div", { class: "buttons" },
        h("button", { type: "button", class: "primary", text: "Save", disabled: !file.editable, onclick: save }),
        h("button", { type: "button", text: "Edit " + basename(file.path) + " in the editor", onclick: openConfig })),
      h("p", { class: "note", text: "Providers, agents, permissions, MCP servers and hooks are configured in the file. Model and sandbox defaults apply to new sessions; restart the server for MCP and hook changes." }),
      h("h3", { text: "Connected models" }),
      h("p", { class: "note", text: state.models.length ? [...new Set(state.models.map((m) => m.providerID))].map((p) =>
        p + " (" + state.models.filter((m) => m.providerID === p).length + ")").join(", ") : "No connected providers. Add one under \"provider\" in the configuration file." })));
  }

  // ---------- layout ----------
  function showView(view) {
    const app = $("#app");
    app.classList.toggle("view-chat", view === "chat");
    app.classList.toggle("view-editor", view === "editor");
    document.querySelectorAll(".views button").forEach((b) => {
      const active = b.dataset.view === view;
      b.classList.toggle("active", active);
      b.setAttribute("aria-selected", String(active));
    });
  }
  function showTab(tab) {
    document.querySelectorAll(".tabs button").forEach((b) => {
      const active = b.dataset.tab === tab;
      b.classList.toggle("active", active);
      b.setAttribute("aria-selected", String(active));
    });
    $("#tab-sessions").hidden = tab !== "sessions";
    $("#tab-files").hidden = tab !== "files";
    store.set("tab", tab);
  }
  function bindArrowNav(selector, activateBtn) {
    const buttons = [...document.querySelectorAll(selector)];
    buttons.forEach((btn, idx) => {
      btn.addEventListener("keydown", (e) => {
        if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
        e.preventDefault();
        const next = buttons[(idx + (e.key === "ArrowRight" ? 1 : buttons.length - 1)) % buttons.length];
        next.focus();
        activateBtn(next);
      });
    });
  }
  function resizer() {
    const divider = $("#divider");
    divider.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      divider.setPointerCapture(e.pointerId);
      const main = document.querySelector("main").getBoundingClientRect();
      const move = (ev) => {
        const width = Math.min(Math.max(main.right - ev.clientX, 240), main.width - 280);
        document.documentElement.style.setProperty("--editor-width", width + "px");
      };
      const up = () => {
        divider.removeEventListener("pointermove", move);
        store.set("editorWidth", getComputedStyle(document.documentElement).getPropertyValue("--editor-width"));
      };
      divider.addEventListener("pointermove", move);
      divider.addEventListener("pointerup", up, { once: true });
    });
    const saved = store.get("editorWidth");
    if (saved) document.documentElement.style.setProperty("--editor-width", saved);
  }

  // ---------- wiring ----------
  function bind() {
    $("#menu").onclick = () => {
      const app = $("#app");
      if (narrow()) app.classList.toggle("sidebar-open");
      else app.classList.toggle("sidebar-hidden");
    };
    document.querySelectorAll(".tabs button").forEach((b) => (b.onclick = () => showTab(b.dataset.tab)));
    document.querySelectorAll(".views button").forEach((b) => (b.onclick = () => showView(b.dataset.view)));
    bindArrowNav(".tabs button", (b) => showTab(b.dataset.tab));
    bindArrowNav(".views button", (b) => showView(b.dataset.view));
    bindArrowNav(".scope button[data-scope]", (b) => {
      state.scope = b.dataset.scope;
      store.set("scope", state.scope);
      loadSessions().catch(fail);
    });
    $("#project").onclick = changeDirectory;
    $("#settings").onclick = () => openSettings();
    $("#mcp-btn").onclick = openMcpModal;
    $("#theme-toggle").onclick = cycleTheme;
    $("#quick-open").onclick = openQuickOpen;
    $("#quick-open-top").onclick = openQuickOpen;
    $("#sandbox").onchange = selectSandbox;
    document.querySelectorAll(".scope button[data-scope]").forEach((b) => (b.onclick = () => {
      state.scope = b.dataset.scope;
      store.set("scope", state.scope);
      loadSessions().catch(fail);
    }));
    $("#archived-toggle").onclick = () => {
      state.showArchived = !state.showArchived;
      loadSessions().catch(fail);
    };
    $("#new-session").onclick = () => { clearSession(); showView("chat"); $("#input").focus(); if (narrow()) $("#app").classList.remove("sidebar-open"); };
    let searchTimer = 0;
    $("#session-search").oninput = () => { clearTimeout(searchTimer); searchTimer = setTimeout(() => loadSessions().catch(fail), 250); };
    $("#more-sessions").onclick = () => loadSessions(true).catch(fail);
    $("#older").onclick = () => loadOlder().catch(fail);
    $("#parent-session").onclick = () => state.session && state.session.parentID && openSession(state.session.parentID);
    $("#session-title").onclick = () => state.session && renameSession(state.session);
    $("#toggle-thinking").onclick = toggleThinking;
    $("#toggle-tools").onclick = cycleToolView;
    $("#instructions").onclick = openInstructionsModal;
    $("#move-session").onclick = moveSessionModal;
    $("#compact").onclick = compactSession;
    $("#fork").onclick = () => forkSession();
    $("#model").onchange = () => { $("#variant").value = ""; selectModel(); };
    $("#variant").onchange = selectModel;
    $("#agent").onchange = selectAgent;
    $("#delivery").onchange = renderStatus;
    $("#composer").onsubmit = submit;
    $("#stop").onclick = interrupt;
    $("#attach-btn").onclick = () => $("#attach").click();
    $("#attach").onchange = (e) => { attachFiles([...e.target.files]); e.target.value = ""; };
    $("#skill-pick").onchange = (e) => {
      const id = e.target.value;
      if (id && !state.selectedSkills.includes(id)) {
        state.selectedSkills.push(id);
        renderAttachments();
      }
      e.target.value = "";
      $("#input").focus();
    };
    const input = $("#input");
    input.addEventListener("input", () => {
      state.historyIndex = -1;
      autosize();
      renderSuggest();
      store.set("draft", input.value);
    });
    input.addEventListener("keydown", (e) => {
      const open = !$("#suggest").hidden;
      if (open && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
        e.preventDefault();
        const count = suggestions().length;
        suggestIndex = (suggestIndex + (e.key === "ArrowDown" ? 1 : count - 1)) % count;
        return renderSuggest();
      }
      if (open && (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey))) {
        e.preventDefault();
        return pickSuggest(suggestions()[suggestIndex]);
      }
      if (open && e.key === "Escape") return hideSuggest();
      if (!open && e.key === "ArrowUp" && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
        const onFirstLine = !input.value.slice(0, input.selectionStart).includes("\n");
        if (onFirstLine && state.promptHistory.length && (input.value === "" || state.historyIndex !== -1)) {
          e.preventDefault();
          if (state.historyIndex === -1) {
            state.historyDraft = input.value;
            state.historyIndex = state.promptHistory.length - 1;
          } else if (state.historyIndex > 0) {
            state.historyIndex--;
          }
          input.value = state.promptHistory[state.historyIndex];
          input.setSelectionRange(input.value.length, input.value.length);
          autosize();
          return;
        }
      }
      if (!open && e.key === "ArrowDown" && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
        const onLastLine = !input.value.slice(input.selectionEnd).includes("\n");
        if (onLastLine && state.historyIndex !== -1) {
          e.preventDefault();
          if (state.historyIndex < state.promptHistory.length - 1) {
            state.historyIndex++;
            input.value = state.promptHistory[state.historyIndex];
          } else {
            state.historyIndex = -1;
            input.value = state.historyDraft;
          }
          input.setSelectionRange(input.value.length, input.value.length);
          autosize();
          return;
        }
      }
      if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        submit();
      }
      if (e.key === "Escape" && state.sid && state.busy.has(state.sid)) interrupt();
    });
    input.addEventListener("blur", () => setTimeout(hideSuggest, 100));
    input.addEventListener("paste", (e) => {
      const files = [...(e.clipboardData ? e.clipboardData.files : [])];
      if (files.length) { e.preventDefault(); attachFiles(files); }
    });
    const composer = $("#composer");
    composer.addEventListener("dragover", (e) => e.preventDefault());
    composer.addEventListener("drop", (e) => {
      if (!e.dataTransfer || !e.dataTransfer.files.length) return;
      e.preventDefault();
      attachFiles([...e.dataTransfer.files]);
    });
    $("#transcript").addEventListener("scroll", () => { stick = nearBottom(); });
    $("#transcript").addEventListener("click", (e) => {
      const button = e.target.closest("[data-copy]");
      if (!button) return;
      const text = button.closest("pre").querySelector("code").textContent;
      navigator.clipboard.writeText(text).then(() => { button.textContent = "Copied"; setTimeout(() => (button.textContent = "Copy"), 1200); }, fail);
    });

    $("#new-file").onclick = newFile;
    $("#new-folder").onclick = newFolder;
    $("#refresh-files").onclick = refreshTree;
    const c = code();
    c.addEventListener("input", onEdit);
    c.addEventListener("keydown", editorKeys);
    c.addEventListener("scroll", () => { $("#gutter").scrollTop = c.scrollTop; });
    c.addEventListener("click", updateCursor);
    c.addEventListener("keyup", updateCursor);
    $("#save").onclick = () => save();
    $("#reload-file").onclick = () => reloadFile();
    $("#attach-file").onclick = () => {
      if (!state.active) return;
      if (state.active.dirty) toast("Attaching the saved version; save first to include your edits");
      state.attachments.push({ name: relative(state.active.path), uri: fileUri(state.active.path) });
      renderAttachments();
      showView("chat");
      $("#input").focus();
    };
    document.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s" && state.active) { e.preventDefault(); save(); }
      else if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "p") { e.preventDefault(); openQuickOpen(); }
      else if (e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "t") { e.preventDefault(); toggleThinking(); }
      else if (e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "o") { e.preventDefault(); cycleToolView(); }
    });
    addEventListener("beforeunload", (e) => {
      stash();
      if (state.files.some((f) => f.dirty)) { e.preventDefault(); e.returnValue = ""; }
    });
    addEventListener("focus", refreshOpenFiles);
    addEventListener("hashchange", () => {
      const sid = location.hash.slice(1);
      if (sid && sid !== state.sid) openSession(sid);
    });
    resizer();
  }

  async function start() {
    bind();
    applyTheme(store.get("theme") || "system");
    state.showThinking = store.get("thinking") !== "off";
    const savedToolView = store.get("toolView");
    if (["brief", "full", "grouped"].includes(savedToolView)) state.toolView = savedToolView;
    renderDisplayControls();
    showTab(store.get("tab") || "sessions");
    state.scope = store.get("scope") === "project" ? "project" : "all";
    try { state.selection = JSON.parse(store.get("model") || "null"); } catch { state.selection = null; }
    const draft = store.get("draft");
    if (draft) { $("#input").value = draft; autosize(); }
    let info;
    try {
      info = await api("GET", "/web/api/info");
    } catch (error) {
      return fail(error);
    }
    state.sep = info.separator || "/";
    connect();
    const sid = location.hash.slice(1);
    let dir = store.get("dir") || info.cwd;
    if (sid) await openSession(sid);
    if (!state.dir) {
      try {
        await setDirectory(dir, !!state.sid);
      } catch {
        dir = info.cwd;
        await setDirectory(dir, !!state.sid).catch(fail);
      }
      if (!state.sid) renderAll();
    }
    let saved = [];
    try { saved = JSON.parse(store.get("files") || "[]"); } catch { /* ignore */ }
    for (const path of saved) await openFile(path, true);
    showView("chat");
  }
  start();
})();
