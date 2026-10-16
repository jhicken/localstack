# Plan: Linux daemon, Docker discovery, external process watching, Dokploy

## Core idea
Don't build a second daemon. The Rust binary (daemon + CLI + TUI) is already ~90% portable; the
macOS-only parts are concentrated in a few modules. Make it cross-platform, run it headless on Linux,
and have Macs pair with it through the existing encrypted link (Noise IK + SPAKE2 pairing), so a
Linux box shows up as just another "machine".

Then add a new concept: **discovered (observed) services**, which come from a *provider* (Docker,
systemd, process-match rules) rather than from `services/*.yaml`. They flow through the same
`ServiceState` → SSE → menu bar / TUI / web / peer link pipeline, so every UI gets them for free.

## What's macOS-specific today (found in the code)
| Area | Today | Linux equivalent |
|---|---|---|
| `procinfo.rs` process tree, CPU/mem | `proc_pidinfo`, `proc_listpids` | `/proc/<pid>/stat`, `/proc/<pid>/status`, `/proc/<pid>/task/*/children` |
| Host CPU/mem | `host_statistics`, `sysctl hw.memsize` | `/proc/stat`, `/proc/meminfo` |
| Per-process network | `/usr/bin/nettop` | none cheap; `/proc/<pid>/net/dev` per netns (great for containers), else omit |
| Listening ports / forward same-user check | `lsof`, libproc FD walk | `/proc/net/tcp{,6}` + socket inode → pid via `/proc/<pid>/fd` |
| Secrets key share | Keychain via `/usr/bin/security` | 0600 file + optional `systemd-creds`/env/Docker secret (`LSTACK_SECRET_KEY_FILE`) |
| Startup | `launchd.rs` LaunchAgents | `systemd.rs`: user unit (`--user`, `loginctl enable-linger`) or system unit |
| Link "Local" transport | accepts only `en*` physical subnets (`remote.rs:1203`) | interface naming differs (`eth*`, `enp*`, `wlp*`); containers sit behind NAT → needs rethink |
| Tailnet helper | Go tsnet binary, darwin build | cross-compile linux/amd64+arm64 (tsnet is pure Go) |
| UI copy | "Mac" everywhere | "machine" |

Approach: a `src/platform/{mod,macos,linux}.rs` layer behind `#[cfg(target_os)]` exposing the same
functions `procinfo.rs`/`secrets.rs`/`launchd.rs` use now. Keep tests for parsers (`/proc`, `/proc/net/tcp`).

## New model: providers + discovered services
- `ServiceState` gains `source: "config" | "docker" | "systemd" | "watch"`, `group: Option<String>`
  (compose project / swarm stack / Dokploy project), `managed: bool`, and `container: Option<ContainerInfo>`
  (image, id, created, state, health, restart count, ports, networks, mounts, labels, redacted env, node).
- IDs: `docker:<project>/<service>` (or `docker:<container>` when standalone) so they can't collide with YAML names.
  `name@machine` keeps working: `lstack logs docker:shop/api@vps`.
- Discovered services are **read-only by default**. Start/stop/restart for them is a separate grant
  (`docker-control`) and a separate local setting, because the Docker socket is root-equivalent.
- Existing metric history, logs (via `follow`), health (Docker HEALTHCHECK passthrough), URLs (published ports),
  exec (`lstack exec` already uses a daemon PTY) all reuse current code paths.

### Docker provider (Mac optional, Linux default-on)
- Settings (`settings.json` / `lstack docker on|off|status`, Settings → Docker on the Mac app):
  `docker: { enabled, socket, includeStopped, include/exclude label filters, groupBy: compose|stack|none }`.
  Default: Linux = on; macOS = off (toggle).
- Engine API over the existing unix-socket client; add `/events` stream so we react instead of polling;
  `/containers/json?all=1` for the list; per-container `stats` (one-shot per tick, or streaming with a cap).
- Grouping: `com.docker.compose.project`/`.service`, `com.docker.stack.namespace`, `com.docker.swarm.service.name`.
  Group rows become collapsible in all UIs (answers the "Compose child rows?" open question in PLAN.md).
- Swarm manager: also list `/services` + `/tasks` to show replicas (desired/running) and which node; stats only for
  local-node tasks (or run lstack as a global service on every node — see Dokploy).
- Skip containers already owned by a YAML docker/compose service to avoid duplicates.
- Optional: remote Docker via `DOCKER_HOST=tcp://` / `ssh://` — later; installing lstack on the box is the main path.

### Watching processes already running (not started by lstack)
- `watch:` rules in `~/.localstack/watch.yaml`: match by `comm`, cmdline regex, user, pidfile, or port.
  Each match becomes a `source: watch` service with CPU/mem/net/ports/uptime; restarts are detected (pid change).
- Linux: optional `systemd` provider: list chosen units (or all non-system ones), status via `systemctl show`,
  logs via `journalctl -fu <unit> -o json`, control gated like Docker.
- macOS: same `watch` rules; launchd jobs could be a later provider.

## Running on Linux
- Artifacts: `lstack-linux-{x86_64,aarch64}` static musl builds (+ `lstack-tailnet`), `install.sh`
  (`curl … | sh`), later `.deb`/apt. CI in `release.yml`.
- `lstack setup install --system` → `/etc/systemd/system/lstack.service`, dedicated `lstack` user in the
  `docker` group (document that this is root-equivalent), state in `/var/lib/lstack` (`LSTACK_HOME`).
  `--user` mode for desktops/dev boxes.
- Headless pairing already works: `lstack remote share on && lstack remote pair` prints the code; the Mac runs
  `lstack machine pair vps --code …` or uses Settings → Machines.
- Reachability: a VPS is not on your LAN, so **Tailscale transport becomes the default for Linux servers**
  (the helper is userspace tsnet — no root, no kernel module). Also generalize the "Local" subnet check to
  Linux interface names. Possibly add an explicit "Direct (internet)" transport later — Noise IK is already
  safe to expose, but needs rate limiting review.

## Dokploy
Dokploy runs apps as Docker Swarm services and Compose apps as compose projects/stacks, routed by Traefik.
Two complementary pieces:

1. **Deploy lstack inside Dokploy** (container image `ghcr.io/jhicken/lstack`):
   - Compose template: mount `/var/run/docker.sock`, a volume for `LSTACK_HOME`, `pid: host` + `/proc:/host/proc:ro`
     (`LSTACK_PROC_ROOT=/host/proc`) to see host processes, secret key from a Docker secret/env.
   - Link port 7381 is raw TCP, not HTTP — Traefik won't route it; publish it in host mode, or (better) use the
     Tailscale transport so nothing is exposed publicly. Web dashboard can go through Traefik with a Dokploy domain.
   - Pairing: `docker exec <ctr> lstack remote pair` (or Dokploy's terminal).
   - Multi-node: deploy as a global service so each node reports local container stats; the manager reports swarm
     service/replica state.
2. **Dokploy enrichment** (optional provider config: Dokploy URL + API key kept in the lstack secret store):
   - Map containers → Dokploy project / environment / app name using labels + `project.all` (`x-api-key`).
   - Pull domains → service URLs (clickable chips), last deployment status/time, and later a "Redeploy" action
     (gated by a grant).
   - Works label-only without an API key (grouping by stack/app name), just with less pretty names.

## Phasing
| Phase | Scope | Done when |
|---|---|---|
| L0 Linux port | platform layer, systemd setup, secrets backend, Linux interface handling, CI builds, install.sh, "Mac"→"machine" copy | A Mac pairs (Tailscale or LAN) with a Linux box and sees its system load + busy processes; integration test runs on Linux CI |
| L1 Docker provider | provider trait, `source/group/container` fields, events, grouping, container info view in menu bar/TUI/web, `docker` settings + Mac toggle, read-only default, `docker-control` grant | Containers on Linux (default) and Mac (opt-in) appear grouped by project with stats, logs, health, ports, exec |
| L2 Process & systemd watching | `watch.yaml` rules, systemd provider | Pre-existing nginx/postgres processes or units show as services with metrics/logs |
| L3 Dokploy | container image + Dokploy compose template, swarm services/tasks, Dokploy API enrichment (names, domains, deploy status) | lstack deployed via Dokploy shows every Dokploy app grouped by project with domains as URLs |
| L4 Polish | multi-node swarm, remote `DOCKER_HOST`, redeploy action, `.deb`, alerts on container restarts | — |

## Risks / decisions to confirm
1. Docker socket = root on that host. Keep discovered services read-only unless explicitly enabled + granted.
2. Linux box role: watched-only (headless) vs. also a watcher (`lstack top` on Linux showing other machines). The
   binary does both; the plan assumes both work but UI work targets watched-first.
3. Run as root vs. `lstack` user in `docker` group (recommend the latter; `pid: host`/`/proc` read works for both
   with `hidepid` off).
4. Transport for cloud hosts: Tailscale-first vs. adding a "Direct" transport.
5. Per-process network on Linux: per-container (netns) is easy; per-host-process is not — accept "n/a".
