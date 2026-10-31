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
- Lets you watch **live logs** and interact: type into a script's stdin, or `docker exec` into a container. The log window opens on the latest lines and pages older ones in from disk as you scroll up, so long logs never load all at once
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
| `logs/<name>.log` | combined stdout/stderr, rotated at 10 MB (`.log.1` keeps the previous file) |
| `metrics.db` | SQLite resource history (7-day retention) |
| `daemon.json` | pid / port / auth token of the running daemon (mode 0600) |
| `remote.json` | remote dashboard settings and password hash (mode 0600) |

## Install

Install or update to the latest release with one line:

```sh
curl -fsSL https://raw.githubusercontent.com/jhicken/localstack/main/install.sh | bash
```

It downloads the latest `LStack-<version>-arm64.zip` from Releases, checks it against the release's SHA-256, quits the menu bar app, replaces `/Applications/LStack.app` (using `sudo` only if that folder isn't writable) and opens it. It does nothing if you're already up to date. Set `LSTACK_VERSION=0.7.1` to pick a release or `LSTACK_APP_DIR=~/Applications` to install elsewhere.

Or download `LStack-<version>-arm64.dmg` from **[Releases](https://github.com/jhicken/localstack/releases/latest)**.

LStack ships as **one app**. `LStack.app` contains the SwiftUI menu bar app and a single ~3 MB Rust binary (`Contents/Helpers/lstack`). That binary is the daemon, the CLI and the TUI.

1. Open `LStack-<version>-arm64.dmg` and drag **LStack** to Applications. Or build it yourself (see Development).
2. Launch it. A **Welcome** window offers to:
   - **Start at login**: writes LaunchAgents for the daemon (`KeepAlive`) and the menu bar app.
   - **Install the `lstack` command**: symlinks the bundled binary into `/usr/local/bin` if that is writable, otherwise `~/.local/bin`. If needed it adds that directory to `PATH` in `~/.zprofile`. An admin option links into `/usr/local/bin`.

   You can reopen this window later from the ⚙ **Settings** button in the popover.

**Updating.** Run the install line above again, or quit LStack, replace the app in Applications and open it again. The background service (daemon) is versioned separately from the app (an API *protocol* number, shown by `lstack setup status`), so a new app release keeps using the daemon that's already running, and your scripts keep running. LStack only asks to restart the daemon when a release needs a newer protocol than the running one provides (the release notes say so). Newer features that need a newer daemon (the CLI says "the background service is older than this command") become available whenever you run `lstack daemon restart`; restarting stops scripts LStack started (autostart ones start again) and containers keep running. Developers can set `LSTACK_STRICT_DAEMON=1` to require the exact same build instead. The first update to a release with this scheme still restarts the daemon once, because older daemons don't report a protocol.

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
lstack docker ls                 # every container in Docker Desktop, and whether LStack manages it
lstack docker sync               # register every container now; drops ones no longer in `docker ps -a`
lstack docker sync on            # ...and keep doing it automatically (new containers appear, destroyed ones go away)
lstack docker import db cache    # or adopt just some, by container name / compose project
lstack add web --health-url http://localhost:3000/health -- npm start
lstack add app -a -- 'cd ~/code/app && PORT=3000 bun run dev'        # cd/env are split out automatically

lstack                 # htop-style dashboard (↑↓ select, s/x/r start/stop/restart,
                       #   a toggle autostart, l logs, i type to stdin, e shell, ? help)
lstack ls              # table of services
lstack logs api -f     # follow logs
lstack attach api      # follow logs + send input to the process
lstack exec db psql -U postgres
lstack metrics api --range 24h
lstack enable api / lstack disable api
lstack rm api
lstack terminal iterm  # app used by Dashboard/Attach: terminal | iterm | cmux | ghostty
lstack ps              # other busy processes (over the CPU / memory / network thresholds)
```

The menu bar footer has two dashboard buttons: **Terminal** opens `lstack top` in your terminal app, and **Web** opens the web dashboard in your browser. Web appears only when the web dashboard is on.

Run `lstack help` for everything.

## Web dashboard

LStack can serve a small web dashboard. It shows services, health, CPU and memory history, live logs, and a whole-machine view per machine: each machine has a **Machine** section with a Load row (CPU and memory sparklines) above its busiest processes; click the Load row for CPU and memory charts covering the last 15 minutes, with LStack's share highlighted, plus the full process list. Machine groups and Machine sections fold away with a click, and the dashboard remembers which ones are folded. It is **off by default**. Turn it on from **Settings → Web dashboard** in the app, or from the CLI.

You choose who can open it: **this Mac only** (the listener binds to 127.0.0.1, and other devices get 403), or **other devices too**. Sharing with other Macs is a separate switch on its own encrypted link port (see [Other machines](#other-machines)), so you can use the dashboard locally without exposing it, or share with paired Macs while the dashboard stays off.

```sh
lstack remote on --local            # asks for a password the first time; http://127.0.0.1:7380
lstack remote web network           # let other devices open it too (listens on 0.0.0.0:7380)
lstack remote web local             # back to this Mac only
lstack remote on --port 8080 --user jeff --read-only
lstack remote passwd                # change the password (signs everyone out)
lstack remote status                # shows the URLs to open, e.g. http://192.168.1.20:7380
lstack remote off                   # dashboard off (sharing is unaffected)
echo "$PW" | lstack remote on --password-stdin   # non-interactive
```

Then open `http://127.0.0.1:7380` (or `http://<this-mac>:7380` from another device) and sign in. Configs from before the split keep their behavior: the dashboard stays open to other devices and sharing stays on if the dashboard was on.

**Two-factor authentication (TOTP).** Run `lstack security 2fa on`, or use **Settings → Security → Two-factor authentication → Set Up…**, then scan the QR code with 1Password, Google Authenticator, Authy or a similar app. After that, pairing a new Mac with this one and the dashboard sign-in page both ask for a 6-digit code. It doesn't depend on the web dashboard being on. A code can be used only once, and the clock may be off by ±30 s. Turn it off with `lstack security 2fa off`.

- **Separate from the local API.** The daemon's own API stays on 127.0.0.1. The remote server is a second listener that exposes a read-mostly subset of it.
- **Accounts.** There is one account. The password is stored as a PBKDF2-SHA256 hash (120k rounds) in `remote.json`.
- **Sessions.** Sessions are HttpOnly, SameSite=Strict cookies that expire after 12 hours. Changing credentials or turning the server off ends every session and closes open log streams.
- **Login throttling.** Each failed login is delayed. After 5 failures from one IP, that IP is locked out for 5 minutes.
- **Controls.** Start/stop/restart buttons appear only when **Allow start / stop / restart** is enabled (`--allow-control`, the default). Use `--read-only` for view-only access.
- **Secrets.** The dashboard never returns secret values: environment variable values, heartbeat ping URLs and approval details are redacted (`••••`) on every route, including this Mac's services under `/api/machines/<this-mac>/…`. Responses send a strict CSP, `X-Frame-Options: DENY` and CSRF header checks.
- **Plain HTTP (no TLS).** With **other devices too**, the dashboard listens on `0.0.0.0` over unencrypted HTTP. Anyone who can watch your network traffic, such as someone on the same Wi-Fi, can capture the username, password, 2FA code and the 12-hour session cookie, then act as you: view services and logs and, if **Allow start / stop / restart** is on, start, stop or restart services here and on paired Macs that granted this Mac control. Secret values are never sent. Unlike the encrypted link between paired Macs, the dashboard doesn't restrict which IPs can connect. Use it on a trusted network only, prefer **this Mac only** or `--read-only`, or put it behind your own HTTPS reverse proxy or VPN. macOS may ask you once to allow incoming connections.

## Secrets

LStack has a write-only secret store for tokens and passwords that your scripts need. Once you store a value, nothing can read it back: no API, no CLI command and no UI shows it. Each service gets only the secrets you give it, as environment variables.

```sh
lstack secret set GITHUB_TOKEN          # hidden prompt (or: … --stdin)
lstack secret ls                        # names, when they were set, which services use them
lstack secret rm GITHUB_TOKEN
lstack secret rotate-key                # re-encrypt everything under a new key
```

```yaml
secrets: [GITHUB_TOKEN, NPM_TOKEN]       # env var = secret name
secrets: { GH_TOKEN: GITHUB_TOKEN }      # or rename: ENV_VAR: SECRET_NAME
```

The usual place for a service's variables is its env file (`envFiles:`; `lstack add` picks up the folder's `.env.lstack`). A line can hold a plain value, a 1Password reference (`op://…`) or a secret stored in LStack, written `KEY=lstack://NAME`:

```sh
lstack env api                          # every variable in api's env files (plain values: --values)
lstack env api set PORT=3000            # edit the file (--file picks one of several)
lstack env api secret DB_PASSWORD       # store the file's current value in LStack; the line becomes lstack://DB_PASSWORD
lstack env api link GH_TOKEN GITHUB_TOKEN   # use a secret that's already stored
lstack env api approve                  # approve lstack:// lines you wrote by hand
```

A `lstack://` line you type into a file yourself needs one approval (in the app or with `lstack env … approve`), so a cloned repo's `.env` can't pull your stored tokens. Lines written by **Convert to Secret**, **Use Stored Secret** or `lstack env` are approved right away. When you edit an env file while its service is running, LStack tells you which keys changed (a notification, an **Env changed** badge and a log line) and applies them on the next restart.

In the app, secrets live with the service that uses them: open a service's **⋯ → Environment Variables…** (or click its environment chips) to see its env files and every variable in them. You can switch or add env files, edit or remove values, and use the lock button (**Convert to Secret**) to move a plain value into the store so it's hidden from then on. A secret you stored for one service can be picked for another with **Use Stored Secret**. Variables set in the service's YAML (`env:`/`secrets:`) show up under **Set in the service definition** only when there are any. **Settings → Secrets** keeps the store itself (lock state, key rotation) and a collapsed **Stored secrets** list for adding, replacing or deleting values directly.

- **At rest.** Secrets are encrypted with ChaCha20-Poly1305 in `secrets.enc`. The 256-bit key is never stored whole. It is split into two XOR shares: one is a Keychain item (`dev.lstack.secrets`, created via `/usr/bin/security` so app updates don't cause new prompts), the other a `0600` file. The daemon reads the Keychain once at boot and keeps the key only in memory, zeroed when dropped. If the Keychain is locked, the store stays locked: services that need secrets fail with a clear error, and `lstack secret unlock` (or **Unlock** in Settings) retries.
- **Logs are scrubbed.** Every secret value, plus its base64, URL-encoded and hex forms, is replaced with `••••[NAME]`. This applies to log files, live streams, the TUI and web views, crash messages, heartbeat `/fail` bodies, logs sent to other machines and `lstack exec` sessions. The daemon runs `docker exec` on a pseudo-terminal and scrubs the output before it reaches your terminal. Existing log files are scrubbed again when you add a secret. To do this the scrubber keeps those forms in memory; old copies are zeroed whenever the secrets change.
- **In memory.** The daemon keeps every value sealed (ChaCha20-Poly1305) under a random key made at each start, held in locked memory that's never swapped. A value is opened only for the moment it's needed: starting a service, sending to a paired Mac you shared it with, or writing `secrets.enc`. The scrubber's patterns are one sealed blob, opened per scrub. Nothing changed on disk, so the store still unlocks at boot with nobody there. The daemon also turns off core dumps and refuses debuggers (`PT_DENY_ATTACH`). The app is built with the hardened runtime, even when ad-hoc signed.
- **Only on this Mac.** Secrets can't be set or read over the remote dashboard or by other machines.
- **The local API.** It accepts the token only in an `Authorization` header (never in a URL), compared in constant time. It refuses requests that carry a browser `Origin` or a non-loopback `Host`, so a web page can't drive it even through DNS rebinding. `LSTACK_*` names can't be set or deleted through it.
- **Limitations.**
  - Docker stores container env, so anyone with Docker access can see it via `docker inspect`. A container keeps its old env until it is recreated.
  - A secret embedded in a larger string and then base64-encoded isn't scrubbed.
  - In `lstack exec`, a secret printed one character at a time can show its first two characters before the rest is masked, and any of it can show if the pauses are longer than 0.3 s. Running `docker exec` directly bypasses LStack entirely.

### Touch ID approvals

Anything that can run as you can read LStack's token from `daemon.json` and use the API. Without approvals, that's enough to point a service that holds secrets at a script that sends them somewhere. With approvals on, **a service holding secrets gets them only for a configuration and values you approved with Touch ID on this Mac**. Restarts of an approved service never need anyone.

- **The key.** LStack.app creates a P-256 key inside the Secure Enclave that signs only after Touch ID (or your login password). The private key never leaves the chip; `~/Library/Application Support/LStack/approval-key` is an encrypted handle only this Mac's Secure Enclave can use. The app registers its public key with the daemon on launch. That turns approvals on, and approves every current service as it is. **Settings → Secrets** shows the status.
- **What's approved.** For each service with secrets: its configuration (command, image, cwd, env, env files, ports, …; not cosmetic fields like description, tags or restart policy) plus, for each secret it gets, the name, source, 1Password reference and a keyed digest of the value. The approved state is stored inside the encrypted store, so it can't be edited on disk, and the API refuses to touch it. Values received from a paired Mac are approved by that Mac's pinned key instead, so its refreshes don't ask again.
- **When something changes.** If an edited YAML file, an API call, an import or a swapped value changes any of that, a running service keeps running as approved, and a restart or start is held back. LStack.app shows what changed (for example `command: npm start → curl … | sh`, `now gets BANK ← secret BANK from 1Password op://Private/…`, `the value of secret DB changed`). **Approve with Touch ID…** signs the request and the service starts; **Deny** leaves it stopped. The CLI waits: `lstack start`/`restart` print what changed, open LStack.app and continue once you approve.
- **Consoles.** Typing into a service that holds secrets (`lstack attach`, the app's input field) or `lstack exec` into one needs its own approval, which lasts 10 minutes.
- **Replacing the key.** A different key is accepted only with a signature from the registered one. If you lose the key (new Mac, deleted file), approvals can't be answered: run `lstack daemon stop`, move `~/.localstack/secrets.*` aside, start again and set your secrets again (1Password ones: `lstack secret import`).
- **Remote.** Approval details never leave this Mac; the remote dashboard and paired Macs only see that one is pending.

#### What approvals and the store can't stop

These need something LStack doesn't have, or are outside what it controls:

- **The Keychain share.** LStack is signed ad hoc (no Apple Developer ID), so its Keychain item trusts `/usr/bin/security`. Malware running as you can read that share plus the `0600` file and decrypt `secrets.enc` offline, with no prompt. A Developer ID signature would let the item trust only LStack.
- **A running service's environment.** A process running as you can read another's environment (`ps eww <pid>`), and Docker records container env (`docker inspect`).
- **A service's own code.** Approvals cover LStack's configuration, not what's in the service's folder. Malware that edits `~/code/api/server.js` gets that service's secrets at its next restart.
- **The app itself.** `/Applications/LStack.app` is writable by you, so malware could replace it. Owning it by root (`sudo chown -R root:wheel /Applications/LStack.app`) stops that; updates then need `sudo`.
- **Prompt phishing.** Malware can ask the Secure Enclave to use the key, which shows a Touch ID prompt with its own text. The system names the app asking; approve only prompts that LStack.app shows after its own dialog.

### 1Password

Keep secrets in 1Password and let services restart any time with nobody around. LStack treats 1Password as a **source you pull from by hand**. The store above is the cache that restarts read from. There is no service-account token and no stored 1Password credential. The daemon has no code that talks to 1Password. Only the `lstack` CLI or LStack.app runs `op`, after you confirm, and the 1Password app asks you to approve.

Use the same `.env` you'd give `op run --env-file`:

```sh
# ~/code/api/.env
PORT=3000
DATABASE_URL=op://Dev/api-db/connection-string
STRIPE_KEY=op://Dev/stripe/$APP_ENV/secret-key   # $VARS resolve from the service's env:
```

```sh
lstack add api -c ~/code/api --env-file .env -- npm run dev
lstack add api -c ~/code/api -- npm run dev   # uses .env.lstack automatically if the folder has one
lstack secret review api        # references, status, vault · item · field, warnings
lstack secret import api        # confirm → 1Password asks you to approve → values cached
lstack secret import .env       # same, naming the file: the service using it (or running in its folder)
lstack restart api              # restarts never need 1Password
lstack secret refresh --all     # re-read cached values (e.g. after rotating them in 1Password)
lstack secret link GH_TOKEN op://Dev/github/token --service api   # one reference, no .env
lstack secret import --environment ~/.config/op/env/api.env --service api   # a 1Password Environment mount
lstack secret audit             # imports, refreshes, removals, transfers (never values)
```

```yaml
envFiles: [.env, .env.local]   # later files win, like repeated --env-file; plain values are read at each start
secrets: { DATABASE_URL: DATABASE_URL }   # written by `secret import`: what you approved
```

- **`.env.lstack`.** Give a project a `.env.lstack` to describe what LStack should inject (plain values and `op://` references) without touching the app's own `.env`. `lstack add` looks for it in the service's folder: on a terminal it asks which file to use (`.env.lstack` by default, any other `.env*` in the folder, or none); otherwise (or with `-y`) it uses `.env.lstack` and says so. `--env-file` picks files explicitly and `--no-env-file` skips them. The app's **Add Service** window preselects it the same way.
- **In the app.** A service's **Environment Variables…** view lists its env files (with the keys each one sets), and every variable in them, including `op://` references with their status and **Review & Pull…** when there's something to pull. To add a reference, put an `op://` line in the env file. A mounted 1Password Environment is imported with `lstack secret import --environment`.
- **What a service gets.** Plain values come from the env files, read at each start, and they override `env:` like `op run` does. For `op://` lines, a service gets only what you approved, stored in its `secrets:` map with the reference recorded next to each stored value. References are never read from the file at runtime. If someone edits the `.env` to point a key at another secret, nothing changes until you review and import it.
- **Watching.** LStack checks imported env files every 2 s, using metadata only until something changes. New or changed references show as **1Password changes to review** in `lstack ls`, the service view, the menu bar and a notification. Until you import them, the service keeps starting with what you last approved, and new keys aren't set. It never pulls by itself. A removed line stops being passed right away (`autoApplyRemovals`), and the cached value is deleted once no running service uses it.
- **Empty values.** A 1Password field that's an empty string is stored as an empty value (the service gets the variable set to `""`) and shown with a ⚠︎ **Empty value** marker under Settings → Secrets. Only values typed in by hand must be non-empty.
- **What the review flags.** It shows warnings for a vault you haven't imported from before, for Private/Personal/Employee vaults, and for vaults outside `onepassword.allowedVaults`. It refuses `LSTACK_*` keys, TOTP (`?attribute=otp`), references with unset `$VARS`, and references hiding control, zero-width or bidi characters. `$VARS` come from earlier lines, the service's `env:`, and only `HOME`, `USER` and `LOGNAME` from LStack's own environment. It warns about `{{ op://… }}` (that's `op inject` syntax, which `op run` passes as written).
- **Variables that load code.** For a service holding secrets, LStack ignores `NODE_OPTIONS`, `DYLD_*`, `LD_*`, `PATH`, `PYTHONPATH`, `BASH_ENV` and similar variables when they come from an env file, and warns. That stops a cloned repo's `.env` from running its own code in a process that has your values. Set them in the service's `env:` if you need them.
- **The `op` binary.** Before each run the client checks `op`'s code signature against an Apple-anchored requirement for AgileBits' Team ID (`2BUA8C4S2C`), and only looks in absolute `PATH` entries. It runs `op` with a minimal environment, so a stray `OP_SERVICE_ACCOUNT_TOKEN` or `OP_CONNECT_*` can't skip your approval. Values come back over a pipe and never appear in argv. The client runs `op signout` after each batch. Each import is tied to a single-use plan that expires after 15 minutes, and values for references not in the plan are refused.
- **1Password Environments.** A mounted Environment is a named pipe, and reading it asks for approval. The watcher never opens it; `--environment` (or the app) reads it when you ask. Don't let the service's own dotenv read the mount, or every restart would need approval.
- **Settings.** **Settings → 1Password**, or `onepassword` in `settings.json`:
  - `watch`: default on.
  - `autoApplyRemovals`: default on.
  - `restartOnChange`: restarts services whose values changed after an import or refresh. Default off.
  - `allowedVaults`.
  - `pushToPeers` and `pullFromPeers`: see below.
- **Value changes in 1Password.** LStack can't see a value change in 1Password while the reference stays the same. Use **Refresh**; `secret ls` shows each value's source and when it was fetched.

**Sharing with paired Macs.** Paired Macs can receive 1Password values cached here, over the encrypted link. It works whichever way they're paired: with a Mac that watches this one or with one this Mac watches. When this Mac is the one watching, it delivers values over its own link, because the other Mac can't connect here. It never reads 1Password here, so it can't cause an approval prompt. Each transfer takes **one ask and one approval**, and either Mac can start it:

- **The receiving Mac asks.** Turn on **Ask paired Macs for missing shared values** (`pullFromPeers`) on the Mac that needs the values. Every 15 seconds it asks its paired Macs for the 1Password references its services' env files wait for. You can also click **Pull Shared Values** (Settings → Secrets, or a service's **Review & Pull…**) or run `lstack secret sync`. Because it keeps asking, the request reaches the other Mac whenever that Mac is online. The Mac that has the value gets a notification naming the requesting Mac and the service, for example "studio asks for DB_PASSWORD for api". Approve or decline it right from the notification (its **Approve** covers every value listed), with **Approve All** per Mac, under Settings → Secrets → Shared with Macs, or with `lstack secret share` / `decline`. Requests are matched by 1Password reference, never by name, so different names on the two Macs don't matter. If the Mac being asked doesn't have that value yet, approving (the notification's **Approve**, or **Read & Approve** in Settings) reads it from 1Password there first. The 1Password app asks you to unlock, and the value is stored under a free name (the other Mac's key, or `KEY_2` if that name is taken, so an existing value is never replaced). Once approved, the value arrives on the next sync. It's stored encrypted, the same way as on the sending Mac, and goes straight to the services that asked. There's no second confirmation.
- **The sending Mac pushes.** Click **Push Secrets…** on a paired Mac's row in Settings → Machines, either under the machines this Mac watches or under the Macs that can see this one. Or run `lstack secret push <machine> <NAME>…`. The button shows while **Share refreshed values with Macs you've shared them with** (`pushToPeers`) is on. The other Mac is notified and accepts or rejects the values on its Settings → Machines row, under Settings → Secrets, or with `lstack secret accept` / `reject`. Nothing is sent until it accepts. Accepted values go to its services whose env files reference them. Otherwise they're kept under the sender's name. A push waits until the other Mac comes online.

```sh
lstack secret shares                       # requests from paired Macs, what you share, pushes waiting here
lstack secret share studio DATABASE_URL    # approve a request (by stored name or exact op:// reference)
lstack secret share studio --all           # approve all of studio's requests at once
lstack secret decline studio op://Dev/db/password
lstack secret unshare studio DATABASE_URL  # stop sharing
lstack secret push laptop DATABASE_URL     # offer a value; laptop accepts it
lstack secret sync [service]               # on the receiving Mac: ask now, take approved values
lstack secret accept|reject studio [op://…]   # on the receiving Mac: answer a push
```

- **Pull from 1Password** is separate. **Pull from 1Password** in the review always reads from 1Password on this Mac (the 1Password app asks for approval). **Pull Shared Values** only asks paired Macs.
- **Sending.** Nothing is shared by default. Each grant is for one exact reference and one Mac, and you can revoke it. Only values read from 1Password on this Mac are sent: never typed-in ones, never ones received from another Mac, never `LSTACK_*`.
- **Receiving.** A Mac takes a new value only for a reference it asked for (its own env files reference it) or a push someone accepted there. It stores each value as `from <machine>`, bound to that Mac's pinned key, and never overwrites a stored value that holds a different reference or came from elsewhere. A request tells the other Mac the reference and the service names, nothing else. Requests are only sent with `pullFromPeers` on, or when you click **Pull Shared Values** or run `lstack secret sync`. After a manual pull, Macs that watch this one can see the request for the next 10 minutes. When a Mac that watches this one pushes values, accepting them doesn't fetch anything right away; that Mac delivers them on its next round, within 15 seconds. If Touch ID approval is on for services, the changed service still needs it before it starts with the new value.
- **What's automatic.** Newer values for references already received arrive by themselves after the sending Mac refreshes them (`pushToPeers`, on by default there), and only from the same Mac key that sent them. `restartOnChange` restarts the services that use them.
- **Audit and revocation.** Both Macs audit transfers. Stopping sharing or unpairing stops updates; values already received stay on that Mac until you remove them.

## Service URLs

When a service starts a web server, LStack finds its listening TCP ports (for scripts, any process in the service's process tree; for containers, their published ports). Each one becomes a clickable link: chips in the menu bar, `o` in `lstack top`, a URL column in `lstack ls`, and links in the web dashboard. To add your own links or label them:

```yaml
urls:
  - http://localhost:3000
  - { url: http://localhost:3000/admin, label: Admin }
detectUrls: false        # show only the urls above
```

On other machines, `localhost` is rewritten to that machine's address. Ports bound only to 127.0.0.1 are marked **local only**, because they usually can't be reached from another machine.

## Heartbeats (healthchecks.io)

LStack can ping [healthchecks.io](https://healthchecks.io), or any compatible server such as a self-hosted Healthchecks, so you get alerted when a service or the whole Mac goes down.

```sh
lstack heartbeat key                                  # project ping key (stored in the secret store)
lstack heartbeat set api                              # pings <server>/<key>/api (check created on first ping)
lstack heartbeat set api --url https://hc-ping.com/<uuid> --interval 120
lstack heartbeat machine mac-mini                     # ping while this Mac and LStack are up
lstack heartbeat test api
lstack heartbeat                                      # last ping per service
```

```yaml
heartbeat:
  url: https://hc-ping.com/<uuid>   # or `slug: api` with a ping key; default slug = service name
  interval: 60                      # success ping while running and healthy
  start: true                       # /start when it (re)starts
  fail: true                        # /fail on crash, unhealthy or giving up
  logs: true                        # attach the last 20 (scrubbed) log lines to /fail
```

You can also manage this in **Settings → Monitoring**, where you set the ping key and server, turn on the machine heartbeat, and use a one-click per-service toggle and Test button. The last ping result appears in each service's details in every interface. Ping URLs are hidden from the remote dashboard.

## Other machines

A Mac can watch other Macs that run LStack. Their services and busy processes then appear, grouped by machine, in the menu bar, `lstack ls`, `lstack top` and the web dashboard. In the menu bar, each other Mac shows a Load row with CPU and memory sparklines above its busy processes. Machine sections can be collapsed, and the menu bar remembers which ones are.

Each paired Mac's web servers can be **forwarded to this Mac**, per service and **off by default**. Turn it on with the switch next to a service's addresses (or **Settings → Machines**, or `lstack forwards enable web@studio`). A service on the other Mac at `127.0.0.1:3000` then shows up here as `http://localhost:3000`, marked 🔒 "via studio" in every interface. Until then its addresses are shown disabled. Everything goes through one end-to-end encrypted link per Mac.

1. On the Mac to be watched, turn on sharing (`lstack remote share on`, or **Settings → Machines → Share this Mac**) and run `lstack remote pair` or click **Show Pairing Code**. You get a one-time code, valid for 10 minutes. No dashboard or login is needed. Turn on 2FA (**Settings → Security**, or `lstack security 2fa on`) if you can.
   Once you've chosen **Show this Mac as**, click **Copy Quick Connect Link** (or read it from `lstack remote pair`). You get `lstack-pair://studio.lan:7381?code=XXXX-XXXX`: the address and the code in one link. The 2FA code is never part of it.
2. On your Mac, paste the link into **Settings → Machines → Pair** (or click the clipboard button), or run `lstack machine pair 'lstack-pair://…'`. You can also enter both parts yourself: `lstack machine pair studio.local --code XXXX-XXXX` (add `--totp 123456` if that Mac uses 2FA). Both Macs then show the same four-word phrase, which you can compare if you want to be sure.

```sh
lstack machine ls                 # status, address, access granted to you
lstack machine id                 # this install's key fingerprint; `machine rekey` replaces it
lstack ls --machine studio        # or --local
lstack logs api@studio -f         # name@machine works for logs/show/start/stop/restart/metrics
lstack ps --machine studio
lstack forwards                   # paired Macs' services and whether each is forwarded to localhost
lstack forwards enable web@studio # or disable (forwarding is off until enabled)
lstack machine rename studio mini
lstack machine rm mini            # unpair: mini forgets this Mac too

# on the watched Mac
lstack remote peers               # paired Macs, their access and link state
lstack remote grant laptop control --tunnel web,api   # or no-control, --tunnel all|none
lstack remote revoke laptop       # unpair now: its link and tunnels drop, and laptop forgets this Mac too
lstack remote address             # ways to reach this Mac (DNS name, Bonjour, IPs), each checked
lstack remote address mini.lan    # the address to show other Macs (`auto` = IP address)
lstack remote allow 192.168.1.0/24   # also accept Macs from this network; `allow` lists, `allow rm <net>` / `allow clear` remove
lstack remote activity            # links, pairings, tunnels and refusals
```

- **Access per Mac.** A new pairing can view and tunnel to all services but can't control them. On the watched Mac, change this per paired Mac: start/stop/restart, and tunnels to all services, none, or a list (Settings → Machines → Share this Mac, or `lstack remote grant`). Changes apply at once: the link reconnects with the new grants. Turning sharing off drops every link; pairings are kept.
- **Forwards.** Forwards exist only for services you've turned on, only if they're granted, and only for ports that service is listening on *right now*. The watched Mac checks this again for every connection, so another program sitting on a stopped service's port is refused. It only ever connects to its own loopback. On this Mac, forwards listen only on 127.0.0.1/::1 and keep the same port number when it's free (otherwise a remembered fallback, saved in `forwards.json`). Each connection must come from a process owned by your macOS user.
- **Unpairing is mutual.** Removing a machine on either Mac (`machine rm`, `remote revoke`, or the Remove/Revoke buttons) tells the other Mac over the link, and it forgets the pairing too. If the other Mac is offline, it's told the next time either side can reach the other.
- **Addresses.** Pair with an IP address or a name: router DNS names such as `studio.lan` and Bonjour `studio.local` keep working when the IP changes. **Settings → Machines → Share this Mac → Show this Mac as** (or `lstack remote address`) lists the names that lead to this Mac and lets you choose the one shown with the pairing code. Tailscale addresses (100.x and `.ts.net` names) are listed too. The address you choose becomes **the only way in** (see below).
- **Forwards are the only way in.** A paired Mac's services are never offered at its network address: the app, web dashboard, CLI and TUI only open them through their local forward, so a service whose forwarding is off can't be opened from here. A service you're not allowed to tunnel to has no link (the CLI marks it `no tunnel`).
- **Allowed networks.** By default a LAN address (or Automatic) accepts only Macs on this Mac's own subnets, which Settings shows as always allowed. If your Macs sit on different routed subnets (say 192.168.1.x reaching a DMZ at 192.168.2.x), add the watching Mac's network on the watched Mac under **Allowed networks** (`lstack remote allow 192.168.1.149`). Use a single host or the narrowest range you can; anything wider than /16 (IPv6: /48) is refused. Public and Tailscale ranges stay refused unless you list them. The Noise keys and the pairing code still authorize every link. This trusts the source address, so across subnets or a DMZ prefer a VPN (Tailscale, NetBird): choose its address under **Show this Mac as**.
- **One way in.** Paired Macs connect directly, LStack daemon to LStack daemon, through the address chosen in **Show this Mac as**. That can be a LAN name or IP, or a Tailscale address. With **Automatic**, only the local network is accepted. There's no relay or helper process in the path. Changing the address drops existing links; Macs that only know the old address must pair again.
- **Secrets stay put unless you share them.** Secret values are never sent, except 1Password-sourced values you explicitly share with a specific Mac (see [1Password](#1password)). Env values and ping URLs are redacted, and logs are scrubbed on the watched Mac before they're sent.
- **Upgrading from an older LStack.** Old pairings used bearer tokens over plain HTTP and are no longer accepted. They show as **pair again**: get a new code on the other Mac and pair again; the name is kept.

### Security model

Each LStack install has its own X25519 key pair. The private key is kept in the secret store (encrypted, split between the Keychain and a 0600 file) and never leaves the Mac. Pairing ties two installs together; after that, only those two keys can talk.

- **Pairing.** The two Macs run SPAKE2 over the one-time code, so the code never crosses the network and an attacker gets one online guess. Three wrong tries burn it. If the watched Mac has 2FA on, the 2FA code goes only inside that encrypted session and must also be right. Then both Macs swap and pin each other's public key.
- **The link.** One TCP connection per paired Mac to the watched Mac's link port (default **7381**, `--link-port`), using **Noise IK** (X25519, ChaCha20-Poly1305, BLAKE2s). It's mutually authenticated by the pinned keys and forward-secret. Status, logs, control, `exec` and tunnels are typed streams multiplexed over it, and each stream is checked against that Mac's grants when it opens. The link port speaks nothing but Noise. Unknown keys are refused and logged, and failed pairings count toward a per-address lockout (5 failures → 5 minutes; the other Mac is told so rather than just seeing the connection reset).
- **No shared secrets stored.** `peers.json` and `remote.json` hold only public keys, names and grants, so copying them gives nothing.

| Attacker | Result |
|---|---|
| Watches the network | Sees only Noise ciphertext; the pairing code and 2FA code are never sent in the clear |
| Man-in-the-middle during pairing | One guess at the code, then it's burned |
| Man-in-the-middle later | Handshake fails without both pinned private keys |
| Copies `peers.json` / `remote.json` | Gets public keys only |
| Knows the dashboard password only | Can't pair: pairing needs a live code from the other Mac's screen |
| Sees the pairing code only | Has one try before it's burned, and also needs a 2FA code if 2FA is on |
| Another macOS user on your Mac | Their connections to forwarded ports are refused |
| Another app running as you | Can use forwards, just like your browser can (same trust as your user) |
| A compromised paired Mac | Limited to its grants: granted services' own ports on the loopback, and the exact 1Password references shared with it; audited and revocable |

- **Daemon to daemon.** Each end of the link is the LStack daemon itself: it dials or accepts the TCP connection, runs the Noise handshake and holds the keys in its own (debugger-protected, no-core-dump) process. No other process ever sees plaintext, so values shared between Macs (such as 1Password secrets) are decrypted only inside the receiving daemon.
- **Only the chosen path.** If **Show this Mac as** is set, pairings and links are accepted only when they arrive at an IP that name resolves to on this Mac. For a LAN address, the sender must also be on this Mac's own LAN subnets, so a VPN routed into the LAN doesn't count. For a Tailscale address, any Mac on your tailnet can connect. With **Automatic**, or a name that doesn't lead here, only this Mac's LAN subnets (and loopback) are accepted, and never overlay ranges such as 100.64.0.0/10. Loopback is always allowed. The Noise keys still authorize every link.
- **Tunnels end on the service.** A tunnel stream reaches only a granted service's own listening port on the loopback, never LStack's own ports (its API, web dashboard or link port).

Not supported: reverse tunnels, UDP, relays or NAT traversal. Paired Macs must reach each other directly, on the same local network or over Tailscale.

**Future: screen sharing.** The link's typed streams are built so that screen sharing can be added later as two more stream kinds, each with its own grant and a consent prompt on the shared Mac: `screen` (ScreenCaptureKit capture, VideoToolbox H.264/HEVC frames) and `input` (CGEvent injection; needs Accessibility). Those frames would use the same Noise link with no extra crypto. This isn't implemented yet.

## Busy processes

Under each machine's services, LStack lists other processes that cross a threshold. The defaults are CPU over 10% of a core, memory over 10% of RAM, or network over 1 MB/s, with at most 15 per machine. LStack services always show, whatever their usage. You can change the thresholds in **Settings → Monitoring**, or with `PUT /api/settings {"processes": {…}}`. The same list is available from `lstack ps` and `GET /api/processes`. Only your own processes' network use and ports can be read without root.

### Shell environment

Scripts run the way they would in a new Terminal tab. That matters because the daemon is started by launchd with a bare environment, so tools like `bun`, `nvm`, Homebrew binaries or the 1Password `op` CLI would otherwise be missing.

- Each script runs in your **login shell** (`$SHELL -l`, so `~/.zprofile` and Homebrew's `shellenv` apply).
- Before the command runs, the shell **sources** the files in `source:`. The default is your shell's rc file (`~/.zshrc` for zsh, `~/.bashrc` for bash).
- The command is `eval`ed after sourcing, so aliases and functions from those files work.
- Use `source: [...]` to replace the list, for example `[~/.zshrc, .venv/bin/activate]`. Use `source: []`, or `lstack add --no-source`, to skip shell config.
- From the CLI, `-s/--source <file>` adds files. In the app, use the **Shell environment → Source files** field.

**Command splitting.** When you add a script, these leading setup steps are moved out of the command into their own fields:

| Leading step | Moved to |
|---|---|
| `cd <dir> &&` | `cwd` |
| `FOO=bar`, `export FOO=bar &&` | `env` |
| `source <file> &&`, `. <file> &&` | `source` |

```sh
lstack add web -- 'cd ~/code/web && export PORT=3000 && source .env && bun run dev'
# ↳ split out cwd ~/code/web · env PORT · source ~/code/web/.env
#   command: bun run dev
```

- **What isn't split.** Anything that needs runtime expansion (`$VAR`, `$(…)`, globs) is left alone, as is everything after a pipe or subshell. Pass `--no-split` to keep the command exactly as typed.
- **In the app.** The Add Service window shows a **Split Out** suggestion as you type.

**1Password CLI (`op`) and other tools that read app data.** macOS privacy rules apply to scripts launched by the LStack daemon. Without permission, `op` can't read 1Password's settings, so it reports "No accounts configured" and never shows the Touch ID prompt. To fix this, grant **Full Disk Access** to LStack. Click **Grant Access…** in the menu bar banner (or **Settings → Full Disk Access → Grant…**). It opens the matching System Settings pane; turn on **LStack** (if it isn't listed, click + and choose LStack from Applications). The background daemon runs from inside the app bundle, so it's covered by the app's grant; there's no separate helper to add. Until access is granted, LStack leaves the running daemon (even an older version) and your services alone; once it detects the grant, it finishes any pending daemon update and restarts your services automatically. Scripts that were running before a daemon restart are started again by the new daemon. If it stops working after an update, remove the entry and add it again.

### Service YAML

```yaml
# ~/.localstack/services/api.yaml
name: api
type: process            # process | docker | compose | k8s
command: npm run dev
cwd: ~/code/api
env: { PORT: "3000" }
source: [~/.zshrc, .env] # sourced first (default: your shell rc); [] = none
envFiles: [.env]         # op run-compatible: plain values at each start, op:// via `lstack secret import`
secrets: [GITHUB_TOKEN]  # from the secret store
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

- Scripts run in their own process group under `$SHELL -l` (see Shell environment). Stopping sends SIGTERM, then SIGKILL after 8 s.
- When the daemon exits, it stops scripts but **leaves containers running**. On restart it re-adopts them, and it reaps orphaned script process groups if the daemon was killed.
- Quitting the menu bar app leaves the daemon running by default. Turn on **Settings → Stop everything when I quit** to run `lstack daemon stop` on quit, which also ends forwards and the link to other Macs. Containers keep running.
- At login, the daemon waits up to 10 minutes for Docker Desktop before it starts container services.
- **Docker sync.** `lstack docker sync on` checks `docker ps -a` every 5 s: containers (and compose projects, one service each) that appear are registered, and services registered this way (`discovered: true`) are removed once their container is gone for two checks in a row. Services you added yourself are never removed. Nothing is dropped while the Docker engine is unreachable. While sync is on, a service you `rm` for a container that still exists is registered again, so use `lstack docker sync off` first.
- **Machine load.** The top of the menu bar panel shows whole-Mac CPU and memory with ~15 minutes of sparkline history. Each is split into **LStack** (your services) and **Other** (every other app), so you can tell when the machine itself is overloaded. The same data is at `GET /api/system`. Machine CPU is a share of all cores, while per-service CPU is a share of one core, as in Activity Monitor.
- **Crash loops.** Crashed scripts restart with exponential backoff (1 s → 30 s, up to `maxRestarts`). While a script waits to restart, the menu bar shows **RESTART IN Ns** with a red stop button. `lstack ls` and `lstack top` show `restarting Ns`. Stopping it cancels the pending restart, and stop waits for a start that's already in progress.
- **Terminal app.** Dashboard, Attach and Shell open in the app chosen under **Settings → Terminal app** (or `lstack terminal <id>`): Terminal, iTerm, cmux or Ghostty. macOS asks once to allow LStack to control iTerm or Terminal.
- **cmux.** By default, cmux only accepts commands from processes started inside cmux, so the daemon gets "Access denied". When that happens, the menu bar offers **Allow LStack** or **Use Terminal**. You'll find the same switch under **Settings → cmux access**, and in the CLI as `lstack terminal cmux --fix`. Allowing it backs up `~/.config/cmux/cmux.json`, switches `automation.socketControlMode` to `password` with a random password kept in LStack's secret store, and reloads cmux. You can change or undo this anytime in cmux's settings.
- The API binds to 127.0.0.1 only and requires the token in `daemon.json`, sent as `Authorization: Bearer`. Debugging the daemon needs `LSTACK_ALLOW_DEBUGGER=1`.

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

Releases are published at **[github.com/jhicken/localstack/releases](https://github.com/jhicken/localstack/releases)**. Each release includes:

- `LStack-<version>-arm64.dmg`: open it and drag LStack to Applications.
- `LStack-<version>-arm64.zip`
- `LStack-<version>-arm64.sha256`: checksums for both files.

To cut a release, commit your work, then run:

```sh
macos/release.sh                  # patch bump: 0.3.3 → 0.3.4 (or: minor | major | 1.2.3 | current)
macos/release.sh minor --dry-run  # run the checks and show the new version, change nothing
macos/release.sh --draft          # publish the release as a draft
macos/release.sh current --skip-build  # retry a failed upload of what's already in dist/
```

Everything runs on this Mac; GitHub Actions isn't used. The script requires a clean tree that is up to date with origin and the GitHub CLI (`gh auth login`). It runs `cargo test` and `cargo clippy -D warnings`, bumps `version` in `Cargo.toml` and `Cargo.lock`, commits `chore: release vX.Y.Z` and tags it. Then it builds the dmg, zip and checksums with `macos/build.sh`, pushes the branch and tag together, and creates the GitHub release with those files (or replaces the assets if it already exists). If the checks or the build fail, it removes the bump commit and tag, leaving the tree as it was.

- **Building on GitHub instead.** `macos/release.sh --ci` pushes without building and starts the release workflow (`.github/workflows/release.yml`), which builds on a macOS runner. The workflow only runs when started this way or from the Actions tab (for example, to rebuild an existing tag); pushing a tag doesn't trigger it.
- **Code signing.** Builds are ad-hoc signed. The first time you open a downloaded copy, right-click the app and choose **Open**.

See [docs/PLAN.md](docs/PLAN.md) for the roadmap.
