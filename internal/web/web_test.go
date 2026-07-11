package web

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"

	"codebridge/internal/daemon"
	"codebridge/internal/ipc"
)

const testToken = "test-token"

// TestMain runs one real daemon (on a temp CB_SOCK) for the whole package.
// Never send it a "shutdown" request: daemon.shutdown os.Exits the test
// process. Sessions are killed individually instead.
func TestMain(m *testing.M) {
	dir, err := os.MkdirTemp("", "cbweb")
	if err != nil {
		panic(err)
	}
	os.Setenv("CB_SOCK", filepath.Join(dir, "d.sock"))
	go func() { _ = daemon.Run() }()
	for i := 0; i < 100; i++ {
		if c, err := net.Dial("unix", ipc.SocketPath()); err == nil {
			c.Close()
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	code := m.Run()
	os.RemoveAll(dir)
	os.Exit(code)
}

func newTestServer(t *testing.T, cfg Config) (*Server, *httptest.Server) {
	t.Helper()
	s := NewServer(cfg)
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	s.Start(ctx)
	ts := httptest.NewServer(s.Handler())
	t.Cleanup(ts.Close)
	return s, ts
}

func wsURL(ts *httptest.Server) string {
	return "ws" + strings.TrimPrefix(ts.URL, "http") + "/ws"
}

func dialWS(t *testing.T, ts *httptest.Server, opts *websocket.DialOptions) *websocket.Conn {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	c, _, err := websocket.Dial(ctx, wsURL(ts), opts)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { c.CloseNow() })
	c.SetReadLimit(attachScannerBuf)
	return c
}

func sendJSON(t *testing.T, c *websocket.Conn, v any) {
	t.Helper()
	b, _ := json.Marshal(v)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := c.Write(ctx, websocket.MessageText, b); err != nil {
		t.Fatalf("write: %v", err)
	}
}

func readDown(t *testing.T, c *websocket.Conn, timeout time.Duration) (wsDown, error) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	_, b, err := c.Read(ctx)
	if err != nil {
		return wsDown{}, err
	}
	var d wsDown
	if err := json.Unmarshal(b, &d); err != nil {
		t.Fatalf("bad down message %q: %v", b, err)
	}
	return d, nil
}

// authOK drives the happy-path handshake and asserts the hello.
func authOK(t *testing.T, c *websocket.Conn) {
	t.Helper()
	sendJSON(t, c, wsUp{Type: "auth", Token: testToken})
	d, err := readDown(t, c, 5*time.Second)
	if err != nil {
		t.Fatalf("reading hello: %v", err)
	}
	if d.Type != "hello" || d.Protocol != ipc.ProtocolVersion || !d.Daemon {
		t.Fatalf("unexpected hello: %+v", d)
	}
}

func TestAuthBadToken(t *testing.T) {
	_, ts := newTestServer(t, Config{Token: testToken})
	c := dialWS(t, ts, nil)
	sendJSON(t, c, wsUp{Type: "auth", Token: "wrong"})

	// Expect an error message, then a policy-violation close.
	d, err := readDown(t, c, 5*time.Second)
	if err == nil {
		if d.Type != "error" {
			t.Fatalf("expected error message, got %+v", d)
		}
		_, err = readDown(t, c, 5*time.Second)
	}
	if websocket.CloseStatus(err) != websocket.StatusPolicyViolation {
		t.Fatalf("expected policy-violation close, got %v", err)
	}
}

func TestAuthTimeout(t *testing.T) {
	s, ts := newTestServer(t, Config{Token: testToken})
	s.authTimeout = 100 * time.Millisecond
	c := dialWS(t, ts, nil)
	// Send nothing: the socket must be closed on us within the deadline.
	if _, err := readDown(t, c, 3*time.Second); err == nil {
		t.Fatal("expected close after auth timeout, got a message")
	}
}

func TestOriginRejected(t *testing.T) {
	_, ts := newTestServer(t, Config{Token: testToken})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_, _, err := websocket.Dial(ctx, wsURL(ts), &websocket.DialOptions{
		HTTPHeader: http.Header{"Origin": []string{"https://evil.example"}},
	})
	if err == nil {
		t.Fatal("expected cross-origin dial to be rejected")
	}
}

func TestOriginAllowedByConfig(t *testing.T) {
	_, ts := newTestServer(t, Config{Token: testToken, AllowedOrigins: []string{"good.example"}})
	c := dialWS(t, ts, &websocket.DialOptions{
		HTTPHeader: http.Header{"Origin": []string{"https://good.example"}},
	})
	authOK(t, c)
}

// TestAttachInputFrame is the end-to-end path: spawn a real `cat` session in
// the daemon, attach through the bridge over a real WebSocket, type into it,
// and assert the PTY echo comes back as a frame.
func TestAttachInputFrame(t *testing.T) {
	_, ts := newTestServer(t, Config{Token: testToken})

	resp, err := ipc.Send(ipc.Request{Type: "spawn", Argv: []string{"cat"}, Cwd: t.TempDir()})
	if err != nil || !resp.OK {
		t.Fatalf("spawn: %v %+v", err, resp)
	}
	id := resp.ID
	t.Cleanup(func() { _, _ = ipc.Send(ipc.Request{Type: "kill", ID: id}) })

	c := dialWS(t, ts, nil)
	authOK(t, c)

	sendJSON(t, c, wsUp{Type: "attach", ID: id})
	sendJSON(t, c, wsUp{Type: "input",
		Data: base64.StdEncoding.EncodeToString([]byte("hello-from-web"))})

	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		d, err := readDown(t, c, 10*time.Second)
		if err != nil {
			t.Fatalf("reading frames: %v", err)
		}
		// sessions snapshots interleave with frames on the multiplexed socket.
		if d.Type == "frame" {
			if d.ID != id {
				t.Fatalf("frame tagged with wrong session: %q", d.ID)
			}
			if strings.Contains(d.Screen, "hello-from-web") {
				return // echo made it: daemon -> bridge -> ws
			}
		}
	}
	t.Fatal("never saw the echoed input in a frame")
}

// TestSessionsBroadcast checks that a client hears about sessions it didn't
// spawn, via the shared list poller.
func TestSessionsBroadcast(t *testing.T) {
	_, ts := newTestServer(t, Config{Token: testToken})
	c := dialWS(t, ts, nil)
	authOK(t, c)

	resp, err := ipc.Send(ipc.Request{Type: "spawn", Argv: []string{"cat"}, Cwd: t.TempDir()})
	if err != nil || !resp.OK {
		t.Fatalf("spawn: %v %+v", err, resp)
	}
	id := resp.ID
	t.Cleanup(func() { _, _ = ipc.Send(ipc.Request{Type: "kill", ID: id}) })

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		d, err := readDown(t, c, 5*time.Second)
		if err != nil {
			t.Fatalf("reading: %v", err)
		}
		if d.Type == "sessions" {
			for _, s := range d.Sessions {
				if s.ID == id {
					// Non-repo cwd: scope is the literal dir, named by base.
					if s.Scope != s.Cwd || s.ScopeName != filepath.Base(s.Cwd) {
						t.Fatalf("bad scope enrichment: %+v", s)
					}
					return
				}
			}
		}
	}
	t.Fatal("session never appeared in a sessions broadcast")
}

func TestScopeKey(t *testing.T) {
	// A repo with a main checkout and a linked worktree must share one scope
	// key (the shared .git), named after the repo root.
	root := t.TempDir()
	repo := filepath.Join(root, "myrepo")
	gitDir := filepath.Join(repo, ".git")
	sub := filepath.Join(repo, "pkg", "deep")
	if err := os.MkdirAll(filepath.Join(gitDir, "worktrees", "wt1"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(sub, 0o755); err != nil {
		t.Fatal(err)
	}
	wt := filepath.Join(root, "myrepo-wt1")
	if err := os.MkdirAll(wt, 0o755); err != nil {
		t.Fatal(err)
	}
	wtGitDir := filepath.Join(gitDir, "worktrees", "wt1")
	if err := os.WriteFile(filepath.Join(wt, ".git"), []byte("gitdir: "+wtGitDir+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(wtGitDir, "commondir"), []byte("../..\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	main := scopeKey(repo)
	if main != resolveDir(gitDir) {
		t.Fatalf("main checkout scope = %q, want %q", main, gitDir)
	}
	if k := scopeKey(sub); k != main {
		t.Fatalf("subdir scope = %q, want %q", k, main)
	}
	if k := scopeKey(wt); k != main {
		t.Fatalf("worktree scope = %q, want main repo scope %q", k, main)
	}
	if n := scopeName(main); n != "myrepo" {
		t.Fatalf("scope name = %q, want myrepo", n)
	}

	// Outside any repo: the cwd itself is the key.
	plain := filepath.Join(root, "plain")
	if err := os.MkdirAll(plain, 0o755); err != nil {
		t.Fatal(err)
	}
	if k := scopeKey(plain); k != plain {
		t.Fatalf("non-repo scope = %q, want %q", k, plain)
	}
}

// TestDaemonWatchStream exercises the daemon's watch push stream directly:
// an immediate snapshot on subscribe, then an event-driven push (no full
// poll interval) when a session spawns.
func TestDaemonWatchStream(t *testing.T) {
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	if err := ipc.WriteJSON(conn, ipc.Request{Type: "watch"}); err != nil {
		t.Fatal(err)
	}
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	readSnap := func() []ipc.SessionInfo {
		t.Helper()
		_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
		if !sc.Scan() {
			t.Fatalf("watch stream ended: %v", sc.Err())
		}
		var resp ipc.Response
		if err := json.Unmarshal(sc.Bytes(), &resp); err != nil || !resp.OK {
			t.Fatalf("bad watch line %q: %v", sc.Bytes(), err)
		}
		return resp.Sessions
	}

	readSnap() // initial snapshot arrives without any change happening

	resp, err := ipc.Send(ipc.Request{Type: "spawn", Argv: []string{"cat"}, Cwd: t.TempDir()})
	if err != nil || !resp.OK {
		t.Fatalf("spawn: %v %+v", err, resp)
	}
	id := resp.ID
	t.Cleanup(func() { _, _ = ipc.Send(ipc.Request{Type: "kill", ID: id}) })

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		for _, s := range readSnap() {
			if s.ID == id {
				return
			}
		}
	}
	t.Fatal("spawn never pushed on the watch stream")
}

func TestConfigTokenLifecycle(t *testing.T) {
	t.Setenv("HOME", t.TempDir()) // ipc.Dir() derives ~/.cb from $HOME

	cfg, err := LoadOrCreate()
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Token) != 64 {
		t.Fatalf("expected 32-byte hex token, got %q", cfg.Token)
	}
	again, err := LoadOrCreate()
	if err != nil || again.Token != cfg.Token {
		t.Fatalf("token not stable across loads: %v %q vs %q", err, again.Token, cfg.Token)
	}
	rotated, err := Rotate()
	if err != nil || rotated.Token == cfg.Token {
		t.Fatalf("rotate did not change token: %v", err)
	}
	info, err := os.Stat(filepath.Join(ipc.Dir(), "web.json"))
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("web.json mode = %v, want 0600", info.Mode().Perm())
	}
}
