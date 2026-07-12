package daemon

import (
	"path/filepath"
	"testing"
	"time"

	"codebridge/internal/ipc"
	"codebridge/internal/task"
)

// newTaskDaemon returns a daemon whose backlog is bound to a temp file, so
// saves triggered by the handlers under test never touch the real store.
func newTaskDaemon(t *testing.T) *Daemon {
	t.Helper()
	// task_* dispatch never touches the session registry, so a nil map is fine.
	return &Daemon{taskStore: task.LoadFrom(filepath.Join(t.TempDir(), "tasks.json"))}
}

// TestTaskDispatchLifecycle exercises add → edit → status → delete through the
// daemon's dispatcher, checking each reply carries the fresh list and the store
// is mutated as expected.
func TestTaskDispatchLifecycle(t *testing.T) {
	d := newTaskDaemon(t)

	// add
	resp := d.taskDispatch(ipc.Request{Type: "task_add", Scope: "cur", Title: "  fix bug  "})
	if !resp.OK || resp.ID == "" {
		t.Fatalf("task_add: %+v", resp)
	}
	id := resp.ID
	if len(resp.Tasks) != 1 || resp.Tasks[0].Title != "fix bug" || resp.Tasks[0].Status != task.StatusPending {
		t.Fatalf("task_add tasks: %+v", resp.Tasks)
	}

	// empty title is rejected
	if r := d.taskDispatch(ipc.Request{Type: "task_add", Scope: "cur", Title: "   "}); r.OK {
		t.Fatalf("task_add with blank title should fail: %+v", r)
	}

	// edit
	resp = d.taskDispatch(ipc.Request{Type: "task_edit", ID: id, Title: "fix the bug", Desc: "details\n"})
	if !resp.OK {
		t.Fatalf("task_edit: %+v", resp)
	}
	if got := d.taskStore.Get(id); got.Title != "fix the bug" || got.Desc != "details" {
		t.Fatalf("edited task: %+v", got)
	}

	// status → completed preserves run history (including a live link).
	d.taskStore.Get(id).Runs = []ipc.TaskRun{{ID: "run-1", CBSessionID: "sess-1", Status: task.StatusInProgress}}
	resp = d.taskDispatch(ipc.Request{Type: "task_status", ID: id, Status: string(task.StatusCompleted)})
	if !resp.OK {
		t.Fatalf("task_status: %+v", resp)
	}
	if got := d.taskStore.Get(id); got.Status != task.StatusCompleted || got.Runs[0].CBSessionID != "sess-1" {
		t.Fatalf("completed task: %+v", got)
	}

	// delete
	resp = d.taskDispatch(ipc.Request{Type: "task_delete", ID: id})
	if !resp.OK || len(resp.Tasks) != 0 {
		t.Fatalf("task_delete: %+v", resp)
	}
	if d.taskStore.Get(id) != nil {
		t.Fatal("task not deleted")
	}

	// operating on a missing id is a clean error, not a panic
	if r := d.taskDispatch(ipc.Request{Type: "task_edit", ID: "nope"}); r.OK {
		t.Fatalf("editing a missing task should fail: %+v", r)
	}
}

// TestReconcileTaskStates covers the reconcile pass (formerly the TUI's
// syncTasks): harvest an agent session id from a live session; pause tasks
// whose session vanished / exited / reported ended (keeping the resume handle);
// leave freshly started tasks alone (grace window) and resting tasks untouched.
func TestReconcileTaskStates(t *testing.T) {
	now := time.Now()
	old := now.Add(-time.Minute)
	mk := func(title, cbID, agentID string, updated time.Time) ipc.Task {
		return ipc.Task{
			Title: title, Status: task.StatusInProgress,
			Runs: []ipc.TaskRun{{ID: cbID, CBSessionID: cbID, AgentSessionID: agentID, Status: task.StatusInProgress, UpdatedAt: updated}}, UpdatedAt: updated,
		}
	}
	tasks := []ipc.Task{
		mk("harvest", "s1", "", old),
		mk("vanished", "s2", "claude-keep", old),
		mk("gone", "s3", "", old),
		mk("fresh", "s5", "", now), // inside the grace window
		{Title: "resting", Status: task.StatusPaused, Runs: []ipc.TaskRun{{ID: "rest", AgentSessionID: "claude-old", Status: task.StatusPaused}}, UpdatedAt: old},
	}
	live := map[string]sessState{
		"s1": {gone: false, harness: "claude-1"},
		"s3": {gone: true},
	}

	if !reconcileTaskStates(tasks, live, now) {
		t.Fatal("expected changes")
	}

	if tasks[0].Status != task.StatusInProgress || tasks[0].Runs[0].AgentSessionID != "claude-1" {
		t.Errorf("harvest: %+v", tasks[0])
	}
	for _, i := range []int{1, 2} { // vanished (not in map), gone (exited)
		if tasks[i].Status != task.StatusPaused || tasks[i].Runs[0].CBSessionID != "" {
			t.Errorf("%s not paused/cleared: %+v", tasks[i].Title, tasks[i])
		}
	}
	if tasks[1].Runs[0].AgentSessionID != "claude-keep" {
		t.Errorf("pause dropped the resume handle: %+v", tasks[1])
	}
	if tasks[3].Status != task.StatusInProgress {
		t.Errorf("fresh task paused despite grace window: %+v", tasks[3])
	}
	if tasks[4].Status != task.StatusPaused || tasks[4].Runs[0].AgentSessionID != "claude-old" {
		t.Errorf("resting task touched: %+v", tasks[4])
	}
}

func TestReconcileTaskStatesKeepsTaskActiveWhileAnotherRunLives(t *testing.T) {
	now := time.Now()
	tasks := []ipc.Task{{
		Title: "parallel", Status: task.StatusInProgress,
		Runs: []ipc.TaskRun{
			{ID: "ended", CBSessionID: "ended", Status: task.StatusInProgress, UpdatedAt: now.Add(-time.Minute)},
			{ID: "live", CBSessionID: "live", Status: task.StatusInProgress, UpdatedAt: now.Add(-time.Minute)},
		},
	}}
	if !reconcileTaskStates(tasks, map[string]sessState{"live": {}}, now) {
		t.Fatal("expected ended run to be reconciled")
	}
	if tasks[0].Status != task.StatusInProgress || tasks[0].Runs[0].Status != task.StatusPaused || tasks[0].Runs[1].Status != task.StatusInProgress {
		t.Fatalf("parallel runs reconciled incorrectly: %+v", tasks[0])
	}
}
