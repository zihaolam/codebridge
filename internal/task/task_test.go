package task

import (
	"os"
	"path/filepath"
	"testing"
)

// TestSaveLoadRoundTrip checks that a store written via Save (which writes
// back to the LoadFrom path) reads back identically, multi-line descriptions
// included.
func TestSaveLoadRoundTrip(t *testing.T) {
	p := filepath.Join(t.TempDir(), "tasks.json")
	st := LoadFrom(p)
	if len(st.Tasks) != 0 {
		t.Fatalf("fresh store has %d tasks, want 0", len(st.Tasks))
	}
	aID := st.Add("scopeA", "first", "line1\nline2").ID
	bID := st.Add("scopeB", "second", "").ID
	if aID == "" || aID == bID {
		t.Fatalf("ids not unique/non-empty: %q vs %q", aID, bID)
	}
	if err := st.Save(); err != nil {
		t.Fatalf("Save: %v", err)
	}

	got := LoadFrom(p)
	if len(got.Tasks) != 2 {
		t.Fatalf("reloaded %d tasks, want 2", len(got.Tasks))
	}
	a := got.Get(aID)
	if a == nil || a.Title != "first" || a.Desc != "line1\nline2" || a.Scope != "scopeA" {
		t.Errorf("task a round-tripped wrong: %+v", a)
	}
	if a.Status != StatusPending {
		t.Errorf("new task status = %q, want pending", a.Status)
	}
	if a.CreatedAt.IsZero() || a.UpdatedAt.IsZero() {
		t.Errorf("timestamps not set: %+v", a)
	}
}

// TestLoadMissingAndCorrupt: both a missing file and unparseable JSON yield an
// empty store (never an error that could block the dashboard), and saving over
// the corrupt file recovers it.
func TestLoadMissingAndCorrupt(t *testing.T) {
	dir := t.TempDir()
	if st := LoadFrom(filepath.Join(dir, "nope.json")); len(st.Tasks) != 0 {
		t.Errorf("missing file: got %d tasks, want 0", len(st.Tasks))
	}

	bad := filepath.Join(dir, "bad.json")
	if err := os.WriteFile(bad, []byte("{not json"), 0o600); err != nil {
		t.Fatal(err)
	}
	st := LoadFrom(bad)
	if len(st.Tasks) != 0 {
		t.Errorf("corrupt file: got %d tasks, want 0", len(st.Tasks))
	}
	st.Add("s", "recovered", "")
	if err := st.Save(); err != nil {
		t.Fatalf("Save over corrupt file: %v", err)
	}
	if got := LoadFrom(bad); len(got.Tasks) != 1 || got.Tasks[0].Title != "recovered" {
		t.Errorf("recovery reload wrong: %+v", got.Tasks)
	}
}

func TestCRUDAndForScope(t *testing.T) {
	st := LoadFrom(filepath.Join(t.TempDir(), "tasks.json"))
	aID := st.Add("s1", "a", "").ID
	bID := st.Add("s2", "b", "").ID
	cID := st.Add("s1", "c", "").ID

	if st.Get("nope") != nil {
		t.Error("Get(unknown) should be nil")
	}
	if got := st.Get(aID); got == nil || got.Title != "a" {
		t.Errorf("Get(a) = %+v", got)
	}

	s1 := st.ForScope("s1")
	if len(s1) != 2 || s1[0].ID != aID || s1[1].ID != cID {
		t.Errorf("ForScope(s1) wrong order/content: %+v", s1)
	}
	if got := st.ForScope("s3"); len(got) != 0 {
		t.Errorf("ForScope(empty scope) = %+v", got)
	}

	st.Delete(bID)
	if st.Get(bID) != nil || len(st.Tasks) != 2 {
		t.Errorf("Delete(b) left %+v", st.Tasks)
	}
	st.Delete("nonexistent") // no-op, must not panic
	if len(st.Tasks) != 2 {
		t.Errorf("Delete(unknown) changed the store: %+v", st.Tasks)
	}
}

func TestLoadMigratesLegacySessionIntoRun(t *testing.T) {
	p := filepath.Join(t.TempDir(), "tasks.json")
	data := []byte(`{"tasks":[{"id":"task-1","title":"legacy","status":"paused","agent":"codex","cb_session_id":"cb-1","agent_session_id":"agent-1"}]}`)
	if err := os.WriteFile(p, data, 0o600); err != nil {
		t.Fatal(err)
	}
	st := LoadFrom(p)
	if len(st.Tasks) != 1 || len(st.Tasks[0].Runs) != 1 {
		t.Fatalf("legacy task was not migrated: %+v", st.Tasks)
	}
	r := st.Tasks[0].Runs[0]
	if r.CBSessionID != "cb-1" || r.AgentSessionID != "agent-1" || r.Agent != "codex" {
		t.Fatalf("migrated run wrong: %+v", r)
	}
}
