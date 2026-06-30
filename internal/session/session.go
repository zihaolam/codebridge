// Package session owns a single child process running under a pseudo-terminal,
// continuously draining its output into a virtual-terminal emulator. A session
// is the unit the daemon manages and the TUI renders.
package session

import (
	"os"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/ansi"
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
// Code (and other Ink-based TUIs) draw full-width rules and boxes that put a
// glyph in the last column; in a real terminal the last column's deferred wrap
// holds the cursor there until the next write, but the emulator wraps the extra
// glyph onto a stray next line. The spare column absorbs that overflow, and
// Render trims back to the PTY width so the stray glyph never shows.
const emuPad = 1

// syncWatchdog bounds how long a Synchronized Output Mode (DEC 2026) block is
// honored before we resume rendering. The DEC 2026 spec recommends a terminal
// timeout in this range so a client that opens a sync block and dies without
// closing it can't permanently freeze the view.
const syncWatchdog = 150 * time.Millisecond

// liveFrame memoizes the live (offset-0) render so every client attached to a
// session — and the 30fps frame ticker driving each — shares one render instead
// of each re-rendering the same screen every tick. It's keyed by gen (below):
// an unchanged gen means the cached frame is still current and no work happens.
type liveFrame struct {
	mu      sync.Mutex
	gen     uint64
	valid   bool
	screen  string
	cursorX int
	cursorY int
	maxOff  int
	alt     bool
}

// Session couples a PTY-backed child process with its emulator and status.
type Session struct {
	ID   string
	Argv []string
	Cwd  string

	ptmx *os.File
	cmd  *exec.Cmd
	emu  *vt.SafeEmulator

	// gen advances on every screen-changing write (PTY output, resize). It's the
	// cheap dirty signal that gates rendering: the frame ticker compares it
	// against the cached frame's gen and skips the (expensive) render entirely
	// when nothing has changed since the last one. Conservatively bumped — a
	// write that doesn't visibly change the screen still bumps it, which only
	// costs one redundant render that the per-client dedup then drops.
	gen atomic.Uint64
	fc  liveFrame

	// syncStartNanos is the unix-nano timestamp when the child entered DEC 2026
	// (Synchronized Output Mode), or 0 when it isn't in a sync block. Set from
	// the emulator's mode callbacks (called inside Write, so atomic-only — no
	// locks that could deadlock the emulator). The frame loop in the daemon
	// reads this to skip rendering mid-batch; see IsSyncBlock.
	syncStartNanos atomic.Int64

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

	// Track DEC 2026 (Synchronized Output Mode) so the daemon's frame ticker
	// can skip rendering mid-batch. Codex/ratatui wraps each redraw in
	// ESC[?2026h … ESC[?2026l (erase display + reposition + rewrite the
	// scroll-region UI); without honoring that, the 30 fps ticker captures the
	// half-erased mid-batch state and ships it to the client, which looks like
	// the pane "overscrolling and jumping" on every codex render. The vt
	// emulator records 2026 in its mode map but applies the intermediate
	// commands immediately, so we have to honor the batch ourselves.
	// Callbacks fire from inside the emulator's Write under its mutex — keep
	// them lock-free (atomic only) to avoid deadlocking with Render.
	// SetCallbacks must run BEFORE readLoop so the first frame the child
	// emits can't race against the (still-nil) callback set.
	s.emu.SetCallbacks(vt.Callbacks{
		EnableMode: func(m ansi.Mode) {
			if m == ansi.ModeSynchronizedOutput {
				s.syncStartNanos.Store(time.Now().UnixNano())
			}
		},
		DisableMode: func(m ansi.Mode) {
			if m == ansi.ModeSynchronizedOutput {
				s.syncStartNanos.Store(0)
			}
		},
	})

	go s.readLoop()
	go s.replyLoop()
	go s.wait()
	return s, nil
}

// IsSyncBlock reports whether the child is currently inside a DEC 2026
// Synchronized Output Mode block. The frame loop checks this and skips
// rendering while it's true so the client never sees the mid-batch state
// (erase-display + cursor moves + half-written content) that codex/ratatui
// brackets between BSU and ESU. A syncWatchdog-bounded stale flag is treated
// as "not in a block" so a client that opens a sync and dies (or holds it
// open longer than the spec recommends) can't freeze the view indefinitely.
func (s *Session) IsSyncBlock() bool {
	started := s.syncStartNanos.Load()
	if started == 0 {
		return false
	}
	return time.Now().UnixNano()-started <= int64(syncWatchdog)
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
			s.gen.Add(1) // mark the screen dirty so the next tick re-renders
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

// Paste forwards pasted text to the child. The emulator wraps it in
// bracketed-paste markers (ESC[200~ … ESC[201~) when the child has bracketed
// paste mode enabled — as Claude Code does — so the whole paste arrives as one
// unit and isn't chunked or interpreted character-by-character. The bytes flow
// through the emulator's reply pipe, which replyLoop drains into the PTY.
func (s *Session) Paste(text string) {
	s.emu.Paste(text)
}

// Resize updates both the PTY window size (triggering SIGWINCH so the child
// repaints) and the emulator grid.
func (s *Session) Resize(rows, cols int) error {
	if err := pty.Setsize(s.ptmx, &pty.Winsize{Rows: uint16(rows), Cols: uint16(cols)}); err != nil {
		return err
	}
	s.emu.Resize(cols+emuPad, rows)
	s.gen.Add(1) // the grid changed shape — invalidate the cached frame
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
	return trimCols(s.emu.Render(), cols)
}

// trimCols clips each line of s to at most cols display columns. The emulator's
// Render encodes styles as inline ANSI escapes, so the clip must be width-aware:
// counting escape bytes as runes (the old behavior) cut visible content short
// and could slice through an escape sequence, leaving a dangling color that
// bleeds into the rest of the frame and throws off the client's width-aware line
// wrapping (full-width rules wrapping onto a stray extra line). ansi.Truncate
// measures display width and keeps escapes — including the trailing reset —
// intact. cols <= 0 is a no-op.
func trimCols(s string, cols int) string {
	if cols <= 0 {
		return s
	}
	lines := strings.Split(s, "\n")
	for i, ln := range lines {
		lines[i] = ansi.Truncate(ln, cols, "")
	}
	return strings.Join(lines, "\n")
}

// RenderScroll returns a snapshot of the screen scrolled `offset` lines up from
// the live bottom (offset 0 == the live screen). It composes lines from the
// scrollback buffer and the visible grid into exactly `rows` lines, and reports
// the clamped offset actually used plus the maximum offset (the scrollback
// length) so a client can bound its scrolling and show the position. The view is
// anchored to the live bottom, so new output shifts a scrolled view downward.
func (s *Session) RenderScroll(offset int) (screen string, clamped int, maxOffset int) {
	maxOffset = s.emu.ScrollbackLen()
	if offset < 0 {
		offset = 0
	}
	if offset > maxOffset {
		offset = maxOffset
	}
	if offset == 0 {
		return s.Render(), 0, maxOffset
	}

	s.mu.RLock()
	rows, cols := s.rows, s.cols
	s.mu.RUnlock()

	// Virtual buffer: scrollback lines [0,maxOffset) followed by the visible
	// grid. top is the first virtual line shown; the window is `rows` tall.
	vis := strings.Split(s.Render(), "\n")
	top := maxOffset - offset
	lines := make([]string, rows)
	for y := 0; y < rows; y++ {
		idx := top + y
		switch {
		case idx < 0:
			lines[y] = ""
		case idx < maxOffset:
			line := make(uv.Line, cols)
			for x := 0; x < cols; x++ {
				if c := s.emu.ScrollbackCellAt(x, idx); c != nil {
					line[x] = *c
				} else {
					line[x] = uv.EmptyCell
				}
			}
			lines[y] = line.Render()
		default:
			if gy := idx - maxOffset; gy >= 0 && gy < len(vis) {
				lines[y] = vis[gy]
			}
		}
	}
	return strings.Join(lines, "\n"), offset, maxOffset
}

// LiveFrame returns the live (offset-0) screen, cursor, scrollback length and
// alt-screen flag, rendering only when the session changed since the last call.
// The result is memoized by gen — the dirty counter bumped on every
// screen-changing write — so N clients attached to one session, each ticking at
// 30fps, share a single render instead of each re-rendering an idle screen every
// tick. Clients browsing scrollback (offset > 0) bypass this and call
// RenderScroll directly, since their window depends on a per-client offset.
func (s *Session) LiveFrame() (screen string, cursorX, cursorY, maxOffset int, alt bool) {
	g := s.gen.Load()
	s.fc.mu.Lock()
	defer s.fc.mu.Unlock()
	if s.fc.valid && s.fc.gen == g {
		return s.fc.screen, s.fc.cursorX, s.fc.cursorY, s.fc.maxOff, s.fc.alt
	}
	screen, _, maxOffset = s.RenderScroll(0)
	cursorX, cursorY = s.Cursor()
	alt = s.IsAltScreen()
	// Assign fields individually — replacing the whole struct would copy over
	// the held mutex. gen is read before rendering, so a write landing mid-render
	// just leaves gen ahead of fc.gen and forces a re-render next tick (never a
	// missed update).
	s.fc.gen, s.fc.valid = g, true
	s.fc.screen = screen
	s.fc.cursorX, s.fc.cursorY = cursorX, cursorY
	s.fc.maxOff, s.fc.alt = maxOffset, alt
	return
}

// ExtractText returns plain text for the virtual-buffer range
// (scrollback + visible) [lineStart, lineEnd]. On the start line, text begins at
// colStart; on the end line, text ends at colEnd (exclusive); intermediate lines
// are returned in full. Lines outside the buffer are skipped. Trailing spaces on
// each line are trimmed so block-rectangular padding doesn't bleed into copied
// text.
func (s *Session) ExtractText(lineStart, lineEnd, colStart, colEnd int) string {
	s.mu.RLock()
	rows, cols := s.rows, s.cols
	s.mu.RUnlock()

	maxOff := s.emu.ScrollbackLen()
	last := maxOff + rows - 1
	if lineStart < 0 {
		lineStart = 0
	}
	if lineEnd > last {
		lineEnd = last
	}
	if lineEnd < lineStart {
		return ""
	}
	clampCol := func(c int) int {
		if c < 0 {
			c = 0
		}
		if c > cols {
			c = cols
		}
		return c
	}
	colStart, colEnd = clampCol(colStart), clampCol(colEnd)

	visLines := strings.Split(s.Render(), "\n")
	lineText := func(line int) string {
		if line < maxOff {
			var sb strings.Builder
			for x := 0; x < cols; x++ {
				if c := s.emu.ScrollbackCellAt(x, line); c != nil && c.Content != "" {
					sb.WriteString(c.Content)
				} else {
					sb.WriteByte(' ')
				}
			}
			return sb.String()
		}
		gy := line - maxOff
		if gy < 0 || gy >= len(visLines) {
			return ""
		}
		return ansi.Strip(visLines[gy])
	}
	sliceCols := func(line string, lo, hi int) string {
		r := []rune(line)
		if lo > len(r) {
			lo = len(r)
		}
		if hi > len(r) {
			hi = len(r)
		}
		if hi < lo {
			hi = lo
		}
		return strings.TrimRight(string(r[lo:hi]), " ")
	}

	out := make([]string, 0, lineEnd-lineStart+1)
	for line := lineStart; line <= lineEnd; line++ {
		txt := lineText(line)
		lo, hi := 0, cols
		if line == lineStart {
			lo = colStart
		}
		if line == lineEnd {
			hi = colEnd
		}
		out = append(out, sliceCols(txt, lo, hi))
	}
	return strings.Join(out, "\n")
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
