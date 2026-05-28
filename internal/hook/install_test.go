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
