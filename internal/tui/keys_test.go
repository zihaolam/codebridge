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
