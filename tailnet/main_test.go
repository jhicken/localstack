package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// fakeNode "dials the tailnet" by dialing loopback.
type fakeNode struct{ id Identity }

func (f *fakeNode) Dial(ctx context.Context, network, addr string) (net.Conn, error) {
	var d net.Dialer
	return d.DialContext(ctx, network, addr)
}
func (f *fakeNode) WhoIs(_ context.Context, addr string) (Identity, error) {
	if f.id.Node == "" {
		return Identity{}, errors.New("unknown peer")
	}
	id := f.id
	id.Src = addr
	return id, nil
}
func (f *fakeNode) Status(context.Context) (Status, error) {
	return Status{State: "Running", Version: version, Health: []string{}, Peers: []Peer{}}, nil
}
func (f *fakeNode) Login(context.Context, string) error { return nil }
func (f *fakeNode) Logout(context.Context) error        { return nil }

func shortTemp(t *testing.T) string {
	// unix socket paths must stay under ~104 bytes, which t.TempDir() can exceed on macOS.
	d, err := os.MkdirTemp("/tmp", "lstn")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(d) })
	return d
}

func echoServer(t *testing.T) string {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go func() { defer c.Close(); _, _ = io.Copy(c, c) }()
		}
	}()
	return ln.Addr().String()
}

func serveCtl(t *testing.T, n node) string {
	sock := filepath.Join(shortTemp(t), "ctl.sock")
	ul, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	hs := &http.Server{Handler: controlHandler(n)}
	go hs.Serve(sameUIDListener{ul})
	t.Cleanup(func() { hs.Close() })
	return sock
}

func TestConnectProxiesAStream(t *testing.T) {
	target := echoServer(t)
	sock := serveCtl(t, &fakeNode{})
	c, err := net.Dial("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	// Send the first payload together with the request, as the daemon may.
	if _, err := io.WriteString(c, "CONNECT "+target+" HTTP/1.1\r\nHost: "+target+"\r\n\r\nhello "); err != nil {
		t.Fatal(err)
	}
	br := bufio.NewReader(c)
	resp, err := http.ReadResponse(br, nil)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != 200 {
		t.Fatalf("status %d", resp.StatusCode)
	}
	io.WriteString(c, "tailnet")
	got := make([]byte, len("hello tailnet"))
	c.SetReadDeadline(time.Now().Add(3 * time.Second))
	if _, err := io.ReadFull(br, got); err != nil {
		t.Fatal(err)
	}
	if string(got) != "hello tailnet" {
		t.Fatalf("got %q", got)
	}
}

func TestConnectReportsUnreachable(t *testing.T) {
	ln, _ := net.Listen("tcp", "127.0.0.1:0")
	dead := ln.Addr().String()
	ln.Close()
	sock := serveCtl(t, &fakeNode{})
	client := &http.Client{Transport: &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
		return net.Dial("unix", sock)
	}}}
	req, _ := http.NewRequest(http.MethodConnect, "http://ctl", nil)
	req.Host = dead
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != 502 {
		t.Fatalf("status %d", resp.StatusCode)
	}
	resp2, err := client.Get("http://ctl/status")
	if err != nil {
		t.Fatal(err)
	}
	var st Status
	json.NewDecoder(resp2.Body).Decode(&st)
	if st.State != "Running" {
		t.Fatalf("status %+v", st)
	}
}

func TestInboundForwardSendsIdentityHeader(t *testing.T) {
	linkSock := filepath.Join(shortTemp(t), "link.sock")
	ul, err := net.Listen("unix", linkSock)
	if err != nil {
		t.Fatal(err)
	}
	defer ul.Close()
	got := make(chan string, 2)
	go func() {
		c, err := ul.Accept()
		if err != nil {
			return
		}
		br := bufio.NewReader(c)
		hdr, _ := br.ReadString('\n')
		got <- hdr
		body := make([]byte, 4)
		io.ReadFull(br, body)
		got <- string(body)
		c.Write([]byte("pong"))
		c.Close()
	}()

	a, b := net.Pipe()
	n := &fakeNode{id: Identity{Node: "lstack-studio.tail1.ts.net", User: "jeff@github", Tailnet: "tail1.ts.net"}}
	done := make(chan struct{})
	go func() { forwardInbound(b, n, linkSock); close(done) }()
	a.Write([]byte("ping"))
	var id Identity
	if err := json.Unmarshal([]byte(strings.TrimSpace(<-got)), &id); err != nil {
		t.Fatal(err)
	}
	if id.Node != "lstack-studio.tail1.ts.net" || id.Tailnet != "tail1.ts.net" || id.User != "jeff@github" || id.Src == "" {
		t.Fatalf("header %+v", id)
	}
	if body := <-got; body != "ping" {
		t.Fatalf("body %q", body)
	}
	reply := make([]byte, 4)
	io.ReadFull(a, reply)
	if string(reply) != "pong" {
		t.Fatalf("reply %q", reply)
	}
	a.Close()
	<-done
}

func TestInboundRefusedWithoutIdentity(t *testing.T) {
	a, b := net.Pipe()
	done := make(chan struct{})
	go func() { forwardInbound(b, &fakeNode{}, "/nonexistent/link.sock"); close(done) }()
	<-done
	if _, err := a.Read(make([]byte, 1)); err == nil {
		t.Fatal("expected the connection to be closed")
	}
}

func TestTailnetOf(t *testing.T) {
	if got := tailnetOf("studio.tail1234.ts.net"); got != "tail1234.ts.net" {
		t.Fatal(got)
	}
	if got := tailnetOf("studio"); got != "" {
		t.Fatal(got)
	}
}
