package hook

import (
	"os"
	"path/filepath"
	"testing"
)

func TestInstallRoundTrip(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "settings.json")
	if err := os.WriteFile(path, []byte(`{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo keep-me"}]}]}}`), 0o644); err != nil {
		t.Fatal(err)
	}

	if installedAt(path) {
		t.Fatal("should not be installed before Install")
	}
	if err := Install([]string{"--settings", path, "--bin", "/usr/local/bin/cb"}); err != nil {
		t.Fatal(err)
	}
	if !installedAt(path) {
		t.Fatal("should be installed after Install")
	}

	// Existing unrelated hook must be preserved.
	data, _ := os.ReadFile(path)
	if !contains(string(data), "echo keep-me") {
		t.Error("Install clobbered an existing hook")
	}
	if !contains(string(data), "/usr/local/bin/cb hook SessionStart") {
		t.Error("Install did not write the SessionStart hook command")
	}
}

func TestInstallHealsStalePath(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "settings.json")
	// Settings with a cb hook pointing at an old, now-moved binary path.
	stale := `{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"/old/build/cb hook SessionStart"}]}]}}`
	if err := os.WriteFile(path, []byte(stale), 0o644); err != nil {
		t.Fatal(err)
	}

	if err := Install([]string{"--settings", path, "--bin", "cb"}); err != nil {
		t.Fatal(err)
	}

	data, _ := os.ReadFile(path)
	if contains(string(data), "/old/build/cb hook SessionStart") {
		t.Error("stale cb hook path was not removed")
	}
	if !contains(string(data), "cb hook SessionStart") {
		t.Error("fresh cb hook was not written")
	}
}

func TestInstallCodex(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEX_HOME", dir)
	path := filepath.Join(dir, "hooks.json")

	if err := InstallCodex([]string{"--bin", "cb"}); err != nil {
		t.Fatal(err)
	}

	data, _ := os.ReadFile(path)
	s := string(data)
	// Codex-appropriate events are registered...
	for _, ev := range []string{"SessionStart", "PreToolUse", "PermissionRequest", "Stop"} {
		if !contains(s, "cb hook "+ev) {
			t.Errorf("codex hooks.json missing %q", ev)
		}
	}
	// ...and Claude-only events Codex doesn't emit are not.
	for _, ev := range []string{"Notification", "SessionEnd"} {
		if contains(s, "cb hook "+ev) {
			t.Errorf("codex hooks.json should not register %q", ev)
		}
	}

	// Re-running heals rather than stacking: still exactly one SessionStart entry.
	if err := InstallCodex([]string{"--bin", "cb"}); err != nil {
		t.Fatal(err)
	}
	data, _ = os.ReadFile(path)
	if n := countSub(string(data), "cb hook SessionStart"); n != 1 {
		t.Errorf("reinstall produced %d SessionStart entries, want 1", n)
	}
}

func countSub(s, sub string) int {
	n, i := 0, 0
	for {
		j := indexFrom(s, sub, i)
		if j < 0 {
			return n
		}
		n++
		i = j + len(sub)
	}
}

func indexFrom(s, sub string, from int) int {
	for i := from; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}

func contains(s, sub string) bool {
	return len(s) >= len(sub) && (func() bool {
		for i := 0; i+len(sub) <= len(s); i++ {
			if s[i:i+len(sub)] == sub {
				return true
			}
		}
		return false
	})()
}
