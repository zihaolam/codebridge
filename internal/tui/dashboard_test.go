package tui

import (
	"strings"
	"testing"

	"command-center/internal/ipc"
)

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
	out := m.View()
	for _, want := range []string{"api-fix", "command-center", "hello from the session", "run rm -rf?"} {
		if !strings.Contains(out, want) {
			t.Errorf("view missing %q", want)
		}
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
