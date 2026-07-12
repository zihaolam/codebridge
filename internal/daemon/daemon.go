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
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/google/uuid"

	"codebridge/internal/ipc"
	"codebridge/internal/notify"
	"codebridge/internal/session"
	"codebridge/internal/task"
)

// Daemon owns the session registry and the unix socket listener.
type Daemon struct {
	mu       sync.RWMutex
	sessions map[string]*session.Session
	order    []string // insertion order for stable listing
	ln       net.Listener

	// taskStore is the workspace-scoped backlog. The daemon is its single
	// writer: clients mutate it over IPC (task_* requests) instead of touching
	// tasks.json directly, so concurrent cb clients can't clobber each other.
	// taskMu guards the store; reconcileTasks keeps in_progress tasks in step
	// with session lifecycle (the job the TUI used to do client-side).
	taskMu    sync.Mutex
	taskStore *task.Store

	// watchers are wakeup channels for `watch` push streams (see watch.go),
	// lazily initialized by subscribeChanges.
	watchMu  sync.Mutex
	watchers map[chan struct{}]struct{}

	// codexTaken tracks rollout ids already attributed to a session, so two
	// concurrent codex spawns in the same cwd can't harvest the same id (see
	// codex.go). Lazily initialized by claimCodexID.
	codexMu    sync.Mutex
	codexTaken map[string]bool
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
		sessions:  make(map[string]*session.Session),
		ln:        ln,
		taskStore: task.Load(),
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
		if req.Type == "watch" {
			// watch takes over the connection as a session-list push stream.
			d.watch(conn, sc)
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
		d.reconcileTasks()
		return ipc.Response{OK: true, Sessions: d.snapshot(), Tasks: d.taskSnapshot()}
	case "kill":
		return d.kill(req.ID)
	case "rename":
		return d.rename(req.ID, req.Name)
	case "hook":
		return d.hook(req)
	case "extract":
		return d.extract(req)
	case "task_list", "task_add", "task_edit", "task_status", "task_delete", "task_start":
		return d.taskDispatch(req)
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
	spawnedAt := time.Now()
	s, err := session.New(id, argv, req.Cwd, rows, cols, req.Prefill)
	if err != nil {
		return ipc.Response{Error: err.Error()}
	}
	d.mu.Lock()
	d.sessions[id] = s
	d.order = append(d.order, id)
	d.mu.Unlock()
	log.Printf("spawned session %s: %v", id, argv)
	// Codex has no hooks to report its session id; attribute it from the
	// rollout journal it writes on startup instead.
	if filepath.Base(argv[0]) == "codex" {
		go d.harvestCodexSession(s, spawnedAt)
	}
	d.notifyChange()
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
	d.notifyChange()
	return ipc.Response{OK: true}
}

// extract returns plain text from a session's virtual buffer (scrollback +
// visible) for the requested line/col range, used by the TUI to copy a
// drag-selected region to the system clipboard.
func (d *Daemon) extract(req ipc.Request) ipc.Response {
	s := d.lookup(req.ID)
	if s == nil {
		return ipc.Response{Error: "no such session: " + req.ID}
	}
	return ipc.Response{OK: true, Text: s.ExtractText(req.LineStart, req.LineEnd, req.ColStart, req.ColEnd)}
}

func (d *Daemon) rename(id, name string) ipc.Response {
	s := d.lookup(id)
	if s == nil {
		return ipc.Response{Error: "no such session: " + id}
	}
	s.SetName(name)
	log.Printf("renamed session %s -> %q", id, name)
	d.notifyChange()
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

	// Any hook event proves the agent's UI is up and accepting input — deliver
	// the pending prefill now rather than waiting out the fallback timer.
	// Idempotent, so later events are no-ops.
	s.DeliverPrefill()

	var p ipc.HookPayload
	_ = json.Unmarshal(req.Payload, &p)
	if p.SessionID != "" {
		s.SetHarnessSessionID(p.SessionID)
	}

	prev := s.Status()
	status, msg := statusForEvent(req.Event, p)
	s.SetStatus(status, msg)
	log.Printf("hook %s -> session %s status=%s", req.Event, req.Session, status)
	d.notifyChange()

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
	case "SessionStart":
		// A freshly spawned (or resumed) session is idle: ready, but no turn
		// has run yet. It maps to StatusIdle so the dashboard can distinguish
		// "just created" (yellow) from "agent turn finished" (green ● via
		// Stop -> waiting_user). Don't show a working spinner until a prompt
		// is actually submitted.
		return session.StatusIdle, ""
	case "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolBatch":
		return session.StatusWorking, ""
	case "PermissionRequest":
		return session.StatusNeedsApproval, p.Message
	case "Notification":
		// Claude Code fires Notification for two distinct things: an actual
		// tool-permission prompt, and an idle "waiting for your input" nudge
		// after a turn completes. Only the former should raise the approval
		// flag; the idle case is just waiting on the user.
		if isApprovalMessage(p.Message) {
			return session.StatusNeedsApproval, p.Message
		}
		return session.StatusWaitingUser, ""
	case "Stop":
		return session.StatusWaitingUser, ""
	case "SessionEnd":
		return session.StatusEnded, ""
	default:
		return session.StatusWorking, ""
	}
}

func statusForClientInterrupt(current session.Status) (session.Status, bool) {
	switch current {
	case session.StatusWorking, session.StatusNeedsApproval:
		return session.StatusWaitingUser, true
	default:
		return current, false
	}
}

// isApprovalMessage reports whether a Notification message is asking the user to
// approve/permit an action (as opposed to the generic idle "waiting for input"
// nudge). Claude Code's permission prompts read like "Claude needs your
// permission to use Bash"; the idle nudge reads "Claude is waiting for your
// input".
func isApprovalMessage(msg string) bool {
	m := strings.ToLower(msg)
	return strings.Contains(m, "permission") ||
		strings.Contains(m, "approval") ||
		strings.Contains(m, "approve")
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
			ID:               s.ID,
			Name:             s.Name(),
			Argv:             s.Argv,
			Cwd:              s.Cwd,
			Status:           string(s.Status()),
			LastMessage:      s.LastMessage(),
			HarnessSessionID: s.HarnessSessionID(),
			Exited:           s.Exited(),
			StatusSince:      s.StatusSince(),
		})
	}
	return out
}
