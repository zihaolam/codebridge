package daemon

import (
	"testing"

	"codebridge/internal/ipc"
	"codebridge/internal/session"
)

func TestStatusForEvent_NotificationSplit(t *testing.T) {
	cases := []struct {
		name    string
		event   string
		message string
		want    session.Status
	}{
		{"permission prompt -> approval", "Notification", "Claude needs your permission to use Bash", session.StatusNeedsApproval},
		{"idle nudge -> waiting", "Notification", "Claude is waiting for your input", session.StatusWaitingUser},
		{"explicit permission event", "PermissionRequest", "approve tool?", session.StatusNeedsApproval},
		{"turn complete", "Stop", "", session.StatusWaitingUser},
		{"fresh session is idle, not turn-complete", "SessionStart", "", session.StatusIdle},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got, _ := statusForEvent(c.event, ipc.HookPayload{Message: c.message})
			if got != c.want {
				t.Fatalf("statusForEvent(%q, %q) = %v, want %v", c.event, c.message, got, c.want)
			}
		})
	}
}
