package daemon

import (
	"strings"
	"time"

	"github.com/google/uuid"

	"codebridge/internal/ipc"
	"codebridge/internal/task"
)

// taskSyncGrace shields a freshly (re)started task from being flipped back to
// paused by reconcileTasks before the spawned session shows up in the registry
// (the spawn and the reconcile can race). Mirrors the window the TUI used
// client-side before the daemon took ownership.
const taskSyncGrace = 2 * time.Second

// taskDispatch handles the backlog mutation requests. Every branch replies with
// the fresh task list so the caller updates its cache without a follow-up read;
// mutations also notifyChange so watch streams (TUI + web) repaint promptly.
func (d *Daemon) taskDispatch(req ipc.Request) ipc.Response {
	switch req.Type {
	case "task_list":
		d.reconcileTasks()
		return ipc.Response{OK: true, Tasks: d.taskSnapshot()}

	case "task_add":
		title := strings.TrimSpace(req.Title)
		if title == "" {
			return ipc.Response{Error: "task title required"}
		}
		d.taskMu.Lock()
		t := d.taskStore.Add(req.Scope, title, strings.TrimSpace(req.Desc))
		id := t.ID
		err := d.taskStore.Save()
		tasks := cloneTasks(d.taskStore.Tasks)
		d.taskMu.Unlock()
		if err != nil {
			return ipc.Response{Error: err.Error()}
		}
		d.notifyChange()
		return ipc.Response{OK: true, ID: id, Tasks: tasks}

	case "task_edit":
		return d.taskMutate(req.ID, func(t *task.Task) {
			if title := strings.TrimSpace(req.Title); title != "" {
				t.Title = title
			}
			t.Desc = strings.TrimRight(req.Desc, "\n ")
		})

	case "task_status":
		return d.taskMutate(req.ID, func(t *task.Task) {
			t.Status = task.Status(req.Status)
		})

	case "task_delete":
		d.taskMu.Lock()
		d.taskStore.Delete(req.ID)
		err := d.taskStore.Save()
		tasks := cloneTasks(d.taskStore.Tasks)
		d.taskMu.Unlock()
		if err != nil {
			return ipc.Response{Error: err.Error()}
		}
		d.notifyChange()
		return ipc.Response{OK: true, Tasks: tasks}

	case "task_start":
		return d.taskStart(req)
	case "task_resume":
		return d.taskResume(req)
	}
	return ipc.Response{Error: "unknown task request: " + req.Type}
}

// taskMutate applies fn to the task with the given id under the lock, stamps
// UpdatedAt, persists, and returns the fresh list. Used by edit/status.
func (d *Daemon) taskMutate(id string, fn func(*task.Task)) ipc.Response {
	d.taskMu.Lock()
	t := d.taskStore.Get(id)
	if t == nil {
		d.taskMu.Unlock()
		return ipc.Response{Error: "no such task: " + id}
	}
	fn(t)
	t.UpdatedAt = time.Now()
	err := d.taskStore.Save()
	tasks := cloneTasks(d.taskStore.Tasks)
	d.taskMu.Unlock()
	if err != nil {
		return ipc.Response{Error: err.Error()}
	}
	d.notifyChange()
	return ipc.Response{OK: true, Tasks: tasks}
}

// taskStart always creates a fresh run. This is deliberately distinct from
// taskResume: a task may have multiple live sessions, so starting work must
// never silently resume an arbitrary earlier run.
func (d *Daemon) taskStart(req ipc.Request) ipc.Response {
	bin := req.Agent
	if bin == "" {
		return ipc.Response{Error: "task_start requires an agent"}
	}

	// Read the task's state under the lock, then release it before spawning so
	// we never hold taskMu across the session machinery (which takes d.mu).
	d.taskMu.Lock()
	t := d.taskStore.Get(req.ID)
	if t == nil {
		d.taskMu.Unlock()
		return ipc.Response{Error: "no such task: " + req.ID}
	}
	argv := []string{bin}
	prefill := t.Title
	if t.Desc != "" {
		prefill += "\n\n" + t.Desc
	}
	d.taskMu.Unlock()

	resp := d.spawn(ipc.Request{
		Type: "spawn", Argv: argv, Cwd: req.Cwd, Prefill: prefill,
		Rows: req.Rows, Cols: req.Cols,
	})
	if !resp.OK {
		return resp // spawn already carries a useful error
	}

	// Link the task to the live session. It may have been deleted between the
	// two locks; if so the session still runs, we just don't track it.
	d.taskMu.Lock()
	if t := d.taskStore.Get(req.ID); t != nil {
		now := time.Now()
		t.Runs = append(t.Runs, ipc.TaskRun{ID: uuid.NewString(), Agent: bin, Cwd: req.Cwd, CBSessionID: resp.ID, Status: task.StatusInProgress, CreatedAt: now, UpdatedAt: now})
		t.Status = derivedTaskStatus(t)
		t.UpdatedAt = time.Now()
		_ = d.taskStore.Save()
	}
	tasks := cloneTasks(d.taskStore.Tasks)
	d.taskMu.Unlock()
	d.notifyChange()
	return ipc.Response{OK: true, ID: resp.ID, Tasks: tasks}
}

// taskResume starts a new daemon session for one paused run, preserving its
// agent-specific resume identity. The resumed session replaces the old daemon
// attachment on that run, rather than creating a second run.
func (d *Daemon) taskResume(req ipc.Request) ipc.Response {
	d.taskMu.Lock()
	t := d.taskStore.Get(req.ID)
	if t == nil {
		d.taskMu.Unlock()
		return ipc.Response{Error: "no such task: " + req.ID}
	}
	var run *ipc.TaskRun
	for i := range t.Runs {
		if t.Runs[i].ID == req.RunID {
			run = &t.Runs[i]
			break
		}
	}
	if run == nil || run.Status != task.StatusPaused {
		d.taskMu.Unlock()
		return ipc.Response{Error: "no such paused task run: " + req.RunID}
	}
	bin, agentID := run.Agent, run.AgentSessionID
	cwd := req.Cwd
	if cwd == "" {
		cwd = run.Cwd
	}
	d.taskMu.Unlock()
	if bin == "" {
		return ipc.Response{Error: "task run has no agent"}
	}
	argv := []string{bin}
	switch {
	case bin == "claude" && agentID != "":
		argv = []string{"claude", "--resume", agentID}
	case bin == "codex" && agentID != "":
		argv = []string{"codex", "resume", agentID}
	case bin == "codex":
		argv = []string{"codex", "resume", "--last"}
	case bin == "opencode":
		argv = []string{"opencode", "--continue"}
	}
	resp := d.spawn(ipc.Request{Type: "spawn", Argv: argv, Cwd: cwd, Rows: req.Rows, Cols: req.Cols})
	if !resp.OK {
		return resp
	}
	d.taskMu.Lock()
	if t := d.taskStore.Get(req.ID); t != nil {
		for i := range t.Runs {
			if t.Runs[i].ID == req.RunID {
				t.Runs[i].CBSessionID, t.Runs[i].Status, t.Runs[i].UpdatedAt = resp.ID, task.StatusInProgress, time.Now()
			}
		}
		t.Status, t.UpdatedAt = derivedTaskStatus(t), time.Now()
		_ = d.taskStore.Save()
	}
	tasks := cloneTasks(d.taskStore.Tasks)
	d.taskMu.Unlock()
	d.notifyChange()
	return ipc.Response{OK: true, ID: resp.ID, Tasks: tasks}
}

// taskSnapshot returns a copy of the backlog for a client reply or push.
func (d *Daemon) taskSnapshot() []ipc.Task {
	d.taskMu.Lock()
	defer d.taskMu.Unlock()
	return cloneTasks(d.taskStore.Tasks)
}

func cloneTasks(in []ipc.Task) []ipc.Task {
	if len(in) == 0 {
		return nil
	}
	out := make([]ipc.Task, len(in))
	copy(out, in)
	for i := range out {
		out[i].Runs = append([]ipc.TaskRun(nil), in[i].Runs...)
	}
	return out
}

// reconcileTasks keeps in_progress tasks in step with the live session list: a
// task whose session vanished, exited, or reported ended goes to paused (its
// agent session id kept as the resume handle); a live session's agent session
// id (claude's via hooks, codex's via the rollout harvest) is picked up
// continuously, since it can't be read back once the session is gone. This ran
// on the TUI's poll before the daemon owned the store; now it runs wherever a
// task snapshot is produced (list/watch/task_list). Saves only when something
// changed. Returns whether anything changed.
func (d *Daemon) reconcileTasks() bool {
	// Snapshot the sessions we care about first, then take taskMu — never hold
	// both locks at once.
	d.mu.RLock()
	live := make(map[string]sessState, len(d.sessions))
	for id, s := range d.sessions {
		if s == nil {
			continue
		}
		live[id] = sessState{
			gone:    s.Exited() || s.Status() == "ended",
			harness: s.HarnessSessionID(),
		}
	}
	d.mu.RUnlock()

	d.taskMu.Lock()
	defer d.taskMu.Unlock()
	if reconcileTaskStates(d.taskStore.Tasks, live, time.Now()) {
		_ = d.taskStore.Save()
		return true
	}
	return false
}

// sessState is the slice of a session reconcileTaskStates needs: whether it's
// effectively gone and its harvested agent session id.
type sessState struct {
	gone    bool
	harness string
}

// reconcileTaskStates is the pure core of reconcileTasks (no locks, no I/O): it
// mutates the in_progress tasks in place against a session snapshot and reports
// whether anything changed. A task whose session is missing/gone pauses (past
// the grace window), keeping its agent session id as the resume handle; a live
// session's freshly harvested id is copied onto the task.
func reconcileTaskStates(tasks []ipc.Task, live map[string]sessState, now time.Time) bool {
	dirty := false
	for i := range tasks {
		t := &tasks[i]
		for j := range t.Runs {
			r := &t.Runs[j]
			if r.Status != task.StatusInProgress {
				continue
			}
			s, ok := live[r.CBSessionID]
			if r.CBSessionID == "" || !ok || s.gone {
				if now.Sub(r.UpdatedAt) < taskSyncGrace {
					continue
				}
				r.Status, r.CBSessionID, r.UpdatedAt = task.StatusPaused, "", now
				dirty = true
				continue
			}
			if s.harness != "" && s.harness != r.AgentSessionID {
				r.AgentSessionID, r.UpdatedAt = s.harness, now
				dirty = true
			}
		}
		status := derivedTaskStatus(t)
		if t.Status != task.StatusCompleted && t.Status != status {
			t.Status, t.UpdatedAt, dirty = status, now, true
		}
	}
	return dirty
}

func derivedTaskStatus(t *ipc.Task) task.Status {
	if t.Status == task.StatusCompleted {
		return task.StatusCompleted
	}
	for _, r := range t.Runs {
		if r.Status == task.StatusInProgress {
			return task.StatusInProgress
		}
	}
	if len(t.Runs) > 0 {
		return task.StatusPaused
	}
	return task.StatusPending
}
