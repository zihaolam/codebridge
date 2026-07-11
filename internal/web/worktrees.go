package web

import (
	"os/exec"
	"strings"
)

// Worktree-picker support, mirroring the TUI's prefix+w dialog
// (internal/tui/worktree.go): the browser can't run git, so the bridge lists
// a repo's worktrees via `git worktree list --porcelain` and reports which
// agent binaries resolve on PATH. The agent is chosen per spawn, never
// remembered — same semantics as the TUI.

// worktreeEntry is one parsed `git worktree list --porcelain` record.
type worktreeEntry struct {
	Path     string `json:"path"`
	Branch   string `json:"branch,omitempty"` // short name; empty when detached/bare
	Detached bool   `json:"detached,omitempty"`
	Bare     bool   `json:"bare,omitempty"`
	Main     bool   `json:"main,omitempty"` // git lists the main worktree first
}

// candidateAgents is the fixed set the picker can launch, in display order.
var candidateAgents = []string{"claude", "codex", "opencode"}

// availableAgents filters candidateAgents to binaries on the bridge's PATH.
func availableAgents() []string {
	out := make([]string, 0, len(candidateAgents))
	for _, a := range candidateAgents {
		if _, err := exec.LookPath(a); err == nil {
			out = append(out, a)
		}
	}
	return out
}

// listWorktrees runs `git worktree list --porcelain` in dir. Any worktree of
// a repo lists all of them, so dir can be any session cwd in the workspace.
// Errors (non-repo dir) surface as an empty list — the caller substitutes the
// dir itself so the picker still offers somewhere to spawn.
func listWorktrees(dir string) []worktreeEntry {
	if dir == "" {
		return nil
	}
	out, err := exec.Command("git", "-C", dir, "worktree", "list", "--porcelain").Output()
	if err != nil {
		return nil
	}
	return parseWorktreeList(out)
}

// parseWorktreeList turns porcelain output into records: each starts at a
// "worktree <path>" line and runs to the next one; record 0 is the main
// worktree.
func parseWorktreeList(out []byte) []worktreeEntry {
	var wts []worktreeEntry
	var cur *worktreeEntry
	flush := func() {
		if cur != nil {
			wts = append(wts, *cur)
			cur = nil
		}
	}
	for _, line := range strings.Split(string(out), "\n") {
		switch {
		case strings.HasPrefix(line, "worktree "):
			flush()
			cur = &worktreeEntry{Path: strings.TrimPrefix(line, "worktree ")}
		case cur == nil:
			// Preamble/blank before the first record — nothing to attach to.
		case strings.HasPrefix(line, "branch "):
			cur.Branch = strings.TrimPrefix(strings.TrimPrefix(line, "branch "), "refs/heads/")
		case line == "detached":
			cur.Detached = true
		case line == "bare":
			cur.Bare = true
		}
	}
	flush()
	if len(wts) > 0 {
		wts[0].Main = true
	}
	return wts
}
