<p align="center">
  <img src="macos/icon/icon-1024.png" width="128" alt="LStack icon">
</p>

<h1 align="center">LStack</h1>

<p align="center">
  A PM2-style manager for your <b>local stack</b>: shell scripts, Docker containers and Docker Compose stacks (Kubernetes is planned).<br>
  It has a native <b>macOS menu bar app</b>, an <b>htop-style terminal UI</b> and an optional <b>remote web dashboard</b>.
</p>

<p align="center">
  <a href="https://github.com/jhicken/localstack/releases/latest"><b>⬇ Download the latest release</b></a>
</p>

<p align="center">
  <img src="docs/screenshots/menubar-detail.png" width="400" alt="Menu bar popover with an expanded service">
  &nbsp;
  <img src="docs/screenshots/menubar.png" width="400" alt="Menu bar popover">
</p>

- Starts the services you mark as **startup items** when you log in
- Shows live **health**, **CPU**, **memory** and **network** for each service, with history kept for 7 days
- Lets you watch **live logs** and interact: type into a script's stdin, or `docker exec` into a container
- Restarts crashed services with exponential backoff
- Optional password-protected **web dashboard**, so you can check on the machine from another computer
- Plain YAML config under `~/.localstack/`

> The CLI is named `lstack` so it doesn't collide with the LocalStack (AWS emulator) CLI.

## Screenshots

| Terminal UI (`lstack top`) | Remote web dashboard |
|---|---|
| ![lstack top](docs/screenshots/tui.png) | ![Web dashboard](docs/screenshots/web-dashboard.png) |

| Settings | Remote sign-in |
|---|---|
| ![Settings window](docs/screenshots/settings-setup.png) | ![Web login](docs/screenshots/web-login.png) |

## Architecture

```
            ┌────────────────────────┐        ┌──────────────────────┐
            │  LStack.app (SwiftUI)  │        │  lstack CLI / TUI    │
            │  menu bar extra        │        │  (lstack top)        │
            └──────────┬─────────────┘        └──────────┬───────────┘
                       │   HTTP + Server-Sent Events     │
                       │   127.0.0.1:7317, bearer token  │
                       └───────────────┬─────────────────┘
                              ┌────────▼────────┐   launchd LaunchAgent
                              │  lstack daemon  │◄─ (dev.lstack.daemon,
                              │  (Rust, ~10 MB) │    KeepAlive, at login)
                              └──┬─────┬─────┬──┘
                    process grp  │     │     │  Docker Engine API / docker compose
                       ┌─────────▼┐ ┌──▼───┐ ┌▼────────┐
                       │ scripts  │ │docker│ │ compose │  …k8s (planned)
                       └──────────┘ └──────┘ └─────────┘
```

All state lives in `~/.localstack` (override with `LSTACK_HOME`):

| Path | Purpose |
|---|---|
| `services/<name>.yaml` | one file per service (edit by hand + `lstack reload`) |
| `logs/<name>.log` | combined stdout/stderr, rotated at 10 MB |
| `metrics.db` | SQLite resource history (7-day retention) |
| `daemon.json` | pid / port / auth token of the running daemon (mode 0600) |
| `remote.json` | remote dashboard settings and password hash (mode 0600) |

## Install

Download `LStack-<version>-arm64.dmg` from **[Releases](https://github.com/jhicken/localstack/releases/latest)**.

LStack ships as **one app**. `LStack.app` contains the SwiftUI menu bar app and a single ~3 MB Rust binary (`Contents/Helpers/lstack`). That binary is the daemon, the CLI and the TUI.

1. Open `LStack-<version>-arm64.dmg` and drag **LStack** to Applications. Or build it yourself (see Development).
2. Launch it. A **Welcome** window offers to:
   - **Start at login**: writes LaunchAgents for the daemon (`KeepAlive`) and the menu bar app.
   - **Install the `lstack` command**: symlinks the bundled binary into `/usr/local/bin` if that is writable, otherwise `~/.local/bin`. If needed it adds that directory to `PATH` in `~/.zprofile`. An admin option links into `/usr/local/bin`.

   You can reopen this window later from the ⚙ **Settings** button in the popover.

You can do the same from a terminal:

```sh
/Applications/LStack.app/Contents/Helpers/lstack setup     # CLI + login items
lstack setup status [--json]                               # what's installed / running
lstack setup repair                                        # re-point links after moving/updating the app
lstack setup uninstall                                     # remove login items + CLI link
```

On every launch the app runs `setup repair --quiet`, so moving or replacing `LStack.app` keeps the CLI link and LaunchAgents correct. No Node, Bun or other runtime is needed.

## Usage

```sh
lstack add api -a -c ~/code/api -- npm run dev                         # script, start at login
lstack add db -t docker --image postgres:16 -p 5432:5432 -e POSTGRES_PASSWORD=dev -a
lstack add stack -t compose --file ~/code/app/docker-compose.yml -a
lstack add web --health-url http://localhost:3000/health -- npm start

lstack                 # htop-style dashboard (↑↓ select, s/x/r start/stop/restart,
                       #   a toggle autostart, l logs, i type to stdin, e shell, ? help)
lstack ls              # table of services
lstack logs api -f     # follow logs
lstack attach api      # follow logs + send input to the process
lstack exec db psql -U postgres
lstack metrics api --range 24h
lstack enable api / lstack disable api
lstack rm api
```

Run `lstack help` for everything.

## Remote dashboard

LStack can serve a small web dashboard, so you can check on a machine from another computer's browser. It shows services, health, CPU and memory history, and live logs. It is **off by default**. Turn it on from **Settings → Remote dashboard** in the app, or from the CLI:

```sh
lstack remote on                    # asks for a password the first time; listens on 0.0.0.0:7380
lstack remote on --port 8080 --user jeff --read-only
lstack remote passwd                # change the password (signs everyone out)
lstack remote status                # shows the URLs to open, e.g. http://192.168.1.20:7380
lstack remote off
echo "$PW" | lstack remote on --password-stdin   # non-interactive
```

Then open `http://<this-mac>:7380` and sign in.

- **Separate from the local API.** The daemon's own API stays on 127.0.0.1. The remote server is a second listener that exposes a read-mostly subset of it.
- **Accounts.** There is one account. The password is stored as a PBKDF2-SHA256 hash (120k rounds) in `remote.json`.
- **Sessions.** Sessions are HttpOnly, SameSite=Strict cookies that expire after 12 hours. Changing credentials or turning the server off ends every session and closes open log streams.
- **Login throttling.** Each failed login is delayed. After 5 failures from one IP, that IP is locked out for 5 minutes.
- **Controls.** Start/stop/restart buttons appear only when **Allow start / stop / restart** is enabled (`--allow-control`, the default). Use `--read-only` for view-only access.
- **Secrets.** Environment variable values are redacted (`••••`). Responses send a strict CSP, `X-Frame-Options: DENY` and CSRF header checks.
- **Plain HTTP.** Use it on a trusted network, or over a VPN such as Tailscale. macOS may ask you once to allow incoming connections.

### Service YAML

```yaml
# ~/.localstack/services/api.yaml
name: api
type: process            # process | docker | compose | k8s
command: npm run dev
cwd: ~/code/api
env: { PORT: "3000" }
autostart: true          # start at login / daemon boot
restart: on-failure      # always | on-failure | never
maxRestarts: 10
healthcheck: { type: http, url: http://localhost:3000/health, interval: 10 }
```

```yaml
name: db
type: docker
image: postgres:16       # or `container: existing-name` to manage an existing container
ports: ["5432:5432"]
volumes: ["pgdata:/var/lib/postgresql/data"]
env: { POSTGRES_PASSWORD: dev }
autostart: true
```

```yaml
name: stack
type: compose
composeFile: ~/code/app/docker-compose.yml
project: app
autostart: true
```

## Behavior notes

- Scripts run in their own process group under `$SHELL -c`. Stopping sends SIGTERM, then SIGKILL after 8 s.
- When the daemon exits, it stops scripts but **leaves containers running**. On restart it re-adopts them, and it reaps orphaned script process groups if the daemon was killed.
- At login, the daemon waits up to 10 minutes for Docker Desktop before it starts container services.
- The API binds to 127.0.0.1 only and requires the token in `daemon.json`.

## Development

Requires Rust ≥ 1.80 and Xcode / Swift 5.10+.

```sh
cargo run -- ls                     # run the CLI from source
cargo test                          # unit + end-to-end daemon test (sandboxed)
LSTACK_HOME=/tmp/lt LSTACK_PORT=7499 cargo run -- daemon run   # isolated daemon

macos/build.sh                      # → dist/LStack.app, .zip and .dmg
macos/build.sh --install            # …and copy to /Applications + launch
SIGN_IDENTITY="Developer ID Application: …" macos/build.sh      # real signing (default: ad-hoc)

LSTACK_SNAPSHOT=out.png dist/LStack.app/Contents/MacOS/LStack   # render the popover to a PNG
```

Layout: `src/daemon/` (supervisor, drivers, Docker Engine API client, metrics store, HTTP/SSE server), `src/cli.rs`, `src/tui.rs` (ratatui), `src/setup.rs` + `src/launchd.rs` (self-install), `macos/` (SwiftUI app).

Layout also includes `src/daemon/remote.rs` + `src/daemon/web/` (remote dashboard, embedded into the binary) and `macos/icon/` (the app icon is drawn in code: `macos/icon/build-icons.sh` regenerates `AppIcon.icns` and the web icon).

## Releases

Releases are published at **[github.com/jhicken/localstack/releases](https://github.com/jhicken/localstack/releases)**. Each release includes a `.dmg`, a `.zip` and SHA-256 checksums for Apple Silicon.

To cut a release:

```sh
# 1. bump `version` in Cargo.toml and commit
# 2. build, tag v<version>, push, and upload to GitHub Releases:
macos/release.sh                    # release notes are generated from commits
macos/release.sh --notes "Fixes…"   # or write your own
macos/release.sh --draft            # create as a draft
```

The script needs the GitHub CLI (`brew install gh && gh auth login`) and a clean working tree. Re-running it for an existing tag replaces that release's assets.

See [docs/PLAN.md](docs/PLAN.md) for the roadmap.
