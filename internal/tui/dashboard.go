package tui

import (
	"fmt"
	"os"
	"sort"
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
	DashAttach
	DashTile
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

	chosen     string
	action     DashAction
	hooksOK    bool
	wantSelect string // pre-select this session id once the list loads
}

// Dashboard runs the central session list. selectID, if non-empty, is the
// session to highlight on entry (e.g. the one just detached from). It returns
// the selected session id and the action the caller should take.
func Dashboard(selectID string) (string, DashAction, error) {
	m := &dashboardModel{hooksOK: hook.Installed(), prev: map[string]string{}, wantSelect: selectID}
	p := tea.NewProgram(m, tea.WithAltScreen())
	res, err := p.Run()
	if err != nil {
		return "", DashQuit, err
	}
	final := res.(*dashboardModel)
	return final.chosen, final.action, nil
}

func (m *dashboardModel) Init() tea.Cmd {
	return tea.Batch(refreshCmd, tick())
}

func refreshCmd() tea.Msg {
	resp, err := ipc.Send(ipc.Request{Type: "list"})
	return sessionsMsg{sessions: resp.Sessions, err: err}
}

func tick() tea.Cmd {
	return tea.Tick(dashRefreshInterval, func(time.Time) tea.Msg { return tickMsg{} })
}

func spawnClaudeCmd() tea.Msg {
	cwd, _ := os.Getwd()
	_, _ = ipc.Send(ipc.Request{Type: "spawn", Argv: []string{"claude"}, Cwd: cwd})
	return refreshCmd()
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
		return m, nil

	case tickMsg:
		m.spin++
		m.expireToasts()
		return m, tea.Batch(refreshCmd, tick())

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
		return m, nil

	case tea.KeyMsg:
		if m.renaming {
			return m.updateRename(msg)
		}
		switch msg.String() {
		case "q", "ctrl+c":
			m.action = DashQuit
			return m, tea.Quit
		case "up", "k":
			if m.cursor > 0 {
				m.cursor--
			}
		case "down", "j":
			if m.cursor < len(m.sessions)-1 {
				m.cursor++
			}
		case "enter":
			if len(m.sessions) > 0 {
				m.chosen = m.sessions[m.cursor].ID
				m.action = DashAttach
				return m, tea.Quit
			}
		case "t":
			if len(m.sessions) > 0 {
				m.action = DashTile
				return m, tea.Quit
			}
		case "n":
			return m, spawnClaudeCmd
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
	var newest int64 = -1
	for _, s := range sessions {
		if s.Status == "needs_approval" {
			count++
			if s.StatusSince > newest {
				newest = s.StatusSince
				latestID = s.ID
			}
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

func (m *dashboardModel) View() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("command-center") + "  ")
	b.WriteString(helpStyle.Render(fmt.Sprintf("%d session(s)", len(m.sessions))) + "\n\n")

	if m.errMsg != "" {
		b.WriteString(statusStyle["needs_approval"].Render("daemon: "+m.errMsg) + "\n\n")
	}

	if !m.hooksOK {
		b.WriteString(statusStyle["waiting_user"].Render("⚠ hooks not installed — status stays \"starting\". run: cb install-hooks") + "\n\n")
	}

	if len(m.sessions) == 0 {
		b.WriteString(helpStyle.Render("no sessions — press n to start a claude session\n"))
	}

	// Surface needs-approval sessions first conceptually by ordering display,
	// but keep stable ids; only sort a copy for the attention summary.
	attention := attentionList(m.sessions)
	if len(attention) > 0 {
		b.WriteString(statusStyle["needs_approval"].Render("⚑ needs you: ") + strings.Join(attention, ", ") + "\n\n")
	}

	for i, s := range m.sessions {
		cursor := "  "
		if i == m.cursor {
			cursor = "▶ "
		}
		line := fmt.Sprintf("%s%s %s  %-8s  %s", cursor, m.indicator(s.Status), badge(s.Status), s.ID[:8], displayLabel(s))
		if i == m.cursor {
			line = selStyle.Render(line)
		}
		b.WriteString(line + "\n")
		if s.Status == "needs_approval" && s.LastMessage != "" {
			b.WriteString("        " + msgStyle.Render(s.LastMessage) + "\n")
		}
	}

	if m.renaming {
		b.WriteString("\n" + titleStyle.Render("rename: ") + m.renameBuf + "▎" +
			"  " + helpStyle.Render("enter save · esc cancel"))
	} else {
		b.WriteString("\n" + helpStyle.Render("↑/↓ select · enter attach · t tile all · n new · x kill · R rename · r refresh · q quit"))
	}
	return m.withToasts(b.String())
}

const toastColWidth = 34

// withToasts places the active toast stack in a right-hand column when there's
// room; otherwise returns the body unchanged.
func (m *dashboardModel) withToasts(body string) string {
	if len(m.toasts) == 0 || m.w < 60 {
		return body
	}
	leftW := m.w - toastColWidth - 1
	left := lipgloss.NewStyle().Width(leftW).Render(body)
	return lipgloss.JoinHorizontal(lipgloss.Top, left, " ", m.renderToasts())
}

func (m *dashboardModel) renderToasts() string {
	boxes := make([]string, 0, len(m.toasts))
	for _, t := range m.toasts {
		color := lipgloss.Color("8")
		if s, ok := statusStyle[t.level]; ok {
			color = s.GetForeground().(lipgloss.Color)
		}
		box := lipgloss.NewStyle().
			Border(lipgloss.RoundedBorder()).
			BorderForeground(color).
			Width(toastColWidth - 2).
			Render(t.text)
		boxes = append(boxes, box)
	}
	return lipgloss.JoinVertical(lipgloss.Left, boxes...)
}

// displayLabel shows the user-assigned name when set (with the command faintly
// appended for context), otherwise just the command.
func displayLabel(s ipc.SessionInfo) string {
	if s.Name != "" {
		return titleStyle.Render(s.Name) + "  " + helpStyle.Render(cmdLabel(s))
	}
	return cmdLabel(s)
}

func cmdLabel(s ipc.SessionInfo) string {
	label := strings.Join(s.Argv, " ")
	if len(label) > 40 {
		label = label[:39] + "…"
	}
	if s.Cwd != "" {
		label += "  " + helpStyle.Render("("+lastPath(s.Cwd)+")")
	}
	return label
}

func lastPath(p string) string {
	parts := strings.Split(strings.TrimRight(p, "/"), "/")
	if len(parts) == 0 {
		return p
	}
	return parts[len(parts)-1]
}

func attentionList(sessions []ipc.SessionInfo) []string {
	var out []string
	for _, s := range sessions {
		if s.Status == "needs_approval" {
			out = append(out, s.ID[:8])
		}
	}
	sort.Strings(out)
	return out
}
