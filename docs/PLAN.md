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
| API | REST + SSE (`/api/events`, `/api/services/:name/logs/stream`); `GET …/logs?tail=N&before=<pos>` pages back through the log file using each line's `pos` cursor |
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
| Releases | `macos/release.sh` tests, lints, bumps and tags `v<version>`, builds the dmg/zip/checksums on this Mac, pushes and uploads the release with `gh` (`--ci` hands the build to a manual-only GitHub Actions workflow instead) |
| Shell environment | Scripts run in the user's login shell and source `source:` files first (default `~/.zshrc`/`~/.bashrc`), then `eval` the command, so PATH, nvm/bun, `op` and aliases match Terminal. Leading `cd`/`FOO=bar`/`export`/`source` steps are split into cwd/env/source by `lstack add` and the app's Add Service form. |

| Remote 2FA | Optional TOTP (RFC 6238, SHA-1, 6 digits, ±1 step, replay-protected). Enroll with `lstack security 2fa on` (terminal QR) or Settings → Security (QR plus confirm code). When enabled, pairing a new Mac and dashboard sign-in ask for the code. |
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
| Machines | Pair other Macs (one-time code, + 2FA code when enabled; no dashboard login). The host keeps an SSE subscription per peer (`peers.json`), and `/api/machines/:m/…` proxies logs/metrics/control (gated by the peer's allowControl). `name@machine` in the CLI, grouped by machine in the menu bar, TUI, CLI and web. Peer tokens are hashed and revocable (`lstack remote peers`). |
| Web vs sharing | The web dashboard (`enabled`, plus `webRemote`: this Mac only or other devices) and sharing with paired Macs (`sharing`) are independent switches on the same listener. It binds to loopback unless something is remote, and requests are gated per path and client IP. `lstack remote web local\|network`, `lstack remote share on\|off`; Settings → Web dashboard / Machines → Share this Mac. Old configs migrate to their previous behavior. |
| Machine charts | The web dashboard has a per-machine overview (local and peers): CPU/memory sparklines in the list, plus whole-machine charts with LStack's share overlaid, tiles and busy processes. `/api/system` was added to the web API. |

## Phase 1.8 — Encrypted machine links & automatic tunnels ✅ (done)

| Area | Delivered |
|---|---|
| Identity | X25519 key pair per install, private half in the secret store. Fingerprint + 4-word phrase (`lstack machine id`), `lstack machine rekey` (all pairings must re-pair). |
| Pairing | SPAKE2 over the one-time code (3 tries, then burned). The TOTP code (if 2FA is on) is sent only inside the PAKE-encrypted session. Keys are pinned on both sides and a confirmation phrase is shown. Per-IP lockout. |
| Link | Dedicated link port (7381) speaking only Noise IK. A small multiplexer carries typed streams (`http` = the peer API router, `tunnel` = byte relay), each checked against the peer's grants. Plain-HTTP bearer `/peer/*` routes and tokens were removed; old pairings show "pair again". |
| Reachability | "Show this Mac as" is the only way in: links/pairings must arrive at an IP that address resolves to here (LAN choice also needs a LAN source; Tailscale accepted). Automatic = LAN subnets only. Changing it drops links. Quick Connect link `lstack-pair://host:port?code=…` (never the 2FA code), pasted in Settings → Machines or `lstack machine pair`. |
| Grants | Per paired Mac: `control` and `tunnel` (all, none or a list). Settings → Share this Mac, `lstack remote grant/revoke/activity`, `/api/remote/peers`. Changes drop the link so new grants apply immediately. |
| Forwards | Automatic localhost forwards (127.0.0.1/::1) for granted services' live ports, same port when free, remembered fallbacks (`forwards.json`). Same-user check via libproc, 128 connections per Mac, per-service pause (`lstack forwards`). The shared Mac re-checks port ownership on each open, dials only its loopback, and audits. URL chips show 🔒 "via <machine>" everywhere. |
| Later | Built-in screen sharing as `screen` / `input` stream kinds with their own grants and consent (design note in the README). |

## Phase 1.9 — 1Password, by hand ✅ (done)

| Area | Delivered |
|---|---|
| Model | 1Password is a source you pull from by hand. The encrypted secret store is the cache that unattended restarts read from. There is no service-account token or stored credential. The daemon contains no `op` code; only the CLI and the app run `op`, after a click, with approval in 1Password. |
| Env files | `envFiles:` is compatible with `op run --env-file` (quoting, `export`, `$VAR` in references, last file wins, values override `env:`). Plain values are read at each start. `op://` values come only from the approved snapshot (the `secrets:` map plus the reference stored with each value). |
| Review & import | `/api/onepassword/{review,plan,import}`. Single-use plans that expire in 15 min; only references in the plan are accepted. Flags vaults that are new, personal or outside the allow-list. Refuses `LSTACK_*`, TOTP and unresolved `$VARS`. `lstack secret review/import/link/refresh/audit`, plus `--env-file` on `lstack add`. |
| `op` runner | Checks the codesign AgileBits Team ID, runs with a minimal environment (token variables stripped), values over a pipe with zeroized buffers, a timeout, and `op signout` after each batch. |
| Watcher | Checks file metadata every 2 s. New or changed references become `secretsPending` with a notification, and nothing is pulled. Removals apply automatically. Cached values nothing uses are garbage-collected, deferred until running users restart. Never opens FIFOs (Environment mounts). |
| Environments | A mounted 1Password Environment (FIFO) is read by the client when you ask (`--environment`), with the values held by the daemon only until you confirm. |
| LStack references | `KEY=lstack://NAME` in an env file uses a secret from LStack's store (`fileSecrets:` lists the keys the file manages). A hand-written line needs one approval; lines written by Convert to Secret, Use Stored Secret or `lstack env` are approved right away. Removed lines take effect at once. Plain-value changes on a running service raise a notification and an `envStale` badge until restart. |
| Sharing | Cache-to-cache over the Noise link. Grants are per exact reference and per Mac. Only `op`/`op-env` values are sent, with no re-sharing. One ask plus one approval, from either side. Either the receiver requests (`pullFromPeers` every 15 s, or Pull Shared Values; the request carries references plus service names) and the sender approves (a notice names the Mac and service, with Approve/Decline actions; matched by reference; a reference not held there is read from 1Password via a `link` plan with `sharePeer` under `secretshare::free_name`, then auto-granted on import), or the sender pushes (`Grants.push`, Machines → Push Secrets) and the receiver accepts (Accept/Reject on the notice). Approved values go straight to the services that reference them. The receiver accepts only references it asked for or accepted, binds each value to the sender's pinned key, and never overwrites a value holding a different reference. Refresh pushes (`pushToPeers`) update only values already held from the same key. Works in either pairing direction. When the sender is the watcher, it polls `/peer/api/secrets/needs` on the watched Mac and delivers over `/peer/api/secrets/deliver`. Grants live in `peers.json` (`secrets`/`push`). Accepted pushes wait in `ACCEPTED` until delivered. Paired both ways, the classic direction is used. Both sides audit. `lstack secret shares/share/unshare/decline/push/sync/accept/reject`. |
| Audit | `secrets-audit.log` (0600, never values). |
| Same-user hardening | API: header-only bearer (constant-time), refuses browser `Origin` and non-loopback `Host`, refuses `LSTACK_*` set/delete. Values sealed in memory under a per-boot mlock'd key; scrubber patterns sealed. No core dumps, `PT_DENY_ATTACH`, hardened runtime on ad-hoc builds. |
| Touch ID approvals | Secure Enclave P-256 key (LStack.app, `.userPresence`), TOFU registration, rekey signed by the old key. Approved spec per secret-holding service (config minus cosmetic fields + grants with HMAC'd value digests) stored in the vault. `env_for` withholds secrets on mismatch; restart pre-checks so a running service isn't taken down; approve signs `lstack-approval-v1\nid\nchallenge\nsubject`. Console (stdin/exec) approvals last 10 min. Open without a Developer ID: Keychain share via `/usr/bin/security`, `ps eww`, `docker inspect`, service code edits, writable app bundle. |
| To verify by hand | Approval prompts when `op` is spawned from LStack.app (no TTY), the number of prompts per batch, whether `op signout` ends the integration session, and Environment FIFO behavior when 1Password is locked. |

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
- [ ] **Secret-store hardening that needs a Developer ID.** Today the app and daemon are signed ad hoc (no Team ID), which leaves two same-user gaps (README → "What approvals and the store can't stop"):
  - **Keychain share readable via `/usr/bin/security`.** The vault key's Keychain half is created through `/usr/bin/security` so ad-hoc rebuilds don't re-prompt. As a result, its ACL trusts that tool, and any process running as you can read it. Together with the `0600` file share, that decrypts `secrets.enc` offline with no prompt, which bypasses Touch ID approvals completely. Fix: the signed daemon writes the item itself, using the Data Protection Keychain with a `keychain-access-groups` entitlement (`<TeamID>.dev.lstack`), so only LStack-signed code can read it. Migrate by re-wrapping the existing key share on first start. Optionally put the share behind a Secure Enclave key, so a copied file plus Keychain export isn't enough.
  - **Approval key usable by any same-user process.** LStack.app's Secure Enclave key is stored as a CryptoKit `dataRepresentation` blob in `~/Library/Application Support/LStack/approval-key`. Copying the blob is harmless: it's wrapped by this Mac's Secure Enclave, and every signature needs Touch ID or the login password. But any process running as you can load the blob and trigger a Touch ID prompt with its own text (prompt phishing). Fix: store the key as a Keychain item (`kSecAttrTokenIDSecureEnclave`) in LStack's access group, so only LStack.app can use it. Re-register it with the daemon through the existing rekey flow (signed by the old key).
  - Ship with a designated-requirement check: the daemon and app verify each other's Team ID (e.g. `SecCodeCheckValidity` on the connecting peer) before trusting the approval-key registration.
  - Still out of scope even then: `ps eww` / `docker inspect`, edits to a service's own code, and a user-writable `/Applications/LStack.app`, which root ownership mitigates.
- [ ] Unix domain socket transport (replaces TCP + token); keep the TCP API opt-in.
- [ ] Import from PM2 `ecosystem.config.js`, Procfile, and existing compose files; `lstack export`.
- [ ] Config profiles/workspaces (switch "client-A stack" vs "personal").
- [ ] Linux support (systemd user units) since the daemon/CLI are portable.
- [ ] Remote dashboard: built-in HTTPS (self-signed or user-supplied cert), source-IP gating like the link port (LAN / Tailscale only) when **other devices too** is on, multiple users/roles, and log streaming for several services at once. Until then it's plain HTTP and open to any IP (README → Web dashboard → Plain HTTP).

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
| 1Password pulled by hand, cached locally | Unattended restarts must never prompt or need the network, and 1Password must never be reached without a person. A cache filled only by approved, client-side `op` reads covers both. |
| ratatui TUI | Mature layout and widgets with a small binary-size cost. Emoji are avoided because their width breaks box drawing. |

## Open questions

- Should stopping the daemon (rather than restarting it) optionally stop containers too? This could be a setting.
- Should Compose services expose each container as a child row in the UI? Currently they are aggregated into a single row.
- Beyond the web dashboard, should the CLI/TUI be able to attach to a remote lstack (for example, manage a dev VM over SSH)?
