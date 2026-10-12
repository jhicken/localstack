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
    history: new Map(), // name -> [{ts,cpu,mem}]
    selected: localStorage.getItem("lstack.sel") || null,
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
  const enc = encodeURIComponent;

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
        <button class="btn-primary" type="submit">Sign In</button>
        <div class="error">${esc(error || "")}</div>
      </form></div>`));
    const form = $app.querySelector("form");
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      const btn = form.querySelector("button");
      btn.disabled = true;
      try {
        await api("/api/login", { method: "POST", login: true, body: { username: form.username.value, password: form.password.value } });
        await start();
      } catch (err) {
        form.querySelector(".error").textContent = err.message;
        form.password.select();
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

  function pushHistory(name, m) {
    if (!m) return;
    const arr = S.history.get(name) || [];
    if (arr.length && arr[arr.length - 1].ts >= m.ts) return;
    arr.push({ ts: m.ts, cpu: m.cpu, mem: m.mem });
    while (arr.length > HISTORY) arr.shift();
    S.history.set(name, arr);
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
      S.services = new Map(d.services.map((s) => [s.name, s]));
      S.configErrors = d.configErrors || [];
      d.services.forEach((s) => { if (!S.history.has(s.name)) seedHistory(s.name); pushHistory(s.name, s.metrics); });
      if (!S.selected || !S.services.has(S.selected)) S.selected = [...S.services.keys()].sort()[0] || null;
      renderList(); renderDetail();
    });
    es.addEventListener("service", (e) => {
      const s = JSON.parse(e.data);
      const prev = S.services.get(s.name);
      S.services.set(s.name, s);
      if (!prev) seedHistory(s.name);
      if (!S.selected) S.selected = s.name;
      renderList();
      if (s.name === S.selected) {
        if (prev && prev.status === s.status && prev.health === s.health && prev.lastError === s.lastError) renderTiles();
        else renderHead();
      }
    });
    es.addEventListener("removed", (e) => {
      const { name } = JSON.parse(e.data);
      S.services.delete(name); S.history.delete(name);
      if (S.selected === name) { S.selected = [...S.services.keys()].sort()[0] || null; renderDetail(); }
      renderList();
    });
    es.addEventListener("metrics", (e) => {
      const { name, metrics } = JSON.parse(e.data);
      const s = S.services.get(name);
      if (s) s.metrics = metrics;
      pushHistory(name, metrics);
      renderList();
      if (name === S.selected) { renderTiles(); if (S.range === "15m") appendChartPoint(metrics); }
    });
  }

  async function seedHistory(name) {
    try {
      const { samples } = await api(`/api/services/${enc(name)}/metrics?range=5m&points=${HISTORY}`);
      const last = samples.length ? samples[samples.length - 1].ts : 0;
      const cur = (S.history.get(name) || []).filter((c) => c.ts > last);
      S.history.set(name, [...samples.map((m) => ({ ts: m.ts, cpu: m.cpu, mem: m.mem })), ...cur].slice(-HISTORY));
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

  function renderList() {
    const el = document.getElementById("list");
    if (!el) return;
    const all = [...S.services.values()].sort((a, b) => a.name.localeCompare(b.name));
    const up = all.filter(live);
    const cpu = up.reduce((a, s) => a + (s.metrics?.cpu || 0), 0);
    const mem = up.reduce((a, s) => a + (s.metrics?.mem || 0), 0);
    document.getElementById("sum").textContent = `${up.length} running · ${all.length - up.length} stopped`;
    document.getElementById("tcpu").textContent = `CPU ${pct(cpu)}`;
    document.getElementById("tmem").textContent = `MEM ${bytes(mem)}`;
    const rows = all.map((s) => {
      const m = live(s) ? s.metrics : null;
      const hist = (S.history.get(s.name) || []).map((p) => p.cpu);
      const uptime = live(s) && s.startedAt ? dur(Date.now() - s.startedAt) : s.status;
      const health = s.health && !["none", "unknown"].includes(s.health) ? `<span class="h-${esc(s.health)}">● ${esc(s.health)}</span>` : "";
      return `<div class="svc ${s.name === S.selected ? "sel" : ""}" data-name="${esc(s.name)}">
        <span class="dot ${esc(s.status)}"></span>
        <span class="name">${esc(s.name)}</span>
        <span class="nums"><b>${m ? pct(m.cpu) : "—"}</b> · ${m ? bytes(m.mem) : "—"}</span>
        <span class="meta"><span class="badge ${esc(s.type)}">${esc(s.type)}</span>${esc(uptime)} ${health}</span>
        ${sparkline(live(s) ? hist : [])}
      </div>`;
    });
    const errors = S.configErrors.length ? `<div class="config-errors">${S.configErrors.map(esc).join("<br>")}</div>` : "";
    el.innerHTML = `<div class="list-head"><span>Services</span><span>${all.length}</span></div>${errors}${rows.join("") || '<div class="empty">No services defined yet.<br>Add one with <code>lstack add</code>.</div>'}`;
    el.querySelectorAll(".svc").forEach((row) => row.addEventListener("click", () => select(row.dataset.name)));
  }

  function select(name) {
    if (name === S.selected) return;
    S.selected = name;
    localStorage.setItem("lstack.sel", name);
    renderList(); renderDetail();
  }

  // ------------------------------------------------------------------ detail
  function renderDetail() {
    const el = document.getElementById("detail");
    if (!el) return;
    S.logStream?.close(); S.logStream = null;
    clearInterval(S.chartTimer);
    const s = S.services.get(S.selected);
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
    openLogs(s.name);
  }

  function renderHead() {
    const el = document.getElementById("head");
    const s = S.services.get(S.selected);
    if (!el || !s) return;
    const c = s.config || {};
    const what = c.command || c.image || c.container || c.composeFile || c.manifest || "";
    const ctl = S.session.allowControl
      ? `<div class="d-actions">
          <button class="btn go" data-act="start" ${live(s) ? "disabled" : ""}>▶ Start</button>
          <button class="btn re" data-act="restart">↻ Restart</button>
          <button class="btn stop" data-act="stop" ${live(s) ? "" : "disabled"}>■ Stop</button>
        </div>`
      : "";
    const health = s.health && !["none", "unknown"].includes(s.health) ? `<span class="pill ${esc(s.health)}">${esc(s.health)}</span>` : "";
    el.innerHTML = `
      <div class="d-head">
        <div class="d-icon ${esc(s.type)}">${ICONS[s.type] || ICONS.process}</div>
        <div class="d-title">
          <h2>${esc(s.name)} <span class="pill ${esc(s.status)}">${esc(s.status)}</span> ${health} ${s.autostart ? '<span class="pill atboot">starts at login</span>' : ""}</h2>
          <div class="sub">${esc(what)}${c.cwd ? ` · ${esc(c.cwd)}` : ""}</div>
        </div>
        ${ctl}
      </div>
      ${s.lastError ? `<div class="lasterr">${esc(s.lastError)}</div>` : ""}`;
    el.querySelectorAll("[data-act]").forEach((b) => b.addEventListener("click", () => act(s.name, b.dataset.act, b)));
    renderTiles();
  }

  function renderTiles() {
    const el = document.getElementById("tiles");
    const s = S.services.get(S.selected);
    if (!el || !s) return;
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

  async function act(name, action, btn) {
    btn.disabled = true;
    try {
      const s = await api(`/api/services/${enc(name)}/${action}`, { method: "POST" });
      S.services.set(s.name, s);
      toast(`${action} ${name} → ${s.status}`);
    } catch (e) { toast(e.message, true); }
    renderHead(); renderList();
  }

  // ------------------------------------------------------------------ charts
  let chartData = [];
  async function loadCharts() {
    const name = S.selected;
    try {
      const { samples } = await api(`/api/services/${enc(name)}/metrics?range=${S.range}&points=120`);
      if (name !== S.selected) return;
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

  function chart(el, data, key, color, fmt, floor) {
    if (!el) return;
    const W = 600, H = 130;
    if (data.length < 2) { el.innerHTML = '<div class="empty">No data for this range yet.</div>'; return; }
    const t0 = data[0].ts, t1 = data[data.length - 1].ts, span = Math.max(1, t1 - t0);
    const max = Math.max(floor, ...data.map((d) => d[key] || 0)) * 1.15;
    const x = (ts) => ((ts - t0) / span) * W, y = (v) => H - (v / max) * H;
    const line = data.map((d, i) => `${i ? "L" : "M"}${x(d.ts).toFixed(1)},${y(d[key] || 0).toFixed(1)}`).join("");
    const id = `g${key}`;
    el.innerHTML = `
      <svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none">
        <defs><linearGradient id="${id}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${color}" stop-opacity="0.35"/><stop offset="1" stop-color="${color}" stop-opacity="0"/></linearGradient></defs>
        ${[0.25, 0.5, 0.75].map((f) => `<line x1="0" x2="${W}" y1="${H * f}" y2="${H * f}" stroke="rgba(255,255,255,0.06)" stroke-width="1" vector-effect="non-scaling-stroke"/>`).join("")}
        <path d="${line}L${W},${H}L0,${H}Z" fill="url(#${id})"/>
        <path d="${line}" fill="none" stroke="${color}" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linejoin="round"/>
      </svg>
      <span class="axis ymax">${esc(fmt(max))}</span>
      <span class="axis t0">${esc(shortTime(t0, S.range))}</span><span class="axis t1">${esc(shortTime(t1, S.range))}</span>
      <div class="cursor"></div><div class="tip"></div>`;
    const tip = el.querySelector(".tip"), cur = el.querySelector(".cursor");
    el.onmousemove = (e) => {
      const r = el.getBoundingClientRect();
      const ts = t0 + ((e.clientX - r.left) / r.width) * span;
      let best = data[0];
      for (const d of data) if (Math.abs(d.ts - ts) < Math.abs(best.ts - ts)) best = d;
      const px = (x(best.ts) / W) * r.width;
      cur.style.left = `${px}px`;
      tip.style.left = `${Math.min(Math.max(px, 50), r.width - 50)}px`;
      tip.textContent = `${fmt(best[key] || 0)} · ${clock(best.ts)}`;
    };
  }

  // ------------------------------------------------------------------ logs
  function openLogs(name) {
    S.logs = [];
    renderLogs();
    const es = new EventSource(`/api/services/${enc(name)}/logs/stream?tail=500`);
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
        if (name !== S.selected || es !== S.logStream) return;
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
