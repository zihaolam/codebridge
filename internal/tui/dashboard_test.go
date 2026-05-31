package tui

import (
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"github.com/charmbracelet/x/ansi"

	"command-center/internal/ipc"
)

// fullWidthScreen builds a session screen of `rows` lines, each a styled
// horizontal rule exactly `cols` display columns wide — the shape (Claude's
// input-box rules) that exposed the wrap/overflow bugs.
func fullWidthScreen(cols, rows int) string {
	lines := make([]string, rows)
	for i := range lines {
		lines[i] = "\x1b[38;5;244m" + strings.Repeat("─", cols) + "\x1b[0m"
	}
	return strings.Join(lines, "\n")
}

func TestRenderScreenDoesNotWrapFullWidthLine(t *testing.T) {
	const paneW, paneH = 100, 8
	m := &dashboardModel{
		paneW: paneW, paneH: paneH,
		streamID: "live",
		screen:   fullWidthScreen(paneW, 1),
	}
	rows := strings.Split(m.renderScreen(), "\n")
	if len(rows) != paneH {
		t.Fatalf("renderScreen produced %d rows, want paneH=%d (extra rows = wrapping)", len(rows), paneH)
	}
	// A full-width rule must sit entirely on the first row; an off-by-one Width
	// wraps it and spills a dash onto the second row.
	if strings.ContainsRune(ansi.Strip(rows[1]), '─') {
		t.Errorf("full-width line wrapped onto row 1: %q", ansi.Strip(rows[1]))
	}
	want := paneW + screenBorderStyle.GetHorizontalFrameSize()
	if w := ansi.StringWidth(rows[0]); w != want {
		t.Errorf("pane width = %d, want %d (paneW + border + padding)", w, want)
	}
}

func TestRenderLiveFitsTerminal(t *testing.T) {
	const w, h = 120, 30
	m := &dashboardModel{
		w: w, h: h,
		prev:     map[string]string{},
		hooksOK:  true,
		streamID: "live00000000",
		sessions: []ipc.SessionInfo{{ID: "live00000000", Status: "working"}},
	}
	m.relayoutStream() // derive paneW/paneH from w/h exactly as the app does
	m.screen = fullWidthScreen(m.paneW, m.paneH)

	rows := strings.Split(m.renderLive(), "\n")
	// No vertical overflow: a wrapped pane line would bake in newlines and push
	// total rows past the terminal height, clipping the bottom (session + help).
	if len(rows) > h {
		t.Errorf("renderLive produced %d rows, exceeds terminal height %d", len(rows), h)
	}
	// No horizontal overflow: a row wider than the terminal wraps at display time
	// and shoves everything below it down off-screen.
	for i, r := range rows {
		if wd := ansi.StringWidth(r); wd > w {
			t.Errorf("row %d width %d exceeds terminal width %d", i, wd, w)
		}
	}
	// The bottom chrome rows are intentionally blank now (rename input renders
	// in the bottom slot when active; the old always-on help line was replaced
	// by the floating prefix panel). What we care about is that the View still
	// fits in the terminal and the body row just above the chrome stays
	// populated (so the session didn't get clipped).
	chrome := m.chromeRows()
	lastBody := len(rows) - chrome - 1
	if body := strings.TrimSpace(ansi.Strip(rows[lastBody])); body == "" {
		t.Errorf("last body row %d is blank — view was clipped", lastBody)
	}
}

func TestPrefixPanelOverlay(t *testing.T) {
	const w, h = 120, 30
	m := &dashboardModel{
		w: w, h: h,
		prev:     map[string]string{},
		hooksOK:  true,
		streamID: "live00000000",
		sessions: []ipc.SessionInfo{{ID: "live00000000", Status: "working"}},
		prefix:   true, // panel visible
	}
	m.relayoutStream()
	m.screen = fullWidthScreen(m.paneW, m.paneH)

	out := m.renderLive()
	if !strings.Contains(ansi.Strip(out), "new claude") {
		t.Error("prefix panel hint 'new claude' missing while m.prefix=true")
	}
	if !strings.Contains(ansi.Strip(out), "quit") {
		t.Error("prefix panel hint 'quit' missing while m.prefix=true")
	}
	// Overlay must not push the View past the terminal height.
	if rows := strings.Split(out, "\n"); len(rows) > h {
		t.Errorf("renderLive with panel produced %d rows, exceeds terminal height %d", len(rows), h)
	}
	// Without the prefix flag the panel should be gone.
	m.prefix = false
	out = m.renderLive()
	if strings.Contains(ansi.Strip(out), "new claude") {
		t.Error("prefix panel leaked into normal render")
	}
}

func sess(id, status, msg string) ipc.SessionInfo {
	return ipc.SessionInfo{ID: id + "00000000", Status: status, LastMessage: msg}
}

func TestDetectTransitions(t *testing.T) {
	m := &dashboardModel{prev: map[string]string{}}

	// First observation while working: no toast.
	m.detectTransitions([]ipc.SessionInfo{sess("a", "working", "")})
	if len(m.toasts) != 0 {
		t.Fatalf("expected no toast on first working observation, got %d", len(m.toasts))
	}

	// Crossing into needs_approval: one toast carrying the message.
	m.detectTransitions([]ipc.SessionInfo{sess("a", "needs_approval", "run rm -rf?")})
	if len(m.toasts) != 1 || m.toasts[0].level != "needs_approval" {
		t.Fatalf("expected needs_approval toast, got %+v", m.toasts)
	}

	// Still needs_approval: no duplicate toast.
	m.detectTransitions([]ipc.SessionInfo{sess("a", "needs_approval", "run rm -rf?")})
	if len(m.toasts) != 1 {
		t.Fatalf("expected no duplicate toast, got %d", len(m.toasts))
	}

	// Turn finished: a waiting_user toast.
	m.detectTransitions([]ipc.SessionInfo{sess("a", "waiting_user", "")})
	if len(m.toasts) != 2 || m.toasts[1].level != "waiting_user" {
		t.Fatalf("expected waiting_user toast, got %+v", m.toasts)
	}
}

func TestFirstWaitingUserDoesNotToast(t *testing.T) {
	m := &dashboardModel{prev: map[string]string{}}
	m.detectTransitions([]ipc.SessionInfo{sess("b", "waiting_user", "")})
	if len(m.toasts) != 0 {
		t.Fatalf("first observation of waiting_user should not toast, got %d", len(m.toasts))
	}
}

func TestLatestPending(t *testing.T) {
	sessions := []ipc.SessionInfo{
		{ID: "aaa", Status: "working", StatusSince: 100},
		{ID: "bbb", Status: "needs_approval", StatusSince: 200},
		{ID: "ccc", Status: "needs_approval", StatusSince: 500}, // most recent
		{ID: "ddd", Status: "waiting_user", StatusSince: 900},
	}
	count, latest := latestPending(sessions)
	if count != 2 {
		t.Errorf("count = %d, want 2", count)
	}
	if latest != "ccc" {
		t.Errorf("latest = %q, want ccc (most recent needs_approval)", latest)
	}

	if c, l := latestPending([]ipc.SessionInfo{{ID: "x", Status: "working"}}); c != 0 || l != "" {
		t.Errorf("no-pending case = (%d,%q), want (0,\"\")", c, l)
	}
}

func TestPendingSummaryExcludesSelf(t *testing.T) {
	sessions := []ipc.SessionInfo{
		{ID: "bbb", Status: "needs_approval", StatusSince: 200},
		{ID: "ccc", Status: "needs_approval", StatusSince: 500},
	}
	// Excluding the most-recent one drops it from the count and the jump target.
	count, latest := pendingSummary(sessions, "ccc")
	if count != 1 || latest != "bbb" {
		t.Errorf("pendingSummary excl ccc = (%d,%q), want (1,\"bbb\")", count, latest)
	}
	// Excluding the only remaining pending session yields nothing to nag about.
	if c, l := pendingSummary([]ipc.SessionInfo{{ID: "x", Status: "needs_approval"}}, "x"); c != 0 || l != "" {
		t.Errorf("pendingSummary excl only = (%d,%q), want (0,\"\")", c, l)
	}
}

func TestSidebarViewRenders(t *testing.T) {
	m := &dashboardModel{
		w: 100, h: 30,
		paneW: 60, paneH: 24,
		sessions: []ipc.SessionInfo{
			{ID: "aaaaaaaa11", Name: "api-fix", Status: "needs_approval", LastMessage: "run rm -rf?"},
			{ID: "bbbbbbbb22", Status: "working"},
		},
		streamID: "aaaaaaaa11",
		screen:   "hello from the session",
	}
	out := m.renderLive()
	for _, want := range []string{"api-fix", "codebridge", "hello from the session"} {
		if !strings.Contains(out, want) {
			t.Errorf("view missing %q", want)
		}
	}
}

func TestDisplayName(t *testing.T) {
	cases := []struct {
		s    ipc.SessionInfo
		want string
	}{
		{ipc.SessionInfo{ID: "abcdefgh12", Name: "api-fix", Cwd: "/home/x/proj"}, "api-fix"},
		{ipc.SessionInfo{ID: "abcdefgh12", Cwd: "/Users/zihaolam/Projects/command-center"}, "command-center"},
		{ipc.SessionInfo{ID: "abcdefgh12", Cwd: "/Users/zihaolam/Projects/command-center/"}, "command-center"},
		{ipc.SessionInfo{ID: "abcdefgh12"}, "abcdefgh"},
	}
	for _, c := range cases {
		if got := displayName(c.s); got != c.want {
			t.Errorf("displayName(%+v) = %q, want %q", c.s, got, c.want)
		}
	}
}

func TestReapplyBGAfterResets(t *testing.T) {
	const bg = "\x1b[48;5;238m"
	cases := []struct {
		name string
		in   string
		want string
	}{
		{
			"no escapes is passthrough",
			"plain text",
			"plain text",
		},
		{
			"full reset re-injects BG",
			"\x1b[1;32mhello\x1b[0m world",
			"\x1b[1;32mhello\x1b[0m" + bg + " world",
		},
		{
			"empty params (\\x1b[m) treated as reset",
			"x\x1b[my",
			"x\x1b[m" + bg + "y",
		},
		{
			"default-BG (49) re-injects",
			"x\x1b[49my",
			"x\x1b[49m" + bg + "y",
		},
		{
			"compound with 0 in param list re-injects",
			"a\x1b[0;1;31mb\x1b[mc",
			"a\x1b[0;1;31m" + bg + "b\x1b[m" + bg + "c",
		},
		{
			"pure FG change does NOT re-inject (would over-paint)",
			"a\x1b[31mb",
			"a\x1b[31mb",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := reapplyBGAfterResets(c.in, bg); got != c.want {
				t.Errorf("reapplyBGAfterResets(%q)\n got  %q\n want %q", c.in, got, c.want)
			}
		})
	}
}

func TestClampTop(t *testing.T) {
	cases := []struct {
		name                        string
		cursor, top, count, maxRows int
		want                        int
	}{
		{"fits entirely", 3, 0, 5, 10, 0},
		{"cursor above window scrolls up", 2, 5, 20, 6, 2},
		{"cursor below window scrolls down", 9, 0, 20, 6, 4},
		{"cursor inside window keeps top", 4, 2, 20, 6, 2},
		{"clamp to last page", 19, 18, 20, 6, 14},
		{"never negative", 0, 0, 2, 6, 0},
	}
	for _, c := range cases {
		if got := clampTop(c.cursor, c.top, c.count, c.maxRows); got != c.want {
			t.Errorf("%s: clampTop(%d,%d,%d,%d) = %d, want %d",
				c.name, c.cursor, c.top, c.count, c.maxRows, got, c.want)
		}
	}
}

func TestWheelScrollsPaneAndExitsAtBottom(t *testing.T) {
	m := &dashboardModel{
		streamID:   "live",
		scrollMode: true, // already browsing scrollback
		scrollMax:  20,
		paneW:      40, paneH: 20,
	}
	// Wheel up over the pane (x past the sidebar band) moves toward older output.
	m.handleWheel(tea.MouseWheelMsg{X: sidebarWidth + 5, Button: tea.MouseWheelUp})
	if m.scrollOff != wheelScrollStep {
		t.Fatalf("scrollOff = %d, want %d after one wheel-up", m.scrollOff, wheelScrollStep)
	}
	// Wheel down past the live bottom clamps to 0 and leaves scroll mode so
	// keystrokes resume flowing to the session.
	m.handleWheel(tea.MouseWheelMsg{X: sidebarWidth + 5, Button: tea.MouseWheelDown})
	if m.scrollOff != 0 {
		t.Fatalf("scrollOff = %d, want 0 after wheel-down past bottom", m.scrollOff)
	}
	if m.scrollMode {
		t.Fatal("expected to leave scroll mode at the live bottom")
	}
}

func TestPushToastCap(t *testing.T) {
	m := &dashboardModel{}
	for i := 0; i < 9; i++ {
		m.pushToast("x", "working")
	}
	if len(m.toasts) != 5 {
		t.Fatalf("toasts should be capped at 5, got %d", len(m.toasts))
	}
}
