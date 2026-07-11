// Package task is the workspace-scoped backlog behind the dashboard's
// prefix+t dialog: a flat list of tasks persisted as JSON under ~/.cb, each
// carrying the sidebar scope key it belongs to. A task can be started as an
// agent session (its text prefilled into the agent's input) and then tracks
// that session until it ends, at which point the task pauses and keeps a
// resume handle. Storage is deliberately simple — one file, last-writer-wins
// across concurrent cb clients; the dialog reloads from disk on open to
// narrow the window.
package task

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"

	"github.com/google/uuid"

	"codebridge/internal/ipc"
)

// Status is a task's lifecycle state. in_progress means a live cb session is
// linked; paused means its session ended but the task kept a resume handle.
type Status string

const (
	StatusPending    Status = "pending"
	StatusInProgress Status = "in_progress"
	StatusPaused     Status = "paused"
	StatusCompleted  Status = "completed"
)

// Task is one backlog entry. CBSessionID links to the live daemon session
// while in_progress and is cleared on pause; AgentSessionID is the agent's own
// session id (harvested from Claude Code hooks) kept across pauses so a later
// start can `claude --resume` it.
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

// Store is the on-disk shape: a flat task list; scoping is per-task. path
// remembers where the store was loaded from so Save writes back to the same
// file (which also lets tests point a store at a temp dir).
type Store struct {
	path  string
	Tasks []Task `json:"tasks"`
}

// Path is the backlog's on-disk location, next to the daemon socket.
func Path() string {
	return filepath.Join(ipc.Dir(), "tasks.json")
}

// Load reads the default store. Missing or corrupt files yield an empty store
// so a bad file never blocks the dashboard.
func Load() *Store {
	return LoadFrom(Path())
}

// LoadFrom reads a store from an explicit path (testable seam for Load).
func LoadFrom(path string) *Store {
	st := &Store{path: path}
	data, err := os.ReadFile(path)
	if err != nil {
		return st
	}
	if json.Unmarshal(data, st) != nil {
		return &Store{path: path}
	}
	return st
}

// Save writes the store back to the path it was loaded from (or the default
// path for a zero-value store).
func (s *Store) Save() error {
	p := s.path
	if p == "" {
		p = Path()
	}
	return s.SaveTo(p)
}

// SaveTo writes the store atomically: marshal to a sibling .tmp file, then
// rename over the target so a crash mid-write never leaves a torn file.
func (s *Store) SaveTo(path string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// Get returns a pointer into the store's slice for in-place mutation, or nil.
func (s *Store) Get(id string) *Task {
	for i := range s.Tasks {
		if s.Tasks[i].ID == id {
			return &s.Tasks[i]
		}
	}
	return nil
}

// Add appends a new pending task with a fresh id and timestamps, returning a
// pointer to the stored copy.
func (s *Store) Add(scope, title, desc string) *Task {
	now := time.Now()
	s.Tasks = append(s.Tasks, Task{
		ID:        uuid.NewString(),
		Scope:     scope,
		Title:     title,
		Desc:      desc,
		Status:    StatusPending,
		CreatedAt: now,
		UpdatedAt: now,
	})
	return &s.Tasks[len(s.Tasks)-1]
}

// Delete removes the task with the given id (no-op when absent).
func (s *Store) Delete(id string) {
	for i := range s.Tasks {
		if s.Tasks[i].ID == id {
			s.Tasks = append(s.Tasks[:i], s.Tasks[i+1:]...)
			return
		}
	}
}

// ForScope returns pointers to the tasks belonging to scope, in storage order.
func (s *Store) ForScope(scope string) []*Task {
	var out []*Task
	for i := range s.Tasks {
		if s.Tasks[i].Scope == scope {
			out = append(out, &s.Tasks[i])
		}
	}
	return out
}
