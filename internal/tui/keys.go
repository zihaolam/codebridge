package tui

import (
	"os"
	"strconv"
	"strings"

	tea "charm.land/bubbletea/v2"
)

// prefixKeyName is the tmux-style control prefix: after it, the next key is a
// command (focus switch, kill, quit, …) rather than session input. It's the
// Bubble Tea key name (e.g. "ctrl+a", "ctrl+b"). Resolution order on startup:
// CB_PREFIX env var > the dashboard's saved config (applied via SetPrefix at
// boot) > the built-in "ctrl+a". The env var is checked here so non-dashboard
// codepaths (tests, the ctl client) see the same effective prefix without
// having to load the config file.
var prefixKeyName = func() string {
	if v := strings.TrimSpace(os.Getenv("CB_PREFIX")); v != "" {
		return v
	}
	return "ctrl+a"
}()

// envPrefixSet records whether CB_PREFIX was set at package init time so
// SetPrefix can honor "env wins" without re-reading the environment on every
// call. Captured once: env vars don't change mid-process for our use cases.
var envPrefixSet = strings.TrimSpace(os.Getenv("CB_PREFIX")) != ""

// SetPrefix updates the prefix to key, unless CB_PREFIX is set in which case
// it's a no-op (the env override always wins). The dashboard calls this once
// at boot with the value loaded from disk, and again whenever the user
// changes it in the config menu.
func SetPrefix(key string) {
	if envPrefixSet {
		return
	}
	if k := strings.TrimSpace(key); k != "" {
		prefixKeyName = k
	}
}

// PrefixOverriddenByEnv reports whether CB_PREFIX is forcing the prefix value.
// The config menu uses this to disable the prefix row and surface a hint.
func PrefixOverriddenByEnv() bool { return envPrefixSet }

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

// csiFinal is the trailing letter of the xterm CSI sequence for arrows /
// Home / End. Used to build the parameterized form when modifiers are set
// (e.g. Option+Left → "\x1b[1;3D", Cmd+Left → "\x1b[1;9D").
var csiFinal = map[rune]byte{
	tea.KeyUp:    'A',
	tea.KeyDown:  'B',
	tea.KeyRight: 'C',
	tea.KeyLeft:  'D',
	tea.KeyHome:  'H',
	tea.KeyEnd:   'F',
}

// csiTilde is the leading number of the xterm CSI "~" sequence for the
// editing keys. Used to build the parameterized form when modifiers are set
// (e.g. Shift+PgUp → "\x1b[5;2~").
var csiTilde = map[rune]byte{
	tea.KeyInsert: '2',
	tea.KeyDelete: '3',
	tea.KeyPgUp:   '5',
	tea.KeyPgDown: '6',
}

// keyToBytes converts a Bubble Tea key press into the raw bytes to forward to a
// child process. Returns nil for keys with no sensible byte encoding.
func keyToBytes(k tea.KeyPressMsg) []byte {
	alt := k.Mod&tea.ModAlt != 0
	// Lock-state bits (CapsLock/NumLock/ScrollLock) can ride along on any
	// keypress when the terminal speaks the Kitty keyboard protocol. They
	// must not affect modifier-equality checks below, so strip them once.
	primary := k.Mod &^ (tea.ModCapsLock | tea.ModNumLock | tea.ModScrollLock)

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

	// macOS text-editing conventions for word/line nav: Option+arrow → word, Cmd+
	// arrow → line. The xterm CSI-parameter form below (\x1b[1;3D for Alt+Left,
	// \x1b[1;9D for Cmd+Left) is technically correct but readline doesn't bind
	// it, so Ink (claude code), zsh, and bash all see "nothing happened". Emit
	// the readline-conventional sequences instead so word/line movement actually
	// fires in the child. Only the unmodified Option/Cmd combos are rewritten;
	// Shift-extended variants fall through to the CSI form below for any TUI
	// that does want selection-by-word.
	if primary == tea.ModAlt {
		switch k.Code {
		case tea.KeyLeft:
			return []byte("\x1bb")
		case tea.KeyRight:
			return []byte("\x1bf")
		}
	}
	if primary == tea.ModSuper || primary == tea.ModMeta {
		switch k.Code {
		case tea.KeyLeft:
			return []byte{0x01}
		case tea.KeyRight:
			return []byte{0x05}
		}
	}

	// iTerm2 / Terminal.app default mappings already pre-translate Option+Left
	// and Option+Right into the readline word-nav bytes (\x1bb and \x1bf). The
	// terminal hands us \x1bb, bubbletea decodes it as Alt+'b' with Text="" (it
	// clears Text for any alt-prefix decode), and the printable-text branch
	// below would skip it because Text is empty. Synthesize the byte from Code
	// so word-nav still reaches the child on those terminals. Constrained to
	// printable ASCII and Alt-only (plus lock bits) so it can't accidentally
	// swallow Shift+Alt+letter or modified specials.
	if primary == tea.ModAlt && k.Code >= 0x20 && k.Code < 0x7f && k.Text == "" {
		return []byte{0x1b, byte(k.Code)}
	}

	// Modified navigation keys (arrows, Home/End, PgUp/PgDn, Insert/Delete) need
	// the xterm CSI-parameter form so the child sees a single Option/Cmd/Shift/
	// Ctrl-prefixed key, not "Escape then arrow" (the legacy ESC-prefix encoding
	// that some Ink/Node-readline TUIs split into two events). Cmd (Super) is
	// otherwise silently dropped if we fall through to the unmodified map.
	if p := csiMod(k.Mod); p != 0 {
		ps := strconv.Itoa(int(p))
		if final, ok := csiFinal[k.Code]; ok {
			return []byte("\x1b[1;" + ps + string(final))
		}
		if n, ok := csiTilde[k.Code]; ok {
			return []byte("\x1b[" + string(n) + ";" + ps + "~")
		}
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

// csiMod converts Bubble Tea modifier flags into the xterm CSI parameter:
// shift=1, alt=2, ctrl=4, super/meta=8, encoded as 1 + sum. Returns 0 when no
// relevant modifier is set (so the caller skips the parameterized form).
// Super and Meta both fold to bit 8 — macOS Cmd reports as Super under Kitty
// keyboard, Meta on some terminals; either way the child sees the same key.
func csiMod(m tea.KeyMod) byte {
	var p byte
	if m&tea.ModShift != 0 {
		p |= 1
	}
	if m&tea.ModAlt != 0 {
		p |= 2
	}
	if m&tea.ModCtrl != 0 {
		p |= 4
	}
	if m&(tea.ModSuper|tea.ModMeta) != 0 {
		p |= 8
	}
	if p == 0 {
		return 0
	}
	return p + 1
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
