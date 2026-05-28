package tui

import (
	"os"
	"strings"

	tea "charm.land/bubbletea/v2"
)

// prefixKeyName is the tmux-style control prefix: after it, the next key is a
// command (focus switch, kill, quit, …) rather than session input. It's the
// Bubble Tea key name (e.g. "ctrl+a", "ctrl+b") and can be overridden with the
// CB_PREFIX environment variable.
var prefixKeyName = func() string {
	if v := strings.TrimSpace(os.Getenv("CB_PREFIX")); v != "" {
		return v
	}
	return "ctrl+a"
}()

// prefixLabel is the human-readable form of the prefix used in help text,
// e.g. "ctrl+a".
func prefixLabel() string {
	return prefixKeyName
}

// codeBytes maps Bubble Tea key codes (Key.Code) to the byte sequences a
// terminal application expects. Covers the common xterm sequences; the
// kitty-keyboard / full-fidelity cases are deferred.
var codeBytes = map[rune][]byte{
	tea.KeyEnter:     {'\r'},
	tea.KeyTab:       {'\t'},
	tea.KeySpace:     {' '},
	tea.KeyEscape:    {0x1b},
	tea.KeyBackspace: {0x7f},
	tea.KeyDelete:    []byte("\x1b[3~"),
	tea.KeyInsert:    []byte("\x1b[2~"),
	tea.KeyUp:        []byte("\x1b[A"),
	tea.KeyDown:      []byte("\x1b[B"),
	tea.KeyRight:     []byte("\x1b[C"),
	tea.KeyLeft:      []byte("\x1b[D"),
	tea.KeyHome:      []byte("\x1b[H"),
	tea.KeyEnd:       []byte("\x1b[F"),
	tea.KeyPgUp:      []byte("\x1b[5~"),
	tea.KeyPgDown:    []byte("\x1b[6~"),
}

// keyToBytes converts a Bubble Tea key press into the raw bytes to forward to a
// child process. Returns nil for keys with no sensible byte encoding.
func keyToBytes(k tea.KeyPressMsg) []byte {
	alt := k.Mod&tea.ModAlt != 0

	// shift+enter -> Kitty CSI-u so the child inserts a newline instead of
	// submitting. Claude Code reads this when it has the Kitty keyboard protocol
	// enabled on its PTY; v2 surfaces shift+enter to us only when the host
	// terminal supports the same protocol (key disambiguation, on by default).
	if k.Code == tea.KeyEnter && k.Mod&tea.ModShift != 0 {
		return []byte("\x1b[13;2u")
	}

	// shift+tab -> CSI Z (back-tab); claude uses it to cycle modes.
	if k.Code == tea.KeyTab && k.Mod&tea.ModShift != 0 {
		return []byte("\x1b[Z")
	}

	// Printable text (letters, digits, symbols, space, multi-rune input). Text is
	// empty for special keys and modifier combos, so this only fires for real
	// characters.
	if k.Text != "" {
		b := []byte(k.Text)
		if alt {
			return append([]byte{0x1b}, b...)
		}
		return b
	}

	// ctrl+<letter> -> control byte (0x01..0x1a); ctrl+space/@ -> NUL, etc.
	if k.Mod&tea.ModCtrl != 0 {
		if b, ok := ctrlBytes(string(k.Code)); ok {
			if alt {
				return append([]byte{0x1b}, b...)
			}
			return b
		}
	}

	// Named special keys (arrows, enter, backspace, …).
	if b, ok := codeBytes[k.Code]; ok {
		if alt {
			return append([]byte{0x1b}, b...)
		}
		return b
	}
	return nil
}

func ctrlBytes(name string) ([]byte, bool) {
	switch name {
	case "space", "@", " ":
		return []byte{0x00}, true
	case "\\":
		return []byte{0x1c}, true
	case "]":
		return []byte{0x1d}, true
	case "^":
		return []byte{0x1e}, true
	case "_":
		return []byte{0x1f}, true
	}
	if len(name) == 1 {
		c := name[0]
		if c >= 'a' && c <= 'z' {
			return []byte{c - 'a' + 1}, true
		}
		if c >= 'A' && c <= 'Z' {
			return []byte{c - 'A' + 1}, true
		}
	}
	return nil, false
}
