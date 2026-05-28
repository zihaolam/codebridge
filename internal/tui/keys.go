package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

// namedKeyBytes maps Bubble Tea key names (msg.String()) to the byte sequences
// a terminal application expects. Covers the common xterm sequences; the
// kitty-keyboard / full-fidelity cases are deferred.
var namedKeyBytes = map[string][]byte{
	"enter":     {'\r'},
	"tab":       {'\t'},
	"space":     {' '},
	"esc":       {0x1b},
	"escape":    {0x1b},
	"backspace": {0x7f},
	"delete":    []byte("\x1b[3~"),
	"insert":    []byte("\x1b[2~"),
	"up":        []byte("\x1b[A"),
	"down":      []byte("\x1b[B"),
	"right":     []byte("\x1b[C"),
	"left":      []byte("\x1b[D"),
	"home":      []byte("\x1b[H"),
	"end":       []byte("\x1b[F"),
	"pgup":      []byte("\x1b[5~"),
	"pgdown":    []byte("\x1b[6~"),
	"shift+tab": []byte("\x1b[Z"),
}

// keyToBytes converts a Bubble Tea key press into the raw bytes to forward to a
// child process. Returns nil for keys with no sensible byte encoding.
func keyToBytes(k tea.KeyMsg) []byte {
	// Pasted text: forward the runes verbatim.
	if k.Paste {
		return []byte(string(k.Runes))
	}

	if k.Type == tea.KeySpace {
		if k.Alt {
			return []byte{0x1b, ' '}
		}
		return []byte{' '}
	}

	// Printable runes (letters, digits, symbols, and multi-rune input).
	if k.Type == tea.KeyRunes || len(k.Runes) > 0 {
		b := []byte(string(k.Runes))
		if k.Alt {
			return append([]byte{0x1b}, b...)
		}
		return b
	}

	s := k.String()

	// ctrl+<letter> -> control byte (0x01..0x1a). ctrl+space/@ -> NUL.
	if rest, ok := strings.CutPrefix(s, "ctrl+"); ok {
		if b, ok := ctrlBytes(rest); ok {
			return b
		}
	}

	base := strings.TrimPrefix(s, "alt+")
	if b, ok := namedKeyBytes[base]; ok {
		if k.Alt || strings.HasPrefix(s, "alt+") {
			return append([]byte{0x1b}, b...)
		}
		return b
	}
	return nil
}

func ctrlBytes(name string) ([]byte, bool) {
	switch name {
	case "space", "@":
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
