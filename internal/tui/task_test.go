package tui

import (
	"fmt"
	"path/filepath"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"

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

// TestTaskMultipleSessions: pressing s on an in_progress task opens the agent
// picker so a second session can be started for the same task.
func TestTaskMultipleSessions(t *testing.T) {
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
	if m.taskStage != taskStageAgent {
		t.Fatal("agent picker did not open for an in_progress task")
	}
}

// TestTaskNewFlow drives the new-task form: n opens it, typed title and
// description plus Ctrl+Enter returns to the list and emits a mutation command
// (the daemon, not the client, performs the actual add — see the daemon's
// task_add test). An empty title emits no command.
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
	for _, ch := range []string{"d", "e", "s", "c"} {
		m.handleTaskKey(key(ch))
	}
	_, cmd := m.handleTaskKey(tea.KeyPressMsg{Code: tea.KeyEnter, Mod: tea.ModCtrl})

	if m.taskStage != taskStageList {
		t.Fatalf("stage = %v, want list after enter", m.taskStage)
	}
	if m.taskTitleBuf != "" {
		t.Errorf("title buffer not cleared: %q", m.taskTitleBuf)
	}
	if m.taskDescBuf != "" {
		t.Errorf("description buffer not cleared: %q", m.taskDescBuf)
	}
	if cmd == nil {
		t.Fatal("enter with a non-empty title should emit an add command")
	}

	// An empty title is dropped: no command.
	m.handleTaskKey(key("n"))
	if _, cmd := m.handleTaskKey(tea.KeyPressMsg{Code: tea.KeyEnter, Mod: tea.ModCtrl}); cmd != nil {
		t.Error("enter with an empty title should not emit a command")
	}
}

func TestTaskPasteIntoNewForm(t *testing.T) {
	m := &dashboardModel{taskOpen: true, taskStage: taskStageNew, taskNewTitle: true}
	m.handleTaskPaste("fix paste\ninclude a description")
	if m.taskTitleBuf != "fix paste" || m.taskDescBuf != "include a description" || m.taskNewTitle {
		t.Fatalf("paste did not fill new-task form: title=%q desc=%q titleActive=%v", m.taskTitleBuf, m.taskDescBuf, m.taskNewTitle)
	}
}
