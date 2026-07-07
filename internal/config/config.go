// Package config persists the user's tunable settings for the cb TUI — the
// command prefix and the prefix+X action bindings — to a JSON file on disk so
// rebindings made in the in-app config menu survive across restarts. The file
// lives at $XDG_CONFIG_HOME/cb/config.json (or ~/.config/cb/config.json) and
// is layered over the built-in defaults at load time: unset fields keep their
// defaults, so a partial file or a future schema addition doesn't wipe the
// rest of the user's customizations.
//
// Precedence for the prefix: CB_PREFIX env var > config file > built-in
// default. The env override exists because it predates the config file and
// some users have it wired into their shell init; the config menu's prefix
// row shows it as read-only when overridden.
package config

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
)

// DefaultPrefix is the tmux-style chord that gates every prefix+X command.
const DefaultPrefix = "ctrl+a"

// Action is one of the named commands surfaced in the config menu. Key is the
// stable identifier used as the JSON map key (don't rename without migration);
// Label is the human-readable description; Default is the factory binding.
type Action struct {
	Key     string
	Label   string
	Default string
}

// Actions is the canonical list of rebindable commands, in display order
// (which doubles as the menu's row order). Adding a new prefix command means
// appending here and dispatching it in the dashboard's runAction.
//
// Sidebar focus isn't here: it stays bound to h / the left arrow as a system
// shortcut, and `?` is reserved for toggling the floating hints panel —
// system layers the user can't rebind from inside this menu.
var Actions = []Action{
	{Key: "new_claude", Label: "new claude session", Default: "n"},
	{Key: "new_codex", Label: "new codex session", Default: "c"},
	{Key: "new_worktree", Label: "new session in worktree", Default: "w"},
	{Key: "kill", Label: "kill session", Default: "x"},
	{Key: "rename", Label: "rename session", Default: "r"},
	{Key: "jump_pending", Label: "jump to pending approval", Default: "g"},
	{Key: "yank", Label: "yank selection", Default: "y"},
	{Key: "scope_toggle", Label: "toggle accordion / this-workspace", Default: "a"},
	{Key: "focus_screen", Label: "focus screen pane", Default: "l"},
	{Key: "scroll", Label: "enter scroll mode", Default: "["},
	{Key: "newline", Label: "insert newline in session", Default: "enter"},
	{Key: "config", Label: "open config menu", Default: "o"},
	{Key: "quit", Label: "quit cb", Default: "q"},
}

// ReservedKeys are key strings the config menu refuses to bind to: they're
// claimed by system-level handlers (menu toggle, focus shortcuts, copy/quit,
// modal escape) that aren't routed through the binding table. Rebinding to
// one would silently never fire because the system handler intercepts first;
// the menu surfaces this as a "reserved by" error so the user picks another.
var ReservedKeys = map[string]string{
	"esc":    "modal escape",
	"h":      "focus sidebar",
	"?":      "toggle hints panel",
	"left":   "focus sidebar",
	"right":  "focus screen pane",
	"up":     "navigation",
	"down":   "navigation",
	"k":      "navigation",
	"j":      "navigation",
	"ctrl+c": "quit / SIGINT to focused session",
}

// Config is the on-disk shape. Bindings maps an Action.Key to the key string
// (the Bubble Tea KeyPressMsg.String() form, e.g. "n", "R", "[", "enter").
type Config struct {
	Prefix   string            `json:"prefix"`
	Bindings map[string]string `json:"bindings"`
}

// Defaults returns a fresh Config seeded with the built-in prefix and
// bindings. Each call returns a new map so mutations on the result don't leak
// into the next call.
func Defaults() *Config {
	b := make(map[string]string, len(Actions))
	for _, a := range Actions {
		b[a.Key] = a.Default
	}
	return &Config{Prefix: DefaultPrefix, Bindings: b}
}

// Path is the on-disk location of the config file. Honors XDG_CONFIG_HOME and
// otherwise falls back to ~/.config/cb/config.json. Returns "" when the home
// directory is unresolvable (Load/Save then no-op safely).
func Path() string {
	if v := strings.TrimSpace(os.Getenv("XDG_CONFIG_HOME")); v != "" {
		return filepath.Join(v, "cb", "config.json")
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".config", "cb", "config.json")
}

// Load returns the user's settings layered over defaults: any field absent
// from the file keeps its default value, so a partially-written file is safe
// and future fields don't wipe out older ones. Read/parse errors fall back to
// defaults silently so a corrupted file never blocks the dashboard.
func Load() *Config {
	cfg := Defaults()
	p := Path()
	if p == "" {
		return cfg
	}
	data, err := os.ReadFile(p)
	if err != nil {
		return cfg
	}
	var stored Config
	if json.Unmarshal(data, &stored) != nil {
		return cfg
	}
	if strings.TrimSpace(stored.Prefix) != "" {
		cfg.Prefix = stored.Prefix
	}
	// Only honor stored bindings for actions we still recognize; unknown keys
	// (renamed/removed in a newer version) are dropped silently.
	for k, v := range stored.Bindings {
		if _, ok := cfg.Bindings[k]; ok && strings.TrimSpace(v) != "" {
			cfg.Bindings[k] = v
		}
	}
	return cfg
}

// Save writes cfg to disk, creating the parent directory if missing.
func Save(cfg *Config) error {
	p := Path()
	if p == "" {
		return errors.New("config: no home directory")
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(p, data, 0o644)
}

// EnvOverridePrefix returns the trimmed CB_PREFIX env value when set; a
// non-empty result means the config-file prefix is overridden and the menu
// should show the prefix row as read-only.
func EnvOverridePrefix() string {
	return strings.TrimSpace(os.Getenv("CB_PREFIX"))
}

// Clone returns a deep copy so the menu can edit a working copy without
// mutating the live model until the user confirms each change.
func (c *Config) Clone() *Config {
	out := &Config{Prefix: c.Prefix, Bindings: make(map[string]string, len(c.Bindings))}
	for k, v := range c.Bindings {
		out.Bindings[k] = v
	}
	return out
}
