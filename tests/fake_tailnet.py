#!/usr/bin/env python3
"""Stand-in for the lstack-tailnet helper in tests (LSTACK_TAILNET_BIN): a fake tailnet between helpers on
this machine. Each helper registers its --dir in $FAKE_TAILNET_REGISTRY/<hostname>; `CONNECT
<hostname>.fake.ts.net:<port>` is delivered to that helper's daemon (its link.sock) with the identity header,
the way the real helper forwards tailnet connections. Same control API on ctl.sock as the real helper."""
import json
import os
import socket
import sys
import threading

VERSION = os.environ.get("FAKE_TAILNET_VERSION", "0.0.1-fake")
SUFFIX = "fake.ts.net"
USER = "tester@example.com"

if sys.argv[1:2] == ["version"]:
    print(VERSION)
    sys.exit(0)

args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
DIR = args["--dir"]
HOST = args["--hostname"]
REG = os.environ["FAKE_TAILNET_REGISTRY"]
LOGGED = os.path.join(DIR, "fake-loggedin")
os.makedirs(REG, exist_ok=True)
if os.environ.get("FAKE_TAILNET_AUTOLOGIN") == "1":
    open(LOGGED, "w").close()
with open(os.path.join(REG, HOST), "w") as f:
    f.write(DIR)


def ip_of(h):
    return "100.100.0.%d" % (sum(h.encode()) % 250 + 1)


def running(d=DIR):
    return os.path.exists(os.path.join(d, "fake-loggedin"))


def status():
    up = running()
    peers = []
    for h in sorted(os.listdir(REG)) if up else []:
        if h != HOST:
            peers.append({"dnsName": f"{h}.{SUFFIX}", "hostName": h, "ips": [ip_of(h)], "online": True, "os": "macOS",
                          "isLStack": h.startswith("lstack-"), "path": "direct"})
    return {
        "state": "Running" if up else "NeedsLogin",
        "authURL": "" if up else "https://login.tailscale.com/a/fake",
        "self": {"dnsName": f"{HOST}.{SUFFIX}", "ips": [ip_of(HOST)]} if up else None,
        # Like the real helper: status shows the tailnet's display name; connection headers carry its DNS domain.
        "tailnet": USER if up else "",
        "user": USER if up else "",
        "version": VERSION,
        "tsnet": "fake",
        "health": [],
        "peers": peers,
    }


def read_head(c):
    head = b""
    while not head.endswith(b"\r\n\r\n"):
        b = c.recv(1)
        if not b:
            return None
        head += b
    return head.decode()


def reply(c, code, obj):
    body = json.dumps(obj).encode()
    c.sendall(b"HTTP/1.1 %d X\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n"
              % (code, len(body)) + body)
    c.close()


def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    for s in (a, b):
        try:
            s.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def connect(c, target):
    host = target.rsplit(":", 1)[0]
    name = host[: -len(SUFFIX) - 1] if host.endswith("." + SUFFIX) else next(
        (h for h in os.listdir(REG) if ip_of(h) == host), host)
    path = os.path.join(REG, name)
    if not running() or not os.path.exists(path):
        return reply(c, 502, {"error": f"no route to {target}"})
    their = open(path).read()
    up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        if not running(their):
            raise OSError("offline")
        up.connect(os.path.join(their, "link.sock"))
    except OSError:
        return reply(c, 502, {"error": f"{target} is offline"})
    up.sendall((json.dumps({"src": f"{ip_of(HOST)}:41641", "node": f"{HOST}.{SUFFIX}", "user": USER, "tailnet": SUFFIX}) + "\n").encode())
    c.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
    threading.Thread(target=pipe, args=(c, up), daemon=True).start()
    pipe(up, c)


def handle(c):
    head = read_head(c)
    if not head:
        return c.close()
    lines = head.split("\r\n")
    method, path = lines[0].split(" ")[:2]
    hdrs = {k.strip().lower(): v.strip() for k, v in (l.split(":", 1) for l in lines[1:] if ":" in l)}
    body = b""
    n = int(hdrs.get("content-length", "0"))
    while len(body) < n:
        body += c.recv(n - len(body))
    if method == "CONNECT":
        return connect(c, path)
    if path == "/status":
        return reply(c, 200, status())
    if path == "/login":
        open(LOGGED, "w").close()
        return reply(c, 200, {})
    if path == "/logout":
        if running():
            os.remove(LOGGED)
        return reply(c, 200, {})
    reply(c, 404, {"error": "not found"})


ctl = os.path.join(DIR, "ctl.sock")
if os.path.exists(ctl):
    os.remove(ctl)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(ctl)
os.chmod(ctl, 0o600)
srv.listen(16)
print(json.dumps({"event": "ready", "version": VERSION}), file=sys.stderr, flush=True)


def watchdog():
    sys.stdin.read()
    os._exit(0)


threading.Thread(target=watchdog, daemon=True).start()
while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
