package web

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"net"
	"sync"
	"time"

	"codebridge/internal/ipc"
)

// listPollInterval is the legacy fallback cadence (matches the TUI's sidebar
// refresh) used only against a daemon too old to speak `watch`.
const listPollInterval = 500 * time.Millisecond

// watchBackoff is how long to wait before redialing a dropped watch stream.
const watchBackoff = time.Second

// pollFallbackFor is how long to poll before probing `watch` again — the old
// daemon may have been restarted into a version that supports it.
const pollFallbackFor = 30 * time.Second

// attachScannerBuf matches the TUI's attach read loop (dashboard.go): frames
// are full-screen ANSI strings and can be large.
const attachScannerBuf = 8 * 1024 * 1024

// poller keeps the session list fresh for every connected WS client and fans
// snapshots out to them. Despite the historical name it is push-first: it
// holds a `watch` stream to the daemon (which pushes a snapshot on every
// change), and only falls back to 500ms `list` polling against a daemon too
// old to speak `watch`. Each subscriber channel is coalescing (latest wins):
// a slow client sees fewer snapshots, never stale ones, and never blocks the
// upstream loop or other clients.
type poller struct {
	mu   sync.Mutex
	subs map[chan []webSession]struct{}
	last []webSession
	seen bool // whether last holds a real snapshot yet

	// lastJSON dedupes snapshots across watch reconnects and poll ticks;
	// scopeCache memoizes cwd → scope key so repeated snapshots don't re-walk
	// the filesystem per session. Both are touched only by the run goroutine.
	lastJSON   string
	scopeCache map[string]string
}

func newPoller() *poller {
	return &poller{
		subs:       make(map[chan []webSession]struct{}),
		scopeCache: make(map[string]string),
	}
}

// errWatchUnsupported means the daemon answered the watch request with an
// error — it predates the watch stream.
var errWatchUnsupported = errors.New("daemon does not support watch")

func (p *poller) run(ctx context.Context) {
	for ctx.Err() == nil {
		err := p.watch(ctx)
		if ctx.Err() != nil {
			return
		}
		if errors.Is(err, errWatchUnsupported) {
			p.pollLoop(ctx, pollFallbackFor)
			continue
		}
		// Stream dropped (daemon restart, transient error): redial shortly.
		select {
		case <-ctx.Done():
			return
		case <-time.After(watchBackoff):
		}
	}
}

// watch holds one push stream to the daemon, forwarding every snapshot line
// until the stream drops. Returns errWatchUnsupported for an old daemon.
func (p *poller) watch(ctx context.Context) error {
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return err
	}
	defer conn.Close()
	// Unblock the scanner if the server shuts down mid-stream.
	stop := make(chan struct{})
	defer close(stop)
	go func() {
		select {
		case <-ctx.Done():
		case <-stop:
		}
		conn.Close()
	}()

	if err := ipc.WriteJSON(conn, ipc.Request{Type: "watch"}); err != nil {
		return err
	}
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for sc.Scan() {
		var resp ipc.Response
		if json.Unmarshal(sc.Bytes(), &resp) != nil {
			continue
		}
		if resp.Error != "" {
			return errWatchUnsupported
		}
		if resp.OK {
			p.publish(resp.Sessions)
		}
	}
	return sc.Err()
}

// pollLoop is the legacy path: poll `list` at the TUI's cadence for a bounded
// window, then let run() probe watch support again.
func (p *poller) pollLoop(ctx context.Context, window time.Duration) {
	t := time.NewTicker(listPollInterval)
	defer t.Stop()
	deadline := time.After(window)
	for {
		select {
		case <-ctx.Done():
			return
		case <-deadline:
			return
		case <-t.C:
			resp, err := ipc.Send(ipc.Request{Type: "list"})
			if err == nil && resp.OK {
				p.publish(resp.Sessions)
			}
		}
	}
}

// publish dedupes, enriches, and broadcasts one snapshot.
func (p *poller) publish(list []ipc.SessionInfo) {
	b, err := json.Marshal(list)
	if err != nil {
		return
	}
	if s := string(b); s != p.lastJSON {
		p.lastJSON = s
		p.broadcast(p.enrich(list))
	}
}

// enrich tags each session with its scope key and display name for grouping.
func (p *poller) enrich(list []ipc.SessionInfo) []webSession {
	out := make([]webSession, len(list))
	for i, s := range list {
		key, ok := p.scopeCache[s.Cwd]
		if !ok {
			key = scopeKey(s.Cwd)
			p.scopeCache[s.Cwd] = key
		}
		out[i] = webSession{SessionInfo: s, Scope: key, ScopeName: scopeName(key)}
	}
	return out
}

func (p *poller) broadcast(snap []webSession) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.last, p.seen = snap, true
	for ch := range p.subs {
		sendLatest(ch, snap)
	}
}

// sendLatest pushes snap into a 1-buffered channel, displacing any unread
// older snapshot.
func sendLatest(ch chan []webSession, snap []webSession) {
	for {
		select {
		case ch <- snap:
			return
		default:
			select {
			case <-ch:
			default:
			}
		}
	}
}

// subscribe registers a coalescing snapshot channel, primed with the current
// snapshot if one exists so new clients render the list immediately.
func (p *poller) subscribe() chan []webSession {
	ch := make(chan []webSession, 1)
	p.mu.Lock()
	defer p.mu.Unlock()
	p.subs[ch] = struct{}{}
	if p.seen {
		ch <- p.last
	}
	return ch
}

func (p *poller) unsubscribe(ch chan []webSession) {
	p.mu.Lock()
	defer p.mu.Unlock()
	delete(p.subs, ch)
}

// dialAttach opens a dedicated bidirectional stream to the daemon for one
// session. When rows/cols are > 0 the daemon resizes the session to them
// (the browser claiming the pane size, TUI-style); zero leaves the session's
// size untouched.
func dialAttach(id string, rows, cols int) (net.Conn, error) {
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return nil, err
	}
	if err := ipc.WriteJSON(conn, ipc.Request{Type: "attach", ID: id, Rows: rows, Cols: cols}); err != nil {
		conn.Close()
		return nil, err
	}
	return conn, nil
}

// readFrames drains StreamDown messages from an attach conn into onDown until
// the conn closes or the daemon reports the session gone.
func readFrames(conn net.Conn, onDown func(ipc.StreamDown) bool) {
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), attachScannerBuf)
	for sc.Scan() {
		var d ipc.StreamDown
		if err := json.Unmarshal(sc.Bytes(), &d); err != nil {
			continue
		}
		if !onDown(d) || d.Type == "gone" {
			return
		}
	}
}

// pingDaemon reports whether a daemon is reachable and speaks our protocol
// version.
func pingDaemon() bool {
	resp, err := ipc.Send(ipc.Request{Type: "ping"})
	return err == nil && resp.OK && resp.Version == ipc.ProtocolVersion
}
