package tui

import (
	"fmt"
	"path/filepath"
	"testing"
	"time"

	"codebridge/internal/ipc"
	"codebridge/internal/task"
)

// testTaskStore returns a store bound to a temp file so saves triggered by the
// handlers under test never touch the real ~/.cb/tasks.json.
func testTaskStore(t *testing.T) *task.Store {
	t.Helper()
	return task.LoadFrom(filepath.Join(t.TempDir(), "tasks.json"))
}

// headersOf extracts the section header texts from a flattened row list.
func headersOf(rows []taskRow) []string {
	var out []string
	for _, r := range rows {
		if r.kind == taskRowHeader {
			out = append(out, r.text)
		}
	}
	return out
}

// TestRebuildTaskRowsSections seeds every status (plus an out-of-scope task)
// and checks the section order, scope filtering, the completed cap with its
// "(N more)" note, and completed newest-first ordering.
func TestRebuildTaskRowsSections(t *testing.T) {
	st := testTaskStore(t)
	other := st.Add("other", "other-scope", "").ID
	pend1 := st.Add("cur", "pend-old", "").ID
	pend2 := st.Add("cur", "pend-new", "").ID
	paused := st.Add("cur", "paused", "").ID
	inprog := st.Add("cur", "running", "").ID
	st.Get(paused).Status = task.StatusPaused
	st.Get(inprog).Status = task.StatusInProgress
	var completed []string
	base := time.Now()
	for i := 0; i < 12; i++ {
		id := st.Add("cur", fmt.Sprintf("done-%02d", i), "").ID
		c := st.Get(id)
		c.Status = task.StatusCompleted
		c.UpdatedAt = base.Add(time.Duration(i) * time.Minute) // done-11 is newest
		completed = append(completed, id)
	}

	m := &dashboardModel{currentScope: "cur", taskStore: st}
	m.rebuildTaskRows()

	wantHeaders := []string{"in progress", "paused", "pending", "completed"}
	got := headersOf(m.taskRows)
	if len(got) != len(wantHeaders) {
		t.Fatalf("headers = %v, want %v", got, wantHeaders)
	}
	for i := range wantHeaders {
		if got[i] != wantHeaders[i] {
			t.Fatalf("headers = %v, want %v", got, wantHeaders)
		}
	}

	var ids []string
	notes := 0
	for _, r := range m.taskRows {
		switch r.kind {
		case taskRowTask:
			ids = append(ids, r.id)
		case taskRowNote:
			notes++
			if r.text != "(2 more)" {
				t.Errorf("note = %q, want (2 more)", r.text)
			}
		}
	}
	// 1 in progress + 1 paused + 2 pending + 10 completed (12 capped).
	if len(ids) != 14 {
		t.Fatalf("got %d task rows, want 14: %v", len(ids), ids)
	}
	if notes != 1 {
		t.Errorf("got %d notes, want 1", notes)
	}
	for _, id := range ids {
		if id == other {
			t.Error("out-of-scope task leaked into the rows")
		}
	}
	// Section membership in order: in_progress, paused, pending (oldest first),
	// then completed newest-first.
	want := append([]string{inprog, paused, pend1, pend2},
		completed[11], completed[10], completed[9], completed[8], completed[7],
		completed[6], completed[5], completed[4], completed[3], completed[2])
	for i := range want {
		if ids[i] != want[i] {
			t.Fatalf("row %d = %s, want %s (all: %v)", i, ids[i], want[i], ids)
		}
	}
	// Cursor must land on a task row, not the leading header.
	if m.taskCursor < 0 || m.taskCursor >= len(m.taskRows) || m.taskRows[m.taskCursor].kind != taskRowTask {
		t.Errorf("cursor %d not on a task row", m.taskCursor)
	}
}

// TestSyncTasksTransitions covers the reconcile pass: harvest a claude session
// id from a live session; pause tasks whose session vanished / exited /
// reported ended (keeping the resume handle); leave freshly started tasks
// alone (grace window) and non-in_progress tasks untouched.
func TestSyncTasksTransitions(t *testing.T) {
	st := testTaskStore(t)
	old := time.Now().Add(-time.Minute)
	mk := func(title, cbID, agentID string) string {
		id := st.Add("cur", title, "").ID
		tk := st.Get(id)
		tk.Status = task.StatusInProgress
		tk.CBSessionID = cbID
		tk.AgentSessionID = agentID
		tk.UpdatedAt = old
		return id
	}
	harvest := mk("harvest", "s1", "")
	vanished := mk("vanished", "s2", "claude-keep")
	exited := mk("exited", "s3", "")
	ended := mk("ended", "s4", "")
	fresh := mk("fresh", "s5", "")
	st.Get(fresh).UpdatedAt = time.Now() // inside the grace window
	pausedID := st.Add("cur", "already-paused", "").ID
	st.Get(pausedID).Status = task.StatusPaused
	st.Get(pausedID).AgentSessionID = "claude-old"

	m := &dashboardModel{currentScope: "cur", taskStore: st}
	m.syncTasks([]ipc.SessionInfo{
		{ID: "s1", Status: "working", HarnessSessionID: "claude-1"},
		{ID: "s3", Status: "working", Exited: true},
		{ID: "s4", Status: "ended"},
	})

	if got := st.Get(harvest); got.Status != task.StatusInProgress || got.AgentSessionID != "claude-1" {
		t.Errorf("harvest task: %+v, want in_progress with claude-1", got)
	}
	for _, id := range []string{vanished, exited, ended} {
		got := st.Get(id)
		if got.Status != task.StatusPaused {
			t.Errorf("%s: status = %s, want paused", got.Title, got.Status)
		}
		if got.CBSessionID != "" {
			t.Errorf("%s: cb_session_id not cleared: %q", got.Title, got.CBSessionID)
		}
	}
	if got := st.Get(vanished); got.AgentSessionID != "claude-keep" {
		t.Errorf("pause dropped the resume handle: %+v", got)
	}
	if got := st.Get(fresh); got.Status != task.StatusInProgress {
		t.Errorf("fresh task paused despite grace window: %+v", got)
	}
	if got := st.Get(pausedID); got.Status != task.StatusPaused || got.AgentSessionID != "claude-old" {
		t.Errorf("paused task touched: %+v", got)
	}
}

// TestTaskDoubleStartGuard: pressing s on an in_progress task must not open
// the agent picker (which would spawn a second agent on the same work) — it
// closes the dialog and jumps to the live session instead.
func TestTaskDoubleStartGuard(t *testing.T) {
	st := testTaskStore(t)
	id := st.Add("cur", "busy", "").ID
	st.Get(id).Status = task.StatusInProgress
	st.Get(id).CBSessionID = "sess-1"

	m := &dashboardModel{
		currentScope: "cur",
		taskStore:    st,
		taskOpen:     true,
		taskStage:    taskStageList,
		expanded:     map[string]bool{},
	}
	m.rebuildTaskRows()
	if got := m.taskUnderCursor(); got == nil || got.ID != id {
		t.Fatalf("cursor not on the in_progress task: %+v", got)
	}

	m.handleTaskKey(key("s"))
	if m.taskStage == taskStageAgent {
		t.Fatal("agent picker opened for an in_progress task")
	}
	if m.taskOpen {
		t.Fatal("dialog should close (jump to session) on s for in_progress")
	}
}

// TestTaskNewFlow drives the new-task input: n opens it, typed text plus enter
// commits a pending task in the current scope, and the cursor lands on it.
func TestTaskNewFlow(t *testing.T) {
	st := testTaskStore(t)
	m := &dashboardModel{
		currentScope: "cur",
		taskStore:    st,
		taskOpen:     true,
		taskStage:    taskStageList,
	}
	m.rebuildTaskRows()

	m.handleTaskKey(key("n"))
	if m.taskStage != taskStageNew {
		t.Fatalf("stage = %v, want new after n", m.taskStage)
	}
	for _, ch := range []string{"f", "i", "x"} {
		m.handleTaskKey(key(ch))
	}
	m.handleTaskKey(key("enter"))

	if m.taskStage != taskStageList {
		t.Fatalf("stage = %v, want list after enter", m.taskStage)
	}
	tasks := st.ForScope("cur")
	if len(tasks) != 1 || tasks[0].Title != "fix" || tasks[0].Status != task.StatusPending {
		t.Fatalf("committed task wrong: %+v", tasks)
	}
	if got := m.taskUnderCursor(); got == nil || got.ID != tasks[0].ID {
		t.Errorf("cursor not on the new task: %+v", got)
	}
}
