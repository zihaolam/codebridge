package session

import (
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"
	"github.com/charmbracelet/x/vt"
)

// TestTrimColsStyledRule guards the width fix: a styled full-width rule (the
// kind Claude Code draws around its input box) must survive trimCols at its
// exact display width with its trailing reset intact. The old rune-count clip
// cut it short and dropped the reset, leaving a dangling color and a line the
// client mis-wrapped onto a stray extra row.
func TestTrimColsStyledRule(t *testing.T) {
	const cols = 40
	emu := vt.NewSafeEmulator(cols+emuPad, 3)
	rule := "\x1b[38;5;244m" + strings.Repeat("─", cols) + "\x1b[0m\r\n"
	if _, err := emu.Write([]byte(rule)); err != nil {
		t.Fatalf("write: %v", err)
	}

	got := trimCols(emu.Render(), cols)
	first := strings.Split(got, "\n")[0]

	if w := ansi.StringWidth(first); w != cols {
		t.Errorf("rule display width = %d, want %d (line %q)", w, cols, first)
	}
	if !strings.HasSuffix(first, "\x1b[m") && !strings.HasSuffix(first, "\x1b[0m") {
		t.Errorf("rule lost its trailing reset, color would bleed: %q", first)
	}
}

// TestTrimColsDropsOverflowGlyph checks the original emuPad purpose still holds:
// a glyph that lands in the spare overflow column is clipped back off so it
// never widens the rendered line past the PTY width.
func TestTrimColsDropsOverflowGlyph(t *testing.T) {
	const cols = 10
	// A plain line one column wider than cols (simulating the deferred-wrap
	// glyph the emulator parks in the spare column).
	line := strings.Repeat("x", cols+emuPad)
	got := trimCols(line, cols)
	if w := ansi.StringWidth(got); w != cols {
		t.Errorf("trimmed width = %d, want %d (%q)", w, cols, got)
	}
}
