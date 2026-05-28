// Package tui implements the cb terminal client. Attach renders a single
// session's live screen and forwards keystrokes to it, with a tmux-style prefix
// key for control commands.
package tui

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net"
	"time"

	tea "github.com/charmbracelet/bubbletea"

	"command-center/internal/ipc"
)

// PrefixKey is the control prefix (tmux-style). After pressing it, the next key
// is a command: q/d detach, a sends a literal prefix to the child.
const prefixKeyName = "ctrl+a"

type frameMsg ipc.StreamDown
type streamClosedMsg struct{}
type attachTickMsg struct{}
type attachPendingMsg struct {
	count  int
	latest string
}

type attachModel struct {
	conn   net.Conn
	frames chan ipc.StreamDown
	screen string
	prefix bool
	status string
	w, h   int
	id     string

	pendingCount  int
	pendingLatest string
	next          string // session id to jump to after quitting (Ctrl-a g)
}

// Attach connects to the daemon, attaches to session id, and runs the client.
// It returns the id of another session to attach to next (when the user jumps
// via Ctrl-a g), or "" to return to the dashboard.
func Attach(id string) (string, error) {
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return "", fmt.Errorf("connect daemon: %w", err)
	}
	if err := ipc.WriteJSON(conn, ipc.Request{Type: "attach", ID: id}); err != nil {
		return "", err
	}

	m := &attachModel{
		conn:   conn,
		frames: make(chan ipc.StreamDown, 64),
		id:     id,
		status: "attached — Ctrl-a then q to detach",
	}

	go m.readLoop()

	p := tea.NewProgram(m, tea.WithAltScreen())
	_, err = p.Run()
	_ = conn.Close()
	return m.next, err
}

// readLoop pumps StreamDown messages from the daemon into the frames channel.
func (m *attachModel) readLoop() {
	sc := bufio.NewScanner(m.conn)
	sc.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	for sc.Scan() {
		var d ipc.StreamDown
		if err := json.Unmarshal(sc.Bytes(), &d); err != nil {
			continue
		}
		m.frames <- d
	}
	close(m.frames)
}

func (m *attachModel) Init() tea.Cmd {
	return tea.Batch(m.waitFrame(), attachPoll(), attachTick())
}

func attachTick() tea.Cmd {
	return tea.Tick(time.Second, func(time.Time) tea.Msg { return attachTickMsg{} })
}

// attachPoll fetches the global pending-approval summary so the attach view can
// show a banner and support Ctrl-a g even while focused on one session.
func attachPoll() tea.Cmd {
	return func() tea.Msg {
		resp, _ := ipc.Send(ipc.Request{Type: "list"})
		c, l := latestPending(resp.Sessions)
		return attachPendingMsg{count: c, latest: l}
	}
}

func (m *attachModel) waitFrame() tea.Cmd {
	return func() tea.Msg {
		d, ok := <-m.frames
		if !ok {
			return streamClosedMsg{}
		}
		return frameMsg(d)
	}
}

func (m *attachModel) send(up ipc.StreamUp) {
	_ = ipc.WriteJSON(m.conn, up)
}

func (m *attachModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.w, m.h = msg.Width, msg.Height
		// Reserve one row for the status line.
		m.send(ipc.StreamUp{Type: "resize", Rows: msg.Height - 1, Cols: msg.Width})
		return m, nil

	case frameMsg:
		if msg.Type == "gone" {
			// Session ended (e.g. /exit): bounce straight back to the dashboard.
			return m, tea.Quit
		}
		m.screen = msg.Screen
		return m, m.waitFrame()

	case streamClosedMsg:
		return m, tea.Quit

	case attachTickMsg:
		return m, tea.Batch(attachPoll(), attachTick())

	case attachPendingMsg:
		m.pendingCount, m.pendingLatest = msg.count, msg.latest
		return m, nil

	case tea.KeyMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

func (m *attachModel) handleKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	if m.prefix {
		m.prefix = false
		switch msg.String() {
		case "q", "d":
			m.send(ipc.StreamUp{Type: "detach"})
			return m, tea.Quit
		case prefixKeyName, "a":
			// Send a literal prefix byte (Ctrl-a = 0x01) to the child.
			m.send(ipc.StreamUp{Type: "input", Data: base64.StdEncoding.EncodeToString([]byte{0x01})})
			return m, nil
		case "g":
			// Jump to the latest session needing approval (handled by the caller).
			if m.pendingLatest != "" && m.pendingLatest != m.id {
				m.next = m.pendingLatest
				m.send(ipc.StreamUp{Type: "detach"})
				return m, tea.Quit
			}
			return m, nil
		default:
			return m, nil // unknown command: swallow
		}
	}

	if msg.String() == prefixKeyName {
		m.prefix = true
		return m, nil
	}

	if b := keyToBytes(msg); b != nil {
		m.send(ipc.StreamUp{Type: "input", Data: base64.StdEncoding.EncodeToString(b)})
	}
	return m, nil
}

func (m *attachModel) View() string {
	status := m.status
	if m.prefix {
		status = "PREFIX — q/d detach · g jump-to-approval · a=literal Ctrl-a"
	} else if m.pendingCount > 0 {
		status = statusStyle["needs_approval"].Render(
			fmt.Sprintf("⚑ %d pending approval(s) — Ctrl-a g to jump", m.pendingCount))
	}
	return m.screen + "\n" + status
}
