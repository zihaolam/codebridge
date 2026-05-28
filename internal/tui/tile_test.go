package tui

import (
	"strings"
	"testing"

	"github.com/charmbracelet/lipgloss"
)

func TestComputeLayoutFits(t *testing.T) {
	for _, n := range []int{1, 2, 3, 4, 5, 6, 9} {
		for _, dim := range [][2]int{{80, 24}, {120, 40}, {200, 50}} {
			w, h := dim[0], dim[1]
			gc, gr, iw, ih := computeLayout(n, w, h)
			if gc*gr < n {
				t.Errorf("n=%d %dx%d: grid %dx%d can't hold %d panes", n, w, h, gc, gr, n)
			}
			// Each box is inner + 2 (border). Grid must fit the viewport.
			if gc*(iw+2) > w {
				t.Errorf("n=%d %dx%d: grid width %d exceeds %d", n, w, h, gc*(iw+2), w)
			}
			if gr*(ih+2) > h {
				t.Errorf("n=%d %dx%d: grid height %d exceeds %d", n, w, h, gr*(ih+2), h)
			}
		}
	}
}

// TestTileViewWidth builds a grid of fake panes and asserts no rendered line
// exceeds the terminal width (which would cause wrapping/corruption).
func TestTileViewWidth(t *testing.T) {
	for _, n := range []int{1, 2, 3, 4, 6} {
		w, h := 120, 40
		m := &tileModel{w: w, h: h}
		for i := 0; i < n; i++ {
			m.panes = append(m.panes, &tilePane{id: string(rune('a' + i))})
		}
		m.gc, m.gr, m.innerW, m.innerH = computeLayout(n, w, h-tileStatusRows)
		// Fill each pane with a block of the right size.
		line := strings.Repeat("X", m.innerW)
		block := strings.TrimSuffix(strings.Repeat(line+"\n", m.innerH), "\n")
		for _, p := range m.panes {
			p.screen = block
		}
		view := m.View()
		for i, ln := range strings.Split(view, "\n") {
			if got := lipgloss.Width(ln); got > w {
				t.Errorf("n=%d: line %d width %d exceeds terminal width %d", n, i, got, w)
			}
		}
	}
}
