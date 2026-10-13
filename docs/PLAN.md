# lstack — Product Plan & Roadmap

## Vision

One trustworthy place on your Mac to **define, start, watch, and talk to** everything your local dev environment needs: scripts, Docker containers, Compose stacks, and Kubernetes workloads. It should be as easy as PM2 but not tied to JavaScript, and it should start itself and your stack when you log in.

**Surfaces**

1. **Menu bar app** (SwiftUI `MenuBarExtra`). This is the polished, glanceable view: status, sparklines, start/stop, logs.
2. **CLI + TUI** (`lstack`, `lstack top`). It is scriptable and htop-style, and it shows the same data as the app.
3. **Daemon**. This is the single supervisor. It owns the processes, metrics, and logs, and it exposes a local HTTP + SSE API that both surfaces use.

**Principles**

- *Config is files.* `~/.localstack/services/*.yaml` is the source of truth. You can edit it by hand, keep it in git, and copy it between machines.
- *One supervisor.* Only the daemon starts or stops anything. The UIs are thin clients, so the CLI and the app always agree.
- *Fail safe.* Containers survive daemon restarts. Script process groups are reaped if the daemon dies. Crash loops back off and then give up.
- *Local-only by default.* The API binds to 127.0.0.1 and requires a token from a 0600 file.

---

## Phase 1 — Foundation ✅ (done)

| Area | Delivered |
|---|---|
| Daemon | Supervisor with restart policy (`always`/`on-failure`/`never`), exponential backoff, `maxRestarts`, config hot reload (applied on the next restart), orphan reaping, re-adopting running containers |
| Drivers | `process` (own process group, stdin pipe), `docker` (run an image or manage an existing container; talks to the Engine API over the Unix socket), `compose` (up/down, per-container stats) |
| Health | HTTP and TCP checks; Docker native `HEALTHCHECK` passthrough |
| Metrics | CPU and memory per process tree (native `proc_pidinfo`), Docker Engine API stats for containers, per-process network via `nettop`; SQLite history with 7-day retention and bucketed queries |
| Logs | Per-service rotating log files, tail, live SSE streaming, stdin injection |
| API | REST + SSE (`/api/events`, `/api/services/:name/logs/stream`) |
| CLI | `add/rm/edit/ls/show/start/stop/restart/enable/disable/logs/attach/exec/metrics/reload/daemon/startup` |
| TUI | `lstack top`: service list, detail pane with sparklines, live log pane, full-screen logs, stdin input, shell into containers |
| Menu bar app | Status icon, service rows with live sparklines, expandable CPU/memory charts, log window with input, Add Service window, daemon auto-launch |
| Startup | launchd LaunchAgent (`KeepAlive` on crash) for the daemon plus a login item for the menu bar app |
| Tests | Unit tests plus an integration test that runs a real daemon in a sandbox |

## Phase 1.5 — Rust rewrite & single-app install ✅ (done)

| Area | Delivered |
|---|---|
| Runtime | Daemon, CLI and TUI are one Rust binary (~3 MB, ~10 MB RSS idle vs ~80 MB on Node). Docker is reached over the Engine API socket with no `docker` child processes. |
| Packaging | `macos/build.sh` → `LStack.app` (app + `Contents/Helpers/lstack`), zip and dmg; ad-hoc or Developer ID signing |
| Self-install | `lstack setup [install\|status\|repair\|uninstall]`: CLI symlink on PATH (with a `~/.zprofile` fix-up), LaunchAgents pointing at the bundled binary, and migration from older installs |
| App onboarding | Welcome/Settings window: start at login, install CLI (with an admin option), daemon status, Move to Applications; repair on every launch |

## Phase 1.6 — Remote dashboard, icon & releases ✅ (done)

| Area | Delivered |
|---|---|
| Remote dashboard | Opt-in second listener (`lstack remote on`, or the Settings toggle) serving an embedded web UI. It shows services, health, CPU/memory history (15m–7d) and live logs with filter, pause and download. Optional start/stop/restart. Single account with a PBKDF2 password hash, HttpOnly session cookies, per-IP login throttling, CSRF header, strict CSP, and redacted env values. Turning it off ends sessions and open streams. |
| Icon | Code-drawn app icon (`macos/icon/make-icon.swift`), used for the app bundle, the in-app headers and the web UI |
| Releases | `macos/release.sh` tags and pushes `v<version>`; a GitHub Actions workflow builds on macOS and attaches the dmg/zip/checksums to the release (`--local` uploads from this Mac with `gh`) |
| Shell environment | Scripts run in the user's login shell and source `source:` files first (default `~/.zshrc`/`~/.bashrc`), then `eval` the command, so PATH, nvm/bun, `op` and aliases match Terminal. Leading `cd`/`FOO=bar`/`export`/`source` steps are split into cwd/env/source by `lstack add` and the app's Add Service form. |

| Remote 2FA | Optional TOTP (RFC 6238, SHA-1, 6 digits, ±1 step, replay-protected). Enroll with `lstack remote 2fa on` (terminal QR) or Settings (QR plus confirm code). The sign-in form asks for the code when enabled. |
| Terminal app | `~/.localstack/settings.json` picks Terminal, iTerm, cmux or Ghostty for Dashboard/Attach/Shell (Settings picker or `lstack terminal`) |
| Crash loops | Restart countdown (`restartAt`) shown in the menu bar, TUI, CLI and web, with an always-visible Stop. Stop cancels the pending restart and waits for an in-flight start instead of returning "busy". |
| System load | Whole-machine CPU/memory sampled from Mach host stats every 3 s (`/api/system`, SSE `system`), shown in the menu bar panel as sparklines plus an LStack vs. other-apps breakdown. The panel also shrinks back when content gets shorter. |
| Full Disk Access | Settings detects whether the helper has Full Disk Access (needed by `op` and similar tools under launchd) and links to the System Settings pane |

## Phase 1.7 — Secrets, machines, URLs, heartbeats ✅ (done)

| Area | Delivered |
|---|---|
| Footer | Menu bar footer has **Terminal** (`lstack top`) and **Web** (remote dashboard; shown only when it's on) side by side |
| cmux | Uses `cmux workspace create` with `CMUX_SOCKET_PASSWORD`. "Access denied" comes back as `cmuxDenied`, and the app offers one-click **Allow LStack** (backs up `cmux.json`, sets password mode, `reload-config`) or **Use Terminal**. Also in Settings and `lstack terminal cmux --fix`. |
| Secrets | Write-only store (`lstack secret`, Settings → Secrets). ChaCha20-Poly1305 file plus a key split into two XOR shares (Keychain via `/usr/bin/security`, and a 0600 file), read once at boot. Explicit per-service `secrets:` injected as env. Every log path scrubs values and their base64/URL/hex variants. `lstack exec` goes through the daemon (an HTTP upgrade to a PTY running `docker exec`) and its output is scrubbed as a stream. |
| Service URLs | Listening ports from libproc (process tree) and Docker published ports, plus `urls:` / `detectUrls:` in YAML. Shown as chips/links in the menu bar, TUI (`o`), CLI and web. Peer URLs are rewritten to the peer host, with a local-only flag. |
| Heartbeats | healthchecks.io-compatible pings per service (`/start`, success interval, `/fail` with scrubbed logs) and a machine heartbeat. Ping key kept in the secret store. `lstack heartbeat`, Settings → Monitoring. |
| Busy processes | All-process CPU/mem/net sampling with thresholds (default 10% CPU, 10% RAM, 1 MB/s, 15 rows). SSE `processes`, `/api/processes`, `lstack ps`, shown per machine in every UI. |
| Machines | Pair other Macs (one-time code or dashboard login + 2FA). The host keeps an SSE subscription per peer (`peers.json`), and `/api/machines/:m/…` proxies logs/metrics/control (gated by the peer's allowControl). `name@machine` in the CLI, grouped by machine in the menu bar, TUI, CLI and web. Peer tokens are hashed and revocable (`lstack remote peers`). |

## Phase 2 — Daily-driver polish

**Interaction**
- [ ] Full PTY support for process services (`openpty`), so `attach` works for REPLs and TUIs (colors, raw mode, resize).
- [ ] Embedded terminal in the app (SwiftTerm) for attach and `docker exec`, instead of handing off to Terminal.app.
- [ ] Log search/filter (regex), level highlighting, pause/resume, and "open log file" actions in the app and TUI (already in the web dashboard).

**Orchestration**
- [ ] `dependsOn` plus start ordering, and "wait until healthy" before dependents start (for example, the API waits for the db).
- [ ] Groups/tags: `lstack start @backend`, and collapsible groups in the app.
- [ ] `envFile:` support (`.env`), and per-service `stopSignal` / `stopTimeout`.
- [ ] `lstack save` / `resurrect`: restore whatever was running before the daemon restarted, not just autostart items.
- [ ] File watching with restart on change (`watch: [src/**]`).
- [ ] Scheduled/cron services (`schedule: "0 * * * *"`).

**App**
- [ ] Edit Service sheet with full form parity with the YAML, and validation.
- [ ] Settings: daemon port, theme, notification preferences (the basic Settings window exists; consider `SMAppService` for the app login item).
- [ ] macOS notifications for crashes, health changes, and gave-up-restarting.
- [ ] Menu bar icon reflects aggregate state (all healthy / something degraded / something crashed).
- [ ] Global hotkey to open the popover; Spotlight-style quick switcher.

**CLI**
- [ ] Shell completions (zsh/bash/fish).
- [ ] `lstack top` mouse support and sorting by CPU/memory.
- [ ] `--json` on every read command.

## Phase 3 — Kubernetes

- [ ] `k8s` driver: `kubectl apply -f manifest` / `delete` on start/stop, scoped by `context` and `namespace`.
- [ ] Status rolled up from pods (Ready/CrashLoopBackOff), restart counts, and events.
- [ ] Logs: multiplexed `kubectl logs -f -l selector --prefix`.
- [ ] Metrics from `kubectl top` (metrics-server).
- [ ] Managed **port-forwards** as first-class services, with reconnect on drop.
- [ ] Helm releases and kustomize overlays as service types.
- [ ] Local cluster lifecycle: kind/minikube/k3d/Docker Desktop k8s as a startable service.
- [ ] Exec into a pod from the TUI or app.

## Phase 4 — Insight & history

- [ ] History view in the app: 1h / 24h / 7d charts for CPU, memory, and network; restart and crash markers.
- [ ] Alerts: thresholds (memory > X for Y minutes), with a notification and an optional auto-restart.
- [ ] Per-service uptime and crash reports (last N exits, exit codes, last log lines).
- [ ] Disk I/O metrics, and port-in-use detection ("port 3000 is held by pid …").
- [ ] Export metrics (CSV / Prometheus endpoint).

## Phase 5 — Distribution & hardening

- [ ] Homebrew cask for the app (and a formula for CLI-only installs).
- [ ] Universal (arm64 + x86_64) build via `lipo`.
- [ ] Developer ID signing, notarization, and Sparkle auto-update for the app.
- [ ] Unix domain socket transport (replaces TCP + token); keep the TCP API opt-in.
- [ ] Import from PM2 `ecosystem.config.js`, Procfile, and existing compose files; `lstack export`.
- [ ] Config profiles/workspaces (switch "client-A stack" vs "personal").
- [ ] Linux support (systemd user units) since the daemon/CLI are portable.
- [ ] Remote dashboard: built-in HTTPS (self-signed or user-supplied cert), multiple users/roles, an optional Tailscale-only bind, and log streaming for several services at once.

---

## Key decisions & rationale

| Decision | Why |
|---|---|
| Rust daemon/CLI/TUI in one binary | Small footprint for an always-on process (~10 MB RSS), no runtime to install, and one artifact to sign and ship. tokio + axum for the API, rusqlite (bundled), ratatui for the TUI. |
| App bundles the binary | A single drag-to-install artifact. The app owns setup (PATH link and LaunchAgents) and re-points them when it moves. |
| Native SwiftUI menu bar app | `MenuBarExtra` gives a truly native look, low memory use, and correct menu bar behavior, which Electron can't match. |
| HTTP + SSE API on localhost | Trivial to consume from Swift (`URLSession.bytes`) and Rust, debuggable with curl, and push updates come without polling. |
| launchd LaunchAgent with `KeepAlive` | The standard macOS mechanism. It restarts the daemon if it crashes and needs no admin rights. |
| Containers outlive the daemon | Restarting or upgrading lstack shouldn't bounce your database. |
| ratatui TUI | Mature layout and widgets with a small binary-size cost. Emoji are avoided because their width breaks box drawing. |

## Open questions

- Should stopping the daemon (rather than restarting it) optionally stop containers too? This could be a setting.
- Should Compose services expose each container as a child row in the UI? Currently they are aggregated into a single row.
- Beyond the web dashboard, should the CLI/TUI be able to attach to a remote lstack (for example, manage a dev VM over SSH)?
