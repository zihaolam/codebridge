package daemon

import (
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"
)

func TestCropViewportFollowsCursor(t *testing.T) {
	screen := "0123456789\nabcdefghij\nABCDEFGHIJ\n!@#$%^&*()"
	got, cx, cy, x, y := cropViewport(screen, 4, 10, 2, 5, 8, 3, 0, 0)
	if want := "EFGHI\n%^&*("; got != want {
		t.Fatalf("screen = %q, want %q", got, want)
	}
	if cx != 4 || cy != 1 || x != 4 || y != 2 {
		t.Fatalf("cursor=(%d,%d) origin=(%d,%d), want cursor=(4,1) origin=(4,2)", cx, cy, x, y)
	}
}

func TestCropViewportPreservesANSIAndWidth(t *testing.T) {
	line := "\x1b[31m0123456789\x1b[0m"
	got, _, _, _, _ := cropViewport(line, 1, 10, 1, 4, 7, 0, 0, 0)
	if plain := ansi.Strip(got); plain != "4567" {
		t.Fatalf("plain screen = %q, want %q (raw %q)", plain, "4567", got)
	}
	if !strings.Contains(got, "\x1b[") {
		t.Fatalf("cropped styled line lost ANSI styling: %q", got)
	}
}
