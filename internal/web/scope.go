package web

import (
	"os"
	"path/filepath"
	"strings"
)

// Scope derivation for sidebar grouping, mirroring the TUI accordion's group
// keys (internal/tui/dashboard.go scopeKeyOf/gitCommonDir): sessions anywhere
// in one repo — main checkout or any linked worktree — share the repo's
// common .git as their key; a non-repo cwd groups by its literal path. This
// copy exists so the bridge doesn't import the TUI package; it skips the
// TUI's on-disk case canonicalization, so at worst an oddly-cased worktree
// path renders as its own group.

// scopeKey returns the group key for a session cwd.
func scopeKey(cwd string) string {
	if cwd == "" {
		return ""
	}
	if c := webGitCommonDir(filepath.Clean(cwd)); c != "" {
		return c
	}
	return filepath.Clean(cwd)
}

// scopeName turns a scope key (a shared .git path or a bare cwd) into the
// short label shown on the group header: the repo root's basename when the
// key points at a .git, else the directory's basename.
func scopeName(key string) string {
	if key == "" {
		return "(unknown)"
	}
	if filepath.Base(key) == ".git" {
		if n := filepath.Base(filepath.Dir(key)); n != "" && n != "/" && n != "." {
			return n
		}
	}
	if n := filepath.Base(key); n != "" && n != "/" && n != "." {
		return n
	}
	return key
}

// webGitCommonDir resolves dir to its repository's shared .git directory, or
// "" outside a repo. Pure filesystem walk, no git subprocess: a .git
// directory is itself the common dir; a linked worktree's .git file points
// (via gitdir + commondir) back to the shared one.
func webGitCommonDir(dir string) string {
	var gitPath string
	for cur := dir; ; {
		p := filepath.Join(cur, ".git")
		if _, err := os.Stat(p); err == nil {
			gitPath = p
			break
		}
		parent := filepath.Dir(cur)
		if parent == cur {
			return ""
		}
		cur = parent
	}
	info, err := os.Stat(gitPath)
	if err != nil {
		return ""
	}
	if info.IsDir() {
		return resolveDir(gitPath) // main checkout (or bare repo)
	}
	// Linked worktree: ".git" is a file "gitdir: <worktree-gitdir>".
	data, err := os.ReadFile(gitPath)
	if err != nil {
		return ""
	}
	rest, ok := strings.CutPrefix(strings.TrimSpace(string(data)), "gitdir:")
	if !ok {
		return ""
	}
	gitDir := strings.TrimSpace(rest)
	if !filepath.IsAbs(gitDir) {
		gitDir = filepath.Join(dir, gitDir)
	}
	gitDir = filepath.Clean(gitDir)
	if data, err := os.ReadFile(filepath.Join(gitDir, "commondir")); err == nil {
		cd := strings.TrimSpace(string(data))
		if !filepath.IsAbs(cd) {
			cd = filepath.Join(gitDir, cd)
		}
		return resolveDir(cd)
	}
	return resolveDir(gitDir)
}

// resolveDir cleans p and resolves symlinks when possible so two routes to
// the same .git compare equal.
func resolveDir(p string) string {
	p = filepath.Clean(p)
	if r, err := filepath.EvalSymlinks(p); err == nil {
		return r
	}
	return p
}
