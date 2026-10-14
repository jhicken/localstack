// lstack-tailnet is LStack's Tailscale helper: a userspace tailnet node (tsnet) that the lstack daemon spawns
// when the Tailscale transport is on. It adds no system VPN, routes or utun device.
//
// It talks to the daemon over two unix sockets in --dir (a 0700 directory, peer uid checked):
//   - ctl.sock (served here): GET /status, POST /login {authKey?}, POST /logout, GET /whois?addr=, and
//     CONNECT host:port for a raw stream to a tailnet address.
//   - link.sock (served by the daemon): each inbound tailnet connection to --port is forwarded there after a
//     one-line JSON header naming who it came from. LStack's own Noise link runs inside, so this helper never
//     sees plaintext.
//
// It exits when its stdin closes, so it never outlives the daemon.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"runtime/debug"
	"sort"
	"strings"
	"sync"
	"time"

	"golang.org/x/sys/unix"
	"tailscale.com/client/local"
	"tailscale.com/ipn"
	"tailscale.com/tsnet"
)

// version is set at build time (-ldflags "-X main.version=..."); it follows the tsnet release it was built on.
var version = "dev"

func main() {
	dir := flag.String("dir", "", "state and socket directory (default ~/.localstack/tailnet)")
	hostname := flag.String("hostname", "", "tailnet device name (default lstack-<hostname>)")
	port := flag.Int("port", 7381, "tailnet port to forward to link.sock")
	watchdog := flag.Bool("watchdog", true, "exit when stdin closes")
	verbose := flag.Bool("verbose", false, "log tsnet internals to stderr")
	flag.Parse()
	if flag.Arg(0) == "version" {
		fmt.Println(version)
		return
	}
	if *dir == "" {
		home, _ := os.UserHomeDir()
		*dir = filepath.Join(home, ".localstack", "tailnet")
	}
	if *hostname == "" {
		h, _ := os.Hostname()
		*hostname = "lstack-" + strings.ToLower(strings.Split(h, ".")[0])
	}
	if err := os.MkdirAll(filepath.Join(*dir, "state"), 0o700); err != nil {
		fatal("cannot create %s: %v", *dir, err)
	}
	_ = os.Chmod(*dir, 0o700)
	if *watchdog {
		go func() {
			_, _ = io.Copy(io.Discard, os.Stdin)
			logLine("exit", map[string]any{"reason": "parent gone"})
			os.Exit(0)
		}()
	}

	srv := &tsnet.Server{
		Dir:      filepath.Join(*dir, "state"),
		Hostname: *hostname,
		AuthKey:  os.Getenv("TS_AUTHKEY"),
		Logf:     func(string, ...any) {},
		UserLogf: func(f string, a ...any) { logLine("tsnet", map[string]any{"msg": fmt.Sprintf(f, a...)}) },
	}
	if *verbose {
		srv.Logf = func(f string, a ...any) { fmt.Fprintf(os.Stderr, f+"\n", a...) }
	}
	if err := srv.Start(); err != nil {
		fatal("tsnet start: %v", err)
	}
	lc, err := srv.LocalClient()
	if err != nil {
		fatal("tsnet local client: %v", err)
	}
	node := &tsNode{srv: srv, lc: lc}

	ln, err := srv.Listen("tcp", fmt.Sprintf(":%d", *port))
	if err != nil {
		fatal("listen on tailnet :%d: %v", *port, err)
	}
	linkSock := filepath.Join(*dir, "link.sock")
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				time.Sleep(200 * time.Millisecond)
				continue
			}
			go forwardInbound(c, node, linkSock)
		}
	}()

	ctlPath := filepath.Join(*dir, "ctl.sock")
	_ = os.Remove(ctlPath)
	ul, err := net.Listen("unix", ctlPath)
	if err != nil {
		fatal("listen %s: %v", ctlPath, err)
	}
	_ = os.Chmod(ctlPath, 0o600)
	logLine("ready", map[string]any{"version": version, "hostname": *hostname, "ctl": ctlPath})
	hs := &http.Server{Handler: controlHandler(node), ReadHeaderTimeout: 10 * time.Second}
	if err := hs.Serve(sameUIDListener{ul}); err != nil {
		fatal("control: %v", err)
	}
}

// node is what the helper needs from tsnet (a fake stands in for it in tests).
type node interface {
	Dial(ctx context.Context, network, addr string) (net.Conn, error)
	WhoIs(ctx context.Context, addr string) (Identity, error)
	Status(ctx context.Context) (Status, error)
	Login(ctx context.Context, authKey string) error
	Logout(ctx context.Context) error
}

// Identity is who a tailnet connection came from. It's the header sent ahead of each link.sock connection.
type Identity struct {
	Src     string `json:"src"`
	Node    string `json:"node"`
	User    string `json:"user"`
	Tailnet string `json:"tailnet"`
}

type Status struct {
	State   string   `json:"state"`
	AuthURL string   `json:"authURL,omitempty"`
	Self    *Self    `json:"self,omitempty"`
	Tailnet string   `json:"tailnet,omitempty"`
	User    string   `json:"user,omitempty"`
	Version string   `json:"version"`
	Tsnet   string   `json:"tsnet"`
	Health  []string `json:"health"`
	Peers   []Peer   `json:"peers"`
}

type Self struct {
	DNSName string   `json:"dnsName"`
	IPs     []string `json:"ips"`
}

type Peer struct {
	DNSName  string   `json:"dnsName"`
	HostName string   `json:"hostName"`
	IPs      []string `json:"ips"`
	Online   bool     `json:"online"`
	OS       string   `json:"os"`
	IsLStack bool     `json:"isLStack"`
	Path     string   `json:"path"` // "direct", "relay", or "" (no traffic yet)
	Relay    string   `json:"relay,omitempty"`
	Endpoint string   `json:"endpoint,omitempty"`
}

type tsNode struct {
	srv *tsnet.Server
	lc  *local.Client
}

func (n *tsNode) Dial(ctx context.Context, network, addr string) (net.Conn, error) {
	return n.srv.Dial(ctx, network, addr)
}

func (n *tsNode) WhoIs(ctx context.Context, addr string) (Identity, error) {
	w, err := n.lc.WhoIs(ctx, addr)
	if err != nil {
		return Identity{}, err
	}
	id := Identity{Src: addr}
	if w.Node != nil {
		id.Node = strings.TrimSuffix(w.Node.Name, ".")
		id.Tailnet = tailnetOf(id.Node)
	}
	if w.UserProfile != nil {
		id.User = w.UserProfile.LoginName
	}
	return id, nil
}

// tailnetOf is the tailnet part of a MagicDNS name: "studio.tail1234.ts.net" → "tail1234.ts.net".
func tailnetOf(dnsName string) string {
	if i := strings.IndexByte(dnsName, '.'); i >= 0 {
		return dnsName[i+1:]
	}
	return ""
}

func (n *tsNode) Status(ctx context.Context) (Status, error) {
	st, err := n.lc.Status(ctx)
	if err != nil {
		return Status{}, err
	}
	out := Status{State: st.BackendState, AuthURL: st.AuthURL, Version: version, Tsnet: tsnetVersion(), Health: st.Health, Peers: []Peer{}}
	if out.Health == nil {
		out.Health = []string{}
	}
	if st.Self != nil && st.BackendState == ipn.Running.String() {
		out.Self = &Self{DNSName: strings.TrimSuffix(st.Self.DNSName, "."), IPs: ipStrings(st.Self.TailscaleIPs)}
		if u, ok := st.User[st.Self.UserID]; ok {
			out.User = u.LoginName
		}
	}
	if st.CurrentTailnet != nil {
		out.Tailnet = st.CurrentTailnet.Name
	}
	for _, p := range st.Peer {
		dns := strings.TrimSuffix(p.DNSName, ".")
		peer := Peer{DNSName: dns, HostName: p.HostName, IPs: ipStrings(p.TailscaleIPs), Online: p.Online, OS: p.OS,
			IsLStack: strings.HasPrefix(dns, "lstack-") || strings.HasPrefix(p.HostName, "lstack-"), Relay: p.Relay}
		switch {
		case p.CurAddr != "":
			peer.Path, peer.Endpoint = "direct", p.CurAddr
		case p.Relay != "" && !p.LastWrite.IsZero():
			peer.Path = "relay"
		}
		out.Peers = append(out.Peers, peer)
	}
	sort.Slice(out.Peers, func(i, j int) bool { return out.Peers[i].DNSName < out.Peers[j].DNSName })
	return out, nil
}

func (n *tsNode) Login(ctx context.Context, authKey string) error {
	if authKey != "" {
		if err := n.lc.Start(ctx, ipn.Options{AuthKey: authKey}); err != nil {
			return err
		}
	}
	return n.lc.StartLoginInteractive(ctx)
}

func (n *tsNode) Logout(ctx context.Context) error { return n.lc.Logout(ctx) }

func ipStrings[T fmt.Stringer](ips []T) []string {
	out := make([]string, 0, len(ips))
	for _, ip := range ips {
		out = append(out, ip.String())
	}
	return out
}

func tsnetVersion() string {
	if bi, ok := debug.ReadBuildInfo(); ok {
		for _, d := range bi.Deps {
			if d.Path == "tailscale.com" {
				return d.Version
			}
		}
	}
	return "unknown"
}

// forwardInbound hands one tailnet connection to the daemon, preceded by who it's from.
func forwardInbound(c net.Conn, n node, linkSock string) {
	defer c.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	id, err := n.WhoIs(ctx, c.RemoteAddr().String())
	cancel()
	if err != nil {
		logLine("inbound-refused", map[string]any{"src": c.RemoteAddr().String(), "error": err.Error()})
		return
	}
	d, err := net.DialTimeout("unix", linkSock, 5*time.Second)
	if err != nil {
		logLine("inbound-refused", map[string]any{"src": id.Src, "error": "daemon not listening: " + err.Error()})
		return
	}
	defer d.Close()
	hdr, _ := json.Marshal(id)
	if _, err := d.Write(append(hdr, '\n')); err != nil {
		return
	}
	pipe(c, d)
}

// pipe copies both ways until either side is done, then closes both.
func pipe(a, b net.Conn) {
	var wg sync.WaitGroup
	wg.Add(2)
	cp := func(dst, src net.Conn) {
		defer wg.Done()
		_, _ = io.Copy(dst, src)
		if cw, ok := dst.(interface{ CloseWrite() error }); ok {
			_ = cw.CloseWrite()
		} else {
			_ = dst.Close()
		}
	}
	go cp(a, b)
	go cp(b, a)
	wg.Wait()
}

func controlHandler(n node) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /status", func(w http.ResponseWriter, r *http.Request) {
		st, err := n.Status(r.Context())
		if err != nil {
			httpErr(w, 503, err)
			return
		}
		writeJSON(w, st)
	})
	mux.HandleFunc("POST /login", func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			AuthKey string `json:"authKey"`
		}
		_ = json.NewDecoder(io.LimitReader(r.Body, 4096)).Decode(&body)
		if err := n.Login(r.Context(), strings.TrimSpace(body.AuthKey)); err != nil {
			httpErr(w, 400, err)
			return
		}
		writeJSON(w, map[string]any{"ok": true})
	})
	mux.HandleFunc("POST /logout", func(w http.ResponseWriter, r *http.Request) {
		if err := n.Logout(r.Context()); err != nil {
			httpErr(w, 500, err)
			return
		}
		writeJSON(w, map[string]any{"ok": true})
	})
	mux.HandleFunc("GET /whois", func(w http.ResponseWriter, r *http.Request) {
		id, err := n.WhoIs(r.Context(), r.URL.Query().Get("addr"))
		if err != nil {
			httpErr(w, 404, err)
			return
		}
		writeJSON(w, id)
	})
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodConnect {
			connect(w, r, n)
			return
		}
		mux.ServeHTTP(w, r)
	})
}

// connect answers `CONNECT host:port` with a raw stream to that tailnet address.
func connect(w http.ResponseWriter, r *http.Request, n node) {
	target := r.Host
	if _, _, err := net.SplitHostPort(target); err != nil {
		httpErr(w, 400, fmt.Errorf("CONNECT needs host:port, got %q", target))
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	up, err := n.Dial(ctx, "tcp", target)
	cancel()
	if err != nil {
		httpErr(w, 502, fmt.Errorf("cannot reach %s over the tailnet: %w", target, err))
		return
	}
	hj, ok := w.(http.Hijacker)
	if !ok {
		up.Close()
		httpErr(w, 500, errors.New("cannot hijack"))
		return
	}
	c, buf, err := hj.Hijack()
	if err != nil {
		up.Close()
		return
	}
	if _, err := c.Write([]byte("HTTP/1.1 200 OK\r\n\r\n")); err != nil {
		c.Close()
		up.Close()
		return
	}
	// Bytes the client sent right after its request are already buffered.
	if n := buf.Reader.Buffered(); n > 0 {
		b, _ := buf.Reader.Peek(n)
		if _, err := up.Write(b); err != nil {
			c.Close()
			up.Close()
			return
		}
	}
	pipe(c, up)
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func httpErr(w http.ResponseWriter, code int, err error) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(map[string]string{"error": err.Error()})
}

// sameUIDListener only hands out connections from processes running as this user.
type sameUIDListener struct{ net.Listener }

func (l sameUIDListener) Accept() (net.Conn, error) {
	for {
		c, err := l.Listener.Accept()
		if err != nil {
			return nil, err
		}
		if sameUID(c) {
			return c, nil
		}
		c.Close()
	}
}

func sameUID(c net.Conn) bool {
	uc, ok := c.(*net.UnixConn)
	if !ok {
		return false
	}
	raw, err := uc.SyscallConn()
	if err != nil {
		return false
	}
	match := false
	_ = raw.Control(func(fd uintptr) {
		cred, err := unix.GetsockoptXucred(int(fd), unix.SOL_LOCAL, unix.LOCAL_PEERCRED)
		match = err == nil && int(cred.Uid) == os.Getuid()
	})
	return match
}

var logMu sync.Mutex

// logLine writes one JSON line to stderr (the daemon keeps them in its log).
func logLine(event string, fields map[string]any) {
	fields["event"] = event
	fields["at"] = time.Now().UTC().Format(time.RFC3339)
	b, _ := json.Marshal(fields)
	logMu.Lock()
	defer logMu.Unlock()
	w := bufio.NewWriter(os.Stderr)
	_, _ = w.Write(append(b, '\n'))
	_ = w.Flush()
}

func fatal(f string, a ...any) {
	logLine("fatal", map[string]any{"error": fmt.Sprintf(f, a...)})
	os.Exit(1)
}
