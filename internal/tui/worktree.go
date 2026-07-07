package tui

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	tea "charm.land/bubbletea/v2"
)

// worktreeInfo is one entry parsed from `git worktree list --porcelain`.
type worktreeInfo struct {
	path     string
	branch   string // short branch name; "" when detached or bare
	detached bool
	bare     bool
	isMain   bool // git lists the main worktree first
}

// agentChoice is a spawnable agent CLI and its display label. Only agents whose
// binary is on PATH are offered in the second picker stage.
type agentChoice struct {
	bin   string
	label string
}

// candidateAgents is the fixed set of agents the worktree picker can launch, in
// display order. availableAgents filters this to the ones actually installed.
var candidateAgents = []agentChoice{
	{bin: "claude", label: "claude code"},
	{bin: "codex", label: "codex"},
	{bin: "opencode", label: "opencode"},
}

// wtStage is which of the picker's two dialogs is showing: pick a worktree
// first, then pick which agent to launch inside it.
type wtStage int

const (
	wtStageWorktree wtStage = iota
	wtStageAgent
)

// availableAgents returns the candidate agents whose binary resolves on the
// user's PATH (the client's PATH tracks their shell — same rationale as
// spawnCmd's up-front LookPath). Order follows candidateAgents.
func availableAgents() []agentChoice {
	var out []agentChoice
	for _, a := range candidateAgents {
		if _, err := exec.LookPath(a.bin); err == nil {
			out = append(out, a)
		}
	}
	return out
}

// listWorktrees runs `git worktree list --porcelain` in dir and parses it. Any
// worktree of the repo lists every worktree, so dir can be the launch cwd. The
// main worktree is always listed first, so it's tagged isMain. Returns an error
// when dir isn't in a git repo (git exits non-zero).
func listWorktrees(dir string) ([]worktreeInfo, error) {
	if dir == "" {
		dir, _ = os.Getwd()
	}
	out, err := exec.Command("git", "-C", dir, "worktree", "list", "--porcelain").Output()
	if err != nil {
		return nil, err
	}
	return parseWorktreeList(out), nil
}

// parseWorktreeList turns `git worktree list --porcelain` output into records.
// Each record starts at a "worktree <path>" line and runs until the next one;
// git emits the main worktree first, so record 0 is tagged isMain.
func parseWorktreeList(out []byte) []worktreeInfo {
	var wts []worktreeInfo
	var cur *worktreeInfo
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
			cur = &worktreeInfo{path: strings.TrimPrefix(line, "worktree ")}
		case cur == nil:
			// Preamble/blank before the first record — nothing to attach to.
		case strings.HasPrefix(line, "branch "):
			cur.branch = strings.TrimPrefix(strings.TrimPrefix(line, "branch "), "refs/heads/")
		case line == "detached":
			cur.detached = true
		case line == "bare":
			cur.bare = true
		}
	}
	flush()
	if len(wts) > 0 {
		wts[0].isMain = true
	}
	return wts
}

// openWorktreePicker (prefix+w) gathers the installed agents and the repo's
// worktrees, then opens the two-stage modal on the worktree list. It bails with
// a toast — rather than opening an empty dialog — when there are no agents to
// launch or the launch dir isn't in a git repo.
func (m *dashboardModel) openWorktreePicker() {
	agents := availableAgents()
	if len(agents) == 0 {
		m.pushToast("✗ no agent binaries found (claude/codex/opencode)", "needs_approval")
		return
	}
	wts, err := listWorktrees(m.launchCwd)
	if err != nil || len(wts) == 0 {
		m.pushToast("✗ no git worktrees here", "needs_approval")
		return
	}
	m.wtOpen = true
	m.wtStage = wtStageWorktree
	m.wtList = wts
	m.wtAgents = agents
	m.wtCursor = m.defaultWorktreeCursor(wts)
	m.wtAgentCursor = 0
	m.wtChosenPath = ""
}

// defaultWorktreeCursor preselects the worktree the user launched cb from, so
// "spawn here" is a single Enter. Falls back to the first (main) worktree.
func (m *dashboardModel) defaultWorktreeCursor(wts []worktreeInfo) int {
	if m.launchCwd == "" {
		return 0
	}
	want := canonicalDir(m.launchCwd)
	for i, w := range wts {
		if canonicalDir(w.path) == want {
			return i
		}
	}
	return 0
}

func (m *dashboardModel) closeWorktreePicker() {
	m.wtOpen = false
}

// handleWorktreeKey owns every keystroke while the picker is open. Stage one
// navigates the worktree list (enter advances to the agent stage); stage two
// picks the agent (enter spawns it in the chosen worktree). esc steps back a
// stage (agent → worktree → closed); q / ctrl+c close outright.
func (m *dashboardModel) handleWorktreeKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch m.wtStage {
	case wtStageAgent:
		switch msg.String() {
		case "q", "ctrl+c":
			m.closeWorktreePicker()
		case "esc", "left", "h":
			m.wtStage = wtStageWorktree // back to worktree selection
		case "up", "k":
			if m.wtAgentCursor > 0 {
				m.wtAgentCursor--
			}
		case "down", "j":
			if m.wtAgentCursor < len(m.wtAgents)-1 {
				m.wtAgentCursor++
			}
		case "enter", "right", "l":
			if m.wtAgentCursor >= 0 && m.wtAgentCursor < len(m.wtAgents) {
				bin := m.wtAgents[m.wtAgentCursor].bin
				path := m.wtChosenPath
				m.closeWorktreePicker()
				return m, m.spawnCmd(bin, path)
			}
		}
	default: // wtStageWorktree
		switch msg.String() {
		case "esc", "q", "ctrl+c":
			m.closeWorktreePicker()
		case "up", "k":
			if m.wtCursor > 0 {
				m.wtCursor--
			}
		case "down", "j":
			if m.wtCursor < len(m.wtList)-1 {
				m.wtCursor++
			}
		case "enter", "right", "l":
			if m.wtCursor >= 0 && m.wtCursor < len(m.wtList) {
				m.wtChosenPath = m.wtList[m.wtCursor].path
				m.wtStage = wtStageAgent
				m.wtAgentCursor = 0
			}
		}
	}
	return m, nil
}

// renderWorktreePicker draws whichever stage is active, reusing the config
// modal's panel styling so the two "I'm in a modal" surfaces look consistent.
func (m *dashboardModel) renderWorktreePicker() string {
	if m.wtStage == wtStageAgent {
		return m.renderAgentStage()
	}
	return m.renderWorktreeStage()
}

func (m *dashboardModel) renderWorktreeStage() string {
	const nameCol = 22
	lines := []string{
		titleStyle.Render("start session in worktree"),
		helpStyle.Render("git worktree list"),
		"",
	}
	for i, w := range m.wtList {
		marker := "  "
		if i == m.wtCursor {
			marker = selBarStyle.Render("▌ ")
		}
		name := truncate(filepath.Base(w.path), nameCol-1)
		lines = append(lines, marker+padRight(name, nameCol)+helpStyle.Render(worktreeTag(w)))
	}
	lines = append(lines, "", helpStyle.Render("↑↓ select · enter next · esc cancel"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

func (m *dashboardModel) renderAgentStage() string {
	lines := []string{
		titleStyle.Render("launch in " + filepath.Base(m.wtChosenPath)),
		helpStyle.Render("choose agent"),
		"",
	}
	for i, a := range m.wtAgents {
		marker := "  "
		if i == m.wtAgentCursor {
			marker = selBarStyle.Render("▌ ")
		}
		lines = append(lines, marker+a.label)
	}
	lines = append(lines, "", helpStyle.Render("↑↓ select · enter start · esc back"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

// worktreeTag is the faint right-hand annotation for a worktree row: which one
// is the main checkout, or the branch it's on (⎇), or its detached/bare state.
func worktreeTag(w worktreeInfo) string {
	switch {
	case w.isMain:
		return "(main)"
	case w.bare:
		return "(bare)"
	case w.detached:
		return "(detached)"
	case w.branch != "":
		return "⎇ " + w.branch
	default:
		return ""
	}
}
