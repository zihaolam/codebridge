package daemon

import (
	"strings"
	"time"

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
			if t.Status == task.StatusCompleted {
				t.CBSessionID = "" // the live link (if any) is done tracking
			}
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

// taskStart spawns an agent session for a task and links the task to it. Fresh
// (pending) tasks — and paused tasks that never reported an agent session id —
// start the bare agent with the task text prefilled (delivered unsubmitted by
// the daemon). Paused tasks resume instead: claude/codex by exact session id,
// codex without one falls back to `resume --last`, opencode to `--continue`.
// This logic used to live in the TUI's startTaskCmd; it moved here so both the
// TUI and the web bridge get identical behavior.
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
	if t.Status == task.StatusPaused {
		switch {
		case bin == "claude" && t.AgentSessionID != "":
			argv = []string{"claude", "--resume", t.AgentSessionID}
			prefill = ""
		case bin == "codex" && t.AgentSessionID != "":
			argv = []string{"codex", "resume", t.AgentSessionID}
			prefill = ""
		case bin == "codex":
			argv = []string{"codex", "resume", "--last"}
			prefill = ""
		case bin == "opencode":
			argv = []string{"opencode", "--continue"}
			prefill = ""
		}
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
		t.Status = task.StatusInProgress
		t.CBSessionID = resp.ID
		t.Agent = bin
		t.Cwd = req.Cwd
		t.UpdatedAt = time.Now()
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
		if t.Status != task.StatusInProgress {
			continue
		}
		s, ok := live[t.CBSessionID]
		if t.CBSessionID == "" || !ok || s.gone {
			// Grace window: a spawn that just linked this task may not be in the
			// session snapshot yet — absence there isn't "ended".
			if now.Sub(t.UpdatedAt) < taskSyncGrace {
				continue
			}
			t.Status = task.StatusPaused
			t.CBSessionID = ""
			t.UpdatedAt = now
			dirty = true
			continue
		}
		if s.harness != "" && s.harness != t.AgentSessionID {
			t.AgentSessionID = s.harness
			t.UpdatedAt = now
			dirty = true
		}
	}
	return dirty
}
