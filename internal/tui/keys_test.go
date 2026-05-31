package tui

import (
	"bytes"
	"testing"

	tea "charm.land/bubbletea/v2"
)

func TestKeyToBytes(t *testing.T) {
	cases := []struct {
		name string
		key  tea.KeyPressMsg
		want []byte
	}{
		{"rune a", tea.KeyPressMsg{Code: 'a', Text: "a"}, []byte("a")},
		{"rune digits", tea.KeyPressMsg{Text: "12"}, []byte("12")},
		{"alt+rune", tea.KeyPressMsg{Code: 'x', Text: "x", Mod: tea.ModAlt}, []byte{0x1b, 'x'}},
		{"enter", tea.KeyPressMsg{Code: tea.KeyEnter}, []byte{'\r'}},
		{"shift+enter", tea.KeyPressMsg{Code: tea.KeyEnter, Mod: tea.ModShift}, []byte("\x1b[13;2u")},
		{"tab", tea.KeyPressMsg{Code: tea.KeyTab}, []byte{'\t'}},
		{"shift+tab", tea.KeyPressMsg{Code: tea.KeyTab, Mod: tea.ModShift}, []byte("\x1b[Z")},
		{"space", tea.KeyPressMsg{Code: tea.KeySpace, Text: " "}, []byte{' '}},
		{"esc", tea.KeyPressMsg{Code: tea.KeyEscape}, []byte{0x1b}},
		{"backspace", tea.KeyPressMsg{Code: tea.KeyBackspace}, []byte{0x7f}},
		{"up", tea.KeyPressMsg{Code: tea.KeyUp}, []byte("\x1b[A")},
		{"down", tea.KeyPressMsg{Code: tea.KeyDown}, []byte("\x1b[B")},
		{"right", tea.KeyPressMsg{Code: tea.KeyRight}, []byte("\x1b[C")},
		{"left", tea.KeyPressMsg{Code: tea.KeyLeft}, []byte("\x1b[D")},
		{"home", tea.KeyPressMsg{Code: tea.KeyHome}, []byte("\x1b[H")},
		{"end", tea.KeyPressMsg{Code: tea.KeyEnd}, []byte("\x1b[F")},
		{"delete", tea.KeyPressMsg{Code: tea.KeyDelete}, []byte("\x1b[3~")},
		{"pgup", tea.KeyPressMsg{Code: tea.KeyPgUp}, []byte("\x1b[5~")},
		{"ctrl+c", tea.KeyPressMsg{Code: 'c', Mod: tea.ModCtrl}, []byte{0x03}},
		{"ctrl+a", tea.KeyPressMsg{Code: 'a', Mod: tea.ModCtrl}, []byte{0x01}},
		{"ctrl+z", tea.KeyPressMsg{Code: 'z', Mod: tea.ModCtrl}, []byte{0x1a}},
		// macOS Option+arrow → readline word movement (ESC b / ESC f). Plain Alt
		// only — Shift/Ctrl-combined Alt still go through the CSI form below.
		{"alt+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModAlt}, []byte("\x1bb")},
		{"alt+right", tea.KeyPressMsg{Code: tea.KeyRight, Mod: tea.ModAlt}, []byte("\x1bf")},
		// macOS Cmd+arrow → readline line nav (Ctrl+A / Ctrl+E).
		{"cmd+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModSuper}, []byte{0x01}},
		{"cmd+right", tea.KeyPressMsg{Code: tea.KeyRight, Mod: tea.ModSuper}, []byte{0x05}},
		{"meta+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModMeta}, []byte{0x01}},
		// Combined modifiers fall through to the xterm CSI-parameter form so any
		// TUI that wants extend-selection-by-word still gets it.
		{"shift+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModShift}, []byte("\x1b[1;2D")},
		{"ctrl+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModCtrl}, []byte("\x1b[1;5D")},
		{"shift+cmd+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModShift | tea.ModSuper}, []byte("\x1b[1;10D")},
		{"shift+alt+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModShift | tea.ModAlt}, []byte("\x1b[1;4D")},
		{"ctrl+alt+left", tea.KeyPressMsg{Code: tea.KeyLeft, Mod: tea.ModCtrl | tea.ModAlt}, []byte("\x1b[1;7D")},
		// Alt+Up/Down aren't word movement — let the CSI form through.
		{"alt+up", tea.KeyPressMsg{Code: tea.KeyUp, Mod: tea.ModAlt}, []byte("\x1b[1;3A")},
		{"alt+down", tea.KeyPressMsg{Code: tea.KeyDown, Mod: tea.ModAlt}, []byte("\x1b[1;3B")},
		{"shift+pgup", tea.KeyPressMsg{Code: tea.KeyPgUp, Mod: tea.ModShift}, []byte("\x1b[5;2~")},
		{"alt+delete", tea.KeyPressMsg{Code: tea.KeyDelete, Mod: tea.ModAlt}, []byte("\x1b[3;3~")},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := keyToBytes(tc.key)
			if !bytes.Equal(got, tc.want) {
				t.Errorf("keyToBytes(%s) = %v, want %v", tc.name, got, tc.want)
			}
		})
	}
}
