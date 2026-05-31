package config

import (
	"os"
	"path/filepath"
	"testing"
)

// TestLoadSaveRoundtrip writes a config and reads it back. Verifies that a
// partial file still inherits defaults for unset bindings (so a future schema
// addition doesn't wipe a user's existing customizations).
func TestLoadSaveRoundtrip(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", dir)
	t.Setenv("CB_PREFIX", "") // env override would suppress the file's prefix

	cfg := Defaults()
	cfg.Prefix = "ctrl+b"
	cfg.Bindings["new_claude"] = "m"
	if err := Save(cfg); err != nil {
		t.Fatalf("Save: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dir, "cb", "config.json")); err != nil {
		t.Fatalf("config file not written: %v", err)
	}

	got := Load()
	if got.Prefix != "ctrl+b" {
		t.Errorf("prefix = %q, want ctrl+b", got.Prefix)
	}
	if got.Bindings["new_claude"] != "m" {
		t.Errorf("new_claude = %q, want m", got.Bindings["new_claude"])
	}
	// Unset bindings should keep their defaults — not vanish.
	if got.Bindings["quit"] != "q" {
		t.Errorf("quit = %q, want q (default)", got.Bindings["quit"])
	}
}

// TestLoadCorruptFile falls back to defaults rather than panicking when the
// on-disk file is unparseable.
func TestLoadCorruptFile(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", dir)
	if err := os.MkdirAll(filepath.Join(dir, "cb"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "cb", "config.json"), []byte("not json"), 0o644); err != nil {
		t.Fatal(err)
	}
	got := Load()
	if got.Prefix != DefaultPrefix {
		t.Errorf("prefix = %q, want %q on corrupt file", got.Prefix, DefaultPrefix)
	}
}

// TestLoadUnknownAction drops bindings for action ids we no longer recognize
// — keeps the in-memory map clean for future code that iterates it.
func TestLoadUnknownAction(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("XDG_CONFIG_HOME", dir)
	if err := os.MkdirAll(filepath.Join(dir, "cb"), 0o755); err != nil {
		t.Fatal(err)
	}
	stored := `{"prefix":"ctrl+a","bindings":{"made_up_action":"z","new_claude":"m"}}`
	if err := os.WriteFile(filepath.Join(dir, "cb", "config.json"), []byte(stored), 0o644); err != nil {
		t.Fatal(err)
	}
	got := Load()
	if _, ok := got.Bindings["made_up_action"]; ok {
		t.Error("unknown action survived Load")
	}
	if got.Bindings["new_claude"] != "m" {
		t.Errorf("new_claude = %q, want m", got.Bindings["new_claude"])
	}
}
