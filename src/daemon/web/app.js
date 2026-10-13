// LStack remote dashboard — dependency-free. Served by the lstack daemon when remote access is on.
(() => {
  "use strict";
  const $app = document.getElementById("app");
  const MAX_LOG_LINES = 3000;
  const HISTORY = 90;

  const S = {
    session: null,
    services: new Map(),
    configErrors: [],
    history: new Map(), // service key -> [{ts,cpu,mem}]
    selected: null, // service key (see keyOf)
    shown: null, // key the detail pane currently shows
    local: "", // this machine's name
    machines: new Map(), // peer name -> {info, services: Map, configErrors, processes, system}
    processes: [],
    thresholds: null,
    system: null, // this machine's {cores, memTotal, samples}
    range: "15m",
    logs: [],
    filter: "",
    paused: false,
    wrap: true,
    events: null,
    logStream: null,
    chartTimer: null,
    tickTimer: null,
  };

  // ------------------------------------------------------------------ utils
  const ESC = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ESC[c]);
  const h = (html) => { const t = document.createElement("template"); t.innerHTML = html.trim(); return t.content.firstElementChild; };
  const bytes = (n) => {
    if (n == null || isNaN(n)) return "—";
    const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0;
    while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
    return `${n >= 100 || i === 0 ? n.toFixed(0) : n.toFixed(1)} ${u[i]}`;
  };
  const rate = (n) => (n == null ? "—" : `${bytes(n)}/s`);
  const pct = (n) => (n == null ? "—" : `${n.toFixed(n >= 100 ? 0 : 1)}%`);
  const dur = (ms) => {
    if (ms == null || ms < 0) return "—";
    const s = Math.floor(ms / 1000), d = Math.floor(s / 86400), hh = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
    if (d) return `${d}d ${hh}h`; if (hh) return `${hh}h ${m}m`; if (m) return `${m}m ${s % 60}s`; return `${s}s`;
  };
  const clock = (ts) => new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const shortTime = (ts, range) => new Date(ts).toLocaleString([], range === "7d" ? { weekday: "short", hour: "2-digit", minute: "2-digit" } : { hour: "2-digit", minute: "2-digit" });
  const live = (s) => ["running", "starting", "stopping"].includes(s.status);
  // A crash-looping service is between attempts: not live, but Stop cancels the pending restart.
  const looping = (s) => !!s.restartAt;
  const statusLabel = (s) => looping(s) ? `restarting ${Math.max(0, Math.ceil((s.restartAt - Date.now()) / 1000))}s` : s.status;
  const enc = encodeURIComponent;
  const ago = (ts) => {
    const s = Math.max(0, Math.round((Date.now() - ts) / 1000));
    if (s < 60) return `${s}s ago`; if (s < 3600) return `${Math.floor(s / 60)}m ago`;
    return s < 86400 ? `${Math.floor(s / 3600)}h ago` : `${Math.floor(s / 86400)}d ago`;
  };

  // Services are keyed by machine + name: names can repeat across machines. Local machine = "".
  const keyOf = (m, n) => JSON.stringify([m || "", n]);
  const parseKey = (k) => { try { const [m, n] = JSON.parse(k); return { m, n }; } catch { return { m: "", n: k }; } };
  const svcKey = (s) => keyOf(s._m, s.name);
  const tagged = (m, s) => { s._m = m || ""; return s; };
  const svcMap = (m) => (m ? S.machines.get(m)?.services : S.services);
  const getSvc = (k) => { if (!k) return null; const { m, n } = parseKey(k); return svcMap(m)?.get(n) || null; };
  const selSvc = () => getSvc(S.selected);
  const svcPath = (s) => (s._m ? `/api/machines/${enc(s._m)}/services/${enc(s.name)}` : `/api/services/${enc(s.name)}`);
  const sortedSvcs = (map) => [...map.values()].sort((a, b) => a.name.localeCompare(b.name));
  const sortedPeers = () => [...S.machines.values()].sort((a, b) => a.info.name.localeCompare(b.info.name));
  // A machine overview is selected with a 3-element key; services use 2 (see keyOf).
  const machKey = (m) => JSON.stringify([m || "", "", "machine"]);
  const selMach = () => { try { const a = JSON.parse(S.selected); return Array.isArray(a) && a.length === 3 ? a[0] : null; } catch { return null; } };
  const machExists = (m) => m === "" || S.machines.has(m);
  const sysOf = (m) => (m ? S.machines.get(m)?.system : S.system);
  const SYS_HISTORY = 300;
  const canControl = (s) => !!S.session?.allowControl && (!s._m || !!S.machines.get(s._m)?.info.allowControl);
  {
    const v = localStorage.getItem("lstack.sel");
    S.selected = v ? (v.startsWith("[") ? v : keyOf("", v)) : null;
  }

  async function api(path, opts = {}) {
    const res = await fetch(path, {
      method: opts.method || "GET",
      credentials: "same-origin",
      headers: { "X-LStack": "1", ...(opts.body ? { "Content-Type": "application/json" } : {}) },
      body: opts.body ? JSON.stringify(opts.body) : undefined,
    });
    let data = null;
    try { data = await res.json(); } catch { /* empty body */ }
    if (res.status === 401 && !opts.login) { showLogin(); throw new Error("login required"); }
    if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
    return data;
  }

  function toast(msg, err) {
    const t = h(`<div class="toast ${err ? "err" : ""}">${esc(msg)}</div>`);
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 3200);
  }

  const ICONS = {
    process: '<svg viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/></svg>',
    docker: '<svg viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M21 8 12 3 3 8v8l9 5 9-5z"/><path d="M3 8l9 5 9-5M12 13v8"/></svg>',
    compose: '<svg viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M12 3 3 7.5l9 4.5 9-4.5z"/><path d="m3 12 9 4.5 9-4.5"/><path d="m3 16.5 9 4.5 9-4.5"/></svg>',
    machine: '<svg viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/></svg>',
    k8s: '<svg viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="3"/><path d="M12 3v6M12 15v6M3 12h6M15 12h6"/></svg>',
  };

  // ------------------------------------------------------------------ login
  function showLogin(error) {
    closeStreams();
    S.session = null;
    document.title = "LStack";
    $app.replaceChildren(h(`
      <div class="login"><form class="login-card" autocomplete="on">
        <img class="logo" src="/icon.png" alt="">
        <h1>LStack</h1>
        <p>Sign in to view this machine's services</p>
        <label class="field"><span>Username</span><input name="username" autocomplete="username" required autofocus></label>
        <label class="field"><span>Password</span><input name="password" type="password" autocomplete="current-password" required></label>
        <label class="field" id="codefield" ${S.totp ? "" : "hidden"}><span>Authenticator code</span><input name="code" inputmode="numeric" pattern="[0-9 ]{6,7}" maxlength="7" autocomplete="one-time-code" placeholder="123 456" ${S.totp ? "required" : ""}></label>
        <button class="btn-primary" type="submit">Sign In</button>
        <div class="error">${esc(error || "")}</div>
      </form></div>`));
    const form = $app.querySelector("form");
    const setTotp = (on) => {
      S.totp = on;
      form.querySelector("#codefield").hidden = !on;
      form.code.required = on;
    };
    api("/api/login", { login: true }).then((v) => setTotp(!!v.totp)).catch(() => {});
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      const btn = form.querySelector("button");
      btn.disabled = true;
      try {
        await api("/api/login", { method: "POST", login: true, body: { username: form.username.value, password: form.password.value, code: S.totp ? form.code.value : undefined } });
        await start();
      } catch (err) {
        form.querySelector(".error").textContent = err.message;
        if (S.totp) { form.code.value = ""; form.code.focus(); } else form.password.select();
      } finally { btn.disabled = false; }
    });
  }

  // ------------------------------------------------------------------ shell
  async function start() {
    try { S.session = await api("/api/session"); } catch { return; }
    if (!S.session.user) return showLogin();
    document.title = `LStack · ${S.session.hostname}`;
    $app.replaceChildren(h(`
      <div class="shell">
        <header class="top">
          <img class="logo" src="/icon.png" alt="">
          <div class="brand"><b>LStack</b><small>${esc(S.session.hostname)} · v${esc(S.session.version)}</small></div>
          <div class="spacer"></div>
          <span class="chip hide-sm" id="sum"></span>
          <span class="chip cpu" id="tcpu" title="Total CPU"></span>
          <span class="chip mem" id="tmem" title="Total memory"></span>
          <span class="chip live off hide-sm" id="live">connecting</span>
          <button class="btn" id="logout" title="Sign out">Sign out</button>
        </header>
        <main class="body">
          <aside class="list" id="list"></aside>
          <section class="detail" id="detail"></section>
        </main>
      </div>`));
    $app.querySelector("#logout").onclick = async () => {
      try { await api("/api/logout", { method: "POST" }); } catch { /* ignore */ }
      showLogin();
    };
    openEvents();
    loadProcesses();
    loadSystem();
    clearInterval(S.tickTimer);
    S.tickTimer = setInterval(() => { renderList(); renderTiles(); }, 1000);
  }

  function closeStreams() {
    S.events?.close(); S.events = null;
    S.logStream?.close(); S.logStream = null;
    clearInterval(S.chartTimer); clearInterval(S.tickTimer);
  }

  function setLive(on) {
    const el = document.getElementById("live");
    if (el) { el.classList.toggle("off", !on); el.textContent = on ? "live" : "reconnecting"; }
  }

  function pushHistory(key, m) {
    if (!m) return;
    const arr = S.history.get(key) || [];
    if (arr.length && arr[arr.length - 1].ts >= m.ts) return;
    arr.push({ ts: m.ts, cpu: m.cpu, mem: m.mem });
    while (arr.length > HISTORY) arr.shift();
    S.history.set(key, arr);
  }

  function trackHistory(s) {
    const k = svcKey(s);
    if (!S.history.has(k)) seedHistory(s);
    pushHistory(k, s.metrics);
  }

  function firstKey() {
    const local = sortedSvcs(S.services)[0];
    if (local) return svcKey(local);
    for (const M of sortedPeers()) { const s = sortedSvcs(M.services)[0]; if (s) return svcKey(s); }
    return machKey("");
  }

  function ensureSelection() {
    const m = selMach();
    if (m !== null ? !machExists(m) : !getSvc(S.selected)) S.selected = firstKey();
  }

  // Re-render the detail pane only when the selection changed; otherwise refresh its header.
  function refreshDetail() {
    if (selMach() !== null) { if (S.shown !== S.selected || !machExists(selMach())) renderDetail(); else renderMachineLive(); return; }
    if (S.shown !== S.selected || !selSvc()) renderDetail(); else renderHead();
  }

  async function loadSystem() {
    try {
      const d = await api("/api/system");
      const cur = (S.system?.samples || []).filter((x) => !d.samples?.length || x.ts > d.samples[d.samples.length - 1].ts);
      S.system = { ...d, samples: [...(d.samples || []), ...cur].slice(-SYS_HISTORY) };
      renderList();
      if (selMach() === "") renderMachineLive();
    } catch { /* ignore */ }
  }

  function onSystem(m, sample) {
    let sys = sysOf(m);
    if (!sys) {
      sys = { samples: [] };
      if (m) { const M = S.machines.get(m); if (!M) return; M.system = sys; } else S.system = sys;
    }
    sys.samples = [...(sys.samples || []).slice(-(SYS_HISTORY - 1)), sample];
    if (selMach() === m) renderMachineLive();
  }

  async function loadProcesses() {
    try {
      const d = await api("/api/processes");
      S.processes = d.processes || [];
      S.thresholds = d.thresholds || null;
      renderList();
    } catch { /* ignore */ }
  }

  function machineEntry(m) {
    const { services = [], configErrors = [], processes = [], system = null, ...info } = m;
    const M = { info, services: new Map(), configErrors, processes, system };
    services.forEach((s) => { M.services.set(s.name, tagged(m.name, s)); trackHistory(s); });
    return M;
  }

  function dropHistory(machine) {
    for (const k of [...S.history.keys()]) if (parseKey(k).m === machine) S.history.delete(k);
  }

  function onService(m, s) {
    const map = svcMap(m);
    if (!map) return;
    tagged(m, s);
    const prev = map.get(s.name);
    map.set(s.name, s);
    if (!prev) seedHistory(s);
    if (!S.selected) S.selected = svcKey(s);
    renderList();
    if (svcKey(s) !== S.selected) return;
    if (S.shown !== S.selected) renderDetail();
    else if (prev && prev.status === s.status && prev.restartAt === s.restartAt && prev.health === s.health && prev.lastError === s.lastError) renderTiles();
    else renderHead();
  }

  function onRemoved(m, name) {
    const map = svcMap(m);
    if (!map) return;
    const k = keyOf(m, name);
    map.delete(name); S.history.delete(k);
    if (S.selected === k) { S.selected = firstKey(); renderDetail(); }
    renderList();
  }

  function onMetrics(m, name, metrics) {
    const s = svcMap(m)?.get(name);
    if (s) s.metrics = metrics;
    const k = keyOf(m, name);
    pushHistory(k, metrics);
    renderList();
    if (k === S.selected) { renderTiles(); if (S.range === "15m") appendChartPoint(metrics); }
  }

  function onPeer(name, event, data) {
    if (event === "removed-machine") {
      const gone = data?.name || name;
      S.machines.delete(gone); dropHistory(gone);
      ensureSelection(); renderList(); refreshDetail();
      return;
    }
    let M = S.machines.get(name);
    if (!M) { M = machineEntry({ name, local: false, status: "connecting" }); S.machines.set(name, M); }
    const onSel = () => S.selected && parseKey(S.selected).m === name;
    switch (event) {
      case "machine":
        Object.assign(M.info, data || {});
        renderList();
        if (onSel()) renderHead();
        break;
      case "snapshot":
        M.services = new Map();
        (data.services || []).forEach((s) => { M.services.set(s.name, tagged(name, s)); trackHistory(s); });
        M.configErrors = data.configErrors || [];
        ensureSelection(); renderList();
        if (onSel() || S.shown !== S.selected) refreshDetail();
        break;
      case "service": onService(name, data); break;
      case "removed": onRemoved(name, data.name); break;
      case "metrics": onMetrics(name, data.name, data.metrics); break;
      case "processes": M.processes = data.processes || []; renderList(); break;
      case "system": onSystem(name, data); break;
    }
  }

  function openEvents() {
    S.events?.close();
    const es = new EventSource("/api/events");
    S.events = es;
    es.onopen = () => setLive(true);
    es.onerror = async () => {
      setLive(false);
      try { await api("/api/session"); } catch { /* 401 → login screen */ }
    };
    es.addEventListener("snapshot", (e) => {
      const d = JSON.parse(e.data);
      if (d.machine) S.local = d.machine;
      S.services = new Map(d.services.map((s) => [s.name, tagged("", s)]));
      S.configErrors = d.configErrors || [];
      d.services.forEach(trackHistory);
      // A peer selection is resolved by the `machines` event that follows.
      const peerSel = S.selected && parseKey(S.selected).m;
      if (!peerSel || S.machines.size) ensureSelection();
      renderList();
      if (!peerSel || selSvc()) renderDetail();
    });
    es.addEventListener("machines", (e) => {
      const d = JSON.parse(e.data);
      if (d.local) S.local = d.local;
      const names = new Set((d.machines || []).map((m) => m.name));
      for (const old of S.machines.keys()) if (!names.has(old)) dropHistory(old);
      S.machines = new Map((d.machines || []).map((m) => [m.name, machineEntry(m)]));
      ensureSelection(); renderList(); refreshDetail();
    });
    es.addEventListener("peer", (e) => {
      const { machine, event, data } = JSON.parse(e.data);
      onPeer(machine, event, data);
    });
    es.addEventListener("service", (e) => onService("", JSON.parse(e.data)));
    es.addEventListener("removed", (e) => onRemoved("", JSON.parse(e.data).name));
    es.addEventListener("metrics", (e) => { const { name, metrics } = JSON.parse(e.data); onMetrics("", name, metrics); });
    es.addEventListener("processes", (e) => {
      S.processes = JSON.parse(e.data).processes || [];
      renderList();
      if (selMach() === "") renderMachineLive();
    });
    es.addEventListener("system", (e) => onSystem("", JSON.parse(e.data)));
  }

  async function seedHistory(s) {
    const k = svcKey(s);
    try {
      const { samples } = await api(`${svcPath(s)}/metrics?range=5m&points=${HISTORY}`);
      const last = samples.length ? samples[samples.length - 1].ts : 0;
      const cur = (S.history.get(k) || []).filter((c) => c.ts > last);
      S.history.set(k, [...samples.map((m) => ({ ts: m.ts, cpu: m.cpu, mem: m.mem })), ...cur].slice(-HISTORY));
      renderList();
    } catch { /* ignore */ }
  }

  // ------------------------------------------------------------------ list
  function sparkline(values, w = 64, hgt = 18, color = "#34d399") {
    if (values.length < 2) return `<svg class="spark" width="${w}" height="${hgt}"></svg>`;
    const max = Math.max(1, ...values);
    const pts = values.map((v, i) => `${((i / (values.length - 1)) * w).toFixed(1)},${(hgt - 1 - (v / max) * (hgt - 2)).toFixed(1)}`);
    return `<svg class="spark" width="${w}" height="${hgt}" viewBox="0 0 ${w} ${hgt}"><polyline points="${pts.join(" ")}" fill="none" stroke="${color}" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" opacity="0.9"/></svg>`;
  }

  const LOCAL_ONLY_TIP = "bound to localhost on that machine – not reachable from here";

  function urlChips(s, max) {
    const urls = s.urls || [];
    const shown = max ? urls.slice(0, max) : urls;
    const more = urls.length - shown.length;
    return shown.map((u) => {
      const warn = !!s._m && u.localOnly;
      const text = u.label || String(u.url || "").replace(/^https?:\/\//i, "").replace(/\/$/, "");
      const inner = `${warn ? `<span class="lwarn" title="${esc(LOCAL_ONLY_TIP)}">⚠</span>` : ""}${esc(text)}`;
      const cls = `link${warn ? " warn" : ""}`;
      if (!/^https?:\/\//i.test(u.url || "")) return `<span class="${cls}" title="${esc(u.url)}">${inner}</span>`;
      return `<a class="${cls}" href="${esc(u.url)}" target="_blank" rel="noopener noreferrer" title="${esc(u.url)}">${inner}</a>`;
    }).join("") + (more > 0 ? `<span class="link more" title="${esc(urls.slice(max).map((u) => u.url).join("\n"))}">+${more}</span>` : "");
  }

  function svcRow(s) {
    const m = live(s) ? s.metrics : null;
    const k = svcKey(s);
    const hist = (S.history.get(k) || []).map((p) => p.cpu);
    const uptime = live(s) && s.startedAt ? dur(Date.now() - s.startedAt) : statusLabel(s);
    const health = s.health && !["none", "unknown"].includes(s.health) ? `<span class="h-${esc(s.health)}">● ${esc(s.health)}</span>` : "";
    const links = s.urls?.length ? `<span class="links">${urlChips(s, 3)}</span>` : "";
    return `<div class="svc ${k === S.selected ? "sel" : ""}" data-key="${esc(k)}">
        <span class="dot ${esc(looping(s) ? "starting" : s.status)}"></span>
        <span class="name">${esc(s.name)}</span>
        <span class="nums"><b>${m ? pct(m.cpu) : "—"}</b> · ${m ? bytes(m.mem) : "—"}</span>
        <span class="meta"><span class="badge ${esc(s.type)}">${esc(s.type)}</span>${esc(uptime)} ${health}</span>
        ${sparkline(live(s) ? hist : [])}
        ${links}
      </div>`;
  }

  function svcSection(list, errors, empty) {
    const errs = errors?.length ? `<div class="config-errors">${errors.map(esc).join("<br>")}</div>` : "";
    return errs + (list.map(svcRow).join("") || empty);
  }

  function procPanel(procs, t) {
    procs = procs || [];
    const note = t ? (t.enabled ? `over CPU ${+t.cpu}% · mem ${+t.mem}% · net ${rate(t.net)}` : "process breakdown is off") : "";
    const rows = procs.map((p) => `<tr>
        <td class="pn" title="${esc(p.name)}">${esc(p.name)}</td><td>${esc(p.pid)}</td>
        <td>${pct(p.cpu)}</td><td>${bytes(p.mem)}</td><td>${p.net != null ? rate(p.net) : "—"}</td></tr>`).join("");
    const body = rows
      ? `<table class="ptable"><thead><tr><th>Name</th><th>PID</th><th>CPU</th><th>MEM</th><th>NET</th></tr></thead><tbody>${rows}</tbody></table>`
      : t && !t.enabled ? "" : '<div class="pnote">nothing busy</div>';
    return `<div class="procs"><div class="list-head"><span>Other processes</span><span>${procs.length || ""}</span></div>${body}${note ? `<div class="pnote">${esc(note)}</div>` : ""}</div>`;
  }

  function machHead(i, count) {
    const st = i.status || "offline";
    const tip = [i.hostname, i.version && `v${i.version}`, i.url, !i.local && st !== "online" && i.lastSeen ? `last seen ${ago(i.lastSeen)}` : ""].filter(Boolean).join(" · ");
    const sub = i.local ? "this machine" : st !== "online" ? st : "";
    const ro = !i.local && !i.allowControl ? '<span class="badge ro" title="Control is disabled on that machine">read-only</span>' : "";
    const err = !i.local && st !== "online" && i.error ? `<div class="m-err">${esc(i.error)}</div>` : "";
    return `<div class="mach" title="${esc(tip)}"><span class="mdot ${esc(st)}"></span><b>${esc(i.name)}</b>${sub ? `<span class="m-sub">${esc(sub)}</span>` : ""}${ro}<span class="m-n">${count}</span></div>${err}`;
  }

  // Clickable whole-machine row: CPU and memory now, with sparklines of the last minutes.
  function machRow(m) {
    const sys = sysOf(m);
    const smp = (sys?.samples || []).slice(-HISTORY);
    const last = smp[smp.length - 1];
    const memPct = last && sys.memTotal ? (last.mem / sys.memTotal) * 100 : null;
    const k = machKey(m);
    return `<div class="msum ${k === S.selected ? "sel" : ""}" data-key="${esc(k)}" title="Whole-machine load">
        <span class="mk">CPU</span><b>${last ? pct(last.cpu) : "—"}</b>${sparkline(smp.map((x) => x.cpu), 54, 16, "#34d399")}
        <span class="mk">MEM</span><b>${memPct != null ? pct(memPct) : last ? bytes(last.mem) : "—"}</b>${sparkline(smp.map((x) => x.mem), 54, 16, "#60a5fa")}
      </div>`;
  }

  function renderList() {
    const el = document.getElementById("list");
    if (!el) return;
    const all = sortedSvcs(S.services);
    const up = all.filter(live);
    const cpu = up.reduce((a, s) => a + (s.metrics?.cpu || 0), 0);
    const mem = up.reduce((a, s) => a + (s.metrics?.mem || 0), 0);
    document.getElementById("sum").textContent = `${up.length} running · ${all.length - up.length} stopped`;
    document.getElementById("tcpu").textContent = `CPU ${pct(cpu)}`;
    document.getElementById("tmem").textContent = `MEM ${bytes(mem)}`;
    // Don't swap the DOM out from under a pressed row or link (the click would be lost); the 1 s tick catches up.
    if (el.querySelector(":active")) return;
    const peers = sortedPeers();
    const total = all.length + peers.reduce((a, M) => a + M.services.size, 0);
    const localBody = machRow("") + svcSection(all, S.configErrors, '<div class="empty">No services defined yet.<br>Add one with <code>lstack add</code>.</div>') + procPanel(S.processes, S.thresholds);
    let html = `<div class="list-head"><span>Services</span><span>${total}</span></div>`;
    if (!peers.length) html += localBody;
    else {
      html += `<div class="machine">${machHead({ name: S.local || S.session?.hostname || "this machine", local: true, status: "online", hostname: S.session?.hostname, version: S.session?.version }, all.length)}${localBody}</div>`;
      for (const M of peers) {
        html += `<div class="machine ${M.info.status === "online" ? "" : "stale"}">${machHead(M.info, M.services.size)}${machRow(M.info.name)}${svcSection(sortedSvcs(M.services), M.configErrors, '<div class="empty sm">No services.</div>')}${procPanel(M.processes)}</div>`;
      }
    }
    el.innerHTML = html;
    el.querySelectorAll(".svc, .msum").forEach((row) => row.addEventListener("click", (e) => {
      if (e.target.closest("a")) return;
      select(row.dataset.key);
    }));
  }

  function select(key) {
    if (key === S.selected) return;
    S.selected = key;
    localStorage.setItem("lstack.sel", key);
    renderList(); renderDetail();
  }

  // ------------------------------------------------------------------ detail
  function renderDetail() {
    const el = document.getElementById("detail");
    if (!el) return;
    S.logStream?.close(); S.logStream = null;
    clearInterval(S.chartTimer);
    const mach = selMach();
    if (mach !== null && machExists(mach)) { S.shown = S.selected; renderMachine(el, mach); return; }
    const s = selSvc();
    S.shown = s ? S.selected : null;
    if (!s) { el.innerHTML = '<div class="empty">Select a service.</div>'; return; }
    el.innerHTML = `
      <div id="head"></div>
      <div class="tiles" id="tiles"></div>
      <div class="card-h"><span class="t">History</span>
        <div class="seg" id="range">${["15m", "1h", "24h", "7d"].map((r) => `<button data-r="${r}" class="${r === S.range ? "on" : ""}">${r}</button>`).join("")}</div>
      </div>
      <div class="charts">
        <div class="card"><div class="card-h"><span class="t">CPU</span><span class="val" id="cpuv"></span></div><div class="chart-wrap"><div class="chart" id="cpuc"></div></div></div>
        <div class="card"><div class="card-h"><span class="t">Memory</span><span class="val" id="memv"></span></div><div class="chart-wrap"><div class="chart" id="memc"></div></div></div>
      </div>
      <div class="card logs">
        <div class="card-h">
          <span class="t">Logs</span>
          <button class="btn ${S.paused ? "on" : ""}" id="pause">${S.paused ? "▶ Resume" : "❚❚ Pause"}</button>
          <button class="btn ${S.wrap ? "on" : ""}" id="wrap">Wrap</button>
          <button class="btn" id="dl">Download</button>
          <input class="filter" id="filter" placeholder="Filter (text or /regex/)" value="${esc(S.filter)}">
        </div>
        <div class="log-body ${S.wrap ? "" : "nowrap"}" id="logbody"></div>
        <div class="log-foot"><span id="logcount"></span><span id="logstate">connecting…</span></div>
      </div>`;
    el.querySelectorAll("#range button").forEach((b) => b.addEventListener("click", () => {
      S.range = b.dataset.r;
      el.querySelectorAll("#range button").forEach((x) => x.classList.toggle("on", x === b));
      loadCharts();
    }));
    el.querySelector("#pause").onclick = (e) => {
      S.paused = !S.paused;
      e.currentTarget.classList.toggle("on", S.paused);
      e.currentTarget.textContent = S.paused ? "▶ Resume" : "❚❚ Pause";
      if (!S.paused) renderLogs();
    };
    el.querySelector("#wrap").onclick = (e) => {
      S.wrap = !S.wrap;
      e.currentTarget.classList.toggle("on", S.wrap);
      document.getElementById("logbody").classList.toggle("nowrap", !S.wrap);
    };
    el.querySelector("#dl").onclick = () => {
      const text = S.logs.map((l) => `${new Date(l.ts).toISOString()} [${l.stream}] ${l.line}`).join("\n");
      const a = document.createElement("a");
      a.href = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
      a.download = `${s.name}-${new Date().toISOString().slice(0, 19)}.log`;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    };
    el.querySelector("#filter").addEventListener("input", (e) => { S.filter = e.target.value; renderLogs(); });
    renderHead();
    loadCharts();
    S.chartTimer = setInterval(() => { if (S.range !== "15m") loadCharts(); }, 30000);
    openLogs(s);
  }

  function renderHead() {
    const el = document.getElementById("head");
    const s = selSvc();
    if (!el || !s) return;
    const c = s.config || {};
    const what = c.command || c.image || c.container || c.composeFile || c.manifest || "";
    const where = s._m ? `<span class="on-m">on ${esc(s._m)}</span>${what || c.cwd ? " · " : ""}` : "";
    const ctl = canControl(s)
      ? `<div class="d-actions">
          <button class="btn go" data-act="start" ${live(s) ? "disabled" : ""}>▶ Start</button>
          <button class="btn re" data-act="restart">↻ Restart</button>
          <button class="btn stop" data-act="stop" ${live(s) || looping(s) ? "" : "disabled"}>■ Stop</button>
        </div>`
      : "";
    const health = s.health && !["none", "unknown"].includes(s.health) ? `<span class="pill ${esc(s.health)}">${esc(s.health)}</span>` : "";
    el.innerHTML = `
      <div class="d-head">
        <div class="d-icon ${esc(s.type)}">${ICONS[s.type] || ICONS.process}</div>
        <div class="d-title">
          <h2>${esc(s.name)} <span class="pill ${esc(looping(s) ? "starting" : s.status)}">${esc(statusLabel(s))}</span> ${health} ${s.autostart ? '<span class="pill atboot">starts at login</span>' : ""}</h2>
          <div class="sub">${where}${esc(what)}${c.cwd ? ` · ${esc(c.cwd)}` : ""}</div>
        </div>
        ${ctl}
      </div>
      ${s.lastError ? `<div class="lasterr">${esc(s.lastError)}</div>` : ""}
      <div class="d-info" id="info" hidden></div>`;
    el.querySelectorAll("[data-act]").forEach((b) => b.addEventListener("click", () => act(s, b.dataset.act, b)));
    renderTiles();
  }

  function renderTiles() {
    const el = document.getElementById("tiles");
    const s = selSvc();
    if (!el || !s) return;
    renderInfo(s);
    const m = live(s) ? s.metrics || {} : {};
    const tiles = [
      ["CPU", pct(m.cpu)], ["Memory", bytes(m.mem)],
      ["Net ↓", rate(m.netRx)], ["Net ↑", rate(m.netTx)],
      ["Uptime", live(s) && s.startedAt ? dur(Date.now() - s.startedAt) : "—"],
      ["PID", s.pid ?? "—"], ["Restarts", s.restarts ?? 0],
    ];
    if (!live(s) && s.exitCode != null) tiles.push(["Exit code", s.exitCode]);
    el.innerHTML = tiles.map(([k, v]) => `<div class="tile"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`).join("");
    const cv = document.getElementById("cpuv"), mv = document.getElementById("memv");
    if (cv) cv.textContent = pct(m.cpu);
    if (mv) mv.textContent = bytes(m.mem);
  }

  const secretNames = (sec) =>
    Array.isArray(sec) ? sec.map((n) => [n, n]) : sec && typeof sec === "object" ? Object.entries(sec) : [];

  function heartbeatHtml(cfg, st) {
    if (cfg.enabled === false) return '<span class="hb off">disabled</span>';
    if (!st || !st.lastPing) return '<span class="hb off">no ping yet</span>';
    const good = st.ok && st.kind !== "fail";
    const icon = !good ? "✗" : st.kind === "start" ? "▶" : "✓";
    return `<span class="hb ${good ? "ok" : "bad"}" title="${esc(new Date(st.lastPing).toLocaleString())}">${icon} ${esc(st.kind)} ping ${esc(ago(st.lastPing))}${st.error ? `: ${esc(st.error)}` : ""}</span>`;
  }

  // URLs, heartbeat and secret names. Only rewritten when changed so links stay clickable.
  function renderInfo(s) {
    const el = document.getElementById("info");
    if (!el || el.querySelector(":active")) return;
    const c = s.config || {};
    const rows = [];
    if (s.urls?.length) rows.push(`<div class="irow"><span class="k">URLs</span><span class="links">${urlChips(s)}</span></div>`);
    if (c.heartbeat) rows.push(`<div class="irow"><span class="k">Heartbeat</span>${heartbeatHtml(c.heartbeat, s.heartbeat)}</div>`);
    const secrets = secretNames(c.secrets);
    if (secrets.length) {
      const badges = secrets.map(([env, name]) => `<span class="secret" title="${esc(env === name ? `$${env}` : `$${env} ← ${name}`)}">🔒 ${esc(name)}</span>`).join("");
      rows.push(`<div class="irow"><span class="k">Secrets</span><span class="secrets">${badges}</span></div>`);
    }
    const html = rows.join("");
    if (el._html === html) return;
    el._html = html;
    el.innerHTML = html;
    el.hidden = !html;
  }

  async function act(svc, action, btn) {
    btn.disabled = true;
    try {
      const s = tagged(svc._m, await api(`${svcPath(svc)}/${action}`, { method: "POST" }));
      svcMap(svc._m)?.set(s.name, s);
      toast(`${action} ${svc.name}${svc._m ? ` on ${svc._m}` : ""} → ${s.status}`);
    } catch (e) { toast(e.message, true); }
    renderHead(); renderList();
  }

  // ------------------------------------------------------------------ machine overview
  function machInfo(m) {
    if (!m) return { name: S.local || S.session?.hostname || "this machine", local: true, status: "online", hostname: S.session?.hostname, version: S.session?.version };
    return S.machines.get(m)?.info || { name: m };
  }

  function renderMachine(el, m) {
    const i = machInfo(m);
    const st = i.status || "offline";
    el.innerHTML = `
      <div class="d-head">
        <div class="d-icon machine">${ICONS.machine}</div>
        <div class="d-title">
          <h2>${esc(i.name)} <span class="pill ${st === "online" ? "running" : "stopped"}">${esc(i.local ? "this machine" : st)}</span></h2>
          <div class="sub">${esc([i.hostname, i.version && `LStack ${i.version}`, !i.local && i.url].filter(Boolean).join(" · "))}</div>
        </div>
      </div>
      <div class="tiles" id="mtiles"></div>
      <div class="card-h"><span class="t">Whole machine · last ${Math.round((SYS_HISTORY * 3) / 60)} min</span>
        <span class="legend"><span class="sw all"></span>all processes <span class="sw ls"></span>LStack services</span></div>
      <div class="charts">
        <div class="card"><div class="card-h"><span class="t">CPU</span><span class="val" id="mcpuv"></span></div><div class="chart-wrap"><div class="chart" id="mcpuc"></div></div></div>
        <div class="card"><div class="card-h"><span class="t">Memory</span><span class="val" id="mmemv"></span></div><div class="chart-wrap"><div class="chart" id="mmemc"></div></div></div>
      </div>
      <div id="mprocs"></div>`;
    renderMachineLive();
  }

  function renderMachineLive() {
    const m = selMach();
    if (m === null || !document.getElementById("mtiles")) return;
    const sys = sysOf(m) || {};
    const data = sys.samples || [];
    const last = data[data.length - 1] || {};
    const total = sys.memTotal;
    const svcs = [...(svcMap(m)?.values() || [])];
    const up = svcs.filter(live).length;
    const tiles = [
      ["CPU", pct(last.cpu)], ["Memory used", bytes(last.mem)], ["RAM", bytes(total)], ["Cores", sys.cores ?? "—"],
      ["LStack CPU", pct(last.lsCpu)], ["LStack memory", bytes(last.lsMem)], ["Running", `${up} / ${svcs.length}`],
    ];
    document.getElementById("mtiles").innerHTML = tiles.map(([k, v]) => `<div class="tile"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`).join("");
    document.getElementById("mcpuv").textContent = pct(last.cpu);
    document.getElementById("mmemv").textContent = total && last.mem != null ? pct((last.mem / total) * 100) : bytes(last.mem);
    const range = "15m";
    chart(document.getElementById("mcpuc"), data, "cpu", "#34d399", pct, 5, { key2: "lsCpu", color2: "#f59e0b", max: 100, range });
    chart(document.getElementById("mmemc"), data, "mem", "#60a5fa", bytes, 1024 * 1024, { key2: "lsMem", color2: "#f59e0b", max: total, range });
    const pe = document.getElementById("mprocs");
    if (!pe.querySelector(":active")) pe.innerHTML = m ? procPanel(S.machines.get(m)?.processes) : procPanel(S.processes, S.thresholds);
  }

  // ------------------------------------------------------------------ charts
  let chartData = [];
  async function loadCharts() {
    const key = S.selected, s = selSvc();
    if (!s) return;
    try {
      const { samples } = await api(`${svcPath(s)}/metrics?range=${S.range}&points=120`);
      if (key !== S.selected) return;
      chartData = samples;
      drawCharts();
    } catch { /* ignore */ }
  }

  function appendChartPoint(m) {
    if (!m) return;
    if (chartData.length && chartData[chartData.length - 1].ts >= m.ts) return;
    chartData.push(m);
    const cutoff = Date.now() - 15 * 60_000;
    while (chartData.length && chartData[0].ts < cutoff) chartData.shift();
    drawCharts();
  }

  function drawCharts() {
    chart(document.getElementById("cpuc"), chartData, "cpu", "#34d399", pct, 5);
    chart(document.getElementById("memc"), chartData, "mem", "#60a5fa", bytes, 1024 * 1024);
  }

  // `o.key2` draws a second series (e.g. LStack's share) over the first; `o.max` fixes the y scale.
  function chart(el, data, key, color, fmt, floor, o = {}) {
    if (!el) return;
    const W = 600, H = 130, range = o.range || S.range;
    if (data.length < 2) { el.innerHTML = '<div class="empty">No data for this range yet.</div>'; return; }
    const t0 = data[0].ts, t1 = data[data.length - 1].ts, span = Math.max(1, t1 - t0);
    const max = o.max || Math.max(floor, ...data.map((d) => d[key] || 0)) * 1.15;
    const x = (ts) => ((ts - t0) / span) * W, y = (v) => H - (Math.min(v, max) / max) * H;
    const path = (k) => data.map((d, i) => `${i ? "L" : "M"}${x(d.ts).toFixed(1)},${y(d[k] || 0).toFixed(1)}`).join("");
    const line = path(key);
    const id = `g${key}`;
    const second = o.key2
      ? `<path d="${path(o.key2)}L${W},${H}L0,${H}Z" fill="${o.color2}" fill-opacity="0.22"/><path d="${path(o.key2)}" fill="none" stroke="${o.color2}" stroke-width="1.5" vector-effect="non-scaling-stroke" stroke-linejoin="round"/>`
      : "";
    el.innerHTML = `
      <svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none">
        <defs><linearGradient id="${id}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${color}" stop-opacity="0.35"/><stop offset="1" stop-color="${color}" stop-opacity="0"/></linearGradient></defs>
        ${[0.25, 0.5, 0.75].map((f) => `<line x1="0" x2="${W}" y1="${H * f}" y2="${H * f}" stroke="rgba(255,255,255,0.06)" stroke-width="1" vector-effect="non-scaling-stroke"/>`).join("")}
        <path d="${line}L${W},${H}L0,${H}Z" fill="url(#${id})"/>
        <path d="${line}" fill="none" stroke="${color}" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linejoin="round"/>
        ${second}
      </svg>
      <span class="axis ymax">${esc(fmt(max))}</span>
      <span class="axis t0">${esc(shortTime(t0, range))}</span><span class="axis t1">${esc(shortTime(t1, range))}</span>
      <div class="cursor"></div><div class="tip"></div>`;
    const tip = el.querySelector(".tip"), cur = el.querySelector(".cursor");
    el.onmousemove = (e) => {
      const r = el.getBoundingClientRect();
      const ts = t0 + ((e.clientX - r.left) / r.width) * span;
      let best = data[0];
      for (const d of data) if (Math.abs(d.ts - ts) < Math.abs(best.ts - ts)) best = d;
      const px = (x(best.ts) / W) * r.width;
      cur.style.left = `${px}px`;
      tip.style.left = `${Math.min(Math.max(px, 70), r.width - 70)}px`;
      tip.textContent = `${fmt(best[key] || 0)}${o.key2 ? ` (LStack ${fmt(best[o.key2] || 0)})` : ""} · ${clock(best.ts)}`;
    };
  }

  // ------------------------------------------------------------------ logs
  function openLogs(s) {
    S.logs = [];
    renderLogs();
    const key = svcKey(s);
    const es = new EventSource(`${svcPath(s)}/logs/stream?tail=500`);
    S.logStream = es;
    const state = (t) => { const el = document.getElementById("logstate"); if (el) el.textContent = t; };
    es.onopen = () => state("● streaming");
    es.onerror = () => state("reconnecting…");
    let pending = [], raf = 0;
    es.addEventListener("log", (e) => {
      pending.push(JSON.parse(e.data));
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        const batch = pending; pending = [];
        if (key !== S.selected || es !== S.logStream) return;
        S.logs.push(...batch);
        if (S.logs.length > MAX_LOG_LINES) S.logs.splice(0, S.logs.length - MAX_LOG_LINES);
        if (!S.paused) appendLogs(batch);
      });
    });
  }

  function matcher() {
    const f = S.filter.trim();
    if (!f) return null;
    const m = f.match(/^\/(.+)\/([a-z]*)$/);
    try {
      return m ? new RegExp(m[1], m[2].includes("i") ? "gi" : "g") : new RegExp(f.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "gi");
    } catch { return null; }
  }

  const test = (re, s) => { re.lastIndex = 0; return re.test(s); };

  function highlight(text, re) {
    if (!re) return esc(text);
    let out = "", last = 0, m;
    re.lastIndex = 0;
    while ((m = re.exec(text)) && m[0].length) {
      out += esc(text.slice(last, m.index)) + `<mark>${esc(m[0])}</mark>`;
      last = m.index + m[0].length;
    }
    return out + esc(text.slice(last));
  }

  const lineHtml = (l, re) =>
    `<span class="ll ${esc(l.stream)}"><span class="ts">${esc(clock(l.ts))}</span><span class="msg">${highlight(l.line, re)}</span></span>`;

  function renderLogs() {
    const body = document.getElementById("logbody");
    if (!body) return;
    const re = matcher();
    const lines = re ? S.logs.filter((l) => test(re, l.line)) : S.logs;
    body.innerHTML = lines.map((l) => lineHtml(l, re)).join("");
    body.scrollTop = body.scrollHeight;
    const c = document.getElementById("logcount");
    if (c) c.textContent = re ? `${lines.length} of ${S.logs.length} lines` : `${S.logs.length} lines`;
  }

  function appendLogs(batch) {
    const body = document.getElementById("logbody");
    if (!body) return;
    const atBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 40;
    const re = matcher();
    body.insertAdjacentHTML("beforeend", batch.filter((l) => !re || test(re, l.line)).map((l) => lineHtml(l, re)).join(""));
    while (body.childElementCount > MAX_LOG_LINES) body.firstElementChild.remove();
    if (atBottom) body.scrollTop = body.scrollHeight;
    const c = document.getElementById("logcount");
    if (c) c.textContent = re ? `${body.childElementCount} of ${S.logs.length} lines` : `${S.logs.length} lines`;
  }

  start();
})();
