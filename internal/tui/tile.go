package tui

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"math"
	"net"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"command-center/internal/ipc"
)

// tilePane is one live session shown in the grid: its own attach connection,
// the latest rendered screen, and the pane size we've told the session to use.
type tilePane struct {
	id     string
	info   ipc.SessionInfo
	conn   net.Conn
	screen string
	rows   int
	cols   int
}

type paneEvt struct {
	id string
	d  ipc.StreamDown
}

type tileModel struct {
	panes  []*tilePane
	focus  int
	prefix bool
	w, h   int
	events chan paneEvt

	// layout, recomputed on resize / pane add / remove
	gc, gr         int
	innerW, innerH int

	// pending-approval summary across all daemon sessions (refreshed on tick)
	pendingCount  int
	pendingLatest string
}

const tileStatusRows = 1 // bottom help/status line

// Tile attaches to all current sessions and shows them as a live grid. It
// returns when the user detaches (Ctrl-a d) or all panes close, along with the
// id of the pane that was focused at exit (for the dashboard to highlight).
func Tile() (string, error) {
	resp, err := ipc.Send(ipc.Request{Type: "list"})
	if err != nil {
		return "", err
	}
	if len(resp.Sessions) == 0 {
		return "", fmt.Errorf("no sessions to tile — press n in the dashboard to start one")
	}

	m := &tileModel{events: make(chan paneEvt, 256)}
	for _, s := range resp.Sessions {
		p, err := attachPane(s)
		if err != nil {
			continue
		}
		m.panes = append(m.panes, p)
	}
	if len(m.panes) == 0 {
		return "", fmt.Errorf("could not attach to any session")
	}
	for _, p := range m.panes {
		go m.readPane(p)
	}

	prog := tea.NewProgram(m, tea.WithAltScreen())
	_, err = prog.Run()
	focused := ""
	if m.focus < len(m.panes) {
		focused = m.panes[m.focus].id
	}
	for _, p := range m.panes {
		_ = p.conn.Close()
	}
	return focused, err
}

func attachPane(s ipc.SessionInfo) (*tilePane, error) {
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return nil, err
	}
	if err := ipc.WriteJSON(conn, ipc.Request{Type: "attach", ID: s.ID}); err != nil {
		conn.Close()
		return nil, err
	}
	return &tilePane{id: s.ID, info: s, conn: conn}, nil
}

func (m *tileModel) readPane(p *tilePane) {
	sc := bufio.NewScanner(p.conn)
	sc.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	for sc.Scan() {
		var d ipc.StreamDown
		if json.Unmarshal(sc.Bytes(), &d) == nil {
			m.events <- paneEvt{id: p.id, d: d}
		}
	}
	m.events <- paneEvt{id: p.id, d: ipc.StreamDown{Type: "gone"}}
}

func (m *tileModel) Init() tea.Cmd {
	return tea.Batch(m.waitEvent(), tileTick())
}

func (m *tileModel) waitEvent() tea.Cmd {
	return func() tea.Msg { return <-m.events }
}

func tileTick() tea.Cmd {
	return tea.Tick(time.Second, func(time.Time) tea.Msg { return tileStatusMsg{} })
}

type tileStatusMsg struct{}

func (m *tileModel) paneByID(id string) (int, *tilePane) {
	for i, p := range m.panes {
		if p.id == id {
			return i, p
		}
	}
	return -1, nil
}

func (m *tileModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.w, m.h = msg.Width, msg.Height
		m.relayout()
		return m, nil

	case tileStatusMsg:
		m.refreshStatuses()
		return m, tileTick()

	case paneEvt:
		i, p := m.paneByID(msg.id)
		if p == nil {
			return m, m.waitEvent()
		}
		if msg.d.Type == "gone" {
			m.removePane(i)
			if len(m.panes) == 0 {
				return m, tea.Quit
			}
			return m, m.waitEvent()
		}
		p.screen = msg.d.Screen
		return m, m.waitEvent()

	case tea.KeyMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

func (m *tileModel) handleKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	if m.prefix {
		m.prefix = false
		return m.handlePrefixCommand(msg)
	}
	if msg.String() == prefixKeyName {
		m.prefix = true
		return m, nil
	}
	// Forward to the focused pane.
	if len(m.panes) > 0 {
		if b := keyToBytes(msg); b != nil {
			m.sendInput(m.panes[m.focus], b)
		}
	}
	return m, nil
}

func (m *tileModel) handlePrefixCommand(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "d", "q":
		return m, tea.Quit
	case prefixKeyName, "a":
		if len(m.panes) > 0 {
			m.sendInput(m.panes[m.focus], []byte{0x01}) // literal Ctrl-a
		}
	case "left", "h":
		m.moveFocus(-1)
	case "right", "l":
		m.moveFocus(1)
	case "up", "k":
		m.moveFocus(-m.gc)
	case "down", "j":
		m.moveFocus(m.gc)
	case "g":
		m.jumpToPending()
	case "n":
		m.addPane()
	case "x":
		if len(m.panes) > 0 {
			_, _ = ipc.Send(ipc.Request{Type: "kill", ID: m.panes[m.focus].id})
			// the pane's stream will deliver "gone"; nothing else to do
		}
	default:
		if n := digit(msg.String()); n >= 1 && n <= len(m.panes) {
			m.focus = n - 1
		}
	}
	return m, nil
}

func (m *tileModel) moveFocus(delta int) {
	if len(m.panes) == 0 {
		return
	}
	m.focus = ((m.focus+delta)%len(m.panes) + len(m.panes)) % len(m.panes)
}

func (m *tileModel) addPane() {
	resp, err := ipc.Send(ipc.Request{Type: "spawn", Argv: []string{"claude"}})
	if err != nil || !resp.OK {
		return
	}
	p, err := attachPane(ipc.SessionInfo{ID: resp.ID, Argv: []string{"claude"}, Status: "starting"})
	if err != nil {
		return
	}
	m.panes = append(m.panes, p)
	m.focus = len(m.panes) - 1
	go m.readPane(p)
	m.relayout()
}

func (m *tileModel) removePane(i int) {
	_ = m.panes[i].conn.Close()
	m.panes = append(m.panes[:i], m.panes[i+1:]...)
	if m.focus >= len(m.panes) {
		m.focus = max(0, len(m.panes)-1)
	}
	m.relayout()
}

func (m *tileModel) sendInput(p *tilePane, b []byte) {
	_ = ipc.WriteJSON(p.conn, ipc.StreamUp{Type: "input", Data: base64.StdEncoding.EncodeToString(b)})
}

func (m *tileModel) refreshStatuses() {
	resp, err := ipc.Send(ipc.Request{Type: "list"})
	if err != nil {
		return
	}
	byID := make(map[string]ipc.SessionInfo, len(resp.Sessions))
	for _, s := range resp.Sessions {
		byID[s.ID] = s
	}
	for _, p := range m.panes {
		if s, ok := byID[p.id]; ok {
			p.info = s
		}
	}
	focused := ""
	if m.focus < len(m.panes) {
		focused = m.panes[m.focus].id
	}
	m.pendingCount, m.pendingLatest = pendingSummary(resp.Sessions, focused)
}

// jumpToPending focuses the pane for the most-recent needs_approval session,
// attaching it as a new pane if it isn't already shown.
func (m *tileModel) jumpToPending() {
	if m.pendingLatest == "" {
		return
	}
	for i, p := range m.panes {
		if p.id == m.pendingLatest {
			m.focus = i
			return
		}
	}
	p, err := attachPane(ipc.SessionInfo{ID: m.pendingLatest})
	if err != nil {
		return
	}
	m.panes = append(m.panes, p)
	m.focus = len(m.panes) - 1
	go m.readPane(p)
	m.relayout()
}

// relayout recomputes the grid for the current pane count and window size, then
// tells each session to resize its PTY to its pane's inner dimensions.
func (m *tileModel) relayout() {
	if m.w == 0 || m.h == 0 || len(m.panes) == 0 {
		return
	}
	m.gc, m.gr, m.innerW, m.innerH = computeLayout(len(m.panes), m.w, m.h-tileStatusRows)
	for _, p := range m.panes {
		if p.rows == m.innerH && p.cols == m.innerW {
			continue
		}
		p.rows, p.cols = m.innerH, m.innerW
		_ = ipc.WriteJSON(p.conn, ipc.StreamUp{Type: "resize", Rows: m.innerH, Cols: m.innerW})
	}
}

// computeLayout returns the grid dimensions and the inner content size of each
// pane (excluding its 1-cell border) such that the grid fits within w x h.
func computeLayout(n, w, h int) (gc, gr, innerW, innerH int) {
	if n <= 1 {
		gc, gr = 1, 1
	} else {
		gc = int(math.Ceil(math.Sqrt(float64(n))))
		gr = int(math.Ceil(float64(n) / float64(gc)))
	}
	boxW := w / gc
	boxH := h / gr
	innerW = boxW - 2 // left/right border
	innerH = boxH - 2 // top/bottom border
	if innerW < 1 {
		innerW = 1
	}
	if innerH < 1 {
		innerH = 1
	}
	return gc, gr, innerW, innerH
}

var (
	tileBorder      = lipgloss.NewStyle().Border(lipgloss.NormalBorder()).BorderForeground(lipgloss.Color("8"))
	tileFocusBorder = lipgloss.NewStyle().Border(lipgloss.NormalBorder()).BorderForeground(lipgloss.Color("12"))
)

func (m *tileModel) View() string {
	if len(m.panes) == 0 || m.innerW == 0 {
		return "attaching…"
	}
	boxes := make([]string, len(m.panes))
	for i, p := range m.panes {
		style := tileBorder
		if i == m.focus {
			style = tileFocusBorder
		}
		boxes[i] = style.Width(m.innerW).Height(m.innerH).Render(p.screen)
	}

	var rows []string
	for r := 0; r < m.gr; r++ {
		var rowBoxes []string
		for c := 0; c < m.gc; c++ {
			idx := r*m.gc + c
			if idx >= len(boxes) {
				break
			}
			rowBoxes = append(rowBoxes, boxes[idx])
		}
		if len(rowBoxes) > 0 {
			rows = append(rows, lipgloss.JoinHorizontal(lipgloss.Top, rowBoxes...))
		}
	}
	grid := lipgloss.JoinVertical(lipgloss.Left, rows...)
	return grid + "\n" + m.statusBar()
}

func (m *tileModel) statusBar() string {
	if m.prefix {
		return helpStyle.Render("PREFIX — h/j/k/l or 1-9 focus · g jump-to-approval · n new · x kill · d detach")
	}
	var banner string
	if m.pendingCount > 0 {
		banner = statusStyle["needs_approval"].Render(
			fmt.Sprintf("⚑ %d pending approval(s) — prefix g to jump", m.pendingCount)) + "   "
	}
	var parts []string
	for i, p := range m.panes {
		label := fmt.Sprintf("[%d] %s", i+1, p.info.Status)
		if i == m.focus {
			label = selStyle.Render(label)
		} else if s, ok := statusStyle[p.info.Status]; ok {
			label = s.Render(label)
		}
		parts = append(parts, label)
	}
	return banner + strings.Join(parts, "  ") + "   " + helpStyle.Render(fmt.Sprintf("prefix (%s): focus/jump/new/kill/detach", prefixLabel()))
}

func digit(s string) int {
	if len(s) == 1 && s[0] >= '1' && s[0] <= '9' {
		return int(s[0] - '0')
	}
	return -1
}
