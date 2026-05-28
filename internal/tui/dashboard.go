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

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"command-center/internal/hook"
	"command-center/internal/ipc"
)

// DashAction is what the dashboard asks the caller to do after it exits.
type DashAction int

const (
	DashQuit DashAction = iota
	DashTile
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
}

// previewMsg carries a frame from the screen-pane attach stream. id tags which
// session it came from so frames from a just-closed connection are ignored.
type previewMsg struct {
	id     string
	screen string
	gone   bool
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
	p := tea.NewProgram(m, tea.WithAltScreen())
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
		ch <- previewMsg{id: id, screen: d.Screen}
	}
}

const sidebarWidth = 22

// chromeRows is the number of non-screen rows the View always renders: the
// title row, the (conditional) hooks-warning row, a reserved toast row, and the
// help row. The screen pane itself adds two more (its header + divider), which
// relayoutStream subtracts separately. Keeping this exact stops the View from
// growing taller than the terminal and clipping the top of the session list.
func (m *dashboardModel) chromeRows() int {
	rows := 3 // title + reserved toast line + help line
	if !m.hooksOK {
		rows++ // hooks-not-installed warning
	}
	return rows
}

// relayoutStream recomputes the screen pane size from the window size and, if
// it changed, tells the currently streamed session to resize to match.
func (m *dashboardModel) relayoutStream() {
	// width: window minus sidebar, the divider border, and its left padding.
	innerW := m.w - sidebarWidth - 3
	// height: window minus the surrounding chrome and the pane's own
	// header + divider rows.
	innerH := m.h - m.chromeRows() - 2
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
// dashboard's working directory, then refreshes the list.
func spawnCmd(bin string) tea.Cmd {
	return func() tea.Msg {
		cwd, _ := os.Getwd()
		_, _ = ipc.Send(ipc.Request{Type: "spawn", Argv: []string{bin}, Cwd: cwd})
		return refreshCmd()
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
		m.relayoutStream()
		return m, nil

	case tickMsg:
		m.spin++
		m.expireToasts()
		return m, tea.Batch(refreshCmd, tick())

	case previewMsg:
		if msg.id == m.streamID {
			if msg.gone {
				m.gone = true
				m.focus = focusSidebar // can't type into a dead session
			} else {
				m.screen = msg.screen
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

	case tea.KeyMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

// handleKey routes a keystroke through the rename prompt, the ctrl+a prefix,
// and then either the screen pane (forwarded as input) or the sidebar.
func (m *dashboardModel) handleKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	if m.renaming {
		return m.updateRename(msg)
	}
	if m.prefix {
		m.prefix = false
		return m.handlePrefix(msg)
	}
	if msg.String() == prefixKeyName {
		m.prefix = true
		return m, nil
	}
	if m.focus == focusScreen {
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
func (m *dashboardModel) handlePrefix(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "left", "h":
		m.focus = focusSidebar
	case "right", "l":
		if m.streamID != "" && !m.gone {
			m.focus = focusScreen
		}
	case "q":
		m.action = DashQuit
		return m, tea.Quit
	case prefixKeyName, "a":
		if m.focus == focusScreen {
			m.sendInput([]byte{0x01}) // literal Ctrl-a
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

// handleSidebarKey handles navigation and dashboard commands while the sidebar
// has focus.
func (m *dashboardModel) handleSidebarKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
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
	case "t":
		if len(m.sessions) > 0 {
			m.action = DashTile
			return m, tea.Quit
		}
	case "n":
		return m, spawnCmd("claude")
	case "c":
		return m, spawnCmd("codex")
	case "x":
		if len(m.sessions) > 0 {
			return m, killCmd(m.sessions[m.cursor].ID)
		}
	case "R":
		if len(m.sessions) > 0 {
			s := m.sessions[m.cursor]
			m.renaming = true
			m.renameID = s.ID
			m.renameBuf = s.Name
		}
	case "r":
		return m, refreshCmd
	}
	return m, nil
}

// updateRename handles keystrokes while the rename prompt is active: enter
// commits, esc cancels, backspace deletes, and printable runes are appended.
func (m *dashboardModel) updateRename(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.Type {
	case tea.KeyEnter:
		id, name := m.renameID, strings.TrimSpace(m.renameBuf)
		m.renaming, m.renameID, m.renameBuf = false, "", ""
		return m, renameCmd(id, name)
	case tea.KeyEsc, tea.KeyCtrlC:
		m.renaming, m.renameID, m.renameBuf = false, "", ""
		return m, nil
	case tea.KeyBackspace, tea.KeyDelete:
		if r := []rune(m.renameBuf); len(r) > 0 {
			m.renameBuf = string(r[:len(r)-1])
		}
		return m, nil
	case tea.KeySpace:
		m.renameBuf += " "
		return m, nil
	case tea.KeyRunes:
		m.renameBuf += string(msg.Runes)
		return m, nil
	}
	return m, nil
}

var (
	titleStyle  = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("12"))
	selStyle    = lipgloss.NewStyle().Reverse(true)
	helpStyle   = lipgloss.NewStyle().Faint(true)
	msgStyle    = lipgloss.NewStyle().Faint(true).Italic(true)
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

func badge(status string) string {
	if s, ok := statusStyle[status]; ok {
		return s.Render(fmt.Sprintf("%-14s", status))
	}
	return fmt.Sprintf("%-14s", status)
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
	screenFocusBorderStyle = screenBorderStyle.BorderForeground(lipgloss.Color("12"))
	rowStyle               = lipgloss.NewStyle().Width(sidebarWidth)
	selRowStyle            = rowStyle.Reverse(true)
	selRowDimStyle         = rowStyle.Foreground(lipgloss.Color("12"))
)

func (m *dashboardModel) View() string {
	header := titleStyle.Render("command-center") + "  " +
		helpStyle.Render(fmt.Sprintf("%d session(s)", len(m.sessions)))
	if m.errMsg != "" {
		header += "  " + statusStyle["needs_approval"].Render("daemon: "+m.errMsg)
	}
	if !m.hooksOK {
		header += "\n" + statusStyle["waiting_user"].Render("⚠ hooks not installed — run: cb install-hooks")
	}

	body := lipgloss.JoinHorizontal(lipgloss.Top, m.renderSidebar(), m.renderScreen())

	// Always reserve the toast row (even when empty) so the View height is
	// constant and never overflows the terminal — see chromeRows.
	out := header + "\n" + body + "\n" + m.toastLine()
	switch {
	case m.renaming:
		out += "\n" + titleStyle.Render("rename: ") + m.renameBuf + "▎  " + helpStyle.Render("enter save · esc cancel")
	case m.focus == focusScreen:
		out += "\n" + helpStyle.Render("typing into session · ^a ← sidebar · ^a a = literal ^a · ^a q quit")
	default:
		out += "\n" + helpStyle.Render("↑/↓ select · enter/^a→ focus · n claude · c codex · x kill · R rename · t tile · ^a q quit")
	}
	return out
}

// renderSidebar draws the narrow left column: one row per session (status glyph
// + name), the cursor row highlighted, and a count footer. The highlight is
// reversed when the sidebar has focus, dimmer when the screen pane does.
func (m *dashboardModel) renderSidebar() string {
	var rows []string
	for i, s := range m.sessions {
		name := s.Name
		if name == "" {
			name = s.ID[:8]
		}
		row := m.indicator(s.Status) + " " + truncate(name, sidebarWidth-3)
		switch {
		case i != m.cursor:
			rows = append(rows, rowStyle.Render(row))
		case m.focus == focusSidebar:
			rows = append(rows, selRowStyle.Render(row))
		default:
			rows = append(rows, selRowDimStyle.Render(row))
		}
	}
	if len(rows) == 0 {
		rows = append(rows, helpStyle.Render("no sessions"), helpStyle.Render("press n"))
	}
	list := strings.Join(rows, "\n")
	footer := helpStyle.Render(fmt.Sprintf("%d session(s)", len(m.sessions)))
	content := lipgloss.JoinVertical(lipgloss.Left, list, "", footer)
	return lipgloss.NewStyle().Width(sidebarWidth).Height(maxInt(m.paneH+2, 1)).Render(content)
}

// renderScreen draws the right pane: a header for the selected session and its
// live screen (the session is sized to fit this pane). When focused, the pane
// border is highlighted and keystrokes are forwarded to the session.
func (m *dashboardModel) renderScreen() string {
	var head, screen string
	if m.streamID == "" {
		head = helpStyle.Render("no session selected")
		screen = ""
	} else {
		s := m.sessionByID(m.streamID)
		label, status := m.streamID[:8], ""
		if s != nil {
			status = s.Status
			if s.Name != "" {
				label = s.Name
			}
		}
		head = m.indicator(status) + " " + titleStyle.Render(label) + "  " + badge(status)
		if m.focus == focusScreen {
			head += "  " + statusStyle["working"].Render("● input")
		}
		switch {
		case m.gone:
			screen = helpStyle.Render("(session ended)")
		case m.screen == "":
			screen = helpStyle.Render("loading…")
		default:
			screen = m.screen
		}
		if s != nil && s.Status == "needs_approval" && s.LastMessage != "" {
			head += "\n" + msgStyle.Render(s.LastMessage)
		}
	}
	divider := strings.Repeat("─", maxInt(m.paneW, 1))
	content := head + "\n" + helpStyle.Render(divider) + "\n" + screen
	border := screenBorderStyle
	if m.focus == focusScreen {
		border = screenFocusBorderStyle
	}
	return border.Width(maxInt(m.paneW, 1)).Height(maxInt(m.paneH+2, 1)).Render(content)
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
