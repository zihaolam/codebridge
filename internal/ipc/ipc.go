// Package ipc defines the line-delimited JSON protocol spoken over the daemon's
// unix socket. Each request is one JSON object on a line; the daemon replies
// with one JSON object on a line. Streaming (screen diffs for the TUI) is added
// in a later phase on top of the same framing.
package ipc

import (
	"bufio"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
)

// ProtocolVersion is bumped whenever the daemon/client wire protocol changes.
// The client checks it on connect so a stale daemon (e.g. left running across a
// rebuild) fails loudly instead of silently dropping attach/input messages.
const ProtocolVersion = 6

// Dir is the per-user state directory for cb.
func Dir() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ".cb"
	}
	return filepath.Join(home, ".cb")
}

// SocketPath is the daemon's unix socket location.
func SocketPath() string {
	if p := os.Getenv("CB_SOCK"); p != "" {
		return p
	}
	return filepath.Join(Dir(), "daemon.sock")
}

// Request is a message from a client (or hook) to the daemon.
type Request struct {
	Type string `json:"type"` // "spawn" | "list" | "kill" | "hook" | "extract"

	// spawn
	Argv []string `json:"argv,omitempty"`
	Cwd  string   `json:"cwd,omitempty"`
	Rows int      `json:"rows,omitempty"`
	Cols int      `json:"cols,omitempty"`

	// kill / rename / extract
	ID string `json:"id,omitempty"`

	// rename
	Name string `json:"name,omitempty"`

	// hook
	Event   string          `json:"event,omitempty"`   // hook_event_name, e.g. "Stop"
	Session string          `json:"session,omitempty"` // value of CB_SESSION
	Payload json.RawMessage `json:"payload,omitempty"` // raw hook stdin JSON

	// extract: virtual-buffer text range (scrollback + visible). Lines/cols are
	// inclusive at the start, exclusive at the end of the last line's text.
	LineStart int `json:"line_start,omitempty"`
	LineEnd   int `json:"line_end,omitempty"`
	ColStart  int `json:"col_start,omitempty"`
	ColEnd    int `json:"col_end,omitempty"`
}

// Response is the daemon's reply.
type Response struct {
	OK       bool          `json:"ok"`
	Error    string        `json:"error,omitempty"`
	ID       string        `json:"id,omitempty"`       // spawn result
	Sessions []SessionInfo `json:"sessions,omitempty"` // list result
	Version  int           `json:"version,omitempty"`  // ping result: daemon protocol version
	PID      int           `json:"pid,omitempty"`      // ping result: daemon process id
	Text     string        `json:"text,omitempty"`     // extract result: plain text
}

// SessionInfo is a snapshot of a session's metadata for the client.
type SessionInfo struct {
	ID              string   `json:"id"`
	Name            string   `json:"name,omitempty"`
	Argv            []string `json:"argv"`
	Cwd             string   `json:"cwd"`
	Status          string   `json:"status"`
	LastMessage     string   `json:"last_message,omitempty"`
	ClaudeSessionID string   `json:"claude_session_id,omitempty"`
	Exited          bool     `json:"exited"`
	StatusSince     int64    `json:"status_since,omitempty"` // unix nanos the status was entered
}

// StreamUp is a client→daemon message sent on an attached connection.
type StreamUp struct {
	Type string `json:"type"`           // "input" | "paste" | "resize" | "detach" | "scroll"
	Data string `json:"data,omitempty"` // base64-encoded input/paste bytes
	Rows int    `json:"rows,omitempty"`
	Cols int    `json:"cols,omitempty"`
	// scroll: how many lines up from the live bottom to show (0 == follow live).
	Offset int `json:"offset,omitempty"`
}

// StreamDown is a daemon→client message sent on an attached connection.
type StreamDown struct {
	Type    string `json:"type"` // "frame" | "gone"
	Screen  string `json:"screen,omitempty"`
	CursorX int    `json:"cursor_x,omitempty"`
	CursorY int    `json:"cursor_y,omitempty"`
	Alt     bool   `json:"alt,omitempty"`
	// Offset is the scroll position this frame was rendered at (lines up from
	// the live bottom); MaxOffset is how far up the scrollback allows.
	Offset    int `json:"offset,omitempty"`
	MaxOffset int `json:"max_offset,omitempty"`
}

// HookPayload captures the common Claude Code hook stdin fields we rely on.
// Unknown fields are ignored; field availability varies by event and version.
type HookPayload struct {
	SessionID     string `json:"session_id"`
	Cwd           string `json:"cwd"`
	HookEventName string `json:"hook_event_name"`
	Message       string `json:"message"`
	ToolName      string `json:"tool_name"`
	Model         string `json:"model"`
	Source        string `json:"source"`
}

// Send dials the daemon, sends one request, and returns the single-line reply.
func Send(req Request) (Response, error) {
	conn, err := net.Dial("unix", SocketPath())
	if err != nil {
		return Response{}, err
	}
	defer conn.Close()
	if err := WriteJSON(conn, req); err != nil {
		return Response{}, err
	}
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	if !sc.Scan() {
		if err := sc.Err(); err != nil {
			return Response{}, err
		}
		return Response{}, nil
	}
	var resp Response
	if err := json.Unmarshal(sc.Bytes(), &resp); err != nil {
		return Response{}, err
	}
	return resp, nil
}

// WriteJSON writes v as a single JSON line.
func WriteJSON(w interface{ Write([]byte) (int, error) }, v any) error {
	b, err := json.Marshal(v)
	if err != nil {
		return err
	}
	b = append(b, '\n')
	_, err = w.Write(b)
	return err
}
