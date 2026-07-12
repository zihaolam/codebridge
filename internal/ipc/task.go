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

// Task is one backlog entry. CBSessionID links to the live daemon session
// while in_progress and is cleared on pause; AgentSessionID is the agent's own
// session id (harvested from Claude Code hooks / the codex rollout journal)
// kept across pauses so a later start can resume it.
type Task struct {
	ID             string    `json:"id"`
	Scope          string    `json:"scope"`
	Title          string    `json:"title"`
	Desc           string    `json:"desc,omitempty"`
	Status         Status    `json:"status"`
	Agent          string    `json:"agent,omitempty"`
	Cwd            string    `json:"cwd,omitempty"`
	CBSessionID    string    `json:"cb_session_id,omitempty"`
	AgentSessionID string    `json:"agent_session_id,omitempty"`
	CreatedAt      time.Time `json:"created_at"`
	UpdatedAt      time.Time `json:"updated_at"`
}
