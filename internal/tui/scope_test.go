package tui

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"codebridge/internal/ipc"
)

func TestPathWithin(t *testing.T) {
	cases := []struct {
		root, path string
		want       bool
	}{
		{"/a/b", "/a/b", true},        // exact match
		{"/a/b", "/a/b/c", true},      // direct child
		{"/a/b", "/a/b/c/d", true},    // deeper descendant
		{"/a/b", "/a/bbb", false},     // sibling sharing a name prefix
		{"/a/b", "/a", false},         // parent is not within child
		{"/a/b", "/x/y", false},       // unrelated
		{"/a/b/", "/a/b/c", true},     // trailing slash on root is cleaned
		{"/a/b", "/a/b/c/../d", true}, // path is cleaned before matching
		{"", "/anything", true},       // empty root means no scoping
	}
	for _, c := range cases {
		if got := pathWithin(c.root, c.path); got != c.want {
			t.Errorf("pathWithin(%q, %q) = %v, want %v", c.root, c.path, got, c.want)
		}
	}
}

func TestVisibleSessions(t *testing.T) {
	all := []ipc.SessionInfo{
		{ID: "1", Cwd: "/repo"},
		{ID: "2", Cwd: "/repo/internal"},
		{ID: "3", Cwd: "/other"},
		{ID: "4", Cwd: "/repo-extra"}, // name-prefix sibling, must not leak in
	}

	scoped := &dashboardModel{scopeRoot: "/repo"}
	got := scoped.visibleSessions(all)
	if len(got) != 2 || got[0].ID != "1" || got[1].ID != "2" {
		t.Errorf("scoped visibleSessions = %v, want sessions 1 and 2", ids(got))
	}

	showingAll := &dashboardModel{scopeRoot: "/repo", showAll: true}
	if got := showingAll.visibleSessions(all); len(got) != len(all) {
		t.Errorf("showAll visibleSessions = %v, want all %d", ids(got), len(all))
	}

	noScope := &dashboardModel{scopeRoot: ""}
	if got := noScope.visibleSessions(all); len(got) != len(all) {
		t.Errorf("empty-scope visibleSessions = %v, want all %d", ids(got), len(all))
	}
}

func ids(ss []ipc.SessionInfo) []string {
	out := make([]string, len(ss))
	for i, s := range ss {
		out[i] = s.ID
	}
	return out
}

// repoLayout builds a fake repo under a temp dir: a main checkout with a real
// .git directory plus one linked worktree whose .git file + commondir point back
// at the shared .git, mirroring what `git worktree add` produces. It returns the
// main checkout and worktree paths.
func repoLayout(t *testing.T) (main, wt string) {
	t.Helper()
	base := t.TempDir()
	main = filepath.Join(base, "main")
	wt = filepath.Join(base, "feature")
	mainGit := filepath.Join(main, ".git")
	wtGitDir := filepath.Join(mainGit, "worktrees", "feature")
	for _, d := range []string{main, wt, wtGitDir} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	// The worktree's .git is a file pointing at its gitdir; commondir (relative)
	// points back up to the shared .git.
	if err := os.WriteFile(filepath.Join(wt, ".git"), []byte("gitdir: "+wtGitDir+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(wtGitDir, "commondir"), []byte("../..\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	return main, wt
}

func TestGitCommonDirSharedAcrossWorktrees(t *testing.T) {
	main, wt := repoLayout(t)

	got := gitCommonDir(main)
	if got == "" {
		t.Fatalf("gitCommonDir(main) = %q, want the shared .git", got)
	}
	// The main checkout, a subdir of it, the worktree, and a subdir of the
	// worktree must all resolve to the same common dir.
	mainSub := filepath.Join(main, "internal", "tui")
	if err := os.MkdirAll(mainSub, 0o755); err != nil {
		t.Fatal(err)
	}
	wtSub := filepath.Join(wt, "internal")
	if err := os.MkdirAll(wtSub, 0o755); err != nil {
		t.Fatal(err)
	}
	for _, dir := range []string{mainSub, wt, wtSub} {
		if c := gitCommonDir(dir); c != got {
			t.Errorf("gitCommonDir(%q) = %q, want %q", dir, c, got)
		}
	}

	// A directory outside any repo resolves to "".
	if c := gitCommonDir(t.TempDir()); c != "" {
		t.Errorf("gitCommonDir(non-repo) = %q, want \"\"", c)
	}
}

func TestDeriveScopeWorktreeUsesMainRoot(t *testing.T) {
	main, wt := repoLayout(t)

	commonMain, rootMain := deriveScope(main)
	commonWt, rootWt := deriveScope(wt)
	if commonMain == "" || commonMain != commonWt {
		t.Fatalf("deriveScope common: main=%q wt=%q, want equal and non-empty", commonMain, commonWt)
	}
	// Both launch points display as the main repo, not the worktree folder.
	if folderBase(rootMain) != "main" || folderBase(rootWt) != "main" {
		t.Errorf("display root: main=%q wt=%q, want both basename \"main\"", rootMain, rootWt)
	}
}

// TestInScopeCaseInsensitiveFS guards the bug where a dashboard launched at a
// lowercased path (e.g. zsh tab-completing into /Users/zihaolam/projects/tcg
// rather than the on-disk /Users/zihaolam/Projects/tcg) failed to match worktree
// sessions whose .git pointed back to the canonical-cased main repo, even
// though both refer to the same inode on APFS/HFS+.
func TestInScopeCaseInsensitiveFS(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "windows" {
		t.Skip("case-insensitive filesystem assumption only holds on darwin/windows")
	}
	main, wt := repoLayout(t)
	lowerMain := strings.ToLower(main)
	if lowerMain == main {
		t.Skip("temp dir already lowercase; case-mismatch not exercised")
	}
	// Launching from the lowercased main path should still pick up the worktree
	// session whose cwd carries the canonical case.
	common, root := deriveScope(lowerMain)
	m := &dashboardModel{scopeCommon: common, scopeRoot: root, repoCache: map[string]string{}}
	got := m.visibleSessions([]ipc.SessionInfo{
		{ID: "wt", Cwd: wt},
		{ID: "outside", Cwd: t.TempDir()},
	})
	if len(got) != 1 || got[0].ID != "wt" {
		t.Fatalf("visibleSessions from lowercased main = %v, want only [wt]", ids(got))
	}
}

func TestInScopeMatchesWorktreeSessions(t *testing.T) {
	main, wt := repoLayout(t)
	common, root := deriveScope(main)
	m := &dashboardModel{scopeCommon: common, scopeRoot: root, repoCache: map[string]string{}}

	all := []ipc.SessionInfo{
		{ID: "main", Cwd: main},
		{ID: "wt", Cwd: wt},
		{ID: "wtsub", Cwd: filepath.Join(wt, "internal")},
		{ID: "outside", Cwd: t.TempDir()},
	}
	got := m.visibleSessions(all)
	if len(got) != 3 {
		t.Fatalf("visibleSessions = %v, want main, wt, wtsub", ids(got))
	}
	for _, s := range got {
		if s.ID == "outside" {
			t.Errorf("out-of-repo session leaked into scope: %v", ids(got))
		}
	}
}
