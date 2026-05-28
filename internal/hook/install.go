package hook

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// events we register cb as an observer for. This is a superset; Claude Code
// simply won't fire events it doesn't support, so extra names are harmless.
var installEvents = []string{
	"SessionStart",
	"UserPromptSubmit",
	"PreToolUse",
	"PostToolUse",
	"Notification",
	"PermissionRequest",
	"Stop",
	"SessionEnd",
}

// Install merges cb hook entries into a Claude Code settings.json, preserving
// any existing content and hooks. Flags:
//
//	--settings <path>  target file (default ~/.claude/settings.json)
//	--bin <path>       cb binary path used in the hook command (default: this exe)
//	--print            print the merged result to stdout instead of writing
func Install(args []string) error {
	settingsPath := defaultSettingsPath()
	binPath := defaultBinCommand()
	print := false

	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--settings":
			i++
			if i >= len(args) {
				return fmt.Errorf("--settings needs a path")
			}
			settingsPath = args[i]
		case "--bin":
			i++
			if i >= len(args) {
				return fmt.Errorf("--bin needs a path")
			}
			binPath = args[i]
		case "--print":
			print = true
		default:
			return fmt.Errorf("unknown flag %q", args[i])
		}
	}

	root := map[string]any{}
	if data, err := os.ReadFile(settingsPath); err == nil {
		if err := json.Unmarshal(data, &root); err != nil {
			return fmt.Errorf("existing %s is not valid JSON: %w", settingsPath, err)
		}
	} else if !os.IsNotExist(err) {
		return err
	}

	hooks, _ := root["hooks"].(map[string]any)
	if hooks == nil {
		hooks = map[string]any{}
	}

	added := 0
	for _, ev := range installEvents {
		cmd := binPath + " hook " + ev
		// Drop any prior cb entry for this event (e.g. an absolute path baked
		// in before the binary was moved) so re-running heals stale commands
		// instead of stacking a second, broken one.
		arr := stripCcmgrHooks(asArray(hooks[ev]), ev)
		entry := map[string]any{
			"matcher": "",
			"hooks": []any{
				map[string]any{"type": "command", "command": cmd},
			},
		}
		hooks[ev] = append(arr, entry)
		added++
	}
	root["hooks"] = hooks

	out, err := json.MarshalIndent(root, "", "  ")
	if err != nil {
		return err
	}
	out = append(out, '\n')

	if print {
		fmt.Print(string(out))
		return nil
	}

	if err := os.MkdirAll(filepath.Dir(settingsPath), 0o755); err != nil {
		return err
	}
	if existing, err := os.ReadFile(settingsPath); err == nil {
		if err := os.WriteFile(settingsPath+".bak", existing, 0o644); err != nil {
			return fmt.Errorf("writing backup: %w", err)
		}
	}
	if err := os.WriteFile(settingsPath, out, 0o644); err != nil {
		return err
	}
	fmt.Printf("installed %d cb hook(s) into %s\n", added, settingsPath)
	return nil
}

// Installed reports whether cb's SessionStart hook is present in the default
// Claude Code settings, used to warn the user when status would otherwise be
// stuck at "starting".
func Installed() bool {
	return installedAt(defaultSettingsPath())
}

func installedAt(path string) bool {
	data, err := os.ReadFile(path)
	if err != nil {
		return false
	}
	var root map[string]any
	if json.Unmarshal(data, &root) != nil {
		return false
	}
	hooks, _ := root["hooks"].(map[string]any)
	arr, _ := hooks["SessionStart"].([]any)
	return hasCommandSuffix(arr, " hook SessionStart")
}

func hasCommandSuffix(arr []any, suffix string) bool {
	for _, e := range arr {
		m, ok := e.(map[string]any)
		if !ok {
			continue
		}
		inner, _ := m["hooks"].([]any)
		for _, h := range inner {
			hm, _ := h.(map[string]any)
			if c, _ := hm["command"].(string); strings.HasSuffix(c, suffix) {
				return true
			}
		}
	}
	return false
}

func defaultSettingsPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ".claude/settings.json"
	}
	return filepath.Join(home, ".claude", "settings.json")
}

// defaultBinCommand is the command string baked into the hook entries. If a
// `cb` is resolvable on PATH we use the bare name, so the hooks keep working
// when the binary is moved (e.g. into /usr/local/bin). Otherwise we fall back to
// this executable's absolute path.
func defaultBinCommand() string {
	if p, err := exec.LookPath("cb"); err == nil && p != "" {
		return "cb"
	}
	if exe, err := os.Executable(); err == nil {
		return exe
	}
	return "cb"
}

func asArray(v any) []any {
	arr, _ := v.([]any)
	return arr
}

// stripCcmgrHooks removes inner hook commands that look like a cb hook for
// the given event (binary basename "cb", command ending in " hook <ev>"),
// dropping any entry left with no inner hooks.
func stripCcmgrHooks(arr []any, ev string) []any {
	suffix := " hook " + ev
	out := arr[:0]
	for _, e := range arr {
		m, ok := e.(map[string]any)
		if !ok {
			out = append(out, e)
			continue
		}
		inner, _ := m["hooks"].([]any)
		kept := inner[:0]
		for _, h := range inner {
			hm, _ := h.(map[string]any)
			c, _ := hm["command"].(string)
			if strings.HasSuffix(c, suffix) && filepath.Base(strings.Fields(c)[0]) == "cb" {
				continue // a cb hook for this event: drop it
			}
			kept = append(kept, h)
		}
		if len(kept) == 0 {
			continue // entry only held cb hooks: drop the whole entry
		}
		m["hooks"] = kept
		out = append(out, m)
	}
	return out
}
