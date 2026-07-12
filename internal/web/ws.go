package web

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/coder/websocket"

	"codebridge/internal/ipc"
)

// client is one connected browser: a single multiplexed WebSocket carrying
// session-list updates, the currently-attached session's frame stream, and
// upstream input.
type client struct {
	srv *Server
	ws  *websocket.Conn
	out chan wsDown // serialized by the single writer goroutine

	mu         sync.Mutex
	attachConn net.Conn
	attachID   string
}

func (s *Server) handleWS(w http.ResponseWriter, r *http.Request) {
	// Same-host origins pass coder/websocket's built-in check; config adds
	// extra patterns (e.g. a custom domain mirroring the PWA).
	ws, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		OriginPatterns: s.cfg.AllowedOrigins,
	})
	if err != nil {
		return
	}
	c := &client{srv: s, ws: ws, out: make(chan wsDown, 64)}
	c.run(r.Context())
}

func (c *client) run(ctx context.Context) {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	defer c.ws.Close(websocket.StatusInternalError, "bridge shutting down")
	defer c.detach()

	// Auth: the first message must carry the token, within the deadline.
	// WebSockets don't obey same-origin, so this is what stops an arbitrary
	// webpage on a tailnet device from driving the daemon.
	authCtx, authCancel := context.WithTimeout(ctx, c.srv.authTimeout)
	_, first, err := c.ws.Read(authCtx)
	authCancel()
	if err != nil {
		return
	}
	var auth wsUp
	if json.Unmarshal(first, &auth) != nil || auth.Type != "auth" ||
		subtle.ConstantTimeCompare([]byte(auth.Token), []byte(c.srv.cfg.Token)) != 1 {
		c.writeNow(ctx, wsDown{Type: "error", Error: "auth failed"})
		c.ws.Close(websocket.StatusPolicyViolation, "auth failed")
		return
	}

	// Single writer goroutine: coder/websocket forbids concurrent writes.
	go func() {
		for {
			select {
			case <-ctx.Done():
				return
			case m := <-c.out:
				if err := c.writeNow(ctx, m); err != nil {
					cancel()
					return
				}
			}
		}
	}()

	c.send(ctx, wsDown{
		Type: "hello", Protocol: ipc.ProtocolVersion,
		Daemon: pingDaemon(), Agents: availableAgents(),
	})

	// Session-list + backlog pump: each snapshot fans out both a `sessions` and
	// a `tasks` frame so the browser repaints the sidebar and the task screen
	// from one wakeup.
	sub := c.srv.poller.subscribe()
	defer c.srv.poller.unsubscribe(sub)
	go func() {
		for {
			select {
			case <-ctx.Done():
				return
			case snap := <-sub:
				c.send(ctx, wsDown{Type: "sessions", Sessions: snap.sessions})
				c.send(ctx, wsDown{Type: "tasks", Tasks: snap.tasks})
			}
		}
	}()

	// Main read loop: dispatch browser messages until the socket closes.
	for {
		_, b, err := c.ws.Read(ctx)
		if err != nil {
			return
		}
		var up wsUp
		if json.Unmarshal(b, &up) != nil {
			continue
		}
		c.dispatch(ctx, up)
	}
}

func (c *client) dispatch(ctx context.Context, up wsUp) {
	switch up.Type {
	case "attach":
		c.attach(ctx, up.ID, up.Rows, up.Cols)
	case "detach":
		c.detach()
	case "input":
		c.forward(ipc.StreamUp{Type: "input", Data: up.Data})
	case "paste":
		c.forward(ipc.StreamUp{Type: "paste", Data: up.Data})
	case "resize":
		c.forward(ipc.StreamUp{Type: "resize", Rows: up.Rows, Cols: up.Cols})
	case "viewport":
		c.forward(ipc.StreamUp{Type: "viewport", Rows: up.Rows, Cols: up.Cols})
	case "scroll":
		c.forward(ipc.StreamUp{Type: "scroll", Offset: up.Offset})
	case "interrupt":
		c.forward(ipc.StreamUp{Type: "interrupt"})
	case "spawn":
		resp, err := ipc.Send(ipc.Request{Type: "spawn", Argv: up.Argv, Cwd: up.Cwd})
		switch {
		case err != nil:
			c.send(ctx, wsDown{Type: "error", Error: err.Error()})
		case !resp.OK:
			c.send(ctx, wsDown{Type: "error", Error: resp.Error})
		default:
			c.send(ctx, wsDown{Type: "spawned", ID: resp.ID})
		}
	case "kill":
		resp, err := ipc.Send(ipc.Request{Type: "kill", ID: up.ID})
		if err != nil {
			c.send(ctx, wsDown{Type: "error", Error: err.Error()})
		} else if !resp.OK {
			c.send(ctx, wsDown{Type: "error", Error: resp.Error})
		}
	case "worktrees":
		wts := listWorktrees(up.Cwd)
		if len(wts) == 0 {
			// Not a git repo (or git failed): the dir itself is the only place
			// to spawn, so the picker still has one option.
			wts = []worktreeEntry{{Path: up.Cwd, Main: true}}
		}
		c.send(ctx, wsDown{Type: "worktrees", Cwd: up.Cwd, Worktrees: wts, Agents: availableAgents()})
	case "task_list", "task_add", "task_edit", "task_status", "task_delete":
		c.proxyTask(ctx, up)
	case "task_start":
		c.taskStart(ctx, up)
	}
}

// proxyTask forwards a backlog mutation straight to the daemon (the single
// writer of tasks.json) and replies with the fresh, scope-enriched list. The
// daemon's notifyChange also pushes the same list to every client's task pump,
// so peers stay in sync; this reply just gives the mutating browser an
// immediate update without waiting for that round-trip.
func (c *client) proxyTask(ctx context.Context, up wsUp) {
	resp, err := ipc.Send(ipc.Request{
		Type: up.Type, ID: up.ID, Scope: up.Scope,
		Title: up.Title, Desc: up.Desc, Status: up.Status,
	})
	switch {
	case err != nil:
		c.send(ctx, wsDown{Type: "error", Error: err.Error()})
	case !resp.OK:
		c.send(ctx, wsDown{Type: "error", Error: resp.Error})
	default:
		c.send(ctx, wsDown{Type: "tasks", Tasks: enrichTasks(resp.Tasks)})
	}
}

// taskStart asks the daemon to spawn an agent session for a task and link it
// (the daemon owns the resume/prefill logic). No rows/cols are sent — like a
// plain attach, a phone must never resize a session out from under the desktop
// TUI. On success the browser gets the fresh backlog plus a `spawned` frame so
// it can jump to the new session.
func (c *client) taskStart(ctx context.Context, up wsUp) {
	resp, err := ipc.Send(ipc.Request{
		Type: "task_start", ID: up.ID, Agent: up.Agent, Cwd: up.Cwd,
	})
	switch {
	case err != nil:
		c.send(ctx, wsDown{Type: "error", Error: err.Error()})
	case !resp.OK:
		c.send(ctx, wsDown{Type: "error", Error: resp.Error})
	default:
		c.send(ctx, wsDown{Type: "tasks", Tasks: enrichTasks(resp.Tasks)})
		c.send(ctx, wsDown{Type: "spawned", ID: resp.ID})
	}
}

// attach switches this client's frame stream to the given session, replacing
// any previous attach. Frames are tagged with the session id so the browser
// can drop stragglers from a session it just switched away from. When the
// Browser attaches normally omit rows/cols: their viewport is independent of
// the canonical PTY. An explicit resize action may still claim the PTY size.
func (c *client) attach(ctx context.Context, id string, rows, cols int) {
	if id == "" {
		return
	}
	conn, err := dialAttach(id, rows, cols)
	if err != nil {
		c.send(ctx, wsDown{Type: "error", Error: "attach: " + err.Error()})
		return
	}

	c.mu.Lock()
	if c.attachConn != nil {
		c.attachConn.Close() // unblocks the old readFrames goroutine
	}
	c.attachConn, c.attachID = conn, id
	c.mu.Unlock()

	go readFrames(conn, func(d ipc.StreamDown) bool {
		m := wsDown{
			Type: d.Type, ID: id,
			Screen: d.Screen, CursorX: d.CursorX, CursorY: d.CursorY,
			Alt: d.Alt, Rows: d.Rows, Cols: d.Cols,
			Offset: d.Offset, MaxOffset: d.MaxOffset,
		}
		select {
		case c.out <- m:
			return true
		case <-ctx.Done():
			return false
		}
	})
}

func (c *client) detach() {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.attachConn != nil {
		c.attachConn.Close()
		c.attachConn, c.attachID = nil, ""
	}
}

// forward writes a StreamUp onto the current attach conn, if any.
func (c *client) forward(up ipc.StreamUp) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.attachConn != nil {
		_ = ipc.WriteJSON(c.attachConn, up)
	}
}

// send queues m for the writer goroutine, giving up if the client is gone.
func (c *client) send(ctx context.Context, m wsDown) {
	select {
	case c.out <- m:
	case <-ctx.Done():
	}
}

// writeNow marshals and writes directly; only the auth failure path and the
// writer goroutine use it.
func (c *client) writeNow(ctx context.Context, m wsDown) error {
	b, err := json.Marshal(m)
	if err != nil {
		return err
	}
	wctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	return c.ws.Write(wctx, websocket.MessageText, b)
}
