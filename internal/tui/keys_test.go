package tui

import (
	"bytes"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

func TestKeyToBytes(t *testing.T) {
	cases := []struct {
		name string
		key  tea.KeyMsg
		want []byte
	}{
		{"rune a", tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune{'a'}}, []byte("a")},
		{"rune digits", tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune{'1', '2'}}, []byte("12")},
		{"alt+rune", tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune{'x'}, Alt: true}, []byte{0x1b, 'x'}},
		{"enter", tea.KeyMsg{Type: tea.KeyEnter}, []byte{'\r'}},
		{"tab", tea.KeyMsg{Type: tea.KeyTab}, []byte{'\t'}},
		{"space", tea.KeyMsg{Type: tea.KeySpace}, []byte{' '}},
		{"esc", tea.KeyMsg{Type: tea.KeyEsc}, []byte{0x1b}},
		{"backspace", tea.KeyMsg{Type: tea.KeyBackspace}, []byte{0x7f}},
		{"up", tea.KeyMsg{Type: tea.KeyUp}, []byte("\x1b[A")},
		{"down", tea.KeyMsg{Type: tea.KeyDown}, []byte("\x1b[B")},
		{"right", tea.KeyMsg{Type: tea.KeyRight}, []byte("\x1b[C")},
		{"left", tea.KeyMsg{Type: tea.KeyLeft}, []byte("\x1b[D")},
		{"home", tea.KeyMsg{Type: tea.KeyHome}, []byte("\x1b[H")},
		{"end", tea.KeyMsg{Type: tea.KeyEnd}, []byte("\x1b[F")},
		{"delete", tea.KeyMsg{Type: tea.KeyDelete}, []byte("\x1b[3~")},
		{"pgup", tea.KeyMsg{Type: tea.KeyPgUp}, []byte("\x1b[5~")},
		{"shift+tab", tea.KeyMsg{Type: tea.KeyShiftTab}, []byte("\x1b[Z")},
		{"ctrl+c", tea.KeyMsg{Type: tea.KeyCtrlC}, []byte{0x03}},
		{"ctrl+a", tea.KeyMsg{Type: tea.KeyCtrlA}, []byte{0x01}},
		{"ctrl+z", tea.KeyMsg{Type: tea.KeyCtrlZ}, []byte{0x1a}},
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
