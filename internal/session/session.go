// Package session owns a single child process running under a pseudo-terminal,
// continuously draining its output into a virtual-terminal emulator. A session
// is the unit the daemon manages and the TUI renders.
package session

import (
	"os"
	"os/exec"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/charmbracelet/x/vt"
	"github.com/creack/pty"
)

// Status is the semantic state of a session, primarily driven by Claude Code
// hook events (see internal/hook) rather than scraped from terminal output.
type Status string

const (
	StatusStarting      Status = "starting"
	StatusWorking       Status = "working"
	StatusNeedsApproval Status = "needs_approval"
	StatusWaitingUser   Status = "waiting_user"
	StatusIdle          Status = "idle"
	StatusEnded         Status = "ended"
)

// emuPad is a spare column we give the emulator beyond the PTY width. Claude
// Code (and other Ink-based TUIs) occasionally draw a full-width rule one
// column wider than the reported terminal size; in a real terminal the last
// column's deferred-wrap swallows it, but the emulator would otherwise wrap the
// extra glyph onto a stray line. The spare column absorbs that overflow, and
// Render trims back to the PTY width so the stray glyph never shows.
const emuPad = 1

// Session couples a PTY-backed child process with its emulator and status.
type Session struct {
	ID   string
	Argv []string
	Cwd  string

	ptmx *os.File
	cmd  *exec.Cmd
	emu  *vt.SafeEmulator

	mu              sync.RWMutex
	name            string
	status          Status
	statusSince     time.Time
	lastMessage     string
	claudeSessionID string
	startedAt       time.Time
	exited          bool
	exitErr         error
	rows, cols      int
}

// New spawns argv under a new PTY of the given size and starts draining it.
// The child gets CB_SESSION=<id> in its environment so that Claude Code
// hooks (which run as descendants) can correlate themselves back to this
// session. pty.StartWithSize puts the child in its own session with the PTY as
// controlling terminal, so signals like Ctrl-C target the child, not us.
func New(id string, argv []string, cwd string, rows, cols int) (*Session, error) {
	c := exec.Command(argv[0], argv[1:]...)
	if cwd != "" {
		c.Dir = cwd
	}
	c.Env = append(os.Environ(),
		"CB_SESSION="+id,
		"TERM=xterm-256color",
	)

	ptmx, err := pty.StartWithSize(c, &pty.Winsize{Rows: uint16(rows), Cols: uint16(cols)})
	if err != nil {
		return nil, err
	}

	s := &Session{
		ID:          id,
		Argv:        argv,
		Cwd:         cwd,
		ptmx:        ptmx,
		cmd:         c,
		emu:         vt.NewSafeEmulator(cols+emuPad, rows),
		status:      StatusStarting,
		statusSince: time.Now(),
		startedAt:   time.Now(),
		rows:        rows,
		cols:        cols,
	}

	go s.readLoop()
	go s.replyLoop()
	go s.wait()
	return s, nil
}

// readLoop continuously drains the PTY into the emulator. It must keep running
// regardless of whether a client is attached: if we stop reading, the child
// eventually blocks on write once the PTY buffer fills.
func (s *Session) readLoop() {
	buf := make([]byte, 32*1024)
	for {
		n, err := s.ptmx.Read(buf)
		if n > 0 {
			_, _ = s.emu.Write(buf[:n])
		}
		if err != nil {
			return // EOF / closed: child has gone away
		}
	}
}

// replyLoop forwards the emulator's generated terminal replies (device-attribute
// answers, cursor-position reports, etc.) back to the child's PTY input. Without
// this, an app that queries the terminal at startup (as Claude Code does) blocks
// waiting for a reply that never comes — and because the emulator writes those
// replies synchronously while holding its write lock, the whole session stalls.
func (s *Session) replyLoop() {
	buf := make([]byte, 4096)
	for {
		n, err := s.emu.Read(buf)
		if n > 0 {
			_, _ = s.ptmx.Write(buf[:n])
		}
		if err != nil {
			return
		}
	}
}

func (s *Session) wait() {
	err := s.cmd.Wait()
	_ = s.ptmx.Close()
	s.mu.Lock()
	s.exited = true
	s.exitErr = err
	s.status = StatusEnded
	s.mu.Unlock()
}

// WriteInput forwards raw input bytes to the child.
func (s *Session) WriteInput(p []byte) (int, error) {
	return s.ptmx.Write(p)
}

// Resize updates both the PTY window size (triggering SIGWINCH so the child
// repaints) and the emulator grid.
func (s *Session) Resize(rows, cols int) error {
	if err := pty.Setsize(s.ptmx, &pty.Winsize{Rows: uint16(rows), Cols: uint16(cols)}); err != nil {
		return err
	}
	s.emu.Resize(cols+emuPad, rows)
	s.mu.Lock()
	s.rows, s.cols = rows, cols
	s.mu.Unlock()
	return nil
}

// Render returns a string snapshot of the current screen, trimmed to the PTY
// width so the emulator's spare overflow column (see emuPad) never shows.
func (s *Session) Render() string {
	s.mu.RLock()
	cols := s.cols
	s.mu.RUnlock()
	full := s.emu.Render()
	if cols <= 0 {
		return full
	}
	lines := strings.Split(full, "\n")
	for i, ln := range lines {
		if r := []rune(ln); len(r) > cols {
			lines[i] = string(r[:cols])
		}
	}
	return strings.Join(lines, "\n")
}

// Cursor returns the current cursor cell position.
func (s *Session) Cursor() (x, y int) {
	p := s.emu.CursorPosition()
	return p.X, p.Y
}

// IsAltScreen reports whether the child is in the alternate screen buffer.
func (s *Session) IsAltScreen() bool {
	return s.emu.IsAltScreen()
}

// Status returns the current semantic status.
func (s *Session) Status() Status {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.status
}

// SetStatus updates the semantic status and optional attached message.
func (s *Session) SetStatus(st Status, message string) {
	s.mu.Lock()
	if s.status != st {
		s.statusSince = time.Now()
	}
	s.status = st
	if message != "" {
		s.lastMessage = message
	}
	s.mu.Unlock()
}

// StatusSince reports when the current status was entered (unix nanoseconds).
func (s *Session) StatusSince() int64 {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.statusSince.UnixNano()
}

// Name returns the user-assigned display name, or "" if unset.
func (s *Session) Name() string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.name
}

// SetName assigns a user-friendly display name for the session list.
func (s *Session) SetName(name string) {
	s.mu.Lock()
	s.name = name
	s.mu.Unlock()
}

// LastMessage returns the most recent status message (e.g. an approval prompt).
func (s *Session) LastMessage() string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.lastMessage
}

// SetClaudeSessionID records the upstream Claude Code session id once a hook
// reports it, for cross-referencing transcripts.
func (s *Session) SetClaudeSessionID(id string) {
	s.mu.Lock()
	s.claudeSessionID = id
	s.mu.Unlock()
}

// Exited reports whether the child process has terminated.
func (s *Session) Exited() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.exited
}

// Kill terminates the child and everything it spawned. pty.StartWithSize starts
// the child as its own session/process-group leader (Setsid), so its pid is the
// group id; signalling the negative pid takes down the whole tree (claude's tool
// subprocesses, MCP servers, shells), not just claude itself.
func (s *Session) Kill() error {
	if s.cmd.Process == nil {
		return nil
	}
	pid := s.cmd.Process.Pid
	if err := syscall.Kill(-pid, syscall.SIGKILL); err != nil {
		return s.cmd.Process.Kill() // fall back to just the leader
	}
	return nil
}
