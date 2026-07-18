package tui

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"sort"
	"strings"

	tea "charm.land/bubbletea/v2"

	"codebridge/internal/ipc"
	"codebridge/internal/task"
)

// taskStage is which of the backlog dialog's views is showing: the sectioned
// task list, the title+description new-task form, the title+description editor, or the
// agent picker for starting a task.
type taskStage int

const (
	taskStageList taskStage = iota
	taskStageNew
	taskStageDetail
	taskStageAgent
	taskStageRun
)

// taskRowKind distinguishes the flattened list's row types: section headers
// and the "(N more)" note aren't selectable; only task rows take the cursor.
type taskRowKind int

const (
	taskRowHeader taskRowKind = iota
	taskRowTask
	taskRowNote
)

// taskRow is one rendered row of the backlog list.
type taskRow struct {
	kind taskRowKind
	text string // header / note label
	id   string // task id when kind == taskRowTask
}

// taskCompletedShown caps how many completed tasks the list renders; the rest
// collapse into a "(N more)" note so a long-lived backlog doesn't bury the
// active sections.
const taskCompletedShown = 10

// tasksMsg carries a fresh backlog snapshot back from a mutation (add / edit /
// status / delete), so the dialog reflects the daemon's authoritative list
// without waiting for the next poll. selectID, when set, lands the cursor on a
// just-created task.
type tasksMsg struct {
	tasks    []ipc.Task
	selectID string
	err      error
}

// taskSpawnedMsg reports that a task's agent session was spawned (and linked to
// the task daemon-side), so Update can jump the dashboard there. tasks carries
// the post-start backlog snapshot.
type taskSpawnedMsg struct {
	sessionID string
	tasks     []ipc.Task
}

// applyTasks replaces the read cache with a daemon snapshot, rebuilding the
// visible rows when the dialog is open. nil is a valid (empty) backlog.
func (m *dashboardModel) applyTasks(tasks []ipc.Task) {
	if m.taskStore == nil {
		m.taskStore = &task.Store{}
	}
	m.taskStore.Tasks = tasks
	if m.taskOpen {
		m.rebuildTaskRows()
	}
}

// openTaskBacklog (prefix+t) opens the dialog on the read cache, which the list
// poll keeps in step with the daemon (the backlog's single writer) — no
// blocking disk read on open. A refresh is already in flight every 500ms, so a
// mutation from another cb client shows up within a tick.
func (m *dashboardModel) openTaskBacklog() {
	if m.taskStore == nil {
		m.taskStore = &task.Store{}
	}
	m.taskOpen = true
	m.taskStage = taskStageList
	m.taskPrefix = false
	m.rebuildTaskRows()
}

func (m *dashboardModel) closeTaskBacklog() {
	m.taskOpen = false
	m.taskPrefix = false
}

// rebuildTaskRows flattens the current scope's tasks into sectioned rows:
// in progress / paused / pending in creation order (oldest first, matching a
// queue), completed newest-first and capped. Runs whenever the underlying
// tasks change while the dialog is open.
func (m *dashboardModel) rebuildTaskRows() {
	if m.taskStore == nil {
		m.taskRows = nil
		return
	}
	var inProg, paused, pending, completed []*task.Task
	for _, t := range m.taskStore.ForScope(m.currentScope) {
		switch t.Status {
		case task.StatusInProgress:
			inProg = append(inProg, t)
		case task.StatusPaused:
			paused = append(paused, t)
		case task.StatusCompleted:
			completed = append(completed, t)
		default:
			pending = append(pending, t)
		}
	}
	sort.SliceStable(completed, func(i, j int) bool {
		return completed[i].UpdatedAt.After(completed[j].UpdatedAt)
	})

	var rows []taskRow
	section := func(name string, ts []*task.Task) {
		if len(ts) == 0 {
			return
		}
		rows = append(rows, taskRow{kind: taskRowHeader, text: name})
		for _, t := range ts {
			rows = append(rows, taskRow{kind: taskRowTask, id: t.ID})
		}
	}
	section("in progress", inProg)
	section("paused", paused)
	section("pending", pending)
	shown := completed
	if len(shown) > taskCompletedShown {
		shown = shown[:taskCompletedShown]
	}
	section("completed", shown)
	if n := len(completed) - len(shown); n > 0 {
		rows = append(rows, taskRow{kind: taskRowNote, text: fmt.Sprintf("(%d more)", n)})
	}
	m.taskRows = rows
	m.clampTaskCursor()
}

// clampTaskCursor parks the cursor on the nearest selectable (task) row,
// preferring forward, since headers/notes can't take it.
func (m *dashboardModel) clampTaskCursor() {
	if len(m.taskRows) == 0 {
		m.taskCursor = 0
		return
	}
	if m.taskCursor >= len(m.taskRows) {
		m.taskCursor = len(m.taskRows) - 1
	}
	if m.taskCursor < 0 {
		m.taskCursor = 0
	}
	if m.taskRows[m.taskCursor].kind == taskRowTask {
		return
	}
	for i := m.taskCursor + 1; i < len(m.taskRows); i++ {
		if m.taskRows[i].kind == taskRowTask {
			m.taskCursor = i
			return
		}
	}
	for i := m.taskCursor - 1; i >= 0; i-- {
		if m.taskRows[i].kind == taskRowTask {
			m.taskCursor = i
			return
		}
	}
}

// moveTaskCursor steps the cursor to the next task row in the given direction,
// hopping over section headers and notes. No wrap.
// moveTaskCursor advances the cursor to the next selectable task row in the
// given direction, skipping section headers and notes and wrapping around the
// ends (down past the last task lands on the first, up past the first lands on
// the last). n steps is enough to visit every row exactly once, so if any task
// row exists the cursor lands on one; if none do it stays put.
func (m *dashboardModel) moveTaskCursor(delta int) {
	n := len(m.taskRows)
	if n == 0 || delta == 0 {
		return
	}
	i := m.taskCursor
	for step := 0; step < n; step++ {
		i = ((i+delta)%n + n) % n
		if m.taskRows[i].kind == taskRowTask {
			m.taskCursor = i
			return
		}
	}
}

// taskUnderCursor resolves the cursor row to its task, or nil when the cursor
// isn't on a task row.
func (m *dashboardModel) taskUnderCursor() *task.Task {
	if m.taskStore == nil || m.taskCursor < 0 || m.taskCursor >= len(m.taskRows) {
		return nil
	}
	r := m.taskRows[m.taskCursor]
	if r.kind != taskRowTask {
		return nil
	}
	return m.taskStore.Get(r.id)
}

// selectTaskRow points the cursor at the row for the given task id (used to
// land on a task the user just created).
func (m *dashboardModel) selectTaskRow(id string) {
	for i, r := range m.taskRows {
		if r.kind == taskRowTask && r.id == id {
			m.taskCursor = i
			return
		}
	}
}

// handleTaskKey owns every keystroke while the backlog dialog is open,
// dispatching by stage.
func (m *dashboardModel) handleTaskKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch m.taskStage {
	case taskStageNew:
		return m.handleTaskNewKey(msg)
	case taskStageDetail:
		return m.handleTaskDetailKey(msg)
	case taskStageAgent:
		return m.handleTaskAgentKey(msg)
	case taskStageRun:
		return m.handleTaskRunKey(msg)
	}
	return m.handleTaskListKey(msg)
}

// handleTaskListKey drives the sectioned list. It has a local prefix layer so
// the muscle-memory chords keep working inside the dialog: prefix+n opens the
// new-task input and prefix+<task_backlog binding> closes the dialog, mirroring
// how the same chords behave outside it.
func (m *dashboardModel) handleTaskListKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	s := msg.String()
	if m.taskPrefix {
		m.taskPrefix = false
		switch s {
		case "n":
			m.beginNewTask()
		case m.keyForAction("task_backlog"):
			m.closeTaskBacklog()
		}
		return m, nil
	}
	if s == prefixKeyName {
		m.taskPrefix = true
		return m, nil
	}
	switch s {
	case "esc", "q", "ctrl+c":
		m.closeTaskBacklog()
	case "up", "k":
		m.moveTaskCursor(-1)
	case "down", "j":
		m.moveTaskCursor(1)
	case "n":
		m.beginNewTask()
	case "enter":
		t := m.taskUnderCursor()
		if t == nil {
			break
		}
		if t.Status == task.StatusInProgress {
			// Jump to the most recently started live run.
			m.closeAndJumpToTask(t)
			break
		}
		m.beginTaskDetail(t)
	case "e":
		if t := m.taskUnderCursor(); t != nil {
			m.beginTaskDetail(t)
		}
	case "s":
		t := m.taskUnderCursor()
		if t == nil {
			break
		}
		if t.Status != task.StatusCompleted {
			agents := availableAgents()
			if len(agents) == 0 {
				m.pushToast("✗ no agent binaries found (claude/codex/opencode)", "needs_approval")
				break
			}
			m.taskAgents = agents
			m.taskAgentCursor = 0
			m.taskStartID = t.ID
			m.taskStage = taskStageAgent
		}
	case "r":
		if t := m.taskUnderCursor(); t != nil {
			for i := len(t.Runs) - 1; i >= 0; i-- {
				if t.Runs[i].Status == task.StatusPaused {
					return m, m.taskResumeCmd(t.ID, t.Runs[i].ID)
				}
			}
		}
	case "K":
		if t := m.taskUnderCursor(); t != nil && len(t.Runs) > 0 {
			m.taskRunTaskID, m.taskRunCursor, m.taskStage = t.ID, 0, taskStageRun
		}
	case "c":
		if t := m.taskUnderCursor(); t != nil {
			next := task.StatusCompleted
			if t.Status == task.StatusCompleted {
				next = task.StatusPending
			}
			return m, m.taskStatusCmd(t.ID, next)
		}
	case "x":
		if t := m.taskUnderCursor(); t != nil {
			return m, m.taskDeleteCmd(t.ID)
		}
	}
	return m, nil
}

func (m *dashboardModel) beginNewTask() {
	m.taskStage = taskStageNew
	m.taskTitleBuf = ""
	m.taskDescBuf = ""
	m.taskNewTitle = true
}

// handleTaskNewKey edits a new task's title and description. Enter advances
// from the title to the description; Ctrl+Enter saves the form.
func (m *dashboardModel) handleTaskNewKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch {
	case msg.Code == tea.KeyEnter && msg.Mod&tea.ModCtrl != 0:
		title := strings.TrimSpace(m.taskTitleBuf)
		desc := strings.TrimSpace(m.taskDescBuf)
		m.taskTitleBuf = ""
		m.taskDescBuf = ""
		m.taskStage = taskStageList
		if title != "" {
			return m, m.taskAddCmd(m.currentScope, title, desc)
		}
	case msg.Code == tea.KeyEscape, msg.Code == 'c' && msg.Mod&tea.ModCtrl != 0:
		m.taskTitleBuf = ""
		m.taskDescBuf = ""
		m.taskStage = taskStageList
	case msg.Code == tea.KeyTab:
		m.taskNewTitle = !m.taskNewTitle
	case msg.Code == tea.KeyEnter:
		if m.taskNewTitle {
			m.taskNewTitle = false
		} else {
			m.taskDescBuf += "\n"
		}
	case msg.Code == tea.KeyBackspace, msg.Code == tea.KeyDelete:
		buf := &m.taskDescBuf
		if m.taskNewTitle {
			buf = &m.taskTitleBuf
		}
		if r := []rune(*buf); len(r) > 0 {
			*buf = string(r[:len(r)-1])
		}
	case msg.Text != "":
		if m.taskNewTitle {
			m.taskTitleBuf += msg.Text
		} else {
			m.taskDescBuf += msg.Text
		}
	}
	return m, nil
}

// handleTaskPaste applies bracketed paste to the focused task form field.
// A multi-line paste into a new task's title naturally fills its description,
// which makes pasting a small task brief convenient without allowing embedded
// newlines in a title.
func (m *dashboardModel) handleTaskPaste(text string) {
	if text == "" {
		return
	}
	switch m.taskStage {
	case taskStageNew:
		if m.taskNewTitle {
			parts := strings.SplitN(text, "\n", 2)
			m.taskTitleBuf += strings.TrimSuffix(parts[0], "\r")
			if len(parts) == 2 {
				m.taskDescBuf += strings.TrimPrefix(parts[1], "\r")
				m.taskNewTitle = false
			}
		} else {
			m.taskDescBuf += text
		}
	case taskStageDetail:
		if m.taskEditTitle {
			m.taskTitleEdit += strings.ReplaceAll(text, "\n", " ")
		} else {
			m.taskDescEdit += text
		}
	}
}

func (m *dashboardModel) beginTaskDetail(t *task.Task) {
	m.taskStage = taskStageDetail
	m.taskDetailID = t.ID
	m.taskEditTitle = true
	m.taskTitleEdit = t.Title
	m.taskDescEdit = t.Desc
}

// handleTaskDetailKey edits a task's title and multi-line description. tab
// switches fields; enter is "go to description" from the title and a literal
// newline inside it; esc saves and returns to the list (there is no separate
// cancel — edits are cheap to redo and losing typed text is worse).
func (m *dashboardModel) handleTaskDetailKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch {
	case msg.Code == tea.KeyEscape:
		id := m.taskDetailID
		title, desc := m.taskTitleEdit, m.taskDescEdit
		m.taskStage = taskStageList
		m.taskDetailID = ""
		if id != "" {
			return m, m.taskEditCmd(id, title, desc)
		}
	case msg.Code == tea.KeyTab:
		m.taskEditTitle = !m.taskEditTitle
	case msg.Code == tea.KeyEnter:
		if m.taskEditTitle {
			m.taskEditTitle = false // jump to the description field
		} else {
			m.taskDescEdit += "\n"
		}
	case msg.Code == tea.KeyBackspace, msg.Code == tea.KeyDelete:
		buf := &m.taskDescEdit
		if m.taskEditTitle {
			buf = &m.taskTitleEdit
		}
		if r := []rune(*buf); len(r) > 0 {
			*buf = string(r[:len(r)-1])
		}
	case msg.Text != "":
		if m.taskEditTitle {
			m.taskTitleEdit += msg.Text
		} else {
			m.taskDescEdit += msg.Text
		}
	}
	return m, nil
}

// handleTaskAgentKey is the picker for which agent runs the task. enter spawns
// it (closing the dialog), esc steps back to the list.
func (m *dashboardModel) handleTaskAgentKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "q", "ctrl+c":
		m.closeTaskBacklog()
	case "esc", "left", "h":
		m.taskStage = taskStageList
	case "up", "k":
		if n := len(m.taskAgents); n > 0 {
			m.taskAgentCursor = (m.taskAgentCursor - 1 + n) % n
		}
	case "down", "j":
		if n := len(m.taskAgents); n > 0 {
			m.taskAgentCursor = (m.taskAgentCursor + 1) % n
		}
	case "enter", "right", "l":
		if m.taskAgentCursor >= 0 && m.taskAgentCursor < len(m.taskAgents) {
			bin := m.taskAgents[m.taskAgentCursor].bin
			id := m.taskStartID
			m.closeTaskBacklog()
			if id != "" {
				return m, m.taskStartCmd(id, bin)
			}
		}
	}
	return m, nil
}

// handleTaskRunKey lets a task own several sessions without making the main
// backlog list unwieldy. Killing here affects only the selected live run, not
// the task or any sibling run.
func (m *dashboardModel) handleTaskRunKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	t := m.taskStore.Get(m.taskRunTaskID)
	if t == nil {
		m.taskStage = taskStageList
		return m, nil
	}
	switch msg.String() {
	case "q", "ctrl+c":
		m.closeTaskBacklog()
	case "esc", "left", "h":
		m.taskStage = taskStageList
	case "up", "k":
		if n := len(t.Runs); n > 0 {
			m.taskRunCursor = (m.taskRunCursor - 1 + n) % n
		}
	case "down", "j":
		if n := len(t.Runs); n > 0 {
			m.taskRunCursor = (m.taskRunCursor + 1) % n
		}
	case "enter", "right", "l":
		if m.taskRunCursor >= 0 && m.taskRunCursor < len(t.Runs) {
			r := t.Runs[m.taskRunCursor]
			if r.Status == task.StatusInProgress {
				m.closeAndJumpToRun(r.CBSessionID)
			}
		}
	case "x":
		if m.taskRunCursor >= 0 && m.taskRunCursor < len(t.Runs) {
			r := t.Runs[m.taskRunCursor]
			if r.Status == task.StatusInProgress && r.CBSessionID != "" {
				return m, killCmd(r.CBSessionID)
			}
		}
	}
	return m, nil
}

// taskCmd sends one backlog request to the daemon and folds the reply into a
// tasksMsg. The daemon is the single writer, so the reply carries the fresh
// authoritative list; selectID (only meaningful for adds) is threaded through.
func taskCmd(req ipc.Request, selectID string) tea.Cmd {
	return func() tea.Msg {
		resp, err := ipc.Send(req)
		if err == nil && !resp.OK {
			err = errors.New(resp.Error)
		}
		if err != nil {
			return tasksMsg{err: err}
		}
		id := selectID
		if req.Type == "task_add" {
			id = resp.ID // the daemon minted the id
		}
		return tasksMsg{tasks: resp.Tasks, selectID: id}
	}
}

func (m *dashboardModel) taskAddCmd(scope, title, desc string) tea.Cmd {
	return taskCmd(ipc.Request{Type: "task_add", Scope: scope, Title: title, Desc: desc}, "pending")
}

func (m *dashboardModel) taskEditCmd(id, title, desc string) tea.Cmd {
	return taskCmd(ipc.Request{Type: "task_edit", ID: id, Title: title, Desc: desc}, "")
}

func (m *dashboardModel) taskStatusCmd(id string, status task.Status) tea.Cmd {
	return taskCmd(ipc.Request{Type: "task_status", ID: id, Status: string(status)}, "")
}

func (m *dashboardModel) taskDeleteCmd(id string) tea.Cmd {
	return taskCmd(ipc.Request{Type: "task_delete", ID: id}, "")
}

// taskStartCmd asks the daemon to spawn an agent session for the task and link
// it. The daemon owns the resume/prefill logic (see daemon.taskStart); the
// client only supplies the agent binary, the launch cwd, and the pane size.
// The binary is checked up front so a missing agent surfaces a clear toast
// rather than a silent no-op (the daemon can't report a PATH miss over IPC).
func (m *dashboardModel) taskStartCmd(id, bin string) tea.Cmd {
	if _, err := exec.LookPath(bin); err != nil {
		return func() tea.Msg { return spawnMissingMsg{bin: bin} }
	}
	cwd := m.spawnTargetCwd()
	rows, cols := m.paneH, m.paneW
	return func() tea.Msg {
		if cwd == "" {
			cwd, _ = os.Getwd()
		}
		req := ipc.Request{Type: "task_start", ID: id, Agent: bin, Cwd: cwd}
		if rows > 0 && cols > 0 {
			req.Rows, req.Cols = rows, cols
		}
		resp, err := ipc.Send(req)
		if err != nil || !resp.OK {
			return refreshCmd()
		}
		return taskSpawnedMsg{sessionID: resp.ID, tasks: resp.Tasks}
	}
}

func (m *dashboardModel) taskResumeCmd(id, runID string) tea.Cmd {
	return func() tea.Msg {
		cwd := m.spawnTargetCwd()
		if cwd == "" {
			cwd, _ = os.Getwd()
		}
		req := ipc.Request{Type: "task_resume", ID: id, RunID: runID, Cwd: cwd}
		if m.paneH > 0 && m.paneW > 0 {
			req.Rows, req.Cols = m.paneH, m.paneW
		}
		resp, err := ipc.Send(req)
		if err != nil || !resp.OK {
			return refreshCmd()
		}
		return taskSpawnedMsg{sessionID: resp.ID, tasks: resp.Tasks}
	}
}

// closeAndJumpToTask closes the dialog and lands the dashboard on the task's
// live session: open its scope group (flipping into accordion mode if the
// session lives outside the launch workspace), select it, re-attach the screen
// pane, and focus it — the same recipe as jump_pending.
func (m *dashboardModel) closeAndJumpToTask(t *task.Task) {
	var sessionID string
	for i := len(t.Runs) - 1; i >= 0; i-- {
		if t.Runs[i].Status == task.StatusInProgress {
			sessionID = t.Runs[i].CBSessionID
			break
		}
	}
	m.closeAndJumpToRun(sessionID)
}

func (m *dashboardModel) closeAndJumpToRun(sessionID string) {
	m.closeTaskBacklog()
	for _, s := range m.sessions {
		if s.ID != sessionID {
			continue
		}
		key := m.scopeKeyOf(s.Cwd)
		if !m.accordionMode && key != m.currentScope {
			m.accordionMode = true
		}
		if !m.expanded[key] {
			m.setScopeExpanded(key, true)
		}
		m.selScope = key
		m.selSession = s.ID
		m.rebuildRows()
		m.syncStream()
		m.focusScreenPane()
		return
	}
}

// taskGlyph is the status indicator for one task row. An in_progress task
// borrows the sidebar's live indicator for its linked session (so it spins,
// flags approvals, etc. for free — m.spin already ticks); the resting states
// get their own glyphs.
func (m *dashboardModel) taskGlyph(t *task.Task) string {
	switch t.Status {
	case task.StatusInProgress:
		status := "working"
		for _, r := range t.Runs {
			if r.Status == task.StatusInProgress {
				if s := m.sessionByID(r.CBSessionID); s != nil {
					status = s.Status
				}
				break
			}
		}
		return m.indicator(status)
	case task.StatusPaused:
		return statusStyle["idle"].Render("‖")
	case task.StatusCompleted:
		return statusStyle["ended"].Render("✓")
	default: // pending
		return helpStyle.Render("○")
	}
}

// renderTaskBacklog draws whichever stage is active, in the same panel style
// as the config and worktree modals.
func (m *dashboardModel) renderTaskBacklog() string {
	switch m.taskStage {
	case taskStageNew:
		return m.renderTaskNew()
	case taskStageDetail:
		return m.renderTaskDetail()
	case taskStageAgent:
		return m.renderTaskAgentStage()
	case taskStageRun:
		return m.renderTaskRunStage()
	}
	return m.renderTaskList()
}

// taskTitleCol is the width budget for task titles in the list stage.
const taskTitleCol = 40

func (m *dashboardModel) renderTaskList() string {
	lines := []string{
		titleStyle.Render("tasks — " + scopeDisplayName(m.currentScope)),
	}
	if len(m.taskRows) == 0 {
		lines = append(lines, "", helpStyle.Render("no tasks — press n to create one"))
	}
	for i, r := range m.taskRows {
		switch r.kind {
		case taskRowHeader:
			lines = append(lines, "", helpStyle.Render(r.text))
		case taskRowNote:
			lines = append(lines, "  "+helpStyle.Render(r.text))
		default:
			t := m.taskStore.Get(r.id)
			if t == nil {
				continue
			}
			marker := "  "
			if i == m.taskCursor {
				marker = selBarStyle.Render("▌ ")
			}
			runs := ""
			if n := len(t.Runs); n > 0 {
				runs = helpStyle.Render(fmt.Sprintf("  %d session", n)) + map[bool]string{true: "s", false: ""}[n != 1]
			}
			lines = append(lines, marker+m.taskGlyph(t)+" "+truncate(t.Title, taskTitleCol)+runs)
		}
	}
	lines = append(lines, "", helpStyle.Render("j/k move · n new · enter open · e details · s new session · r resume · K sessions · c done · x delete · esc close"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

func (m *dashboardModel) renderTaskNew() string {
	titleLine := m.taskTitleBuf
	descLines := strings.Split(m.taskDescBuf, "\n")
	if m.taskNewTitle {
		titleLine += "▎"
	} else {
		descLines[len(descLines)-1] += "▎"
	}
	lines := []string{
		titleStyle.Render("new task"),
		"",
		helpStyle.Render("title"),
		titleLine,
		"",
		helpStyle.Render("description"),
		"",
	}
	lines = append(lines, descLines...)
	lines = append(lines, "", helpStyle.Render("tab switch field · enter newline · ctrl+enter add · esc cancel"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

func (m *dashboardModel) renderTaskDetail() string {
	titleLine := m.taskTitleEdit
	descLines := strings.Split(m.taskDescEdit, "\n")
	// Fake input cursor on whichever field is being edited.
	if m.taskEditTitle {
		titleLine += "▎"
	} else {
		descLines[len(descLines)-1] += "▎"
	}
	lines := []string{
		titleStyle.Render("edit task"),
		"",
		helpStyle.Render("title"),
		titleLine,
		"",
		helpStyle.Render("description"),
	}
	lines = append(lines, descLines...)
	if t := m.taskStore.Get(m.taskDetailID); t != nil && len(t.Runs) > 0 {
		lines = append(lines, "", helpStyle.Render("sessions"))
		for _, r := range t.Runs {
			state := string(r.Status)
			if r.Status == task.StatusInProgress {
				state = "active"
			}
			lines = append(lines, fmt.Sprintf("  %s  %s · %s", m.runGlyph(r), r.Agent, state))
		}
	}
	lines = append(lines, "", helpStyle.Render("tab switch field · enter newline · esc save"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

func (m *dashboardModel) runGlyph(r ipc.TaskRun) string {
	if r.Status == task.StatusInProgress {
		if s := m.sessionByID(r.CBSessionID); s != nil {
			return m.indicator(s.Status)
		}
		return m.indicator("working")
	}
	return statusStyle["idle"].Render("‖")
}

func (m *dashboardModel) renderTaskAgentStage() string {
	header := "start task"
	var note string
	if t := m.taskStore.Get(m.taskStartID); t != nil {
		header = "start: " + truncate(t.Title, 32)
		if t.Status == task.StatusPaused {
			note = "resumes the previous agent session where possible"
		}
	}
	lines := []string{
		titleStyle.Render(header),
		helpStyle.Render("choose agent"),
		"",
	}
	for i, a := range m.taskAgents {
		marker := "  "
		if i == m.taskAgentCursor {
			marker = selBarStyle.Render("▌ ")
		}
		lines = append(lines, marker+a.label)
	}
	if note != "" {
		lines = append(lines, "", helpStyle.Render(note))
	}
	lines = append(lines, "", helpStyle.Render("↑↓ select · enter start · esc back"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}

func (m *dashboardModel) renderTaskRunStage() string {
	t := m.taskStore.Get(m.taskRunTaskID)
	if t == nil {
		return configPanelStyle.Render(helpStyle.Render("task no longer exists"))
	}
	lines := []string{titleStyle.Render("sessions: " + truncate(t.Title, 32)), ""}
	for i, r := range t.Runs {
		marker := "  "
		if i == m.taskRunCursor {
			marker = selBarStyle.Render("▌ ")
		}
		state := "paused"
		if r.Status == task.StatusInProgress {
			state = "active"
		}
		label := r.Agent
		if label == "" {
			label = "agent"
		}
		lines = append(lines, marker+m.runGlyph(r)+" "+label+" · "+state)
	}
	lines = append(lines, "", helpStyle.Render("j/k select · enter open active · x kill active · esc back"))
	return configPanelStyle.Render(strings.Join(lines, "\n"))
}
