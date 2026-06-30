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

// TestLiveFrameCachesUntilDirty verifies the render cache is gated on the gen
// dirty counter: a second call with no gen bump must return the same frame even
// if the emulator's buffer has secretly changed (proving the render was skipped,
// not just deduped), and a gen bump must surface the new content. This is what
// lets many clients attached to one session share a single render per change
// instead of each re-rendering an idle screen 30× a second.
func TestLiveFrameCachesUntilDirty(t *testing.T) {
	const cols, rows = 20, 4
	s := &Session{
		emu:  vt.NewSafeEmulator(cols+emuPad, rows),
		rows: rows,
		cols: cols,
	}

	// First write + dirty bump — exactly what readLoop does.
	if _, err := s.emu.Write([]byte("hello")); err != nil {
		t.Fatalf("write: %v", err)
	}
	s.gen.Add(1)

	screen1, _, _, _, _ := s.LiveFrame()
	if !strings.Contains(screen1, "hello") {
		t.Fatalf("first frame missing content: %q", screen1)
	}

	// Mutate the emulator WITHOUT bumping gen. LiveFrame must hand back the
	// cached frame, proving it never re-rendered.
	if _, err := s.emu.Write([]byte(" world")); err != nil {
		t.Fatalf("write: %v", err)
	}
	if screen2, _, _, _, _ := s.LiveFrame(); screen2 != screen1 {
		t.Errorf("LiveFrame re-rendered despite unchanged gen:\n got %q\nwant %q", screen2, screen1)
	}

	// Bump gen as readLoop would; the new content must now surface.
	s.gen.Add(1)
	if screen3, _, _, _, _ := s.LiveFrame(); !strings.Contains(screen3, "hello world") {
		t.Errorf("LiveFrame did not refresh after dirty bump: %q", screen3)
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
