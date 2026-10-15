// hearth dashboard. Reads /api/frame (what `hearth --json top` prints) on
// an interval and patches the page in place, so nothing re-animates on a
// refresh. Everything from the server is set as text, never as markup:
// journal notes and server output are written by other programs.
"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const reduced = matchMedia("(prefers-reduced-motion: reduce)");
  const view = { frame: null, selected: 0, drawer: null, armed: null, seen: new Set(), first: true };
  let refreshMs = 2000;
  let timer = 0;

  document.documentElement.classList.add("js");

  // Text helpers

  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };
  const setText = (node, text) => {
    const t = String(text);
    if (node.textContent !== t) node.textContent = t;
  };
  const bytes = (n) => {
    if (typeof n !== "number") return "-";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let i = 0;
    while (n >= 1024 && i < units.length - 1) { n /= 1024; i += 1; }
    return `${n.toFixed(i >= 3 ? 2 : i === 0 ? 0 : 1)} ${units[i]}`;
  };
  const age = (ms) => {
    if (typeof ms !== "number") return "";
    const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (s < 60) return `${s} s ago`;
    if (s < 3600) return `${Math.round(s / 60)} min ago`;
    if (s < 86400) return `${Math.round(s / 3600)} h ago`;
    return `${Math.round(s / 86400)} d ago`;
  };
  const tone = (state) => ({
    ready: "ok", starting: "wait", waiting: "wait",
    gave_up: "bad", unsupervised: "bad",
  })[state] || "idle";
  const humanState = (s) => String(s || "unknown").replace(/_/g, " ");

  // Network

  async function api(path, method = "GET") {
    const res = await fetch(path, {
      method,
      headers: method === "GET" ? {} : { "X-Hearth": "1" },
      cache: "no-store",
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok || body.error) {
      const e = body.error || {};
      throw new Error([e.message || res.statusText, e.remediation].filter(Boolean).join(". "));
    }
    return body;
  }

  // Toasts

  function toast(text, bad = false) {
    const t = el("div", "toast", text);
    if (bad) t.dataset.tone = "bad";
    $("toasts").append(t);
    const ttl = bad ? 6000 : 2600;
    setTimeout(() => {
      t.classList.add("leaving");
      t.addEventListener("animationend", () => t.remove(), { once: true });
      if (reduced.matches) t.remove();
    }, ttl);
  }

  // Machine

  function meter(fillId, meterId, used, total, level) {
    const f = total > 0 ? Math.min(1, Math.max(0, used / total)) : 0;
    $(fillId).style.transform = `scaleX(${f.toFixed(4)})`;
    const m = $(meterId);
    if (level) m.dataset.level = level; else delete m.dataset.level;
    if (m.getAttribute("role") === "meter") m.setAttribute("aria-valuenow", String(Math.round(f * 100)));
  }

  function renderMachine(machine) {
    if (!machine || machine.error) {
      setText($("mem-avail"), "-");
      setText($("mem-total"), machine && machine.error ? machine.error : "");
      return;
    }
    const mem = (machine.health || {}).memory || {};
    const lim = (machine.health || {}).limits || {};
    setText($("mem-avail"), bytes(mem.available_bytes));
    setText($("mem-total"), `of ${bytes(mem.total_bytes)}`);
    const pressure = mem.pressure || "unknown";
    const level = pressure === "warn" ? "warn" : pressure === "critical" ? "critical" : "";
    const used = (mem.total_bytes || 0) - (mem.available_bytes || 0);
    meter("mem-fill", "mem-meter", used, mem.total_bytes || 0, level);
    setText($("mem-used"), mem.total_bytes ? `${Math.round((100 * used) / mem.total_bytes)}% in use` : "");
    const p = $("pressure");
    setText(p, pressure);
    p.dataset.tone = pressure === "normal" ? "ok" : pressure === "warn" ? "wait" : pressure === "critical" ? "bad" : "";
    setText($("swap"), mem.swap_total_bytes ? `${bytes(mem.swap_used_bytes)} of ${bytes(mem.swap_total_bytes)}` : "none");
    const acc = (machine.accelerators || [])[0];
    setText($("device"), acc ? `${acc.name}${acc.recommended_working_set_bytes ? `, ${bytes(acc.recommended_working_set_bytes)} working set` : ""}` : "-");

    const pair = (id, a, b) => {
      const n = $(id);
      n.replaceChildren(document.createTextNode(typeof a === "number" ? a.toLocaleString() : "-"),
        el("span", "of", typeof b === "number" ? `of ${b.toLocaleString()}` : ""));
    };
    pair("files", lim.files_open, lim.files_max);
    pair("threads", lim.threads_total, lim.threads_max);
    meter("files-fill", "files-meter", lim.files_open || 0, lim.files_max || 0);
    meter("threads-fill", "threads-meter", lim.threads_total || 0, lim.threads_max || 0);

    const findings = machine.findings || [];
    setText($("findings-count"), findings.length ? String(findings.length) : "");
    $("findings-none").hidden = findings.length > 0;
    const list = $("finding-list");
    const key = JSON.stringify(findings.map((f) => [f.severity, f.message, f.remediation]));
    if (list.dataset.key !== key) {
      list.dataset.key = key;
      list.replaceChildren(...findings.map((f) => {
        const li = el("li", "finding");
        li.dataset.tone = f.severity === "critical" ? "bad" : "wait";
        li.append(el("p", "", `${f.severity}: ${f.message}`));
        if (f.remediation) li.append(el("p", "fix", f.remediation));
        return li;
      }));
    }
  }

  // Servers

  function serverRow(s) {
    const li = el("li", "server");
    li.dataset.name = s.name;
    li.append(el("span", "dot"));
    const who = el("div", "who");
    const top = el("p");
    top.append(el("span", "name", s.name), el("span", "state"));
    who.append(top, el("p", "sub"));
    const numbers = el("div", "numbers");
    for (const k of ["memory", "restarts", "pid"]) {
      const span = el("span");
      span.dataset.k = k;
      span.append(el("small", "", k), el("b", "num"));
      numbers.append(span);
    }
    const actions = el("div", "actions");
    for (const [op, label] of [["restart", "Restart"], ["stop", "Stop"], ["start", "Start"], ["logs", "Output"]]) {
      const b = el("button", "act", label);
      b.type = "button";
      b.dataset.op = op;
      if (op === "logs") b.setAttribute("aria-pressed", "false");
      b.setAttribute("aria-label", `${label} ${s.name}`);
      actions.append(b);
    }
    li.append(who, numbers, actions, el("p", "event"));
    return li;
  }

  function patchServer(li, s, index) {
    const state = s.state || "unknown";
    li.dataset.tone = tone(state);
    li.setAttribute("aria-current", index === view.selected ? "true" : "false");
    setText(li.querySelector(".state"), humanState(state));
    setText(li.querySelector(".sub"), [s.addr, s.expect && `expects ${s.expect}`, s.program].filter(Boolean).join("  ·  "));
    setText(li.querySelector('[data-k="memory"] b'), s.footprint_bytes ? bytes(s.footprint_bytes) : "-");
    setText(li.querySelector('[data-k="restarts"] b'), s.restarts ?? 0);
    setText(li.querySelector('[data-k="pid"] b'), s.server_pid ?? s.pid ?? "-");
    const stopped = state === "stopped" || state === "gave_up";
    li.querySelector('[data-op="restart"]').disabled = stopped;
    li.querySelector('[data-op="stop"]').disabled = stopped;
    li.querySelector('[data-op="start"]').disabled = !stopped;
    li.querySelector('[data-op="logs"]').setAttribute("aria-pressed", String(view.drawer === s.name));
    const ev = s.last_event;
    const what = ev ? [ev.event, ev.cause, ev.decision].filter(Boolean).join(", ") : "";
    setText(li.querySelector(".event"), what ? `last: ${what} ${age(s.last_event_at_ms)}` : "");
  }

  function renderServers(servers) {
    const list = $("server-list");
    const err = $("servers-error");
    if (!Array.isArray(servers)) {
      err.hidden = false;
      setText(err, (servers && servers.error) || "hearthd did not answer");
      list.replaceChildren();
      $("servers-empty").hidden = true;
      return;
    }
    err.hidden = true;
    $("servers-empty").hidden = servers.length > 0;
    view.selected = Math.min(view.selected, Math.max(0, servers.length - 1));
    const existing = new Map([...list.children].map((li) => [li.dataset.name, li]));
    servers.forEach((s, i) => {
      let li = existing.get(s.name);
      if (!li) {
        li = serverRow(s);
        if (!reduced.matches) { li.classList.add("enter"); li.style.setProperty("--i", String(i)); }
      }
      existing.delete(s.name);
      patchServer(li, s, i);
      if (list.children[i] !== li) list.insertBefore(li, list.children[i] || null);
    });
    for (const gone of existing.values()) gone.remove();
  }

  // Journal

  function renderJournal(journal) {
    const err = $("journal-error");
    const tl = $("timeline");
    if (!Array.isArray(journal)) {
      err.hidden = false;
      setText(err, (journal && journal.error) || "journal not readable");
      return;
    }
    err.hidden = true;
    setText($("journal-count"), journal.length ? String(journal.length) : "");
    const nodes = journal.map((e) => {
      const id = e.id || `${e.kind}-${e.taken_at_ms}`;
      const li = el("li", "entry");
      li.dataset.kind = e.kind || "";
      const top = el("p", "entry-top");
      top.append(el("span", "kind", e.kind || ""), el("span", "age", age(e.taken_at_ms)));
      li.append(top, el("p", "summary", e.summary || ""));
      if (!view.first && !view.seen.has(id) && !reduced.matches) li.classList.add("new");
      view.seen.add(id);
      return li;
    });
    tl.replaceChildren(...nodes);
  }

  // Drawer

  let lastFocus = null;
  async function fillLog(name) {
    try {
      const a = await api(`/api/logs/${encodeURIComponent(name)}`);
      const log = $("log");
      const atEnd = log.scrollHeight - log.scrollTop - log.clientHeight < 24;
      setText(log, (a.lines || []).join("\n") || "(no output yet)");
      if (atEnd) log.scrollTop = log.scrollHeight;
    } catch (e) {
      setText($("log"), e.message);
    }
  }
  function openDrawer(name) {
    lastFocus = document.activeElement;
    view.drawer = name;
    setText($("drawer-h"), name);
    setText($("log"), "");
    const d = $("drawer");
    d.classList.add("open");
    d.setAttribute("aria-hidden", "false");
    $("scrim").hidden = false;
    for (const id of ["main", "bar"]) $(id).inert = true;
    fillLog(name);
    $("drawer-close").focus();
    syncPressed();
  }
  function closeDrawer() {
    if (!view.drawer) return;
    view.drawer = null;
    const d = $("drawer");
    d.classList.remove("open");
    d.setAttribute("aria-hidden", "true");
    $("scrim").hidden = true;
    for (const id of ["main", "bar"]) $(id).inert = false;
    syncPressed();
    if (lastFocus && document.contains(lastFocus)) lastFocus.focus();
  }
  function syncPressed() {
    for (const b of document.querySelectorAll('[data-op="logs"]')) {
      b.setAttribute("aria-pressed", String(b.closest(".server").dataset.name === view.drawer));
    }
  }

  // Actions

  function disarm() {
    if (!view.armed) return;
    clearTimeout(view.armed.timer);
    const b = view.armed.button;
    b.dataset.armed = "false";
    b.textContent = "Stop";
    view.armed = null;
  }

  async function act(name, op, button) {
    if (op === "logs") {
      if (view.drawer === name) closeDrawer(); else openDrawer(name);
      return;
    }
    if (op === "stop" && !(view.armed && view.armed.button === button)) {
      // Stopping frees a model's memory and keeps it down: ask once more.
      disarm();
      button.dataset.armed = "true";
      button.textContent = "Confirm stop";
      view.armed = { button, timer: setTimeout(disarm, 3500) };
      return;
    }
    disarm();
    if (button) button.disabled = true;
    try {
      await api(`/api/servers/${encodeURIComponent(name)}/${op}`, "POST");
      toast(op === "stop" ? `${name} stopping; its memory is freed until you start it`
        : op === "start" ? `${name} starting` : `${name} restarting`);
    } catch (e) {
      toast(`${op} ${name}: ${e.message}`, true);
    }
    refresh();
  }

  $("server-list").addEventListener("click", (ev) => {
    const row = ev.target.closest(".server");
    if (!row) return;
    const names = [...$("server-list").children].map((li) => li.dataset.name);
    view.selected = names.indexOf(row.dataset.name);
    const b = ev.target.closest("button[data-op]");
    if (b && !b.disabled) act(row.dataset.name, b.dataset.op, b);
    if (view.frame) renderServers(view.frame.servers);
  });

  // Keyboard: the same keys as `hearth top`.

  function selectedRow() {
    return $("server-list").children[view.selected] || null;
  }
  document.addEventListener("keydown", (ev) => {
    if (ev.metaKey || ev.ctrlKey || ev.altKey) return;
    const tag = (ev.target.tagName || "").toLowerCase();
    if (tag === "input" || tag === "textarea") return;
    if ($("help").open) return;
    const rows = $("server-list").children.length;
    const row = selectedRow();
    const press = (op) => {
      const b = row && row.querySelector(`[data-op="${op}"]`);
      if (b && !b.disabled) { ev.preventDefault(); act(row.dataset.name, op, b); }
    };
    switch (ev.key) {
      case "ArrowDown": case "j":
        if (rows) { ev.preventDefault(); view.selected = Math.min(rows - 1, view.selected + 1); }
        break;
      case "ArrowUp": case "k":
        if (rows) { ev.preventDefault(); view.selected = Math.max(0, view.selected - 1); }
        break;
      case "r": press("restart"); return;
      case "s": press("stop"); return;
      case "S": press("start"); return;
      case "l": press("logs"); return;
      case "Escape": disarm(); closeDrawer(); return;
      case "?": ev.preventDefault(); $("help").showModal(); return;
      default: return;
    }
    if (view.frame) renderServers(view.frame.servers);
    const r = selectedRow();
    if (r) r.scrollIntoView({ block: "nearest", behavior: reduced.matches ? "auto" : "smooth" });
  });

  $("help-open").addEventListener("click", () => $("help").showModal());
  $("help").querySelector("form").addEventListener("submit", (ev) => {
    ev.preventDefault();
    $("help").close();
  });
  $("drawer-close").addEventListener("click", closeDrawer);
  $("scrim").addEventListener("click", closeDrawer);

  // Refresh loop: slower while the tab is hidden, and after a failure.

  function link(state, text) {
    const l = $("link");
    l.dataset.state = state;
    setText($("link-text"), text);
  }
  async function refresh() {
    clearTimeout(timer);
    let wait = refreshMs;
    try {
      const frame = await api("/api/frame");
      view.frame = frame;
      if (typeof frame.refresh_ms === "number") refreshMs = frame.refresh_ms;
      renderMachine(frame.machine);
      renderServers(frame.servers);
      renderJournal(frame.journal);
      if (view.drawer) fillLog(view.drawer);
      link("live", `live, every ${Math.round(refreshMs / 100) / 10} s`);
      const p = document.querySelector(".pulse");
      p.classList.remove("beat"); void p.offsetWidth; p.classList.add("beat");
      if (view.first) { view.first = false; reveal(); }
    } catch (e) {
      link("lost", "hearthd not answering, retrying");
      wait = refreshMs * 3;
    }
    if (document.hidden) wait *= 5;
    timer = setTimeout(refresh, wait);
  }
  document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });

  // First paint: sections arrive in order; ones below the fold as they
  // scroll into view.

  function reveal() {
    const sections = [...document.querySelectorAll(".reveal")];
    if (reduced.matches || !("IntersectionObserver" in window)) {
      sections.forEach((s) => s.classList.add("in"));
      return;
    }
    let order = 0;
    const io = new IntersectionObserver((entries) => {
      for (const en of entries) {
        if (!en.isIntersecting) continue;
        en.target.style.setProperty("--i", String(order++));
        en.target.classList.add("in");
        io.unobserve(en.target);
      }
    }, { rootMargin: "0px 0px -8% 0px" });
    sections.forEach((s) => io.observe(s));
    // An observer that never fires (a hidden frame, print) must not leave
    // the page blank.
    setTimeout(() => sections.forEach((s) => s.classList.add("in")), 1500);
  }

  refresh();
})();
