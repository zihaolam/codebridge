package tui

import (
	"testing"

	tea "charm.land/bubbletea/v2"
)

// TestParseWorktreeList covers the porcelain shapes git actually emits: a
// branch-checked-out main worktree (tagged isMain), a linked worktree on a
// branch, a detached HEAD, and a bare repo.
func TestParseWorktreeList(t *testing.T) {
	out := []byte("worktree /repo/main\n" +
		"HEAD abc123\n" +
		"branch refs/heads/main\n" +
		"\n" +
		"worktree /repo/feature\n" +
		"HEAD def456\n" +
		"branch refs/heads/feature-x\n" +
		"\n" +
		"worktree /repo/detached\n" +
		"HEAD ghi789\n" +
		"detached\n" +
		"\n" +
		"worktree /repo/bare\n" +
		"bare\n")

	wts := parseWorktreeList(out)
	if len(wts) != 4 {
		t.Fatalf("got %d worktrees, want 4: %+v", len(wts), wts)
	}
	if !wts[0].isMain || wts[0].branch != "main" || wts[0].path != "/repo/main" {
		t.Errorf("main worktree parsed wrong: %+v", wts[0])
	}
	if wts[1].isMain || wts[1].branch != "feature-x" {
		t.Errorf("linked worktree parsed wrong: %+v", wts[1])
	}
	if !wts[2].detached || wts[2].branch != "" {
		t.Errorf("detached worktree parsed wrong: %+v", wts[2])
	}
	if !wts[3].bare {
		t.Errorf("bare worktree parsed wrong: %+v", wts[3])
	}

	tags := map[int]string{0: "(main)", 1: "⎇ feature-x", 2: "(detached)", 3: "(main)"}
	// index 3 isn't main; recompute its expected tag without the isMain shortcut.
	tags[3] = "(bare)"
	for i, want := range tags {
		if got := worktreeTag(wts[i]); got != want {
			t.Errorf("worktreeTag(wts[%d]) = %q, want %q", i, got, want)
		}
	}
}

func key(s string) tea.KeyPressMsg {
	switch s {
	case "enter":
		return tea.KeyPressMsg{Code: tea.KeyEnter}
	case "esc":
		return tea.KeyPressMsg{Code: tea.KeyEscape}
	default:
		return tea.KeyPressMsg{Code: rune(s[0]), Text: s}
	}
}

// TestWorktreePickerNavigation drives the two-stage state machine directly
// (bypassing openWorktreePicker's git/PATH lookups): move in the worktree list,
// enter to advance, then enter on an agent returns a spawn command carrying the
// chosen worktree path and closes the modal.
func TestWorktreePickerNavigation(t *testing.T) {
	m := &dashboardModel{
		wtOpen:  true,
		wtStage: wtStageWorktree,
		wtList: []worktreeInfo{
			{path: "/repo/main", branch: "main", isMain: true},
			{path: "/repo/feature", branch: "feature-x"},
		},
		wtAgents: []agentChoice{{bin: "claude", label: "claude code"}, {bin: "codex", label: "codex"}},
	}

	// j moves down to the second worktree, enter advances to the agent stage.
	m.handleWorktreeKey(key("j"))
	if m.wtCursor != 1 {
		t.Fatalf("cursor = %d, want 1 after down", m.wtCursor)
	}
	m.handleWorktreeKey(key("enter"))
	if m.wtStage != wtStageAgent {
		t.Fatalf("stage = %v, want agent after enter", m.wtStage)
	}
	if m.wtChosenPath != "/repo/feature" {
		t.Fatalf("chosen path = %q, want /repo/feature", m.wtChosenPath)
	}

	// esc steps back to the worktree stage without closing.
	m.handleWorktreeKey(key("esc"))
	if m.wtStage != wtStageWorktree || !m.wtOpen {
		t.Fatalf("esc should step back to worktree stage, still open: stage=%v open=%v", m.wtStage, m.wtOpen)
	}

	// Re-enter, pick the second agent, enter should return a spawn cmd + close.
	m.handleWorktreeKey(key("enter"))
	m.handleWorktreeKey(key("j")) // agent cursor -> codex
	if m.wtAgentCursor != 1 {
		t.Fatalf("agent cursor = %d, want 1", m.wtAgentCursor)
	}
	_, cmd := m.handleWorktreeKey(key("enter"))
	if cmd == nil {
		t.Fatal("expected a spawn command from enter on agent stage")
	}
	if m.wtOpen {
		t.Fatal("picker should close after launching")
	}
}
