// Package hook implements `cb hook <event>`: a pure no-op observer that
// Claude Code invokes for each hook event. It reads the hook JSON from stdin,
// reads CB_SESSION from its environment (inherited from the claude process
// the daemon spawned), forwards both to the daemon, and always exits 0 so it
// can never block or interfere with Claude Code.
package hook

import (
	"io"
	"os"
	"time"

	"codebridge/internal/ipc"
)

// Run forwards a hook event to the daemon. argv is the args after "hook"
// (argv[0] is the event name). It never returns a fatal error to the caller:
// any failure is swallowed so the hook stays a no-op from Claude Code's view.
func Run(argv []string) error {
	event := ""
	if len(argv) > 0 {
		event = argv[0]
	}
	payload, _ := io.ReadAll(io.LimitReader(os.Stdin, 1<<20))
	sess := os.Getenv("CB_SESSION")
	if sess == "" {
		return nil // not a cb-managed session; nothing to report
	}

	// Best-effort, short-lived send. Don't hang Claude Code if the daemon is
	// down or slow.
	done := make(chan struct{})
	go func() {
		_, _ = ipc.Send(ipc.Request{
			Type:    "hook",
			Event:   event,
			Session: sess,
			Payload: payload,
		})
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
	}
	return nil
}
