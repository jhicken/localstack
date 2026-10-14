# Link transports: Local and Tailscale

Each paired Mac connects over one **transport**:

| Transport | Path | Who can reach the link port | Default |
|---|---|---|---|
| **Local** | Noise link over the LAN | Devices on the same local network | ✓ |
| **Tailscale** | The same Noise link, inside Tailscale's WireGuard tunnel | Devices in your tailnet, from anywhere | opt-in |

Both transports share one pairing: the same code exchange, pinned Noise keys, grants and mutual revoke. Switching
never needs a new pairing code. Tailscale only changes how the two Macs reach each other. The link inside is still
end-to-end encrypted and authenticated with each Mac's own key, so Tailscale never sees the plaintext.

Nothing is downloaded and nothing reaches the tailnet until you turn Tailscale on.

## The helper (`lstack-tailnet`)

LStack ships its own small Tailscale node, built on Tailscale's [`tsnet`](https://tailscale.com/kb/1244/tsnet)
library (source in [`tailnet/`](../tailnet)):

- **Userspace.** It needs no root and adds no system VPN, `utun` device or routes. It doesn't conflict with the
  Tailscale app or another VPN, and it's used even when the Tailscale app is installed.
- **Its own device.** It joins your tailnet as `lstack-<LocalHostName>`. Its state lives in `~/.localstack/tailnet/`.
  Sign in once, in the browser or with an auth key.
- **Run by the daemon.** The daemon starts and supervises it, and it exits when the daemon goes away.
- **Two local sockets.** The daemon talks to it over Unix sockets in a `0700` directory, and both ends check the
  peer's uid:
  - `ctl.sock`: status, login/logout, and `CONNECT host:port`, which gives a raw stream to a tailnet peer.
  - `link.sock`: each tailnet connection to `:7381`, preceded by one JSON line `{src, node, user, tailnet}`. The
    helper fills that line in with Tailscale's whois for the caller.

### Install and updates

- **Download on demand.** When you turn Tailscale on, the daemon downloads the helper from the latest GitHub release
  (`tailnet-manifest.json` + `.sig` + `lstack-tailnet-<ver>-darwin-universal.tar.gz`, about 25 MB).
- **Strict checks.** Before installing, it checks all of these:
  - The manifest's ed25519 signature, against the public key compiled into lstack.
  - The tarball's SHA-256 against the manifest.
  - `codesign --verify` on the binary.
  - That the binary reports the manifest's version.
- **Refusals.** It never installs an older version than the current one, or one that needs a newer LStack
  (`minLStack`).
- **Layout.** Versions live in `~/.localstack/bin/lstack-tailnet-<ver>`, with a `current` symlink. The previous
  version is kept.
- **Daily updates.** It checks once a day ("Update automatically", on by default), or when you run
  `lstack tailnet update`. After updating, it restarts the helper; the node identity is kept.
- **Paired Macs follow.** After an update it asks each Mac on the Tailscale transport to update to at least the same
  version, waits up to 2 minutes for each, then reconnects. A Mac that can't update stays connected on its old helper,
  which is compatible, and shows a warning.

## Switching transport

On the watching Mac, use **Settings → Machines → Use Tailscale / Use Local Network**, or
`lstack machine transport <m> tailscale|local`.

**Upgrade (Local → Tailscale)**
1. Tailscale must be on and signed in on this Mac. Turning it on the first time shows the exposure warning (below).
2. The other Mac is asked to *prepare*: it turns its helper on and installs or updates it. If it isn't signed in
   yet, it shows a notification, and this Mac shows "waiting for <m> to sign in". Signing in is the one step that
   always needs a person on that Mac (or an auth key).
3. Both Macs must be on the same tailnet; otherwise the switch stops and explains why.
4. The other Mac is asked to switch, then a test link is made over the tailnet. If that fails, the other Mac is moved
   back to Local and nothing changes. Only after it succeeds does the link move to the tailnet.

**Downgrade (Tailscale → Local)**
1. First a Noise handshake is tried against each LAN address (2 s each).
2. If none answers: "<m> doesn't answer on this network — Keep Tailscale / Switch anyway". The CLI asks the same
   question; `--force` skips it.
3. The other Mac is asked to switch, and the link redials over the LAN.

**From the shared Mac.** On the shared Mac, "Local Only" next to a paired Mac (or `lstack remote transport <peer>
local`) moves it back to the LAN. The link is closed with a reason, and the watching Mac follows on its own.

**Approval.** The shared Mac applies upgrade and downgrade requests automatically and posts a notification. This is
"Allow paired Macs to switch between LAN and Tailscale without asking", on by default
(`lstack remote auto-approve on|off`). With it off, requests wait under Share this Mac (or `lstack remote requests`)
until you choose Allow or Deny. The "prepare" request (turning Tailscale on there) also needs auto-approve.

**Service links.** Services are reached through forwards on `localhost`, which work the same over Tailscale. While
a machine is on Tailscale, links to its LAN address (e.g. `http://studio.lan:3000`, for a service that isn't
forwarded) are hidden, since that name may not resolve away from home. A link whose forward is paused shows a pause
icon; clicking it resumes the forward and opens the local URL.

## What the shared Mac accepts

The shared Mac checks every link it accepts against that peer's transport:

| Arrives via | Local peer | Tailscale peer |
|---|---|---|
| TCP from this Mac's own LAN subnet, loopback, or IPv6 link-local | ✓ | ✓ (fallback while Tailscale reconnects) |
| TCP from anywhere else: a VPN subnet (even 10.x), 100.64/10, Tailscale's ULA | ✗ | ✗ |
| `link.sock` (the helper) | ✗ | ✓ if from the tailnet recorded when it switched |

"LAN" means the subnet of a physical interface (`en*`). Being RFC 1918 isn't enough, because a corporate VPN's 10.x
network must not count as local. Refusals are audited (Share this Mac → Activity) and the dialing Mac is told why.

## Staying connected

- The app asks the daemon to redial machines that are down right away after wake, a network change, or a VPN going
  on or off. The daemon also notices sleep by itself (a wall-clock jump), and redials when its helper comes up.
- Machines on the Tailscale transport try the tailnet first, then their LAN addresses.
- Tailscale picks a direct path whenever it can, including on the same LAN. The machine row shows
  "Tailscale · direct" or "relayed (DERP <region>)".

## VPNs

The helper uses no OS routes, so it doesn't fight other VPNs for the routing table.

| VPN type | Tailscale transport | Local transport |
|---|---|---|
| Split tunnel (company subnets only) | No effect | No effect |
| Full tunnel, LAN access allowed | Stays direct after a short re-probe | Works |
| Full tunnel, LAN blocked | Falls back to a DERP relay over HTTPS through the VPN (slower, still works) | Breaks until the VPN is off |
| Blocks Tailscale (UDP + DERP, or TLS inspection) | Offline; the helper's health warning is shown | Breaks if the LAN is blocked |

LStack detects an active VPN from the default route (`GET /api/network`). When a Local machine is offline, it says
the VPN may be blocking the local network. When a Tailscale link is relayed, it says that the VPN is why.

## Security

- **A wider door.** Turning Tailscale on makes this Mac's link port reachable from your whole tailnet, over the
  internet. The link is still Noise-encrypted and only accepts paired keys, and each peer is checked against its
  transport and tailnet. You confirm this the first time you turn Tailscale on.
- **Signing key custody.** Only manifests signed with LStack's ed25519 key are installed:
  - The private key lives in the release Mac's login keychain (`dev.lstack.tailnet-signing`) and in the
    `TAILNET_SIGNING_KEY` GitHub Actions secret. To set the secret, run
    `security find-generic-password -s dev.lstack.tailnet-signing -w | gh secret set TAILNET_SIGNING_KEY`.
  - The public key is `PUBLIC_KEY_HEX` in `src/daemon/tailnet.rs`. Rotating it needs an LStack release that ships
    the new public key.
- **Tailnet ACLs.** Helpers are subject to your tailnet's ACLs. The default policy allows everything. With a custom
  policy, allow your own devices to reach `lstack-*` devices on TCP `7381`. For example:
  ```json
  {"action": "accept", "src": ["autogroup:member"], "dst": ["autogroup:self:7381"]}
  ```
- **Isolated surfaces.** Auth URLs and Tailscale status are served only on the local API: never over the link and
  never to the web dashboard.

## CLI

```bash
lstack tailnet on | off | status | update
lstack tailnet login [--auth-key tskey-…]     # opens the sign-in page and waits
lstack tailnet logout
lstack machine transport studio tailscale     # or local [--force] [--yes]
lstack machine ls                             # TRANSPORT column
lstack remote auto-approve on|off
lstack remote requests | allow <peer> | deny <peer>
```

## Development and tests

- `LSTACK_TAILNET_BIN=<path>` runs that binary instead of installing one, and disables updates.
  `LSTACK_TAILNET_HOSTNAME` overrides the device name.
- `tests/fake_tailnet.py` stands in for the helper. It's a fake tailnet between daemons on one machine, used by the
  `tailscale_transport` end-to-end test, which covers upgrade with sign-in, following the shared Mac, approval and
  unpairing over Tailscale.
- `cd tailnet && go test ./...` tests the helper's header and CONNECT proxy.
