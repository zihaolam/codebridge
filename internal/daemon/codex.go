package daemon

import (
	"bufio"
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"time"

	"codebridge/internal/session"
)

// Codex never fires cb hooks, so unlike claude there's no callback carrying
// its session id. But codex journals every session as a rollout file under
// $CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl whose first line
// is a session_meta record with the id, start timestamp and cwd. After
// spawning a codex session the daemon polls that tree for a rollout that
// started after the spawn in the same cwd and attributes its id to the
// session — giving the task backlog an exact `codex resume <id>` handle
// instead of best-effort `--last`.

const (
	codexHarvestTimeout = 60 * time.Second
	codexHarvestPoll    = 500 * time.Millisecond
)

// codexSessionsDir resolves codex's session journal root, honoring CODEX_HOME.
func codexSessionsDir() string {
	if h := os.Getenv("CODEX_HOME"); h != "" {
		return filepath.Join(h, "sessions")
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".codex", "sessions")
}

// codexMeta is the session_meta first line of a rollout file, trimmed to the
// fields attribution needs.
type codexMeta struct {
	Type    string `json:"type"`
	Payload struct {
		ID        string    `json:"id"`
		Timestamp time.Time `json:"timestamp"`
		Cwd       string    `json:"cwd"`
	} `json:"payload"`
}

// readCodexMeta parses a rollout file's first line. The meta line embeds the
// full base instructions, so it runs to tens of KB — hence the large scanner
// cap.
func readCodexMeta(path string) (codexMeta, bool) {
	f, err := os.Open(path)
	if err != nil {
		return codexMeta{}, false
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	if !sc.Scan() {
		return codexMeta{}, false
	}
	var m codexMeta
	if err := json.Unmarshal(sc.Bytes(), &m); err != nil || m.Type != "session_meta" || m.Payload.ID == "" {
		return codexMeta{}, false
	}
	return m, true
}

// sameDir reports whether two paths name the same directory, tolerating
// symlink aliases (macOS /tmp vs /private/tmp).
func sameDir(a, b string) bool {
	if filepath.Clean(a) == filepath.Clean(b) {
		return true
	}
	ra, err1 := filepath.EvalSymlinks(a)
	rb, err2 := filepath.EvalSymlinks(b)
	return err1 == nil && err2 == nil && ra == rb
}

// findCodexSessionID scans root for the earliest unclaimed rollout that
// started at/after since in cwd. Earliest-wins keeps two concurrent spawns in
// the same cwd from both grabbing the newer session.
func findCodexSessionID(root string, since time.Time, cwd string, claimed func(string) bool) (string, bool) {
	if root == "" {
		return "", false
	}
	paths, _ := filepath.Glob(filepath.Join(root, "*", "*", "*", "rollout-*.jsonl"))
	// Small slack for fs/clock jitter; the meta timestamp comes from the same
	// machine clock, so anything older belongs to a pre-existing session.
	cutoff := since.Add(-time.Second)
	var bestID string
	var bestTS time.Time
	for _, p := range paths {
		// mtime is a cheap pre-filter: rollouts are append-only, so a session
		// started after the cutoff has mtime at/after it too. This skips the
		// meta parse for the (large) historical majority.
		if fi, err := os.Stat(p); err != nil || fi.ModTime().Before(cutoff) {
			continue
		}
		m, ok := readCodexMeta(p)
		if !ok || m.Payload.Timestamp.Before(cutoff) || !sameDir(m.Payload.Cwd, cwd) {
			continue
		}
		if claimed != nil && claimed(m.Payload.ID) {
			continue
		}
		if bestID == "" || m.Payload.Timestamp.Before(bestTS) {
			bestID, bestTS = m.Payload.ID, m.Payload.Timestamp
		}
	}
	return bestID, bestID != ""
}

// codexClaimed reports whether a rollout id is already attributed to a session.
func (d *Daemon) codexClaimed(id string) bool {
	d.codexMu.Lock()
	defer d.codexMu.Unlock()
	return d.codexTaken[id]
}

// claimCodexID attributes a rollout id atomically; false means another
// harvester raced us to it.
func (d *Daemon) claimCodexID(id string) bool {
	d.codexMu.Lock()
	defer d.codexMu.Unlock()
	if d.codexTaken[id] {
		return false
	}
	if d.codexTaken == nil {
		d.codexTaken = map[string]bool{}
	}
	d.codexTaken[id] = true
	return true
}

// harvestCodexSession polls the rollout tree until the spawned codex session's
// id shows up (or the session dies / the timeout lapses), then records it on
// the session so list snapshots expose it as the task backlog's resume handle.
// Runs for `codex resume ...` spawns too: resuming writes a fresh rollout with
// a new id, and harvesting it keeps the task's handle pointing at the latest
// journal.
func (d *Daemon) harvestCodexSession(s *session.Session, since time.Time) {
	root := codexSessionsDir()
	if root == "" {
		return
	}
	cwd := s.Cwd
	if cwd == "" {
		cwd, _ = os.Getwd()
	}
	deadline := time.Now().Add(codexHarvestTimeout)
	for time.Now().Before(deadline) {
		if s.Exited() {
			return
		}
		id, ok := findCodexSessionID(root, since, cwd, d.codexClaimed)
		if ok && d.claimCodexID(id) {
			s.SetHarnessSessionID(id)
			log.Printf("session %s: harvested codex session id %s", s.ID, id)
			d.notifyChange()
			return
		}
		time.Sleep(codexHarvestPoll)
	}
}
