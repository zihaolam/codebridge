package daemon

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// writeRollout drops a codex rollout journal with a session_meta first line
// (plus a payload line, like the real thing) into root's date tree.
func writeRollout(t *testing.T, root, id string, ts time.Time, cwd string) {
	t.Helper()
	dir := filepath.Join(root, "2026", "07", "09")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	meta := map[string]any{
		"timestamp": ts.UTC().Format(time.RFC3339Nano),
		"type":      "session_meta",
		"payload": map[string]any{
			"id":        id,
			"timestamp": ts.UTC().Format(time.RFC3339Nano),
			"cwd":       cwd,
		},
	}
	line, err := json.Marshal(meta)
	if err != nil {
		t.Fatal(err)
	}
	name := fmt.Sprintf("rollout-%s-%s.jsonl", ts.Format("2006-01-02T15-04-05"), id)
	body := string(line) + "\n" + `{"type":"turn_context"}` + "\n"
	if err := os.WriteFile(filepath.Join(dir, name), []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
}

// TestFindCodexSessionID covers attribution: pre-existing sessions and other
// cwds are ignored, claimed ids are skipped, and among several fresh matches
// the earliest-started wins.
func TestFindCodexSessionID(t *testing.T) {
	root := t.TempDir()
	since := time.Now()
	cwd := "/repo/here"

	writeRollout(t, root, "old-session", since.Add(-time.Hour), cwd)
	writeRollout(t, root, "other-cwd", since.Add(2*time.Second), "/repo/elsewhere")
	writeRollout(t, root, "claimed-one", since.Add(1*time.Second), cwd)
	writeRollout(t, root, "want-this", since.Add(2*time.Second), cwd)
	writeRollout(t, root, "too-late", since.Add(3*time.Second), cwd)

	claimed := func(id string) bool { return id == "claimed-one" }
	id, ok := findCodexSessionID(root, since, cwd, claimed)
	if !ok || id != "want-this" {
		t.Fatalf("findCodexSessionID = %q, %v; want want-this", id, ok)
	}

	// Nothing matching at all: unrelated cwd.
	if id, ok := findCodexSessionID(root, since, "/nowhere", nil); ok {
		t.Fatalf("unexpected match %q for unrelated cwd", id)
	}
	// Empty root is a graceful no-op.
	if _, ok := findCodexSessionID("", since, cwd, nil); ok {
		t.Fatal("empty root should never match")
	}
}

// TestReadCodexMetaRejectsGarbage: unparseable or wrong-typed first lines must
// be skipped, not crash attribution.
func TestReadCodexMetaRejectsGarbage(t *testing.T) {
	dir := t.TempDir()
	cases := map[string]string{
		"not-json.jsonl":   "{nope\n",
		"wrong-type.jsonl": `{"type":"turn_context","payload":{"id":"x"}}` + "\n",
		"no-id.jsonl":      `{"type":"session_meta","payload":{"cwd":"/a"}}` + "\n",
		"empty.jsonl":      "",
	}
	for name, body := range cases {
		p := filepath.Join(dir, name)
		if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
			t.Fatal(err)
		}
		if m, ok := readCodexMeta(p); ok {
			t.Errorf("%s: parsed unexpectedly: %+v", name, m)
		}
	}
	if _, ok := readCodexMeta(filepath.Join(dir, "missing.jsonl")); ok {
		t.Error("missing file should not parse")
	}
}

// TestClaimCodexID: first claim wins, repeat claims are rejected, and the
// claimed set feeds codexClaimed.
func TestClaimCodexID(t *testing.T) {
	d := &Daemon{}
	if d.codexClaimed("a") {
		t.Fatal("fresh daemon claims nothing")
	}
	if !d.claimCodexID("a") {
		t.Fatal("first claim must succeed")
	}
	if d.claimCodexID("a") {
		t.Fatal("second claim of the same id must fail")
	}
	if !d.codexClaimed("a") {
		t.Fatal("claimed id must be reported as claimed")
	}
}
