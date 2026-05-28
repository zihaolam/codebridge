// Package notify sends best-effort desktop notifications. It is a no-op when
// disabled (CB_NO_NOTIFY set) or on platforms without a known mechanism.
package notify

import (
	"os"
	"os/exec"
	"runtime"
	"strings"
)

// Enabled reports whether notifications should be sent.
func Enabled() bool {
	return strings.TrimSpace(os.Getenv("CB_NO_NOTIFY")) == ""
}

// Send fires a desktop notification asynchronously. Failures are ignored.
func Send(title, body string) {
	if !Enabled() {
		return
	}
	switch runtime.GOOS {
	case "darwin":
		// osascript is invoked directly (no shell), and the strings are escaped
		// as AppleScript literals, so there is no injection surface.
		script := "display notification " + applescriptString(body) +
			" with title " + applescriptString(title)
		run("osascript", "-e", script)
	case "linux":
		run("notify-send", title, body)
	}
}

func run(name string, args ...string) {
	go func() { _ = exec.Command(name, args...).Run() }()
}

func applescriptString(s string) string {
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	return `"` + s + `"`
}
