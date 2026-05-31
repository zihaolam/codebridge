package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/charmbracelet/x/ansi"

	"command-center/internal/config"
)

// configRow is one navigable line in the modal config view. kind tags the
// behavior on enter ("prefix" / "binding" / "reset"); action is populated for
// binding rows so capture knows which Action to update.
type configRow struct {
	kind   string
	label  string
	action string
}

// configRows enumerates the menu's rows top-to-bottom: prefix at the top,
// every rebindable Action below it, then the reset row. Built fresh each
// frame so a future config that adds Actions picks them up without state
// reset.
func configRows() []configRow {
	rows := make([]configRow, 0, len(config.Actions)+2)
	rows = append(rows, configRow{kind: "prefix", label: "prefix"})
	for _, a := range config.Actions {
		rows = append(rows, configRow{kind: "binding", label: a.Label, action: a.Key})
	}
	rows = append(rows, configRow{kind: "reset", label: "reset all to defaults"})
	return rows
}

// openConfig enters the modal config view. When CB_PREFIX has locked the
// prefix, start the cursor on the first editable row instead of the prefix
// (which is shown read-only) so enter actually does something useful.
func (m *dashboardModel) openConfig() {
	m.configOpen = true
	m.configCapture = false
	m.configErr = ""
	m.configCursor = 0
	if PrefixOverriddenByEnv() {
		m.configCursor = 1
	}
}

func (m *dashboardModel) closeConfig() {
	m.configOpen = false
	m.configCapture = false
	m.configErr = ""
}

// handleConfigKey routes keystrokes while the modal is open. In capture mode
// the next non-esc keystroke replaces the selected row's binding; otherwise
// up/down navigate, enter starts capture (or fires the reset row), and esc/q
// close.
func (m *dashboardModel) handleConfigKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	rows := configRows()
	if m.configCapture {
		return m.applyCapture(msg, rows)
	}
	switch msg.String() {
	case "esc", "q", "ctrl+c":
		m.closeConfig()
		return m, nil
	case "up", "k":
		m.moveConfigCursor(-1, rows)
		return m, nil
	case "down", "j":
		m.moveConfigCursor(1, rows)
		return m, nil
	case "enter":
		return m.activateConfigRow(rows)
	}
	return m, nil
}

// moveConfigCursor steps the cursor by delta, skipping the prefix row when
// CB_PREFIX has locked it (so users can't enter capture on a row they can't
// actually change).
func (m *dashboardModel) moveConfigCursor(delta int, rows []configRow) {
	n := len(rows)
	if n == 0 {
		return
	}
	next := m.configCursor
	for i := 0; i < n; i++ {
		next = (next + delta + n) % n
		if rows[next].kind == "prefix" && PrefixOverriddenByEnv() {
			continue
		}
		m.configCursor = next
		return
	}
}

// activateConfigRow handles enter on the selected row: prefix/binding rows
// enter capture mode; the reset row restores factory defaults and saves
// immediately so the user sees the change without an extra step.
func (m *dashboardModel) activateConfigRow(rows []configRow) (tea.Model, tea.Cmd) {
	if m.configCursor < 0 || m.configCursor >= len(rows) {
		return m, nil
	}
	r := rows[m.configCursor]
	switch r.kind {
	case "prefix":
		if PrefixOverriddenByEnv() {
			m.configErr = "locked: unset CB_PREFIX to edit"
			return m, nil
		}
		m.configCapture = true
		m.configErr = ""
	case "binding":
		m.configCapture = true
		m.configErr = ""
	case "reset":
		d := config.Defaults()
		m.cfg.Prefix = d.Prefix
		for k, v := range d.Bindings {
			m.cfg.Bindings[k] = v
		}
		SetPrefix(m.cfg.Prefix)
		m.persistConfig()
		m.configErr = ""
		m.pushToast("✓ bindings reset to defaults", "working")
	}
	return m, nil
}

// applyCapture consumes the next keystroke and turns it into the new binding.
// Esc cancels capture without changing anything; reserved keys (left/right/h/?,
// nav letters, etc.) are refused with an inline error because the system
// layer would always intercept them before dispatch.
func (m *dashboardModel) applyCapture(msg tea.KeyPressMsg, rows []configRow) (tea.Model, tea.Cmd) {
	if msg.String() == "esc" {
		m.configCapture = false
		m.configErr = ""
		return m, nil
	}
	key := msg.String()
	if key == "" {
		return m, nil
	}
	if m.configCursor < 0 || m.configCursor >= len(rows) {
		return m, nil
	}
	r := rows[m.configCursor]
	// Reserved keys: same blocklist applies to both prefix and bindings since
	// the prefix has to be a single keystroke too (typically a ctrl+chord, but
	// nothing structurally prevents the user from trying a plain letter).
	if reason, ok := config.ReservedKeys[key]; ok {
		m.configErr = fmt.Sprintf("reserved: %s", reason)
		return m, nil
	}
	switch r.kind {
	case "prefix":
		m.cfg.Prefix = key
		SetPrefix(key)
		m.configErr = ""
	case "binding":
		// Conflict guard: refuse a key already bound to a different action so
		// dispatch stays unambiguous. The user can rebind the conflicting
		// action first, then come back.
		for action, bound := range m.cfg.Bindings {
			if action != r.action && bound == key {
				m.configErr = fmt.Sprintf("already bound to %q", labelForAction(action))
				return m, nil
			}
		}
		m.cfg.Bindings[r.action] = key
		m.configErr = ""
	}
	m.configCapture = false
	m.persistConfig()
	return m, nil
}

// labelForAction returns the human-readable label for an action id, falling
// back to the id when unknown (defensive — every id in cfg.Bindings should
// also be in config.Actions).
func labelForAction(id string) string {
	for _, a := range config.Actions {
		if a.Key == id {
			return a.Label
		}
	}
	return id
}

// persistConfig writes the current cfg to disk. Failures surface as a toast
// but don't block the in-memory change, so the user's edit still applies for
// this session even if disk write fails (e.g. read-only home dir).
func (m *dashboardModel) persistConfig() {
	if err := config.Save(m.cfg); err != nil {
		m.pushToast("✗ save failed: "+err.Error(), "needs_approval")
	}
}

// configPanelStyle is the bordered box around the modal. Magenta border keeps
// "I'm in a mode" visually consistent with the prefix-hints panel and the
// scrollback indicator.
var configPanelStyle = lipgloss.NewStyle().
	Border(lipgloss.RoundedBorder()).
	BorderForeground(lipgloss.Color("13")).
	Padding(0, 2)

// renderConfigMenu builds the modal's content. The right column shows the
// current binding (or a "[press key…]" prompt when this row is in capture
// mode), and the prefix row displays "(CB_PREFIX)" when the env override has
// locked it.
func (m *dashboardModel) renderConfigMenu() string {
	rows := configRows()
	const labelCol = 28
	lines := []string{
		titleStyle.Render("codebridge config"),
		helpStyle.Render("prefix + key (set the prefix at the top)"),
		"",
	}
	for i, r := range rows {
		marker := "  "
		if i == m.configCursor {
			marker = selBarStyle.Render("▌ ")
		}
		var line string
		switch r.kind {
		case "prefix":
			value := kbdStyle.Render(m.cfg.Prefix)
			if m.configCapture && i == m.configCursor {
				value = helpStyle.Render("[press key · esc cancel]")
			} else if PrefixOverriddenByEnv() {
				value = kbdStyle.Render(prefixLabel()) + " " + helpStyle.Render("(locked by CB_PREFIX)")
			}
			line = marker + padRight("prefix", labelCol) + value
		case "binding":
			value := kbdStyle.Render(m.cfg.Bindings[r.action])
			if m.configCapture && i == m.configCursor {
				value = helpStyle.Render("[press key · esc cancel]")
			}
			line = marker + padRight(r.label, labelCol) + value
		case "reset":
			lines = append(lines, "") // visual gap before the reset row
			line = marker + helpStyle.Render(r.label)
		}
		lines = append(lines, line)
	}
	if m.configErr != "" {
		lines = append(lines, "", statusStyle["needs_approval"].Render("  "+m.configErr))
	}
	hint := "↑↓ select · enter rebind · esc close (saved automatically)"
	if m.configCapture {
		hint = "press any key · esc cancel"
	}
	lines = append(lines, "", helpStyle.Render(hint))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

// padRight pads label with spaces so the value column lines up. Display-width
// aware (labels are plain ASCII; ANSI styling is applied after, so plain len
// is fine here).
func padRight(s string, n int) string {
	pad := n - len(s)
	if pad <= 0 {
		return s + " "
	}
	return s + strings.Repeat(" ", pad)
}

// centerOnScreen positions panel at the visual center of a (w, h) viewport,
// filling the surrounding space with blank lines / left padding. ANSI-width
// aware so a styled panel measures correctly.
func centerOnScreen(panel string, w, h int) string {
	lines := strings.Split(panel, "\n")
	contentH := len(lines)
	contentW := 0
	for _, l := range lines {
		if x := ansi.StringWidth(l); x > contentW {
			contentW = x
		}
	}
	topPad := (h - contentH) / 2
	if topPad < 0 {
		topPad = 0
	}
	leftPad := (w - contentW) / 2
	if leftPad < 0 {
		leftPad = 0
	}
	pad := strings.Repeat(" ", leftPad)
	out := make([]string, 0, topPad+contentH)
	for i := 0; i < topPad; i++ {
		out = append(out, "")
	}
	for _, l := range lines {
		out = append(out, pad+l)
	}
	// Pad to full height so the alt-screen is fully painted (otherwise the
	// dashboard underneath bleeds through on terminals that don't clear).
	for len(out) < h {
		out = append(out, "")
	}
	return strings.Join(out, "\n")
}
