package tui

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"

	"command-center/internal/hook"
	"command-center/internal/ipc"
)

// DashAction is what the dashboard asks the caller to do after it exits.
type DashAction int

const (
	DashQuit DashAction = iota
)

// focusZone is which pane currently receives keystrokes: the sidebar (list
// navigation + dashboard commands) or the screen pane (forwarded to the
// session as raw input).
type focusZone int

const (
	focusSidebar focusZone = iota
	focusScreen
)

const dashRefreshInterval = 500 * time.Millisecond

type tickMsg struct{}
type sessionsMsg struct {
	sessions []ipc.SessionInfo
	err      error
}

// spawnedMsg carries the id of a session just created via n/c so the next list
// refresh selects it and the screen pane takes focus.
type spawnedMsg struct{ id string }

type toast struct {
	text  string
	level string // status key for coloring
	born  time.Time
}

const toastTTL = 6 * time.Second

type dashboardModel struct {
	sessions []ipc.SessionInfo
	cursor   int
	errMsg   string
	w, h     int

	prev   map[string]string // last seen status per session id (for transition detection)
	toasts []toast
	spin   int

	renaming  bool   // capturing keystrokes into renameBuf instead of navigating
	renameID  string // session being renamed
	renameBuf string

	action     DashAction
	hooksOK    bool
	wantSelect string // pre-select this session id once the list loads

	focus  focusZone // sidebar navigation vs. screen input
	prefix bool      // ctrl+a pressed; next key is a command

	// live screen of the selected session (right pane). When focus==focusScreen,
	// keystrokes are forwarded to this session over the same connection.
	streamID string   // session currently streamed into the right pane
	screen   string   // latest rendered frame (kept across switches to avoid flicker)
	gone     bool     // streamed session ended
	conn     net.Conn // attach stream (nil when none)
	ch       chan previewMsg
	paneW    int // inner cols of the screen pane (the session is sized to this)
	paneH    int // inner rows available for the session screen

	// scrolling: scrollMode freezes the screen pane and shows scrollback; the
	// daemon renders the window at scrollOff lines up from the live bottom and
	// reports scrollMax (how far up it can go). sidebarTop is the first visible
	// session row so a long list scrolls to keep the cursor in view.
	scrollMode bool
	scrollOff  int
	scrollMax  int
	sidebarTop int

	// frozen freezes the whole view for text selection: while set, View returns a
	// cached snapshot so Bubble Tea writes nothing to the terminal, letting the
	// host terminal keep a native drag-selection alive. Sessions keep running
	// underneath; we just stop painting. See freeze/unfreeze.
	frozen     bool
	frozenView string
}

// previewMsg carries a frame from the screen-pane attach stream. id tags which
// session it came from so frames from a just-closed connection are ignored.
type previewMsg struct {
	id     string
	screen string
	gone   bool
	offset int
	max    int
}

// Dashboard runs the unified two-zone view: a session list on the left and the
// selected session's live screen on the right. selectID, if non-empty, is the
// session to highlight on entry. It returns the action the caller should take.
func Dashboard(selectID string) (DashAction, error) {
	m := &dashboardModel{
		hooksOK:    hook.Installed(),
		prev:       map[string]string{},
		wantSelect: selectID,
		ch:         make(chan previewMsg, 64),
	}
	// No mouse capture: leaving the terminal's native mouse handling alone keeps
	// text selection / copy working. Scrollback is browsed via the keyboard
	// scroll mode (prefix [) instead. Alt-screen is requested via the View now
	// (v2 moved terminal feature flags out of program options).
	p := tea.NewProgram(m)
	res, err := p.Run()
	if m.conn != nil {
		_ = m.conn.Close()
	}
	if err != nil {
		return DashQuit, err
	}
	final := res.(*dashboardModel)
	return final.action, nil
}

func (m *dashboardModel) Init() tea.Cmd {
	return tea.Batch(refreshCmd, tick(), m.waitFrame())
}

// waitFrame blocks on the shared screen channel and surfaces the next frame as
// a message. It's re-armed after each frame, so a single long-lived loop serves
// whichever session is currently selected.
func (m *dashboardModel) waitFrame() tea.Cmd {
	return func() tea.Msg { return <-m.ch }
}

// selectedID is the id of the session under the cursor, or "" if none.
func (m *dashboardModel) selectedID() string {
	if m.cursor >= 0 && m.cursor < len(m.sessions) {
		return m.sessions[m.cursor].ID
	}
	return ""
}

// displayName is the label shown for a session: the user-assigned name if set,
// otherwise the basename of the directory it was started in (e.g. a session
// launched in ~/Projects/command-center shows as "command-center"), falling
// back to a short id.
func displayName(s ipc.SessionInfo) string {
	if s.Name != "" {
		return s.Name
	}
	if base := folderBase(s.Cwd); base != "" {
		return base
	}
	if len(s.ID) >= 8 {
		return s.ID[:8]
	}
	return s.ID
}

// folderBase returns the last path segment of a directory path.
func folderBase(dir string) string {
	dir = strings.TrimRight(dir, "/")
	if dir == "" {
		return ""
	}
	if i := strings.LastIndex(dir, "/"); i >= 0 {
		return dir[i+1:]
	}
	return dir
}

func (m *dashboardModel) sessionByID(id string) *ipc.SessionInfo {
	for i := range m.sessions {
		if m.sessions[i].ID == id {
			return &m.sessions[i]
		}
	}
	return nil
}

// syncStream ensures the screen pane is attached to the currently selected
// session: if the selection changed, it tears down the old stream and opens a
// new one. The session is resized to the pane so its render fits. The old
// screen is intentionally kept on display until the first frame of the new
// session arrives, so switching sessions doesn't flash a blank pane.
func (m *dashboardModel) syncStream() {
	id := m.selectedID()
	if id == m.streamID && (id == "" || m.conn != nil) {
		return
	}
	if m.conn != nil {
		_ = m.conn.Close()
		m.conn = nil
	}
	m.streamID = id
	m.gone = false
	// Scroll position is per-session; reset to live when switching.
	m.scrollMode = false
	m.scrollOff = 0
	m.scrollMax = 0
	if id == "" {
		m.screen = ""
		return
	}
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return
	}
	req := ipc.Request{Type: "attach", ID: id}
	if m.paneW > 0 && m.paneH > 0 {
		req.Rows, req.Cols = m.paneH, m.paneW
	}
	if err := ipc.WriteJSON(conn, req); err != nil {
		_ = conn.Close()
		return
	}
	m.conn = conn
	go previewReadLoop(id, conn, m.ch)
}

// sendInput forwards raw bytes to the streamed session (used when the screen
// pane has focus).
func (m *dashboardModel) sendInput(b []byte) {
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "input", Data: base64.StdEncoding.EncodeToString(b)})
	}
}

// sendPaste forwards pasted text as a single paste event so the daemon can
// deliver it to the session as one bracketed paste.
func (m *dashboardModel) sendPaste(text string) {
	if m.conn != nil && text != "" {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "paste", Data: base64.StdEncoding.EncodeToString([]byte(text))})
	}
}

// previewReadLoop pumps a screen attach stream into ch until the connection
// closes (which happens when we switch away or the session ends).
func previewReadLoop(id string, conn net.Conn, ch chan previewMsg) {
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	for sc.Scan() {
		var d ipc.StreamDown
		if json.Unmarshal(sc.Bytes(), &d) != nil {
			continue
		}
		if d.Type == "gone" {
			ch <- previewMsg{id: id, gone: true}
			return
		}
		ch <- previewMsg{id: id, screen: d.Screen, offset: d.Offset, max: d.MaxOffset}
	}
}

const sidebarWidth = 22

// chromeRows is the number of non-pane rows the View always renders below the
// body: a reserved toast row, an optional hooks-warning row, and the help row.
// The title now lives at the top of the sidebar (not a full-width header), so
// both panes span the full height above this chrome. Keeping this exact stops
// the View from growing taller than the terminal and clipping content.
func (m *dashboardModel) chromeRows() int {
	rows := 2 // reserved toast line + help line
	if !m.hooksOK {
		rows++ // hooks-not-installed warning
	}
	return rows
}

// relayoutStream recomputes the screen pane size from the window size and, if
// it changed, tells the currently streamed session to resize to match.
func (m *dashboardModel) relayoutStream() {
	// width: window minus sidebar, the pane's left border, and its left padding.
	innerW := m.w - sidebarWidth - 3
	// height: the full pane height — everything left after the surrounding chrome.
	innerH := m.h - m.chromeRows()
	if innerW < 1 {
		innerW = 1
	}
	if innerH < 1 {
		innerH = 1
	}
	if innerW == m.paneW && innerH == m.paneH {
		return
	}
	m.paneW, m.paneH = innerW, innerH
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "resize", Rows: innerH, Cols: innerW})
	}
}

func refreshCmd() tea.Msg {
	resp, err := ipc.Send(ipc.Request{Type: "list"})
	return sessionsMsg{sessions: resp.Sessions, err: err}
}

func tick() tea.Cmd {
	return tea.Tick(dashRefreshInterval, func(time.Time) tea.Msg { return tickMsg{} })
}

// spawnCmd starts a new session running bin (e.g. "claude" or "codex") in the
// dashboard's working directory. The session is spawned at the current screen
// pane size so the child paints itself once at the right width — otherwise it
// would paint at the daemon's default size and then repaint after the attach
// resize, leaving overlapping/garbled output (e.g. "Claude CodClaude Code"). On
// success it reports the new session's id so the dashboard can select and focus it.
func (m *dashboardModel) spawnCmd(bin string) tea.Cmd {
	rows, cols := m.paneH, m.paneW
	return func() tea.Msg {
		cwd, _ := os.Getwd()
		req := ipc.Request{Type: "spawn", Argv: []string{bin}, Cwd: cwd}
		if rows > 0 && cols > 0 {
			req.Rows, req.Cols = rows, cols
		}
		resp, err := ipc.Send(req)
		if err != nil || !resp.OK {
			return refreshCmd()
		}
		return spawnedMsg{id: resp.ID}
	}
}

func killCmd(id string) tea.Cmd {
	return func() tea.Msg {
		_, _ = ipc.Send(ipc.Request{Type: "kill", ID: id})
		return refreshCmd()
	}
}

func renameCmd(id, name string) tea.Cmd {
	return func() tea.Msg {
		_, _ = ipc.Send(ipc.Request{Type: "rename", ID: id, Name: name})
		return refreshCmd()
	}
}

func (m *dashboardModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.w, m.h = msg.Width, msg.Height
		// A resize repaints the whole terminal and clears any selection anyway, so
		// drop the frozen snapshot rather than show it at the wrong size.
		m.frozen, m.frozenView = false, ""
		m.relayoutStream()
		return m, nil

	case tickMsg:
		m.spin++
		m.expireToasts()
		return m, tea.Batch(refreshCmd, tick())

	case spawnedMsg:
		// Select the just-created session and drop straight into it so you can
		// start typing; the refresh picks it up and syncStream attaches.
		m.wantSelect = msg.id
		m.focus = focusScreen
		return m, refreshCmd

	case previewMsg:
		if msg.id == m.streamID {
			if msg.gone {
				m.gone = true
				m.focus = focusSidebar // can't type into a dead session
				m.scrollMode = false
			} else {
				m.screen = msg.screen
				m.scrollMax = msg.max
				if m.scrollOff > m.scrollMax {
					m.scrollOff = m.scrollMax
					m.sendScroll()
				}
			}
		}
		return m, m.waitFrame()

	case sessionsMsg:
		if msg.err != nil {
			m.errMsg = msg.err.Error()
		} else {
			m.errMsg = ""
			m.detectTransitions(msg.sessions)
			m.sessions = msg.sessions
		}
		if m.wantSelect != "" {
			for i, s := range m.sessions {
				if s.ID == m.wantSelect {
					m.cursor = i
					break
				}
			}
			m.wantSelect = ""
		}
		if m.cursor >= len(m.sessions) {
			m.cursor = max(0, len(m.sessions)-1)
		}
		m.syncStream()
		return m, nil

	case tea.PasteMsg:
		// Bracketed paste is its own message in v2. Forward it to the focused
		// session as a single paste so the daemon wraps it in paste markers.
		if m.focus == focusScreen && !m.scrollMode && !m.frozen {
			m.sendPaste(msg.Content)
		}
		return m, nil

	case tea.KeyPressMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

// handleKey routes a keystroke through the rename prompt, the ctrl+a prefix,
// and then either the screen pane (forwarded as input) or the sidebar.
func (m *dashboardModel) handleKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	if m.renaming {
		return m.updateRename(msg)
	}
	// The prefix is honored from any mode, so ctrl+a q / ctrl+a [ always work.
	if m.prefix {
		m.prefix = false
		return m.handlePrefix(msg)
	}
	if msg.String() == prefixKeyName {
		m.prefix = true
		return m, nil
	}
	// While frozen, swallow everything except an explicit unfreeze so a stray
	// keystroke can't type into the session or move the cursor (which would
	// repaint and clear the selection you're trying to copy).
	if m.frozen {
		switch msg.String() {
		case "esc", "q":
			m.unfreeze()
		}
		return m, nil
	}
	if m.scrollMode {
		return m.handleScrollKey(msg)
	}
	if m.focus == focusScreen {
		// Bracketed paste arrives as a separate tea.PasteMsg (handled in Update),
		// so here we only forward genuine key presses as raw bytes.
		if b := keyToBytes(msg); b != nil {
			m.sendInput(b)
		}
		return m, nil
	}
	return m.handleSidebarKey(msg)
}

// handlePrefix handles the key following ctrl+a: switch focus between the
// sidebar and the screen pane, jump to a pending session, or pass a literal
// ctrl+a through to the focused session.
func (m *dashboardModel) handlePrefix(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "left", "h":
		m.focus = focusSidebar
	case "right", "l":
		if m.streamID != "" && !m.gone {
			m.focus = focusScreen
		}
	case "[":
		if m.scrollMode {
			m.exitScroll()
		} else {
			m.enterScroll()
		}
	case "f":
		if m.frozen {
			m.unfreeze()
		} else {
			m.freeze()
		}
	case "enter":
		// Insert a newline into the focused session without submitting. This works
		// on every terminal because we inject the newline ourselves (as a paste)
		// rather than relying on the terminal being able to send a distinct
		// shift+enter, which legacy terminals can't.
		if m.streamID != "" && !m.gone {
			m.sendPaste("\n")
		}
	case "n":
		return m, m.spawnCmd("claude")
	case "c":
		return m, m.spawnCmd("codex")
	case "q":
		m.action = DashQuit
		return m, tea.Quit
	case "x":
		// Kill the current session (works while typing into it). Drop focus back
		// to the sidebar; the stream's "gone" notice will tidy up the pane.
		if m.streamID != "" {
			id := m.streamID
			m.focus = focusSidebar
			return m, killCmd(id)
		}
	case "g":
		if _, latest := pendingSummary(m.sessions, ""); latest != "" {
			for i, s := range m.sessions {
				if s.ID == latest {
					m.cursor = i
					break
				}
			}
			m.syncStream()
			m.focus = focusScreen
		}
	}
	return m, nil
}

// scrollPage is roughly one screenful, used for pgup/pgdn.
func (m *dashboardModel) scrollPage() int { return maxInt(m.paneH-1, 1) }

// enterScroll freezes the screen pane and switches into scrollback browsing.
// It's a no-op when there's no live session to scroll.
func (m *dashboardModel) enterScroll() {
	if m.streamID == "" || m.gone || m.conn == nil {
		return
	}
	m.scrollMode = true
}

// exitScroll returns the screen pane to following the live bottom.
func (m *dashboardModel) exitScroll() {
	m.scrollMode = false
	m.scrollOff = 0
	m.sendScroll()
}

// freeze snapshots the current view and stops repainting so the host terminal
// keeps a native text selection alive (sessions keep running underneath; we just
// stop drawing). The snapshot is taken with frozen already set so it carries the
// "FROZEN" help line, and View returns the identical string every call after —
// Bubble Tea then diffs to a no-op and writes nothing.
func (m *dashboardModel) freeze() {
	m.frozen = true
	m.frozenView = m.renderLive()
}

// unfreeze resumes live painting; the next View reflects current state.
func (m *dashboardModel) unfreeze() {
	m.frozen = false
	m.frozenView = ""
}

// scrollBy moves the scroll position by delta lines (positive = toward older
// output), clamps to the daemon-reported bounds, and pushes the new offset.
func (m *dashboardModel) scrollBy(delta int) {
	m.scrollOff += delta
	if m.scrollOff < 0 {
		m.scrollOff = 0
	}
	if m.scrollOff > m.scrollMax {
		m.scrollOff = m.scrollMax
	}
	m.sendScroll()
}

// sendScroll tells the daemon which scrollback window to render.
func (m *dashboardModel) sendScroll() {
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "scroll", Offset: m.scrollOff})
	}
}

// handleScrollKey handles keystrokes while browsing scrollback in the screen pane.
func (m *dashboardModel) handleScrollKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "up", "k":
		m.scrollBy(1)
	case "down", "j":
		m.scrollBy(-1)
	case "pgup", "b":
		m.scrollBy(m.scrollPage())
	case "pgdown", "f", "space", " ":
		m.scrollBy(-m.scrollPage())
	case "g", "home":
		m.scrollBy(m.scrollMax) // oldest
	case "G", "end":
		m.scrollBy(-m.scrollMax) // back to live
	case "esc", "q", "ctrl+c":
		m.exitScroll()
	}
	return m, nil
}

// handleSidebarKey handles navigation and dashboard commands while the sidebar
// has focus.
func (m *dashboardModel) handleSidebarKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "ctrl+c":
		m.action = DashQuit
		return m, tea.Quit
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
			m.syncStream()
		}
	case "down", "j":
		if m.cursor < len(m.sessions)-1 {
			m.cursor++
			m.syncStream()
		}
	case "enter", "right", "l":
		if m.streamID != "" && !m.gone {
			m.focus = focusScreen
		}
	case "n":
		return m, m.spawnCmd("claude")
	case "c":
		return m, m.spawnCmd("codex")
	case "x":
		if len(m.sessions) > 0 {
			return m, killCmd(m.sessions[m.cursor].ID)
		}
	case "R":
		if len(m.sessions) > 0 {
			s := m.sessions[m.cursor]
			m.renaming = true
			m.renameID = s.ID
			m.renameBuf = displayName(s)
		}
	case "r":
		return m, refreshCmd
	}
	return m, nil
}

// updateRename handles keystrokes while the rename prompt is active: enter
// commits, esc cancels, backspace deletes, and printable runes are appended.
func (m *dashboardModel) updateRename(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch {
	case msg.Code == tea.KeyEnter:
		id, name := m.renameID, strings.TrimSpace(m.renameBuf)
		m.renaming, m.renameID, m.renameBuf = false, "", ""
		return m, renameCmd(id, name)
	case msg.Code == tea.KeyEscape, msg.Code == 'c' && msg.Mod&tea.ModCtrl != 0:
		m.renaming, m.renameID, m.renameBuf = false, "", ""
		return m, nil
	case msg.Code == tea.KeyBackspace, msg.Code == tea.KeyDelete:
		if r := []rune(m.renameBuf); len(r) > 0 {
			m.renameBuf = string(r[:len(r)-1])
		}
		return m, nil
	case msg.Text != "":
		// Printable input (includes space, whose Text is " ").
		m.renameBuf += msg.Text
		return m, nil
	}
	return m, nil
}

var (
	titleStyle  = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("12"))
	helpStyle   = lipgloss.NewStyle().Faint(true)
	statusStyle = map[string]lipgloss.Style{
		"needs_approval": lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("9")), // red
		"waiting_user":   lipgloss.NewStyle().Foreground(lipgloss.Color("11")),           // yellow
		"working":        lipgloss.NewStyle().Foreground(lipgloss.Color("10")),           // green
		"starting":       lipgloss.NewStyle().Foreground(lipgloss.Color("14")),           // cyan
		"idle":           lipgloss.NewStyle().Faint(true),
		"ended":          lipgloss.NewStyle().Faint(true).Foreground(lipgloss.Color("8")), // grey
	}
)

var spinnerFrames = []string{"⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"}

// indicator returns a short, colored glyph that conveys status at a glance: a
// spinner while working, a flag when approval is needed, a dot otherwise.
func (m *dashboardModel) indicator(status string) string {
	st := statusStyle[status]
	switch status {
	case "working":
		return st.Render(spinnerFrames[m.spin%len(spinnerFrames)])
	case "needs_approval":
		return st.Render("⚑")
	case "waiting_user":
		return st.Render("●")
	case "starting":
		return st.Render("…")
	case "ended":
		return st.Render("✗")
	default:
		return st.Render("•")
	}
}

// detectTransitions compares incoming statuses to the last-seen ones and raises
// a toast when a session crosses into needs_approval or finishes its turn.
func (m *dashboardModel) detectTransitions(next []ipc.SessionInfo) {
	seen := make(map[string]bool, len(next))
	for _, s := range next {
		seen[s.ID] = true
		old := m.prev[s.ID]
		if s.Status != old {
			switch s.Status {
			case "needs_approval":
				txt := s.LastMessage
				if txt == "" {
					txt = "needs your approval"
				}
				m.pushToast("⚑ "+s.ID[:8]+" — "+txt, "needs_approval")
			case "waiting_user":
				if old != "" { // don't toast the very first observation
					m.pushToast("● "+s.ID[:8]+" — ready for input", "waiting_user")
				}
			}
		}
		m.prev[s.ID] = s.Status
	}
	for id := range m.prev {
		if !seen[id] {
			delete(m.prev, id)
		}
	}
}

// latestPending returns how many sessions need approval and the id of the one
// that entered needs_approval most recently (the "latest" to jump to).
func latestPending(sessions []ipc.SessionInfo) (count int, latestID string) {
	return pendingSummary(sessions, "")
}

// pendingSummary is latestPending with an exclusion: the session you're already
// viewing (excludeID) is left out of both the count and the jump target, so the
// banner doesn't nag you about an approval screen you're already looking at —
// while still counting any other sessions that need approval.
func pendingSummary(sessions []ipc.SessionInfo, excludeID string) (count int, latestID string) {
	var newest int64 = -1
	for _, s := range sessions {
		if s.Status != "needs_approval" || s.ID == excludeID {
			continue
		}
		count++
		if s.StatusSince > newest {
			newest = s.StatusSince
			latestID = s.ID
		}
	}
	return count, latestID
}

func (m *dashboardModel) pushToast(text, level string) {
	m.toasts = append(m.toasts, toast{text: text, level: level, born: time.Now()})
	if len(m.toasts) > 5 {
		m.toasts = m.toasts[len(m.toasts)-5:]
	}
}

func (m *dashboardModel) expireToasts() {
	kept := m.toasts[:0]
	for _, t := range m.toasts {
		if time.Since(t.born) < toastTTL {
			kept = append(kept, t)
		}
	}
	m.toasts = kept
}

var (
	screenBorderStyle = lipgloss.NewStyle().
				Border(lipgloss.NormalBorder(), false, false, false, true).
				BorderForeground(lipgloss.Color("8")).
				PaddingLeft(1)
	screenFocusBorderStyle  = screenBorderStyle.BorderForeground(lipgloss.Color("12"))
	screenScrollBorderStyle = screenBorderStyle.BorderForeground(lipgloss.Color("13")) // magenta: browsing scrollback
	rowStyle                = lipgloss.NewStyle().Width(sidebarWidth)
	selBarStyle             = lipgloss.NewStyle().Foreground(lipgloss.Color("11")) // yellow
	selBarDimStyle          = lipgloss.NewStyle().Foreground(lipgloss.Color("8"))  // grey
)

func (m *dashboardModel) View() tea.View {
	// While frozen, return the cached snapshot unchanged so Bubble Tea writes
	// nothing and the host terminal keeps the user's selection. The View struct
	// must be identical each call for the renderer to diff to a no-op, so the
	// same AltScreen flag is set on both paths.
	content := m.frozenView
	if !m.frozen {
		content = m.renderLive()
	}
	v := tea.NewView(content)
	v.AltScreen = true
	return v
}

func (m *dashboardModel) renderLive() string {
	// The title lives at the top of the sidebar (renderSidebar), so the screen
	// pane spans the full height beside it — no full-width header band.
	body := lipgloss.JoinHorizontal(lipgloss.Top, m.renderSidebar(), m.renderScreen())

	// Always reserve the toast row (even when empty) so the View height is
	// constant and never overflows the terminal — see chromeRows.
	out := body + "\n" + m.toastLine()
	if !m.hooksOK {
		out += "\n" + statusStyle["waiting_user"].Render("⚠ hooks not installed — run: cb install-hooks")
	}
	p := prefixLabel()
	switch {
	case m.frozen:
		out += "\n" + statusStyle["needs_approval"].Render("❄ FROZEN") + helpStyle.Render(" — drag to select, copy, then esc/q to resume")
	case m.renaming:
		out += "\n" + titleStyle.Render("rename: ") + m.renameBuf + "▎  " + helpStyle.Render("enter save · esc cancel")
	case m.focus == focusScreen || m.scrollMode:
		out += "\n" + helpStyle.Render(fmt.Sprintf("prefix ← sidebar · prefix ⏎ newline · prefix [ scroll · prefix f freeze/copy · prefix n/c new · prefix x kill · prefix q quit  (prefix = %s)", p))
	default:
		out += "\n" + helpStyle.Render(fmt.Sprintf("↑/↓ select · enter/prefix → focus · prefix [ scroll · prefix f freeze/copy · n claude · c codex · x kill · R rename · prefix q quit  (prefix = %s)", p))
	}
	return out
}

// renderSidebar draws the narrow left column: one row per session (status glyph
// + name), the cursor row highlighted, and a count footer. The highlight is
// reversed when the sidebar has focus, dimmer when the screen pane does.
func (m *dashboardModel) renderSidebar() string {
	var rows []string
	for i, s := range m.sessions {
		// Mark the selected row with a colored left bar instead of a full-row
		// highlight: bright yellow when the sidebar has focus, grey when the
		// screen pane does (selection still visible, but clearly not active).
		gutter := " "
		if i == m.cursor {
			if m.focus == focusSidebar {
				gutter = selBarStyle.Render("▌")
			} else {
				gutter = selBarDimStyle.Render("▌")
			}
		}
		row := gutter + m.indicator(s.Status) + " " + truncate(displayName(s), sidebarWidth-3)
		rows = append(rows, rowStyle.Render(row))
	}
	if len(rows) == 0 {
		rows = append(rows, helpStyle.Render(" no sessions"), helpStyle.Render(" press n"))
	}

	// The sidebar carries the app title at the top, then the session list, then a
	// footer at the bottom. errMsg (a daemon problem) rides under the title.
	header := titleStyle.Render("codebridge")
	if m.errMsg != "" {
		header += "\n" + statusStyle["needs_approval"].Render(truncate("daemon: "+m.errMsg, sidebarWidth-1))
	}
	headerH := strings.Count(header, "\n") + 1

	// The list is a window that scrolls to keep the cursor visible. It gets
	// whatever height is left after the header, a blank spacer on each side, and
	// the footer row.
	maxRows := maxInt(m.paneH-headerH-3, 1)
	top := clampTop(m.cursor, m.sidebarTop, len(rows), maxRows)
	m.sidebarTop = top
	end := minInt(top+maxRows, len(rows))
	list := strings.Join(rows[top:end], "\n")
	footer := helpStyle.Render(fmt.Sprintf(" %d session(s)", len(m.sessions)))
	if len(m.sessions) > maxRows {
		footer = helpStyle.Render(fmt.Sprintf(" %d-%d of %d", top+1, end, len(m.sessions)))
	}
	content := firstLines(lipgloss.JoinVertical(lipgloss.Left, header, "", list, "", footer), maxInt(m.paneH, 1))
	return lipgloss.NewStyle().Width(sidebarWidth).Height(maxInt(m.paneH, 1)).Render(content)
}

// renderScreen draws the right pane: just the selected session's live screen
// (the session is sized to fill this pane). Focus is shown by the border color;
// the session's own status/title lives in the sidebar, so there's no header
// here. Keystrokes are forwarded to the session when this pane has focus.
func (m *dashboardModel) renderScreen() string {
	var screen string
	switch {
	case m.streamID == "":
		screen = helpStyle.Render("no session selected")
	case m.gone:
		screen = helpStyle.Render("(session ended)")
	case m.screen == "":
		screen = helpStyle.Render("loading…")
	default:
		screen = m.screen
	}
	// Bound the screen to the pane height so a tall session render can't overflow
	// the View and clip the top (which would hide the session list).
	screen = lastLines(screen, m.paneH)
	border := screenBorderStyle
	switch {
	case m.scrollMode:
		border = screenScrollBorderStyle
	case m.focus == focusScreen:
		border = screenFocusBorderStyle
	}
	// lipgloss Width includes padding, and the style has PaddingLeft(1); add it
	// back so the content area is exactly paneW (matching the session cols) and a
	// full-width line doesn't wrap into an extra row.
	return border.Width(maxInt(m.paneW+1, 1)).Height(maxInt(m.paneH, 1)).Render(screen)
}

// toastLine renders any active toasts as a single compact line beneath the body.
func (m *dashboardModel) toastLine() string {
	if len(m.toasts) == 0 {
		return ""
	}
	parts := make([]string, 0, len(m.toasts))
	for _, t := range m.toasts {
		style := helpStyle
		if s, ok := statusStyle[t.level]; ok {
			style = s
		}
		parts = append(parts, style.Render(t.text))
	}
	return strings.Join(parts, helpStyle.Render("  ·  "))
}

// truncate shortens s to at most n runes, adding an ellipsis when cut. It
// assumes s has no ANSI escapes (true for plain names/ids).
func truncate(s string, n int) string {
	if n < 1 {
		n = 1
	}
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n == 1 {
		return "…"
	}
	return string(r[:n-1]) + "…"
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// clampTop returns the first visible row index for a scrolling list window: it
// keeps `cursor` within the `maxRows`-tall window starting near `top`, without
// scrolling past the end of a list of `count` rows.
func clampTop(cursor, top, count, maxRows int) int {
	if maxRows < 1 {
		maxRows = 1
	}
	if cursor < top {
		top = cursor
	}
	if cursor >= top+maxRows {
		top = cursor - maxRows + 1
	}
	if maxTop := maxInt(0, count-maxRows); top > maxTop {
		top = maxTop
	}
	if top < 0 {
		top = 0
	}
	return top
}

// lastLines keeps at most the final n lines of s (like a terminal showing the
// bottom of the scrollback). Used to bound the live screen so a tall session
// render can't push the whole View past the terminal height and clip the list.
func lastLines(s string, n int) string {
	if n <= 0 {
		return ""
	}
	lines := strings.Split(s, "\n")
	if len(lines) > n {
		lines = lines[len(lines)-n:]
	}
	return strings.Join(lines, "\n")
}

// firstLines keeps at most the first n lines of s.
func firstLines(s string, n int) string {
	if n <= 0 {
		return ""
	}
	lines := strings.Split(s, "\n")
	if len(lines) > n {
		lines = lines[:n]
	}
	return strings.Join(lines, "\n")
}
