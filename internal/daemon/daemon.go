// Package daemon is the long-lived hub that owns all PTY-backed sessions and
// serves clients (and hook callbacks) over a unix socket. It survives client
// disconnects so sessions persist across TUI restarts.
package daemon

import (
	"bufio"
	"encoding/json"
	"fmt"
	"log"
	"net"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/google/uuid"

	"command-center/internal/ipc"
	"command-center/internal/notify"
	"command-center/internal/session"
)

// Daemon owns the session registry and the unix socket listener.
type Daemon struct {
	mu       sync.RWMutex
	sessions map[string]*session.Session
	order    []string // insertion order for stable listing
	ln       net.Listener
}

// Run starts the daemon, listening on ipc.SocketPath until the process exits.
func Run() error {
	sockPath := ipc.SocketPath()
	if err := os.MkdirAll(ipc.Dir(), 0o700); err != nil {
		return err
	}
	// Remove a stale socket left by a previous crash. We only reach here after
	// failing to connect, so this is safe.
	if _, err := os.Stat(sockPath); err == nil {
		if c, derr := net.Dial("unix", sockPath); derr == nil {
			c.Close()
			return fmt.Errorf("daemon already running at %s", sockPath)
		}
		_ = os.Remove(sockPath)
	}

	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		return err
	}

	d := &Daemon{
		sessions: make(map[string]*session.Session),
		ln:       ln,
	}
	log.Printf("cb daemon listening on %s", sockPath)

	// Clean up the socket on SIGINT/SIGTERM so a restart doesn't see a stale one.
	sigc := make(chan os.Signal, 1)
	signal.Notify(sigc, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigc
		log.Printf("cb daemon shutting down (signal)")
		_ = os.Remove(sockPath)
		os.Exit(0)
	}()

	for {
		conn, err := ln.Accept()
		if err != nil {
			return err
		}
		go d.handle(conn)
	}
}

// shutdown kills every session and exits the process, removing the socket. Used
// by the `cb stop` command.
func (d *Daemon) shutdown() {
	d.mu.Lock()
	sessions := make([]*session.Session, 0, len(d.sessions))
	for _, s := range d.sessions {
		sessions = append(sessions, s)
	}
	d.sessions = map[string]*session.Session{}
	d.order = nil
	d.mu.Unlock()

	for _, s := range sessions {
		_ = s.Kill()
	}
	log.Printf("cb daemon stopped (%d session(s) killed)", len(sessions))
	_ = os.Remove(ipc.SocketPath())
	os.Exit(0)
}

func (d *Daemon) handle(conn net.Conn) {
	defer conn.Close()
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for sc.Scan() {
		var req ipc.Request
		if err := json.Unmarshal(sc.Bytes(), &req); err != nil {
			_ = ipc.WriteJSON(conn, ipc.Response{Error: "bad request: " + err.Error()})
			continue
		}
		if req.Type == "attach" {
			// attach takes over the connection as a bidirectional stream.
			d.attach(conn, sc, req)
			return
		}
		_ = ipc.WriteJSON(conn, d.dispatch(req))
	}
}

func (d *Daemon) lookup(id string) *session.Session {
	d.mu.RLock()
	defer d.mu.RUnlock()
	return d.sessions[id]
}

func (d *Daemon) dispatch(req ipc.Request) ipc.Response {
	switch req.Type {
	case "ping":
		return ipc.Response{OK: true, Version: ipc.ProtocolVersion, PID: os.Getpid()}
	case "shutdown":
		// Reply first, then tear down so the client sees success.
		go func() { time.Sleep(100 * time.Millisecond); d.shutdown() }()
		return ipc.Response{OK: true}
	case "spawn":
		return d.spawn(req)
	case "list":
		d.pruneExited()
		return ipc.Response{OK: true, Sessions: d.snapshot()}
	case "kill":
		return d.kill(req.ID)
	case "rename":
		return d.rename(req.ID, req.Name)
	case "hook":
		return d.hook(req)
	default:
		return ipc.Response{Error: "unknown request type: " + req.Type}
	}
}

func (d *Daemon) spawn(req ipc.Request) ipc.Response {
	argv := req.Argv
	if len(argv) == 0 {
		argv = []string{"claude"}
	}
	rows, cols := req.Rows, req.Cols
	if rows == 0 {
		rows = 24
	}
	if cols == 0 {
		cols = 80
	}
	id := uuid.NewString()
	s, err := session.New(id, argv, req.Cwd, rows, cols)
	if err != nil {
		return ipc.Response{Error: err.Error()}
	}
	d.mu.Lock()
	d.sessions[id] = s
	d.order = append(d.order, id)
	d.mu.Unlock()
	log.Printf("spawned session %s: %v", id, argv)
	return ipc.Response{OK: true, ID: id}
}

func (d *Daemon) kill(id string) ipc.Response {
	d.mu.Lock()
	s := d.sessions[id]
	if s == nil {
		d.mu.Unlock()
		return ipc.Response{Error: "no such session: " + id}
	}
	delete(d.sessions, id)
	for i, oid := range d.order {
		if oid == id {
			d.order = append(d.order[:i], d.order[i+1:]...)
			break
		}
	}
	d.mu.Unlock()

	if err := s.Kill(); err != nil {
		return ipc.Response{Error: err.Error()}
	}
	log.Printf("killed session %s", id)
	return ipc.Response{OK: true}
}

func (d *Daemon) rename(id, name string) ipc.Response {
	s := d.lookup(id)
	if s == nil {
		return ipc.Response{Error: "no such session: " + id}
	}
	s.SetName(name)
	log.Printf("renamed session %s -> %q", id, name)
	return ipc.Response{OK: true}
}

// hook applies a Claude Code hook event to the referenced session, translating
// the event name into a semantic status.
func (d *Daemon) hook(req ipc.Request) ipc.Response {
	d.mu.RLock()
	s := d.sessions[req.Session]
	d.mu.RUnlock()
	if s == nil {
		// Not an error: hooks may fire from sessions cb didn't spawn.
		log.Printf("hook %q for unknown session %q (ignored)", req.Event, req.Session)
		return ipc.Response{OK: true}
	}

	var p ipc.HookPayload
	_ = json.Unmarshal(req.Payload, &p)
	if p.SessionID != "" {
		s.SetClaudeSessionID(p.SessionID)
	}

	prev := s.Status()
	status, msg := statusForEvent(req.Event, p)
	s.SetStatus(status, msg)
	log.Printf("hook %s -> session %s status=%s", req.Event, req.Session, status)

	// Fire a desktop notification on the edge into needs_approval, so you're
	// alerted even when cb isn't focused (or no client is attached).
	if status == session.StatusNeedsApproval && prev != session.StatusNeedsApproval {
		body := msg
		if body == "" {
			body = "A session needs your approval"
		}
		notify.Send("Claude Code · "+shortLabel(s), body)
	}
	return ipc.Response{OK: true}
}

// shortLabel is a human hint for which session a notification is about.
func shortLabel(s *session.Session) string {
	if n := s.Name(); n != "" {
		return n
	}
	if s.Cwd != "" {
		parts := strings.Split(strings.TrimRight(s.Cwd, "/"), "/")
		if n := len(parts); n > 0 && parts[n-1] != "" {
			return parts[n-1]
		}
	}
	return s.ID[:8]
}

// statusForEvent maps a hook event name to a semantic status. The mapping is
// deliberately tolerant: the hook CLI forwards whatever event name Claude Code
// uses, and unknown events leave status unchanged (StatusWorking fallback while
// active). Confirm names against the installed Claude Code version.
func statusForEvent(event string, p ipc.HookPayload) (session.Status, string) {
	switch event {
	case "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolBatch":
		return session.StatusWorking, ""
	case "Notification", "PermissionRequest":
		return session.StatusNeedsApproval, p.Message
	case "Stop":
		return session.StatusWaitingUser, ""
	case "SessionEnd":
		return session.StatusEnded, ""
	default:
		return session.StatusWorking, ""
	}
}

// pruneExited drops sessions whose child process has terminated, so ended
// sessions (e.g. after Claude Code's /exit) disappear from the list rather than
// lingering as "ended".
func (d *Daemon) pruneExited() {
	d.mu.Lock()
	defer d.mu.Unlock()
	kept := make([]string, 0, len(d.order))
	for _, id := range d.order {
		s := d.sessions[id]
		if s == nil || s.Exited() {
			delete(d.sessions, id)
			continue
		}
		kept = append(kept, id)
	}
	d.order = kept
}

func (d *Daemon) snapshot() []ipc.SessionInfo {
	d.mu.RLock()
	defer d.mu.RUnlock()
	out := make([]ipc.SessionInfo, 0, len(d.order))
	for _, id := range d.order {
		s := d.sessions[id]
		if s == nil {
			continue
		}
		out = append(out, ipc.SessionInfo{
			ID:          s.ID,
			Name:        s.Name(),
			Argv:        s.Argv,
			Cwd:         s.Cwd,
			Status:      string(s.Status()),
			LastMessage: s.LastMessage(),
			Exited:      s.Exited(),
			StatusSince: s.StatusSince(),
		})
	}
	return out
}
