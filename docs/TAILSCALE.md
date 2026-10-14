# Tailscale integration (proposed, not scheduled)

Status: design only. Nothing here is implemented yet.

## Goal

Let paired Macs reach each other from anywhere, reconnect quickly after sleep or a network change, and optionally keep
the link port invisible to everything except your own tailnet. We do this by **using the Tailscale app that's already
installed**, not by embedding Tailscale.

## What works today (no code)

Install Tailscale on both Macs and pair using the other Mac's Tailscale name:

```sh
lstack machine pair studio.tail1234.ts.net --code XXXX-XXXX
```

The Noise link (port 7381) then runs inside Tailscale's WireGuard tunnel. The existing reconnect loop handles drops.
Retries back off from 2 s up to 60 s, and a dead link is detected after 45 s.

Gaps this leaves:
- A Mac has exactly one address. Pairing over the LAN breaks when you leave home, and pairing over Tailscale means
  traffic goes through Tailscale even at home.
- After sleep or a network change, reconnecting can take up to about a minute.
- You have to look up and type the Tailscale name yourself.
- The link port listens on every interface (LAN included) whenever sharing is on.

## Options considered

| Option | Verdict |
|---|---|
| 1. Documentation only (pair with the Tailscale name) | Works today; leaves the gaps above |
| 2. **Built-in awareness of the installed Tailscale** | **Chosen**: small, no new dependencies, fixes the gaps |
| 3. Embed Tailscale (`libtailscale`/tsnet via C bindings) | Rejected for now: adds ~20 MB, a Go runtime and a second Tailscale login. Only worth it if users can't install the Tailscale app. |

## Design (option 2)

### 1. Detect Tailscale (`src/daemon/tailscale.rs`, new)
- Find the CLI at `/Applications/Tailscale.app/Contents/MacOS/Tailscale`, then `tailscale` on `PATH`. Run
  `status --json` with a 3 s timeout, cache the result for 30 s, and refresh it on network change (see §5).
- Parse: `BackendState` (Running?), `Self.DNSName`, `Self.TailscaleIPs`, `Self.UserID`, `MagicDNSSuffix`, and
  `Peer{}` (HostName, DNSName, TailscaleIPs, Online, OS, UserID).
- Fall back to interface scanning: any 100.64.0.0/10 or fd7a:115c:a1e0::/48 address on a `utun*` interface means
  Tailscale is up, even if the CLI can't be found (App Store build sandboxing).
- `GET /api/tailscale` → `{installed, running, name, ips, user, peers: [{name, dnsName, ips, online, os, lstack?}]}`.
  This is local only (never served over the link), and peers are listed only when sharing or pairing UI asks.

### 2. Better addresses when sharing
- `shareUrls` (remote.rs ~574) lists the Tailscale DNS name and its 100.x address first, labelled "Tailscale",
  then the LAN addresses. The pairing code screen shows the same.
- `lstack remote status` / `remote pair` print them with labels.

### 3. Find Macs to pair with
- **Add machine** (Settings → Machines, `lstack machine pair` with no address) lists online tailnet peers with
  `OS == "macOS"`.
- A cheap probe marks the ones running LStack. It's a TCP connect to `:7381` that reads the Noise listener's banner
  frame (or just checks that the connect succeeds), with a 1 s timeout, run in parallel and capped at 32 peers.
- Picking one fills in the address; the code, login and 2FA steps stay exactly as they are.

### 4. Several addresses per Mac
- `PeerCfg.addr` becomes `addrs: Vec<String>`. Old configs load `addr` as a one-element list.
- At pairing, the shared Mac sends its known addresses inside the encrypted pairing session (LAN IPs, `.local` name,
  Tailscale name and IPs). The host stores them, putting the one it actually used first.
- The shared Mac can refresh them over the link (`info` already round-trips; add `addresses`) so a DHCP change on
  the LAN is picked up the next time the link is up.
- To dial, try the addresses in order with a short connect timeout (LAN first, 1.5 s), or race them happy-eyeballs
  style with ~250 ms staggering. The first Noise handshake that completes wins. Because the pinned key is checked on
  every address, a wrong or hijacked address simply fails.
- `lstack machine ls` and Settings show which path is in use: "LAN" / "Tailscale" / "Other".
- `lstack machine addr <m> add|rm|ls <address>` for manual edits.

### 5. Reconnect quickly
- Redial every peer right away (resetting the backoff) when any of these happens:
  - wake from sleep: the app forwards `NSWorkspace.didWakeNotification` → `POST /api/machines/reconnect`; the
    daemon also checks for a wall-clock jump (> 2× the tick) as a fallback;
  - a network path change: the app uses `NWPathMonitor` and posts the same endpoint; the daemon fallback is a 10 s
    poll of the interface list;
  - Tailscale going from not running to running.
- `Peers::reconnect_all` already exists; it only needs a "skip the backoff sleep" signal (a condvar or generation
  bump instead of `sleep(stop, n)`).
- Expected result: back up within seconds of wake or a Wi-Fi switch.

### 6. "Tailscale only" option for the link port
- `remote.json` gets `linkBind: "all" | "tailscale" | "loopback"` (default `all`).
- `tailscale`: bind the link listener to the Tailscale IPs only (v4 + v6). If Tailscale isn't up, the listener
  stays down with a clear status ("waiting for Tailscale") and retries when it comes up (§5).
- Settings → Share this Mac: "Reachable from: Any network / Tailscale only". In the CLI:
  `lstack remote share on --via tailscale`.
- This is also where the item "optional Tailscale-only bind" from Phase 5 for the web dashboard could share code.

### 7. Optional tailnet identity check (defense in depth)
- When a link connects from a 100.x address, ask Tailscale who owns that address (LocalAPI
  `whois?addr=ip:port`; for the CLI, `tailscale whois --json ip:port`).
- New grant setting per paired Mac: **"Only over my tailnet"**. The link is refused unless it arrives over Tailscale
  *and* from the same tailnet user as this Mac (or a specific user recorded at pairing).
- This sits on top of the Noise keys and doesn't replace them. Refusals are audited like other refusals.

## Security notes
- Noise IK keys stay the only thing that authorizes a link. Tailscale adds reachability, an optional bind
  restriction and an optional extra identity check; it never loosens anything.
- Address lists come either from inside the encrypted pairing session or over the authenticated link, never from
  unauthenticated sources. Even so, a bad address can only cause a failed handshake.
- The probe in §3 sends no secrets and learns only "something answers on 7381".
- `/api/tailscale` is served only on the local daemon API, never to paired Macs or the web dashboard.

## Surfaces
| Where | Change |
|---|---|
| Settings → Machines | Tailnet Macs to pick from; the path in use per Mac; "Only over my tailnet" checkbox per paired Mac |
| Settings → Share this Mac | Tailscale address first; "Reachable from" picker; "waiting for Tailscale" state |
| CLI | `machine pair` (interactive picker), `machine addr`, `machine ls` path column, `remote share on --via tailscale` |
| Menu bar | Machine header tooltip shows the path (LAN / Tailscale) |
| Web / TUI | Path shown in the machine header (read only) |

## Work items
1. `tailscale.rs`: detection, status parsing, cache, `/api/tailscale`; interface-scan fallback.
2. Show Tailscale names and IPs in `shareUrls` and on the pairing screen.
3. Multiple addresses per Mac: config migration, exchange at pairing and over `info`, ordered/raced dialing,
   showing the path in use, `machine addr`.
4. Quick reconnect: backoff interrupt, `POST /api/machines/reconnect`, app wake/`NWPathMonitor` hooks, daemon
   fallbacks.
5. Discovery: list tailnet Macs with an LStack probe; the add-machine UI and CLI picker.
6. `linkBind` (all / tailscale / loopback) with a waiting state and rebind on change.
7. Optional tailnet whois check as a grant setting.
8. Tests: status JSON fixtures, address-order dialing (one dead address, one live), backoff interrupt,
   `linkBind` selection, migration from `addr`. Docs: README "Other machines".

Order: 1 → 2, 1 → 5, 1 → 6, 1 → 7; 3 and 4 are independent and give most of the benefit even without Tailscale.

## Open questions
- Should `linkBind` default to `tailscale` when Tailscale is detected at the moment sharing is turned on?
- Should the "Only over my tailnet" check be on by default for Macs paired over Tailscale?
- Is it worth supporting Headscale (self-hosted control servers)? The status JSON is the same, so probably free.
