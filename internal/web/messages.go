package web

import "codebridge/internal/ipc"

// wsUp is a browser→bridge message. Stream types (input/paste/resize/scroll/
// interrupt/detach) are passthroughs to ipc.StreamUp on the client's current
// attach; auth/attach/spawn/kill are handled by the bridge itself.
type wsUp struct {
	Type  string `json:"type"` // auth|attach|input|paste|resize|scroll|interrupt|detach|spawn|kill|worktrees
	Token string `json:"token,omitempty"`
	// ID is the target session for attach/kill.
	ID string `json:"id,omitempty"`
	// Data carries base64-encoded bytes for input/paste, same encoding as
	// ipc.StreamUp.Data (forwarded verbatim).
	Data   string   `json:"data,omitempty"`
	Rows   int      `json:"rows,omitempty"`
	Cols   int      `json:"cols,omitempty"`
	Offset int      `json:"offset,omitempty"`
	Argv   []string `json:"argv,omitempty"` // spawn
	Cwd    string   `json:"cwd,omitempty"`  // spawn
}

// webSession is a SessionInfo enriched with the sidebar grouping scope,
// computed bridge-side (the browser has no filesystem access).
type webSession struct {
	ipc.SessionInfo
	Scope     string `json:"scope"`      // group key (shared .git or bare cwd)
	ScopeName string `json:"scope_name"` // short header label
}

// wsDown is a bridge→browser message. Frame fields mirror ipc.StreamDown,
// plus ID so the client can discard frames from a session it has switched
// away from.
type wsDown struct {
	Type string `json:"type"` // hello|sessions|frame|gone|spawned|worktrees|error

	// hello
	Protocol int  `json:"protocol,omitempty"` // ipc.ProtocolVersion of this bridge
	Daemon   bool `json:"daemon,omitempty"`   // daemon reachable + version match

	// sessions
	Sessions []webSession `json:"sessions,omitempty"`

	// worktrees (reply to the same-named up message; Cwd echoes the request
	// so the client can pair reply with picker)
	Cwd       string          `json:"cwd,omitempty"`
	Worktrees []worktreeEntry `json:"worktrees,omitempty"`
	Agents    []string        `json:"agents,omitempty"`

	// frame / gone / spawned
	ID        string `json:"id,omitempty"`
	Screen    string `json:"screen,omitempty"`
	CursorX   int    `json:"cursor_x,omitempty"`
	CursorY   int    `json:"cursor_y,omitempty"`
	Alt       bool   `json:"alt,omitempty"`
	Rows      int    `json:"rows,omitempty"`
	Cols      int    `json:"cols,omitempty"`
	Offset    int    `json:"offset,omitempty"`
	MaxOffset int    `json:"max_offset,omitempty"`

	Error string `json:"error,omitempty"`
}
