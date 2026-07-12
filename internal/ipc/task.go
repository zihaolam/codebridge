package ipc

import "time"

// Status is a task's lifecycle state. in_progress means a live daemon session
// is linked; paused means its session ended but the task kept a resume handle.
// The wire type lives here (next to SessionInfo) so both the daemon — the
// single owner of the backlog — and every client speak the same shape; the
// internal/task package aliases these for its on-disk Store.
type Status string

const (
	StatusPending    Status = "pending"
	StatusInProgress Status = "in_progress"
	StatusPaused     Status = "paused"
	StatusCompleted  Status = "completed"
)

// TaskRun is one agent session launched for a task. A task may have several
// live runs at once, and each paused run retains its own agent resume handle.
type TaskRun struct {
	ID             string    `json:"id"`
	Agent          string    `json:"agent,omitempty"`
	Cwd            string    `json:"cwd,omitempty"`
	CBSessionID    string    `json:"cb_session_id,omitempty"`
	AgentSessionID string    `json:"agent_session_id,omitempty"`
	Status         Status    `json:"status"`
	CreatedAt      time.Time `json:"created_at"`
	UpdatedAt      time.Time `json:"updated_at"`
}

// Task is one backlog entry. Runs is authoritative for session tracking. The
// legacy single-run fields remain for decoding older tasks.json files and are
// folded into Runs by task.Load.
type Task struct {
	ID             string    `json:"id"`
	Scope          string    `json:"scope"`
	Title          string    `json:"title"`
	Desc           string    `json:"desc,omitempty"`
	Status         Status    `json:"status"`
	Runs           []TaskRun `json:"runs,omitempty"`
	Agent          string    `json:"agent,omitempty"`
	Cwd            string    `json:"cwd,omitempty"`
	CBSessionID    string    `json:"cb_session_id,omitempty"`
	AgentSessionID string    `json:"agent_session_id,omitempty"`
	CreatedAt      time.Time `json:"created_at"`
	UpdatedAt      time.Time `json:"updated_at"`
}
