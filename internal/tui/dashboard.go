package tui

import (
	"bufio"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/charmbracelet/x/ansi"

	"codebridge/internal/config"
	"codebridge/internal/hook"
	"codebridge/internal/ipc"
	"codebridge/internal/task"
)

// DashAction is what the dashboard asks the caller to do after it exits.
type DashAction int

const (
	DashQuit DashAction = iota
)

// focusZone is which pane currently receives keystrokes: the sidebar (list
// navigation + dashboard commands) or the screen pane (forwarded to the
// session as raw input).
type focusZone int

const (
	focusSidebar focusZone = iota
	focusScreen
)

const dashRefreshInterval = 500 * time.Millisecond

type tickMsg struct{}
type sessionsMsg struct {
	sessions []ipc.SessionInfo
	tasks    []ipc.Task // backlog snapshot piggybacked on the list poll
	err      error
}

// spawnedMsg carries the id of a session just created via n/c so the next list
// refresh selects it and the screen pane takes focus.
type spawnedMsg struct{ id string }

// spawnMissingMsg fires when the binary the user asked to spawn isn't on PATH,
// so Update can surface a toast instead of silently no-op'ing.
type spawnMissingMsg struct{ bin string }

type toast struct {
	text  string
	level string // status key for coloring
	born  time.Time
	// sessionID, when non-empty, makes the toast sticky: it ignores TTL and
	// stays painted until the session's status moves out of `status`, the
	// session ends, or the user focuses its screen pane. status is the
	// triggering state ("needs_approval" / "waiting_user") so expireToasts
	// can tell when the prompt has been resolved.
	sessionID string
	status    string
}

const toastTTL = 6 * time.Second

type dashboardModel struct {
	// sessions is the full session list from the daemon — the sidebar is
	// globally scoped, so this is the master list every render and counter
	// reads from. visRows is the flat list the accordion actually renders
	// (scope headers interleaved with their child sessions when expanded) and
	// is the source of truth for cursor indexing.
	sessions []ipc.SessionInfo
	visRows  []visRow
	cursor   int // index into visRows
	errMsg   string
	w, h     int

	// currentScope is the scope key (git common dir, or cwd for non-repo) of
	// the directory cb was launched in. expanded[currentScope] is seeded true,
	// so the user's repo opens by default; every other scope starts collapsed
	// and stays that way until explicitly opened, so new scopes appearing in
	// later polls don't quietly steal sidebar real estate.
	currentScope string
	expanded     map[string]bool

	// launchCwd is the directory cb was invoked in. Worktrees share a scope
	// with the main checkout (so their sessions group together) but live in
	// different directories; new sessions spawned from the dashboard should
	// land in the worktree the user is actually working in, not whichever
	// in-scope session happens to be streamed. See spawnTargetCwd.
	launchCwd string

	// accordionMode controls whether the sidebar shows the multi-workspace
	// accordion (true, default) or a flat list of just the current scope's
	// sessions (false — the pre-accordion behavior). Toggled via prefix+a.
	accordionMode bool

	// selSession / selScope carry selection identity across refreshes so the
	// cursor lands on the same logical row when visRows rebuilds. On a session
	// row both are set (sessionID + its parent scope); on a scope row,
	// selSession is "" and selScope names the row.
	selSession string
	selScope   string

	repoCache     map[string]string
	worktreeCache map[string]bool // cwd -> "is a linked worktree" (cached for the same reason as repoCache)

	prev   map[string]string // last seen status per session id (for transition detection)
	toasts []toast
	spin   int

	renaming  bool   // capturing keystrokes into renameBuf instead of navigating
	renameID  string // session being renamed
	renameBuf string

	action     DashAction
	hooksOK    bool
	wantSelect string // pre-select this session id once the list loads

	focus  focusZone // sidebar navigation vs. screen input
	prefix bool      // ctrl+a pressed; next key is a command
	// menu, when true, keeps the prefix-commands panel open until the user
	// picks something or dismisses it. Triggered by prefix+h / prefix+?. While
	// it's open, keystrokes are routed through the prefix handler so users can
	// browse the hints and pick a command without re-pressing the prefix.
	menu bool

	// cfg is the live binding table loaded from ~/.config/cb/config.json on
	// startup and edited in place by the config modal; runAction dispatches
	// through cfg.Bindings so a rebind takes effect immediately.
	cfg *config.Config
	// Config-modal state. configOpen replaces the whole body when true;
	// configCursor indexes into configRows(); configCapture means the next
	// keystroke replaces the selected row's binding; configErr surfaces
	// validation messages (reserved key, duplicate binding, save failure).
	configOpen    bool
	configCursor  int
	configCapture bool
	configErr     string

	// Worktree/agent picker (prefix+w). A two-stage modal that owns the whole
	// viewport while open: pick a git worktree, then pick which agent binary to
	// launch in it. See worktree.go.
	wtOpen        bool
	wtStage       wtStage
	wtList        []worktreeInfo
	wtCursor      int
	wtChosenPath  string        // worktree carried from stage one into stage two
	wtAgents      []agentChoice // installed agents, computed when the picker opens
	wtAgentCursor int

	// Task backlog dialog (prefix+t). A modal listing the current workspace's
	// queued work; tasks can start agent sessions with their text prefilled and
	// then track that session live. See task.go.
	taskOpen        bool
	taskStage       taskStage
	taskStore       *task.Store
	taskRows        []taskRow // flattened section headers + tasks + notes
	taskCursor      int       // index into taskRows, always on a task row
	taskTitleBuf    string    // new-task title being typed
	taskDetailID    string    // task being edited in the detail stage
	taskEditTitle   bool      // detail stage: true = title field active, false = description
	taskTitleEdit   string
	taskDescEdit    string
	taskAgents      []agentChoice // installed agents, computed when the agent stage opens
	taskAgentCursor int
	taskStartID     string // task carried from the list into the agent stage
	taskPrefix      bool   // local prefix chord state while the dialog is open

	// live screen of the selected session (right pane). When focus==focusScreen,
	// keystrokes are forwarded to this session over the same connection.
	streamID string   // session currently streamed into the right pane
	screen   string   // latest rendered frame (kept across switches to avoid flicker)
	cursorX  int      // latest cursor column within the session screen
	cursorY  int      // latest cursor row within the session screen
	gone     bool     // streamed session ended
	conn     net.Conn // attach stream (nil when none)
	ch       chan previewMsg
	paneW    int // inner cols of the screen pane (the session is sized to this)
	paneH    int // inner rows available for the session screen

	// scrolling: scrollMode freezes the screen pane and shows scrollback; the
	// daemon renders the window at scrollOff lines up from the live bottom and
	// reports scrollMax (how far up it can go). sidebarTop is the first visible
	// session row so a long list scrolls to keep the cursor in view.
	scrollMode bool
	scrollOff  int
	scrollMax  int
	sidebarTop int

	// sidebarCache memoizes the rendered sidebar between frames. The screen pane
	// streams at up to 30fps, but the sidebar only changes on the 500ms list
	// poll, the spinner tick, or a cursor/focus/scope move — so we hash the
	// inputs that affect it (sidebarSignature) and reuse the last string when
	// they're unchanged, instead of re-styling every accordion row on every
	// screen frame.
	sidebarCache string
	sidebarSig   uint64
	sidebarValid bool

	// In-app drag selection over the screen pane. Anchored in virtual-line space
	// (scrollback index + visible-grid index) so the selection stays bound to its
	// content as the view autoscrolls. After release the highlight stays put and
	// the user yanks with prefix y; any new click clears it. While `selecting`,
	// selDragRow/Col remembers the mouse's raw position so a repeating tick can
	// keep autoscrolling while the cursor is parked at an edge (no motion = no
	// MouseMotion events, so we need the tick).
	selecting  bool
	selStart   selPos
	selEnd     selPos
	selDragRow int
	selDragCol int
}

// selPos is a position in the session's virtual buffer: line is an index into
// (scrollback + visible grid), col is a display column on that line.
type selPos struct{ line, col int }

// visRow is one rendered row of the sidebar accordion. Scope rows are
// expand/collapse headers (count = number of sessions in the group); session
// rows carry the underlying SessionInfo and only appear when their parent
// scope is expanded.
type visRow struct {
	isScope    bool
	scopeKey   string
	scopeCount int             // populated only when isScope
	expanded   bool            // populated only when isScope
	session    ipc.SessionInfo // populated only when !isScope
}

// previewMsg carries a frame from the screen-pane attach stream. id tags which
// session it came from so frames from a just-closed connection are ignored.
type previewMsg struct {
	id     string
	screen string
	cx     int
	cy     int
	gone   bool
	offset int
	max    int
}

// Dashboard runs the unified two-zone view: a session list on the left and the
// selected session's live screen on the right. selectID, if non-empty, is the
// session to highlight on entry. cwd is the directory cb was launched in; the
// sidebar is always globally scoped and groups sessions by their cwd-derived
// scope key (git common dir, else cwd) — the group containing cwd opens by
// default, every other group starts collapsed. showAll is accepted but no
// longer carries meaning (the sidebar is unconditionally global now); the
// param is kept so callers that pass --all still link cleanly.
func Dashboard(selectID, cwd string, showAll bool) (DashAction, error) {
	_ = showAll // sidebar is always global now; flag is a historical no-op
	common, root := deriveScope(cwd)
	currentScope := common
	if currentScope == "" {
		currentScope = root // launch dir when not in a git repo
	}
	cfg := config.Load()
	// Apply the on-disk prefix to the package-level var that prefixLabel /
	// handleKey read. SetPrefix is a no-op when CB_PREFIX is set, so the env
	// override still wins for users who haven't migrated to the config file.
	SetPrefix(cfg.Prefix)
	m := &dashboardModel{
		hooksOK:       hook.Installed(),
		prev:          map[string]string{},
		wantSelect:    selectID,
		currentScope:  currentScope,
		launchCwd:     cwd,
		expanded:      map[string]bool{currentScope: true},
		accordionMode: false,
		repoCache:     map[string]string{},
		worktreeCache: map[string]bool{},
		ch:            make(chan previewMsg, 64),
		cfg:           cfg,
		// The daemon owns the backlog now; this store is a read cache filled by
		// the list poll (and mutation replies), never written to disk here.
		taskStore: &task.Store{},
	}
	// Mouse wheel is captured (see View's MouseMode) so scrolling the screen pane
	// browses scrollback rather than the terminal turning the wheel into arrow
	// keys that leak into the session as history nav. Plain click+drag selects
	// text in-app (autoscrolls continuously while the cursor parks at an edge —
	// see selTickMsg) — that's what mouse capture costs us, since otherwise the
	// host terminal would handle drag-selection but couldn't autoscroll on
	// alt-screen. On mouse-release the selection is auto-copied to the system
	// clipboard via OSC52 (macOS terminals swallow cmd+c, so a key-driven copy
	// is unreliable); the highlight stays painted as a hint that the text
	// landed, ctrl+c / prefix y still re-copy on demand, and any new click
	// dismisses the highlight.
	// Holding Shift bypasses our capture so the host terminal's native
	// (no-autoscroll) selection still works for users on terminals where OSC52
	// is disabled. Scrollback is also browsable via the keyboard scroll mode
	// (prefix [). Alt-screen and mouse mode are requested via the View now (v2
	// moved terminal feature flags out of program options).
	p := tea.NewProgram(m)
	res, err := p.Run()
	if m.conn != nil {
		_ = m.conn.Close()
	}
	if err != nil {
		return DashQuit, err
	}
	final := res.(*dashboardModel)
	return final.action, nil
}

func (m *dashboardModel) Init() tea.Cmd {
	return tea.Batch(refreshCmd, tick(), m.waitFrame())
}

// waitFrame blocks on the shared screen channel and surfaces the next frame as
// a message. It's re-armed after each frame, so a single long-lived loop serves
// whichever session is currently selected.
func (m *dashboardModel) waitFrame() tea.Cmd {
	return func() tea.Msg { return <-m.ch }
}

// selectedID is the id of the session under the cursor — only meaningful
// when the cursor is on a session row. On a scope header (or with no rows
// at all), it returns "" so callers know there's no session to act on.
func (m *dashboardModel) selectedID() string {
	if r := m.currentRow(); r != nil && !r.isScope {
		return r.session.ID
	}
	return ""
}

// displayName is the label shown for a session: the user-assigned name if set,
// otherwise the basename of the directory it was started in (e.g. a session
// launched in ~/Projects/codebridge shows as "codebridge"), falling
// back to a short id.
func displayName(s ipc.SessionInfo) string {
	if s.Name != "" {
		return s.Name
	}
	if base := folderBase(s.Cwd); base != "" {
		return base
	}
	if len(s.ID) >= 8 {
		return s.ID[:8]
	}
	return s.ID
}

// folderBase returns the last path segment of a directory path.
func folderBase(dir string) string {
	dir = strings.TrimRight(dir, "/")
	if dir == "" {
		return ""
	}
	if i := strings.LastIndex(dir, "/"); i >= 0 {
		return dir[i+1:]
	}
	return dir
}

func (m *dashboardModel) sessionByID(id string) *ipc.SessionInfo {
	for i := range m.sessions {
		if m.sessions[i].ID == id {
			return &m.sessions[i]
		}
	}
	return nil
}

// scopeKeyOf is the accordion's group key for a session's cwd: the git common
// directory when the session sits in a repo (so the main checkout and every
// linked worktree of one repo collapse into one group), and the literal cwd
// otherwise (each non-repo directory becomes its own group). Resolution is
// memoized via repoCache so the 500ms poll doesn't re-walk the filesystem.
func (m *dashboardModel) scopeKeyOf(cwd string) string {
	if c := m.commonDirCached(cwd); c != "" {
		return c
	}
	return cwd
}

// commonDirCached resolves cwd to its git common dir, memoizing the (stable)
// result so the 500ms poll doesn't re-walk the filesystem for every session.
func (m *dashboardModel) commonDirCached(cwd string) string {
	if m.repoCache == nil {
		m.repoCache = map[string]string{}
	}
	if v, ok := m.repoCache[cwd]; ok {
		return v
	}
	v := gitCommonDir(cwd)
	m.repoCache[cwd] = v
	return v
}

// scopeLabel is the short header line under the title that says what the
// sidebar is currently showing: every workspace ("scope: all") in accordion
// mode, or just the launch repo's name in flat mode. Mirrors the toast that
// fires on toggle so the header reinforces the current mode at rest.
func (m *dashboardModel) scopeLabel() string {
	txt := "scope: all"
	if !m.accordionMode {
		name := scopeDisplayName(m.currentScope)
		if name == "" {
			name = "this workspace"
		}
		txt = "scope: " + name
	}
	return helpStyle.Render(truncate(txt, sidebarWidth-1))
}

// scopeDisplayName turns a scope key (a .git path, a worktree gitdir, or a
// bare cwd) into a one-word label for the accordion header. We use the
// basename of the repo root (parent of .git) when the key points into a
// gitdir; otherwise the basename of the cwd. Empty keys fall back to a
// placeholder so an unset cwd doesn't render a blank row.
func scopeDisplayName(key string) string {
	if key == "" {
		return "(unknown)"
	}
	if filepath.Base(key) == ".git" {
		if n := folderBase(filepath.Dir(key)); n != "" {
			return n
		}
	}
	if n := folderBase(key); n != "" {
		return n
	}
	return key
}

// isWorktreeCached reports whether cwd is inside a linked git worktree (as
// opposed to the main checkout). A linked worktree's nearest .git is a file
// pointing at a gitdir; the main checkout's nearest .git is a directory.
// Memoized so the 500ms list poll doesn't re-stat.
func (m *dashboardModel) isWorktreeCached(cwd string) bool {
	if m.worktreeCache == nil {
		m.worktreeCache = map[string]bool{}
	}
	if v, ok := m.worktreeCache[cwd]; ok {
		return v
	}
	v := isLinkedWorktree(cwd)
	m.worktreeCache[cwd] = v
	return v
}

// isLinkedWorktree walks up from dir to the nearest .git and reports whether
// it's a file (linked worktree) rather than a directory (main checkout or bare
// repo). Returns false when dir isn't in a git repo at all.
func isLinkedWorktree(dir string) bool {
	dir = filepath.Clean(dir)
	for cur := dir; ; {
		p := filepath.Join(cur, ".git")
		if info, err := os.Lstat(p); err == nil {
			return !info.IsDir()
		}
		parent := filepath.Dir(cur)
		if parent == cur {
			return false
		}
		cur = parent
	}
}

// rebuildRows recomputes the accordion's flat row list from m.sessions and
// the current expansion state, then restores the cursor to the same logical
// row it was on before (preferring the same session, falling back to the same
// scope header, then clamping). It's the single chokepoint every code path
// hits when the underlying data or the open/closed state changes: the daemon
// poll, scope toggles, and the explicit "expand current" jumps.
//
// Groups are ordered with currentScope pinned first (so the user's repo sits
// at the top regardless of session arrival order); other scopes sort by
// display name to keep ordering stable as new sessions appear.
func (m *dashboardModel) rebuildRows() {
	groups := map[string][]ipc.SessionInfo{}
	var keys []string
	for _, s := range m.sessions {
		k := m.scopeKeyOf(s.Cwd)
		if _, ok := groups[k]; !ok {
			keys = append(keys, k)
		}
		groups[k] = append(groups[k], s)
	}
	// currentScope is always shown — even when it has no sessions yet — so the
	// user can see "their" group as a closed header rather than wondering
	// whether the panel is empty.
	if _, ok := groups[m.currentScope]; !ok && m.currentScope != "" {
		groups[m.currentScope] = nil
		keys = append(keys, m.currentScope)
	}
	cur := m.currentScope
	sort.SliceStable(keys, func(i, j int) bool {
		if keys[i] == cur {
			return true
		}
		if keys[j] == cur {
			return false
		}
		return scopeDisplayName(keys[i]) < scopeDisplayName(keys[j])
	})

	rows := make([]visRow, 0, len(m.sessions)+len(keys))
	if m.accordionMode {
		for _, k := range keys {
			rows = append(rows, visRow{
				isScope:    true,
				scopeKey:   k,
				scopeCount: len(groups[k]),
				expanded:   m.expanded[k],
			})
			if m.expanded[k] {
				for _, s := range groups[k] {
					rows = append(rows, visRow{scopeKey: k, session: s})
				}
			}
		}
	} else {
		// Flat single-workspace view: just the current scope's sessions, no
		// headers — matches the pre-accordion sidebar.
		for _, s := range groups[m.currentScope] {
			rows = append(rows, visRow{scopeKey: m.currentScope, session: s})
		}
	}
	m.visRows = rows

	// Restore the cursor onto the same logical row (session if known, else
	// its parent scope). If neither survives, clamp to a valid index and
	// re-derive selection from whatever's now under the cursor.
	idx := -1
	if m.selSession != "" {
		for i, r := range rows {
			if !r.isScope && r.session.ID == m.selSession {
				idx = i
				break
			}
		}
	}
	if idx < 0 && m.selScope != "" {
		for i, r := range rows {
			if r.isScope && r.scopeKey == m.selScope {
				idx = i
				break
			}
		}
	}
	if idx < 0 {
		idx = m.cursor
	}
	if idx >= len(rows) {
		idx = len(rows) - 1
	}
	if idx < 0 {
		idx = 0
	}
	m.cursor = idx
	m.syncSelFromCursor()
}

// syncSelFromCursor copies the cursor's row identity into selSession/selScope
// so subsequent rebuilds can restore the same row. Called after every cursor
// move (keyboard, click, programmatic).
func (m *dashboardModel) syncSelFromCursor() {
	if m.cursor < 0 || m.cursor >= len(m.visRows) {
		m.selSession, m.selScope = "", ""
		return
	}
	r := m.visRows[m.cursor]
	m.selScope = r.scopeKey
	if r.isScope {
		m.selSession = ""
	} else {
		m.selSession = r.session.ID
	}
}

// currentRow returns the row under the cursor, or nil when there are no rows.
func (m *dashboardModel) currentRow() *visRow {
	if m.cursor < 0 || m.cursor >= len(m.visRows) {
		return nil
	}
	return &m.visRows[m.cursor]
}

// neighborInScope returns the id of the session row that should inherit the
// cursor when the session with the given id is removed: the previous sibling
// in the same scope group, or the next sibling if there is none before it.
// Returns "" when the target isn't present, has no siblings in the same scope,
// or accordion mode is off (the flat view collapses to a single group, so the
// natural index clamp in rebuildRows already does the right thing).
func (m *dashboardModel) neighborInScope(id string) string {
	if !m.accordionMode {
		return ""
	}
	idx := -1
	for i, r := range m.visRows {
		if !r.isScope && r.session.ID == id {
			idx = i
			break
		}
	}
	if idx < 0 {
		return ""
	}
	scope := m.visRows[idx].scopeKey
	for i := idx - 1; i >= 0; i-- {
		r := m.visRows[i]
		if r.isScope {
			break
		}
		if r.scopeKey == scope {
			return r.session.ID
		}
	}
	for i := idx + 1; i < len(m.visRows); i++ {
		r := m.visRows[i]
		if r.isScope || r.scopeKey != scope {
			break
		}
		return r.session.ID
	}
	return ""
}

// toggleScope flips a scope group's expanded flag and rebuilds the row list.
// The cursor stays on the same scope header (rebuildRows finds it by scopeKey).
func (m *dashboardModel) toggleScope(key string) {
	if m.expanded == nil {
		m.expanded = map[string]bool{}
	}
	m.expanded[key] = !m.expanded[key]
	// Cursor needs to anchor to the scope row through the rebuild, even if
	// it was on a child session that's about to disappear.
	m.selSession = ""
	m.selScope = key
	m.rebuildRows()
}

// setScopeExpanded forces a scope's expanded state. Used by left/right arrow
// affordances that should set, not toggle.
func (m *dashboardModel) setScopeExpanded(key string, expanded bool) {
	if m.expanded == nil {
		m.expanded = map[string]bool{}
	}
	if m.expanded[key] == expanded {
		return
	}
	m.expanded[key] = expanded
	m.selSession = ""
	m.selScope = key
	m.rebuildRows()
}

// pathWithin reports whether path is root itself or lives beneath it. Both
// sides are run through canonicalCase so a lowercase os.Getwd doesn't fail to
// match an uppercase on-disk path (or vice versa) on case-insensitive
// filesystems.
func pathWithin(root, path string) bool {
	if root == "" {
		return true
	}
	root = canonicalCase(filepath.Clean(root))
	path = canonicalCase(filepath.Clean(path))
	if path == root {
		return true
	}
	return strings.HasPrefix(path, root+string(filepath.Separator))
}

// deriveScope computes the session-list scope for the directory cb was launched
// in. common is the git common directory — the shared .git of the repo, which is
// the SAME for the main checkout and every linked worktree, so sessions in any
// worktree of one repo share a scope. root is a human-friendly directory for the
// header (and the non-repo fallback): the main worktree root in a repo, else the
// launch dir. When cwd isn't in a git repo, common is "" and the scope is the
// launch-dir subtree.
func deriveScope(cwd string) (common, root string) {
	if cwd == "" {
		return "", ""
	}
	cwd = filepath.Clean(cwd)
	common = gitCommonDir(cwd)
	if common == "" {
		return "", cwd
	}
	// The main worktree root is the parent of the shared .git directory.
	return common, filepath.Dir(common)
}

// gitCommonDir resolves dir to the absolute path of its repository's common
// directory (the shared .git), or "" when dir isn't inside a git repo. This is
// the key that ties a repo's main checkout and all its linked worktrees
// together. Pure filesystem resolution, no git subprocess: find the nearest
// .git; a .git directory is itself the common dir, while a worktree's .git file
// points (via its gitdir + commondir files) back to the shared .git.
func gitCommonDir(dir string) string {
	dir = filepath.Clean(dir)
	var gitPath string
	for cur := dir; ; {
		p := filepath.Join(cur, ".git")
		if _, err := os.Stat(p); err == nil {
			gitPath = p
			break
		}
		parent := filepath.Dir(cur)
		if parent == cur {
			return "" // reached the filesystem root without finding .git
		}
		cur = parent
	}
	info, err := os.Stat(gitPath)
	if err != nil {
		return ""
	}
	if info.IsDir() {
		return canonicalDir(gitPath) // main checkout (or bare repo)
	}
	// Linked worktree: ".git" is a file "gitdir: <path-to-worktree-gitdir>".
	gitDir := readGitdir(gitPath)
	if gitDir == "" {
		return ""
	}
	// The worktree's gitdir holds a commondir file pointing at the shared .git.
	if data, err := os.ReadFile(filepath.Join(gitDir, "commondir")); err == nil {
		cd := strings.TrimSpace(string(data))
		if !filepath.IsAbs(cd) {
			cd = filepath.Join(gitDir, cd)
		}
		return canonicalDir(cd)
	}
	return canonicalDir(gitDir)
}

// readGitdir reads a worktree's ".git" file and returns the absolute path named
// by its "gitdir:" line, or "" if it can't be parsed.
func readGitdir(gitFile string) string {
	data, err := os.ReadFile(gitFile)
	if err != nil {
		return ""
	}
	rest, ok := strings.CutPrefix(strings.TrimSpace(string(data)), "gitdir:")
	if !ok {
		return ""
	}
	gd := strings.TrimSpace(rest)
	if !filepath.IsAbs(gd) {
		gd = filepath.Join(filepath.Dir(gitFile), gd)
	}
	return filepath.Clean(gd)
}

// canonicalDir cleans p and resolves symlinks when it can, so two paths reaching
// the same .git compare equal regardless of how they were reached. On
// case-insensitive filesystems (macOS APFS/HFS+) it also asks the kernel for
// the on-disk case, because EvalSymlinks alone preserves whatever case the
// caller typed — so `git worktree add` recording an uppercase gitdir and a
// later `cd lowercase` produce two paths to the same .git that don't compare
// equal as strings.
func canonicalDir(p string) string {
	p = filepath.Clean(p)
	if resolved, err := filepath.EvalSymlinks(p); err == nil {
		p = resolved
	}
	return canonicalCase(p)
}

// syncStream ensures the screen pane is attached to the currently selected
// session: if the selection changed, it tears down the old stream and opens a
// new one. The session is resized to the pane so its render fits. The old
// screen is intentionally kept on display until the first frame of the new
// session arrives, so switching sessions doesn't flash a blank pane.
func (m *dashboardModel) syncStream() {
	id := m.selectedID()
	// Cursor parked on a scope header? Keep streaming the last attached
	// session so scrolling through accordion headers doesn't kill the live
	// screen pane. Only an explicit move onto a *different* session row
	// (or a session ending) should swap the stream.
	if id == "" && m.streamID != "" {
		return
	}
	if id == m.streamID && (id == "" || m.conn != nil) {
		return
	}
	if m.conn != nil {
		_ = m.conn.Close()
		m.conn = nil
	}
	m.streamID = id
	m.gone = false
	// Scroll position and any in-progress drag selection are per-session; reset
	// to live when switching so we don't anchor a selection in another session's
	// virtual-line space.
	m.scrollMode = false
	m.scrollOff = 0
	m.scrollMax = 0
	m.selecting = false
	m.selStart, m.selEnd = selPos{}, selPos{}
	if id == "" {
		m.screen = ""
		return
	}
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return
	}
	req := ipc.Request{Type: "attach", ID: id}
	if m.paneW > 0 && m.paneH > 0 {
		req.Rows, req.Cols = m.paneH, m.paneW
	}
	if err := ipc.WriteJSON(conn, req); err != nil {
		_ = conn.Close()
		return
	}
	m.conn = conn
	go previewReadLoop(id, conn, m.ch)
}

// sendInput forwards raw bytes to the streamed session (used when the screen
// pane has focus).
func (m *dashboardModel) sendInput(b []byte) {
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "input", Data: base64.StdEncoding.EncodeToString(b)})
	}
}

// sendInterrupt records user-driven turn cancellation. Claude Code and Codex do
// not always emit a Stop/Notification hook when Esc or Ctrl-C interrupts a turn,
// so the daemon needs a client-side signal to clear the working spinner.
func (m *dashboardModel) sendInterrupt() {
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "interrupt"})
	}
}

// sendPaste forwards pasted text as a single paste event so the daemon can
// deliver it to the session as one bracketed paste.
func (m *dashboardModel) sendPaste(text string) {
	if m.conn != nil && text != "" {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "paste", Data: base64.StdEncoding.EncodeToString([]byte(text))})
	}
}

// previewReadLoop pumps a screen attach stream into ch until the connection
// closes (which happens when we switch away or the session ends).
func previewReadLoop(id string, conn net.Conn, ch chan previewMsg) {
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	for sc.Scan() {
		var d ipc.StreamDown
		if json.Unmarshal(sc.Bytes(), &d) != nil {
			continue
		}
		if d.Type == "gone" {
			ch <- previewMsg{id: id, gone: true}
			return
		}
		ch <- previewMsg{id: id, screen: d.Screen, cx: d.CursorX, cy: d.CursorY, offset: d.Offset, max: d.MaxOffset}
	}
}

const sidebarWidth = 22

// chromeRows is the count of rows renderLive draws below the body block.
// Chrome (toasts, the hooks-not-installed banner, the rename input) is now
// painted as an overlay on top of the bottom rows of the body, so the sidebar
// and screen pane fill the whole terminal and the overlay never changes the
// pane size. Kept as a method so the layout-fits-terminal test can address
// "the last body row" the same way whether or not chrome is overlaid.
func (m *dashboardModel) chromeRows() int { return 0 }

// relayoutStream recomputes the screen pane size from the window size and, if
// it changed, tells the currently streamed session to resize to match.
func (m *dashboardModel) relayoutStream() {
	// width: window minus sidebar, the pane's left border, and its left padding.
	innerW := m.w - sidebarWidth - 3
	// height: the full pane height — everything left after the surrounding chrome.
	innerH := m.h - m.chromeRows()
	if innerW < 1 {
		innerW = 1
	}
	if innerH < 1 {
		innerH = 1
	}
	if innerW == m.paneW && innerH == m.paneH {
		return
	}
	m.paneW, m.paneH = innerW, innerH
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "resize", Rows: innerH, Cols: innerW})
	}
}

func refreshCmd() tea.Msg {
	resp, err := ipc.Send(ipc.Request{Type: "list"})
	return sessionsMsg{sessions: resp.Sessions, tasks: resp.Tasks, err: err}
}

func tick() tea.Cmd {
	return tea.Tick(dashRefreshInterval, func(time.Time) tea.Msg { return tickMsg{} })
}

// spawnCmd starts a new session running bin (e.g. "claude" or "codex") in cwd.
// An empty cwd falls back to the cb process's working directory. The session is
// spawned at the current screen pane size so the child paints itself once at
// the right width — otherwise it would paint at the daemon's default size and
// then repaint after the attach resize, leaving overlapping/garbled output
// (e.g. "Claude CodClaude Code"). On success it reports the new session's id
// so the dashboard can select and focus it.
func (m *dashboardModel) spawnCmd(bin, cwd string) tea.Cmd {
	// Check the binary up-front: the daemon would silently fail to start a child
	// that isn't on PATH (no error surfaces over IPC), so the user would just see
	// the list refresh with no new session and no explanation. The client's PATH
	// tracks the user's shell, which is where they'd install claude/codex.
	if _, err := exec.LookPath(bin); err != nil {
		return func() tea.Msg { return spawnMissingMsg{bin: bin} }
	}
	rows, cols := m.paneH, m.paneW
	return func() tea.Msg {
		if cwd == "" {
			cwd, _ = os.Getwd()
		}
		req := ipc.Request{Type: "spawn", Argv: []string{bin}, Cwd: cwd}
		if rows > 0 && cols > 0 {
			req.Rows, req.Cols = rows, cols
		}
		resp, err := ipc.Send(req)
		if err != nil || !resp.OK {
			return refreshCmd()
		}
		return spawnedMsg{id: resp.ID}
	}
}

// activeSessionCwd returns the cwd of the session currently shown in the
// screen pane. Empty when no session is being viewed.
func (m *dashboardModel) activeSessionCwd() string {
	if m.streamID == "" {
		return ""
	}
	if s := m.sessionByID(m.streamID); s != nil {
		return s.Cwd
	}
	return ""
}

// spawnTargetCwd picks the cwd a new session created via prefix+n / prefix+c
// should land in. Worktrees share a scope with their main checkout, so the
// streamed session's cwd is often a sibling directory rather than where the
// user actually launched cb; prefer the launch cwd whenever it lives in the
// same scope (covers main↔worktree and worktree↔worktree). Only when the
// active session has been navigated into a different scope do we fall back
// to its cwd — there, the user has explicitly moved off their launch repo
// and "spawn next to the session I'm looking at" is the better default.
func (m *dashboardModel) spawnTargetCwd() string {
	active := m.activeSessionCwd()
	if m.launchCwd != "" {
		if active == "" || m.scopeKeyOf(active) == m.scopeKeyOf(m.launchCwd) {
			return m.launchCwd
		}
	}
	return active
}

func killCmd(id string) tea.Cmd {
	return func() tea.Msg {
		_, _ = ipc.Send(ipc.Request{Type: "kill", ID: id})
		return refreshCmd()
	}
}

func renameCmd(id, name string) tea.Cmd {
	return func() tea.Msg {
		_, _ = ipc.Send(ipc.Request{Type: "rename", ID: id, Name: name})
		return refreshCmd()
	}
}

func (m *dashboardModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.FocusMsg:
		// The native TUI owns the canonical size. Reclaim it when the host
		// terminal regains focus; Session.Resize suppresses identical requests.
		if m.conn != nil && m.paneW > 0 && m.paneH > 0 {
			_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "resize", Rows: m.paneH, Cols: m.paneW})
		}
		return m, nil

	case tea.WindowSizeMsg:
		m.w, m.h = msg.Width, msg.Height
		m.relayoutStream()
		return m, nil

	case tickMsg:
		m.spin++
		m.expireToasts()
		return m, tea.Batch(refreshCmd, tick())

	case spawnedMsg:
		// Select the just-created session and drop straight into it so you can
		// start typing; the refresh picks it up and syncStream attaches.
		m.wantSelect = msg.id
		m.focusScreenPane()
		return m, refreshCmd

	case spawnMissingMsg:
		m.pushToast(fmt.Sprintf("✗ %s not installed — check your PATH", msg.bin), "needs_approval")
		return m, nil

	case taskSpawnedMsg:
		// A task's agent session just spawned (the daemon already linked the task
		// to it). Refresh the backlog cache, then select and focus the new
		// session like spawnedMsg does.
		m.applyTasks(msg.tasks)
		m.wantSelect = msg.sessionID
		m.focusScreenPane()
		return m, refreshCmd

	case tasksMsg:
		// A backlog mutation (add/edit/status/delete) replied with the fresh
		// list; adopt it immediately rather than waiting on the next poll.
		if msg.err != nil {
			m.pushToast("✗ tasks: "+msg.err.Error(), "needs_approval")
			return m, nil
		}
		m.applyTasks(msg.tasks)
		if msg.selectID != "" {
			m.selectTaskRow(msg.selectID)
		}
		return m, nil

	case previewMsg:
		if msg.id == m.streamID {
			if msg.gone {
				m.gone = true
				m.focus = focusSidebar // can't type into a dead session
				m.scrollMode = false
			} else {
				// Lock the view while a selection is held or the user is
				// browsing scrollback: every new line the session emits grows
				// scrollMax by one, which would drift the rendered window up
				// toward the new bottom within milliseconds for an actively
				// streaming Claude. Bumping scrollOff by the same delta keeps
				// the content pinned where the user parked it. From a selection
				// the user resumes live by clearing the selection (any new
				// click clears it) and then scrolling down or pressing G in
				// scroll mode; from scroll mode they resume live by pressing G
				// (which sets scrollOff to 0 so this branch stops pinning) or
				// exiting scroll mode entirely.
				pin := m.selStart != m.selEnd || (m.scrollMode && m.scrollOff > 0)
				if pin && msg.max > m.scrollMax {
					m.scrollOff += msg.max - m.scrollMax
					if m.scrollOff > msg.max {
						m.scrollOff = msg.max
					}
					m.sendScroll()
					if m.scrollOff > 0 {
						m.scrollMode = true
					}
				}
				m.scrollMax = msg.max
				if m.scrollOff > m.scrollMax {
					m.scrollOff = m.scrollMax
					m.sendScroll()
				}
				// Only apply the frame when it was rendered at our current
				// scroll position. Right after a pin bump (or any user-driven
				// scroll) the daemon's in-flight frames carry the *old*
				// offset; rendering them would shift the pane by the delta and
				// snap back on the next frame — that's the flicker. Skipping
				// them keeps the previously-rendered pinned frame on display
				// until the daemon catches up and emits a frame at the new
				// offset (one ~33ms tick later).
				if msg.offset == m.scrollOff {
					m.screen = msg.screen
					m.cursorX = msg.cx
					m.cursorY = msg.cy
				}
			}
		}
		return m, m.waitFrame()

	case sessionsMsg:
		if msg.err != nil {
			m.errMsg = msg.err.Error()
		} else {
			m.errMsg = ""
			m.sessions = msg.sessions
			// Sidebar is globally scoped — toasts fire across every session
			// the daemon knows about, not just the visible accordion rows
			// (collapsing a group shouldn't silence notifications from it).
			m.detectTransitions(m.sessions)
			// The daemon reconciles the backlog now (pausing tasks whose session
			// ended, harvesting resume ids); we just refresh the read cache.
			m.applyTasks(msg.tasks)
		}
		// On the first poll that includes wantSelect, ensure its parent scope
		// is open and stamp the selection so rebuildRows lands the cursor on
		// the requested session row.
		if m.wantSelect != "" {
			for _, s := range m.sessions {
				if s.ID == m.wantSelect {
					key := m.scopeKeyOf(s.Cwd)
					if m.expanded == nil {
						m.expanded = map[string]bool{}
					}
					m.expanded[key] = true
					m.selScope = key
					m.selSession = s.ID
					break
				}
			}
			m.wantSelect = ""
		}
		m.rebuildRows()
		m.syncStream()
		return m, nil

	case tea.PasteMsg:
		// Bracketed paste is its own message in v2. Forward it to the focused
		// session as a single paste so the daemon wraps it in paste markers.
		if m.focus == focusScreen && !m.scrollMode {
			m.sendPaste(msg.Content)
		}
		return m, nil

	case tea.MouseWheelMsg:
		return m.handleWheel(msg)

	case tea.MouseClickMsg:
		return m.handleMouseClick(msg)
	case tea.MouseMotionMsg:
		return m.handleMouseMotion(msg)
	case tea.MouseReleaseMsg:
		return m.handleMouseRelease(msg)

	case selTickMsg:
		// While the mouse is held at an edge during a drag, the host terminal
		// emits no motion events — so we tick on our own and step the scroll on
		// each fire, keeping the selection's end anchored to the row currently
		// under the cursor. The loop dies on release (selecting == false).
		if !m.selecting {
			return m, nil
		}
		switch {
		case m.selDragRow <= 0 && m.scrollOff < m.scrollMax:
			m.edgeAutoscroll(0)
			m.selEnd = selPos{line: m.vLine(0), col: m.selDragCol}
		case m.selDragRow >= m.paneH-1 && m.scrollOff > 0:
			m.edgeAutoscroll(m.paneH - 1)
			m.selEnd = selPos{line: m.vLine(m.paneH - 1), col: m.selDragCol}
		}
		return m, selTick()

	case extractedMsg:
		// Extraction came back from the daemon — push it to the system clipboard
		// and flash a toast so the user knows it landed.
		if msg.text != "" {
			m.pushToast(fmt.Sprintf("⎘ copied %d chars", len(msg.text)), "working")
			return m, tea.SetClipboard(msg.text)
		}
		return m, nil

	case tea.KeyPressMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

// extractedMsg carries the text returned by the daemon's extract RPC so we can
// hand it to tea.SetClipboard from the main Update goroutine.
type extractedMsg struct{ text string }

// extractCmd asks the daemon for the plain text of the selected range. start
// and end must already be normalized (start <= end in virtual-line / col order).
func extractCmd(id string, start, end selPos) tea.Cmd {
	return func() tea.Msg {
		resp, err := ipc.Send(ipc.Request{
			Type:      "extract",
			ID:        id,
			LineStart: start.line,
			LineEnd:   end.line,
			ColStart:  start.col,
			ColEnd:    end.col,
		})
		if err != nil || !resp.OK {
			return extractedMsg{}
		}
		return extractedMsg{text: resp.Text}
	}
}

// paneCellAt converts a terminal-space mouse coordinate to the cell inside the
// screen pane's content area: (col, row) in [0, paneW) × [0, paneH). It also
// reports whether the click landed inside the pane at all. The pane begins one
// column for the left border plus one column of left padding to the right of
// the sidebar; rows start at the top of the View.
func (m *dashboardModel) paneCellAt(x, y int) (col, row int, inside bool) {
	col = x - sidebarWidth - 2
	row = y
	inside = col >= 0 && col < m.paneW && row >= 0 && row < m.paneH
	return
}

// vLine maps a visual row in the current frame to its index in the virtual
// buffer (scrollback length minus current scroll offset, plus the row). This
// is the anchor we record so a selection survives autoscrolling: as the offset
// changes, the visual row of the same virtual line shifts but the virtual line
// itself is fixed.
func (m *dashboardModel) vLine(row int) int { return row + m.scrollMax - m.scrollOff }

// edgeAutoscroll bumps the scroll offset by one line. Caller picks the
// direction (top edge → scroll up to expose older content; bottom edge → scroll
// down toward live). Stays a no-op when we're already at the bound.
func (m *dashboardModel) edgeAutoscroll(row int) {
	switch {
	case row <= 0 && m.scrollOff < m.scrollMax:
		if !m.scrollMode {
			m.scrollMode = true
		}
		m.scrollOff++
		m.sendScroll()
	case row >= m.paneH-1 && m.scrollOff > 0:
		m.scrollOff--
		m.sendScroll()
		if m.scrollOff == 0 {
			m.scrollMode = false
		}
	}
}

// selTickMsg fires the in-progress drag's continuous edge-autoscroll. See the
// selTickMsg handler in Update for why a separate tick is needed (held-still
// drags emit no motion events).
type selTickMsg struct{}

// selTickInterval is how often we re-check the cursor's row during a drag. Too
// fast (10–20ms) and content blurs past unselectably; too slow and the edge
// feels sticky. 60ms ≈ 16 lines/s, which matches the feel of native macOS
// drag-scroll in Terminal.app.
const selTickInterval = 60 * time.Millisecond

func selTick() tea.Cmd {
	return tea.Tick(selTickInterval, func(time.Time) tea.Msg { return selTickMsg{} })
}

// handleMouseClick routes a plain left click. Sticky toasts (rendered at the
// bottom-left as their own rows) are clickable hit targets — a click in one
// jumps straight to its session — and take priority over the drag-selection
// path so a toast resting over the screen-pane area doesn't get hijacked.
// Clicks inside the screen pane otherwise start an in-app drag selection;
// other buttons / modifiers are left to the host terminal.
func (m *dashboardModel) handleMouseClick(msg tea.MouseClickMsg) (tea.Model, tea.Cmd) {
	e := msg.Mouse()
	if e.Button != tea.MouseLeft || e.Mod != 0 {
		return m, nil
	}
	if id := m.toastSessionAt(e.X, e.Y); id != "" {
		m.focusSession(id)
		return m, nil
	}
	m.selStart, m.selEnd = selPos{}, selPos{}
	col, row, inside := m.paneCellAt(e.X, e.Y)
	if !inside || m.streamID == "" || m.gone {
		return m, nil
	}
	// A click inside the screen pane focuses it, so subsequent keystrokes flow
	// to the session without needing the prefix chord. focusScreenPane also
	// drops the sticky toast (if any) for the now-visible session.
	m.focusScreenPane()
	pos := selPos{line: m.vLine(row), col: col}
	m.selecting = true
	m.selStart, m.selEnd = pos, pos
	m.selDragRow, m.selDragCol = row, col
	return m, selTick()
}

// handleMouseMotion updates the end of an active drag and stores the raw row
// for the autoscroll tick (we want unclamped row so the tick can tell the
// cursor is past the edge, not merely at it). Without a live drag, motion
// events are ignored.
func (m *dashboardModel) handleMouseMotion(msg tea.MouseMotionMsg) (tea.Model, tea.Cmd) {
	if !m.selecting {
		return m, nil
	}
	e := msg.Mouse()
	col := e.X - sidebarWidth - 2
	row := e.Y
	if col < 0 {
		col = 0
	}
	if col > m.paneW {
		col = m.paneW
	}
	m.selDragRow, m.selDragCol = row, col
	if row < 0 {
		row = 0
	}
	if row > m.paneH-1 {
		row = m.paneH - 1
	}
	m.selEnd = selPos{line: m.vLine(row), col: col}
	return m, nil
}

// handleMouseRelease ends the drag, keeps the highlight painted, and pushes
// the selected text onto the system clipboard via OSC52 right away. macOS
// terminals almost always swallow cmd+c, and ctrl+c is reserved as the SIGINT
// key for the focused session, so auto-copy-on-release is the only ergonomic
// path: drag, release, paste anywhere. prefix+y is the explicit re-copy. A
// no-drag click — start == end — clears the state immediately so a tap doesn't
// leave a stale zero-width selection lingering on the model.
func (m *dashboardModel) handleMouseRelease(msg tea.MouseReleaseMsg) (tea.Model, tea.Cmd) {
	if !m.selecting {
		return m, nil
	}
	m.selecting = false
	if m.selStart == m.selEnd {
		m.selStart, m.selEnd = selPos{}, selPos{}
		return m, nil
	}
	if m.streamID == "" {
		return m, nil
	}
	start, end := normalizeSel(m.selStart, m.selEnd)
	return m, extractCmd(m.streamID, start, end)
}

// normalizeSel returns (a, b) in forward reading order: earlier line first,
// and on the same line the earlier column first. Without this a backwards drag
// (right-to-left or bottom-to-top) would produce an empty extraction.
func normalizeSel(a, b selPos) (selPos, selPos) {
	if a.line < b.line || (a.line == b.line && a.col <= b.col) {
		return a, b
	}
	return b, a
}

// isForwardedCmdC reports whether a terminal sent the macOS copy chord through
// to the application. Most macOS terminals intercept Cmd+C themselves, but
// Kitty-keyboard-capable terminals can surface it as Super+C or Meta+C.
func isForwardedCmdC(msg tea.KeyPressMsg) bool {
	if msg.Mod&(tea.ModSuper|tea.ModMeta) == 0 {
		return false
	}
	return msg.Code == 'c' || msg.Code == 'C'
}

// handleKey routes a keystroke through the rename prompt, the ctrl+a prefix,
// and then either the screen pane (forwarded as input) or the sidebar.
func (m *dashboardModel) handleKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	// The config modal owns every keystroke while open: nothing else dispatches
	// (not even ctrl+c, since that's "close the modal" inside the menu).
	if m.configOpen {
		return m.handleConfigKey(msg)
	}
	// The worktree/agent picker is a full-viewport modal too: it owns every
	// keystroke while open, ahead of the prefix layer and screen forwarding.
	if m.wtOpen {
		return m.handleWorktreeKey(msg)
	}
	// So is the task backlog dialog (it runs its own local prefix layer).
	if m.taskOpen {
		return m.handleTaskKey(msg)
	}
	if m.renaming {
		return m.updateRename(msg)
	}
	// The prefix is honored from any mode, so ctrl+a q / ctrl+a [ always work.
	// While the sticky menu is open, keystrokes route through the same handler
	// so users can pick a command from the visible hints; handlePrefix is
	// responsible for closing the menu when a command runs or esc is pressed.
	if m.prefix || m.menu {
		m.prefix = false
		return m.handlePrefix(msg)
	}
	if msg.String() == prefixKeyName {
		m.prefix = true
		return m, nil
	}
	// Copy on selection is auto-fired from handleMouseRelease (drag → release →
	// clipboard). If a terminal forwards Cmd+C via Kitty keyboard reporting,
	// honor it as a re-copy of the held highlight. Ctrl+C is intentionally NOT a
	// copy shortcut here: it stays SIGINT for the focused session.
	if !m.selecting && m.streamID != "" && m.selStart != m.selEnd && isForwardedCmdC(msg) {
		start, end := normalizeSel(m.selStart, m.selEnd)
		return m, extractCmd(m.streamID, start, end)
	}
	if m.scrollMode {
		return m.handleScrollKey(msg)
	}
	if m.focus == focusScreen {
		// Bracketed paste arrives as a separate tea.PasteMsg (handled in Update),
		// so here we only forward genuine key presses as raw bytes.
		if b := keyToBytes(msg); b != nil {
			m.sendInput(b)
			if isInterruptKey(msg) {
				m.sendInterrupt()
			}
		}
		return m, nil
	}
	return m.handleSidebarKey(msg)
}

func isInterruptKey(msg tea.KeyPressMsg) bool {
	return msg.Code == tea.KeyEscape || (msg.Code == 'c' && msg.Mod&tea.ModCtrl != 0)
}

// handlePrefix handles the key following the prefix chord. System keys
// (hints toggle, sidebar/screen focus shortcuts, esc) are reserved here and
// don't go through the rebinding table — see config.ReservedKeys for why.
// Everything else is dispatched through cfg.Bindings so rebinds in the
// config modal take effect immediately.
//
// Also entered while the sticky hints panel is open; any non-toggle key
// closes the menu after dispatching so the hints don't linger over what
// the user just did.
func (m *dashboardModel) handlePrefix(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	s := msg.String()
	switch s {
	case "?":
		m.menu = !m.menu
		return m, nil
	case "esc":
		m.menu = false
		return m, nil
	case "h", "left":
		m.menu = false
		m.focus = focusSidebar
		return m, nil
	case "right":
		m.menu = false
		if m.streamID != "" && !m.gone {
			m.focusScreenPane()
		}
		return m, nil
	}
	// Bound action? cfg.Bindings is the source of truth; rebinds at runtime
	// flow straight through this lookup with no extra refresh step.
	if a := m.actionForKey(s); a != "" {
		m.menu = false
		return m.runAction(a)
	}
	// Unrecognized key — close the menu so the panel doesn't linger.
	m.menu = false
	return m, nil
}

// actionForKey returns the action id bound to key, or "" when nothing is
// bound. The capture path's conflict guard keeps bindings unique, so the
// first match is also the only match.
func (m *dashboardModel) actionForKey(key string) string {
	if m.cfg == nil {
		return ""
	}
	for action, k := range m.cfg.Bindings {
		if k == key {
			return action
		}
	}
	return ""
}

// keyForAction returns the current binding for action, or "" when unknown.
// Used by the help text / empty-state copy to show live bindings rather
// than hardcoded letters that may not match what the user pressed.
func (m *dashboardModel) keyForAction(action string) string {
	if m.cfg == nil {
		return ""
	}
	return m.cfg.Bindings[action]
}

// runAction is the dispatch table for everything reachable from the prefix
// layer. Adding a new prefix command means adding an entry to config.Actions
// and a case here.
func (m *dashboardModel) runAction(action string) (tea.Model, tea.Cmd) {
	switch action {
	case "focus_screen":
		if m.streamID != "" && !m.gone {
			m.focusScreenPane()
		}
	case "scroll":
		if m.scrollMode {
			m.exitScroll()
		} else {
			m.enterScroll()
		}
	case "newline":
		// Inject a newline into the focused session without submitting. We send
		// it as a paste so it works on terminals that can't distinguish
		// shift+enter from enter.
		if m.streamID != "" && !m.gone {
			m.sendPaste("\n")
		}
	case "scope_toggle":
		// Toggle workspace-accordion mode. When on (default), the sidebar
		// shows every workspace as a collapsible accordion. When off, it
		// shrinks to just this-repo's sessions as a flat list — the
		// pre-accordion behavior.
		m.accordionMode = !m.accordionMode
		if m.accordionMode {
			// Returning to the accordion view: re-open the launch scope so
			// the sessions we were just looking at don't vanish behind a
			// collapsed header.
			if m.expanded == nil {
				m.expanded = map[string]bool{}
			}
			m.expanded[m.currentScope] = true
		} else {
			// Flat mode mutes notifications from other workspaces — drop any
			// sticky toasts already showing for sessions that are about to
			// fall out of scope so they don't linger over the new view.
			m.dropOutOfScopeToasts()
		}
		m.rebuildRows()
	case "new_claude":
		return m, m.spawnCmd("claude", m.spawnTargetCwd())
	case "new_codex":
		return m, m.spawnCmd("codex", m.spawnTargetCwd())
	case "new_worktree":
		// Two-stage picker: choose a git worktree, then the agent to launch in
		// it. Opening is synchronous (a fast local `git worktree list`).
		m.openWorktreePicker()
	case "task_backlog":
		m.openTaskBacklog()
	case "quit":
		m.action = DashQuit
		return m, tea.Quit
	case "kill":
		if m.streamID != "" {
			id := m.streamID
			m.focus = focusSidebar
			// Pre-stamp the cursor target so the post-kill rebuild lands on
			// the previous sibling session in the same scope group rather
			// than falling through to the scope header (rebuildRows' default
			// when selSession can't be found). Prefer previous sibling, then
			// next sibling; if the killed session was the only one in its
			// group, leave the targets alone so the cursor sticks to the
			// header (the current behavior for that case).
			if nb := m.neighborInScope(id); nb != "" {
				m.selSession = nb
			}
			return m, killCmd(id)
		}
	case "rename":
		// Prefer the session under the cursor; fall back to whatever the
		// screen pane is showing when the cursor is parked on a scope row.
		var target *ipc.SessionInfo
		if r := m.currentRow(); r != nil && !r.isScope {
			target = m.sessionByID(r.session.ID)
		} else if m.streamID != "" {
			target = m.sessionByID(m.streamID)
		}
		if target != nil {
			m.renaming = true
			m.renameID = target.ID
			m.renameBuf = displayName(*target)
		}
	case "jump_pending":
		// Jump to whichever session has the freshest sticky toast: a
		// needs_approval flag if any exists (those are most urgent), else
		// the most recent turn-complete (waiting_user). Matches what the
		// user sees in the notification stack.
		latest := latestAttention(m.sessions)
		if latest != "" {
			// Make sure the target session's group is open before pointing
			// the cursor at it; selScope/selSession let rebuildRows land
			// the cursor on the right row.
			for _, s := range m.sessions {
				if s.ID != latest {
					continue
				}
				key := m.scopeKeyOf(s.Cwd)
				// Flat (single-workspace) mode can't show sessions from
				// other scopes — flip back into accordion mode when the
				// jump target lives outside the launch workspace.
				if !m.accordionMode && key != m.currentScope {
					m.accordionMode = true
				}
				if !m.expanded[key] {
					m.setScopeExpanded(key, true)
				}
				m.selScope = key
				m.selSession = latest
				m.rebuildRows()
				m.syncStream()
				m.focusScreenPane()
				break
			}
		}
	case "yank":
		// Hand the held drag selection to the daemon for extraction. Clipboard
		// write happens in the extractedMsg handler (main goroutine). Only
		// fires after the mouse release so the range isn't still-updating.
		if !m.selecting && m.streamID != "" && m.selStart != m.selEnd {
			start, end := normalizeSel(m.selStart, m.selEnd)
			return m, extractCmd(m.streamID, start, end)
		}
	case "config":
		m.openConfig()
	}
	return m, nil
}

// scrollPage is roughly one screenful, used for pgup/pgdn.
func (m *dashboardModel) scrollPage() int { return maxInt(m.paneH-1, 1) }

// enterScroll freezes the screen pane and switches into scrollback browsing.
// It's a no-op when there's no live session to scroll.
func (m *dashboardModel) enterScroll() {
	if m.streamID == "" || m.gone || m.conn == nil {
		return
	}
	m.scrollMode = true
}

// exitScroll returns the screen pane to following the live bottom. It also
// drops any held text selection: the selection is a pin trigger (see the
// previewMsg handler), so leaving it set would let the next typed/pasted line
// re-bump scrollOff and yank the view back up — defeating the jump-to-live the
// caller just asked for.
func (m *dashboardModel) exitScroll() {
	m.scrollMode = false
	m.scrollOff = 0
	m.selecting = false
	m.selStart, m.selEnd = selPos{}, selPos{}
	m.sendScroll()
}

// scrollBy moves the scroll position by delta lines (positive = toward older
// output), clamps to the daemon-reported bounds, and pushes the new offset.
func (m *dashboardModel) scrollBy(delta int) {
	m.scrollOff += delta
	if m.scrollOff < 0 {
		m.scrollOff = 0
	}
	if m.scrollOff > m.scrollMax {
		m.scrollOff = m.scrollMax
	}
	m.sendScroll()
}

// sendScroll tells the daemon which scrollback window to render.
func (m *dashboardModel) sendScroll() {
	if m.conn != nil {
		_ = ipc.WriteJSON(m.conn, ipc.StreamUp{Type: "scroll", Offset: m.scrollOff})
	}
}

// wheelScrollStep is how many scrollback lines one wheel notch moves.
const wheelScrollStep = 3

// handleWheel routes a mouse-wheel event by where it happened. Over the sidebar
// (x within its column band) it moves the selection; over the screen pane it
// browses the session's scrollback, entering scroll mode on the way up and
// leaving it once a scroll-down returns to the live bottom so typing resumes
// flowing to the session.
func (m *dashboardModel) handleWheel(msg tea.MouseWheelMsg) (tea.Model, tea.Cmd) {
	e := msg.Mouse()
	if e.X < sidebarWidth {
		switch e.Button {
		case tea.MouseWheelUp:
			if m.cursor > 0 {
				m.cursor--
				m.syncSelFromCursor()
				m.syncStream()
			}
		case tea.MouseWheelDown:
			if m.cursor < len(m.visRows)-1 {
				m.cursor++
				m.syncSelFromCursor()
				m.syncStream()
			}
		}
		return m, nil
	}
	switch e.Button {
	case tea.MouseWheelUp:
		if !m.scrollMode {
			m.enterScroll()
		}
		if m.scrollMode {
			m.scrollBy(wheelScrollStep)
		}
	case tea.MouseWheelDown:
		if m.scrollMode {
			m.scrollBy(-wheelScrollStep)
			if m.scrollOff == 0 {
				m.exitScroll()
			}
		}
	}
	return m, nil
}

// handleScrollKey handles keystrokes while browsing scrollback in the screen pane.
func (m *dashboardModel) handleScrollKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "up", "k":
		m.scrollBy(1)
	case "down", "j":
		m.scrollBy(-1)
	case "pgup", "b":
		m.scrollBy(m.scrollPage())
	case "pgdown", "f", "space", " ":
		m.scrollBy(-m.scrollPage())
	case "g", "home":
		m.scrollBy(m.scrollMax) // oldest
	case "G", "end":
		m.exitScroll() // back to live
	case "esc", "q", "ctrl+c":
		m.exitScroll()
	}
	return m, nil
}

// handleSidebarKey handles navigation and dashboard commands while the
// sidebar has focus. The cursor walks the accordion's visRows (mix of scope
// headers and session children) — enter on a scope row toggles its
// collapse; enter on a session row focuses the screen pane. Right and left
// give vim-ish expand/collapse affordances.
func (m *dashboardModel) handleSidebarKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "ctrl+c":
		m.action = DashQuit
		return m, tea.Quit
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
			m.syncSelFromCursor()
			m.syncStream()
		}
	case "down", "j":
		if m.cursor < len(m.visRows)-1 {
			m.cursor++
			m.syncSelFromCursor()
			m.syncStream()
		}
	case "enter", " ", "space":
		if r := m.currentRow(); r != nil && r.isScope {
			m.toggleScope(r.scopeKey)
		} else if m.streamID != "" && !m.gone {
			m.focusScreenPane()
		}
	case "right", "l":
		if r := m.currentRow(); r != nil && r.isScope {
			// First press expands a collapsed group; once it's already
			// open, right behaves like "step into" by moving the cursor
			// to the first child session and re-streaming it.
			if !r.expanded {
				m.setScopeExpanded(r.scopeKey, true)
			} else if m.cursor+1 < len(m.visRows) && !m.visRows[m.cursor+1].isScope {
				m.cursor++
				m.syncSelFromCursor()
				m.syncStream()
			}
		} else if m.streamID != "" && !m.gone {
			m.focusScreenPane()
		}
	case "left", "h":
		// On a session row, collapse the parent group and park the cursor
		// on the header. On an already-collapsed header, no-op (matches
		// how tree views feel).
		if r := m.currentRow(); r != nil {
			if !r.isScope {
				m.setScopeExpanded(r.scopeKey, false)
			} else if r.expanded {
				m.setScopeExpanded(r.scopeKey, false)
			}
		}
	}
	return m, nil
}

// updateRename handles keystrokes while the rename prompt is active: enter
// commits, esc cancels, backspace deletes, and printable runes are appended.
func (m *dashboardModel) updateRename(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch {
	case msg.Code == tea.KeyEnter:
		id, name := m.renameID, strings.TrimSpace(m.renameBuf)
		m.renaming, m.renameID, m.renameBuf = false, "", ""
		return m, renameCmd(id, name)
	case msg.Code == tea.KeyEscape, msg.Code == 'c' && msg.Mod&tea.ModCtrl != 0:
		m.renaming, m.renameID, m.renameBuf = false, "", ""
		return m, nil
	case msg.Code == tea.KeyBackspace, msg.Code == tea.KeyDelete:
		if r := []rune(m.renameBuf); len(r) > 0 {
			m.renameBuf = string(r[:len(r)-1])
		}
		return m, nil
	case msg.Text != "":
		// Printable input (includes space, whose Text is " ").
		m.renameBuf += msg.Text
		return m, nil
	}
	return m, nil
}

var (
	titleStyle  = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("12"))
	helpStyle   = lipgloss.NewStyle().Faint(true)
	statusStyle = map[string]lipgloss.Style{
		"needs_approval": lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("9")),  // red
		"waiting_user":   lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("10")), // green: agent turn complete, ready for you
		"working":        lipgloss.NewStyle().Foreground(lipgloss.Color("10")),            // green: spinner distinguishes from waiting_user
		"starting":       lipgloss.NewStyle().Foreground(lipgloss.Color("14")),            // cyan
		"idle":           lipgloss.NewStyle().Foreground(lipgloss.Color("11")),            // yellow: fresh session, no turn yet
		"ended":          lipgloss.NewStyle().Faint(true).Foreground(lipgloss.Color("8")), // grey
	}
)

var spinnerFrames = []string{"⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"}

// indicator returns a short, colored glyph that conveys status at a glance: a
// spinner while working, a flag when approval is needed, a colored circle for
// the resting states (green = turn complete, yellow = fresh / never run).
func (m *dashboardModel) indicator(status string) string {
	st := statusStyle[status]
	switch status {
	case "working":
		return st.Render(spinnerFrames[m.spin%len(spinnerFrames)])
	case "needs_approval":
		return st.Render("⚑")
	case "waiting_user":
		return st.Render("●") // green — agent turn complete
	case "idle":
		return st.Render("●") // yellow — session created, no turn yet
	case "starting":
		return st.Render("…")
	case "ended":
		return st.Render("✗")
	default:
		return st.Render("•")
	}
}

// detectTransitions compares incoming statuses to the last-seen ones and
// raises sticky toasts when a session crosses into needs_approval or
// finishes its turn. The toasts persist (no TTL) and are anchored to the
// session — expireToasts clears them once the agent is past the prompt or
// the user focuses its screen pane.
//
// In flat (workspace-scoped) mode, only sessions whose scope matches the
// launch scope produce toasts; out-of-scope statuses still update m.prev
// so toggling back to global mode doesn't replay stale transitions as a
// fresh burst of notifications.
func (m *dashboardModel) detectTransitions(next []ipc.SessionInfo) {
	seen := make(map[string]bool, len(next))
	for _, s := range next {
		seen[s.ID] = true
		old := m.prev[s.ID]
		if s.Status != old && m.toastsAllowed(s) {
			switch s.Status {
			case "needs_approval":
				txt := s.LastMessage
				if txt == "" {
					txt = "needs your approval"
				}
				m.pushStickyToast("⚑ "+s.ID[:8]+" — "+txt, "needs_approval", s.ID, "needs_approval")
			case "waiting_user":
				if old != "" { // don't toast the very first observation
					m.pushStickyToast("● "+s.ID[:8]+" — turn completed", "waiting_user", s.ID, "waiting_user")
				}
			}
		}
		m.prev[s.ID] = s.Status
	}
	for id := range m.prev {
		if !seen[id] {
			delete(m.prev, id)
		}
	}
}

// toastsAllowed reports whether a session is currently eligible to raise
// per-session sticky toasts. Accordion (global) mode lets every session
// through; flat mode only allows sessions in the launch scope.
func (m *dashboardModel) toastsAllowed(s ipc.SessionInfo) bool {
	if m.accordionMode {
		return true
	}
	return m.scopeKeyOf(s.Cwd) == m.currentScope
}

// latestPending returns how many sessions need approval and the id of the one
// that entered needs_approval most recently (the "latest" to jump to).
func latestPending(sessions []ipc.SessionInfo) (count int, latestID string) {
	return pendingSummary(sessions, "")
}

// latestAttention picks the session jump_pending should land on: the
// most recently flagged needs_approval session, or — if none need approval —
// the freshest waiting_user (turn complete). Returns "" when nothing needs
// the user's eyes. Mirrors the priority of the sticky-toast stack so
// "prefix g" jumps to whatever's currently buzzing.
func latestAttention(sessions []ipc.SessionInfo) string {
	var pickID string
	var pickSince int64 = -1
	for _, s := range sessions {
		if s.Status == "needs_approval" && s.StatusSince > pickSince {
			pickSince = s.StatusSince
			pickID = s.ID
		}
	}
	if pickID != "" {
		return pickID
	}
	for _, s := range sessions {
		if s.Status == "waiting_user" && s.StatusSince > pickSince {
			pickSince = s.StatusSince
			pickID = s.ID
		}
	}
	return pickID
}

// pendingSummary is latestPending with an exclusion: the session you're already
// viewing (excludeID) is left out of both the count and the jump target, so the
// banner doesn't nag you about an approval screen you're already looking at —
// while still counting any other sessions that need approval.
func pendingSummary(sessions []ipc.SessionInfo, excludeID string) (count int, latestID string) {
	var newest int64 = -1
	for _, s := range sessions {
		if s.Status != "needs_approval" || s.ID == excludeID {
			continue
		}
		count++
		if s.StatusSince > newest {
			newest = s.StatusSince
			latestID = s.ID
		}
	}
	return count, latestID
}

func (m *dashboardModel) pushToast(text, level string) {
	m.toasts = append(m.toasts, toast{text: text, level: level, born: time.Now()})
	if len(m.toasts) > 5 {
		m.toasts = m.toasts[len(m.toasts)-5:]
	}
}

// dropOutOfScopeToasts removes sticky session-anchored toasts whose session
// is no longer eligible to raise toasts (i.e. it's out of scope under the
// current accordionMode). Ephemeral toasts (no sessionID) are untouched.
func (m *dashboardModel) dropOutOfScopeToasts() {
	if len(m.toasts) == 0 {
		return
	}
	kept := m.toasts[:0]
	for _, t := range m.toasts {
		if t.sessionID != "" {
			if s := m.sessionByID(t.sessionID); s != nil && !m.toastsAllowed(*s) {
				continue
			}
		}
		kept = append(kept, t)
	}
	m.toasts = kept
}

// pushStickyToast adds a notification anchored to a specific session. It
// persists past TTL — only expireToasts clears it, when the session's status
// moves out of `status`, the session ends, or its screen pane is focused.
// Replaces any prior sticky toast for the same session so consecutive
// status transitions don't pile up multiple lines for one agent.
func (m *dashboardModel) pushStickyToast(text, level, sessionID, status string) {
	if sessionID == "" {
		m.pushToast(text, level)
		return
	}
	// Drop any existing sticky for this session — keeps the strip tidy and
	// ensures only the latest message wins.
	kept := m.toasts[:0]
	for _, t := range m.toasts {
		if t.sessionID == sessionID {
			continue
		}
		kept = append(kept, t)
	}
	m.toasts = kept
	m.toasts = append(m.toasts, toast{
		text: text, level: level, born: time.Now(),
		sessionID: sessionID, status: status,
	})
	if len(m.toasts) > 5 {
		m.toasts = m.toasts[len(m.toasts)-5:]
	}
}

// expireToasts drops ephemeral toasts past TTL and clears sticky toasts
// whose triggering condition no longer applies: the session ended, its
// status moved out of the prompting state, or the user focused its screen
// pane. Runs on every tick (and any time the model changes focus).
func (m *dashboardModel) expireToasts() {
	cur := map[string]string{}
	for _, s := range m.sessions {
		cur[s.ID] = s.Status
	}
	kept := m.toasts[:0]
	for _, t := range m.toasts {
		if t.sessionID != "" {
			status, alive := cur[t.sessionID]
			if !alive {
				continue
			}
			if t.status != "" && status != t.status {
				continue
			}
			if m.streamID == t.sessionID && m.focus == focusScreen {
				continue
			}
			kept = append(kept, t)
			continue
		}
		if time.Since(t.born) < toastTTL {
			kept = append(kept, t)
		}
	}
	m.toasts = kept
}

var (
	screenBorderStyle = lipgloss.NewStyle().
				Border(lipgloss.NormalBorder(), false, false, false, true).
				BorderForeground(lipgloss.Color("8")).
				PaddingLeft(1)
	screenFocusBorderStyle  = screenBorderStyle.BorderForeground(lipgloss.Color("12"))
	screenScrollBorderStyle = screenBorderStyle.BorderForeground(lipgloss.Color("13")) // magenta: browsing scrollback
	rowStyle                = lipgloss.NewStyle().Width(sidebarWidth)
	selBarStyle             = lipgloss.NewStyle().Foreground(lipgloss.Color("11")) // yellow
	selBarDimStyle          = lipgloss.NewStyle().Foreground(lipgloss.Color("8"))  // grey
	// scopeNameStyle is the accordion header text style — bold so a group
	// label is clearly distinct from the indented session rows beneath it.
	scopeNameStyle = lipgloss.NewStyle().Bold(true)
)

func (m *dashboardModel) View() tea.View {
	v := tea.NewView(m.renderLive())
	v.AltScreen = true
	v.ReportFocus = true
	if m.shouldShowSessionCursor() {
		v.Cursor = tea.NewCursor(sidebarWidth+screenBorderStyle.GetHorizontalFrameSize()+m.cursorX, m.cursorY)
	}
	// Capture the wheel (cell-motion = click/release/wheel/drag) so scrolling the
	// screen pane browses scrollback instead of leaking arrow keys into the
	// session.
	v.MouseMode = tea.MouseModeCellMotion
	return v
}

func (m *dashboardModel) shouldShowSessionCursor() bool {
	return m.focus == focusScreen &&
		!m.configOpen &&
		!m.wtOpen &&
		!m.taskOpen &&
		!m.renaming &&
		!m.prefix &&
		!m.menu &&
		m.streamID != "" &&
		!m.gone &&
		!m.scrollMode &&
		m.screen != "" &&
		m.cursorX >= 0 &&
		m.cursorX < m.paneW &&
		m.cursorY >= 0 &&
		m.cursorY < m.paneH
}

func (m *dashboardModel) renderLive() string {
	// Config modal owns the whole viewport — simpler than overlaying, and the
	// dashboard underneath is incidental while the user is rebinding keys.
	// Toasts and the hooks warning are suspended here; they'll be visible
	// again as soon as the menu closes.
	if m.configOpen {
		return clampLines(centerOnScreen(m.renderConfigMenu(), m.w, m.h), m.w)
	}
	// The worktree/agent picker likewise takes over the whole viewport while
	// the user is choosing where and what to launch.
	if m.wtOpen {
		return clampLines(centerOnScreen(m.renderWorktreePicker(), m.w, m.h), m.w)
	}
	// And the task backlog dialog.
	if m.taskOpen {
		return clampLines(centerOnScreen(m.renderTaskBacklog(), m.w, m.h), m.w)
	}
	// The title lives at the top of the sidebar (renderSidebar), so the screen
	// pane spans the full height beside it — no full-width header band.
	body := lipgloss.JoinHorizontal(lipgloss.Top, m.renderSidebar(), m.renderScreen())

	// Chrome (toasts, hooks-not-installed banner, rename input) is overlaid
	// onto the bottom rows of the body so the panes themselves fill the whole
	// terminal — no more reserved blank rows that leave the borders short of
	// the bottom. The overlay is left-aligned and only as wide as the chrome
	// line itself, so the rest of the underlying row stays visible past it.
	// Drawn *before* the prefix panel so the panel (which the user explicitly
	// summons) wins for any rows where both want to paint.
	if chrome := m.chromeLines(); len(chrome) > 0 {
		body = overlayBottomLeft(body, chrome, 0)
	}

	// Float the prefix-command hints panel over the bottom of the body when
	// the user has tapped the prefix key (one-shot) or opened the sticky menu
	// (prefix+h / prefix+?). Full terminal width, which-key-style.
	if m.prefix || m.menu {
		body = overlayBottom(body, m.renderPrefixPanel(m.w), 0, m.w)
	}

	// Bound every line to the terminal width. The body lines are already within
	// width, but chrome lines can be longer than a narrow terminal; left
	// unbounded they wrap at display time, adding a visual row that pushes the
	// view past the terminal height and clips the bottom. Truncating is
	// ANSI-aware so styling is preserved and not left dangling.
	return clampLines(body, m.w)
}

// clampLines truncates each line of s to at most w display columns, preserving
// ANSI styling (so a faint/colored line keeps its trailing reset). w <= 0 is a
// no-op.
func clampLines(s string, w int) string {
	if w <= 0 {
		return s
	}
	lines := strings.Split(s, "\n")
	for i, ln := range lines {
		lines[i] = ansi.Truncate(ln, w, "")
	}
	return strings.Join(lines, "\n")
}

// renderSidebar draws the narrow left column as an accordion: one row per
// scope group (collapse glyph + name + right-justified count) followed by
// indented session rows whenever the scope is expanded. The cursor lands on
// whichever row the user navigated to (either kind). The highlight bar is
// bright yellow when the sidebar has focus, grey when the screen pane does.
func (m *dashboardModel) renderSidebar() string {
	// Rebuild on every render so tests that mutate m.sessions directly still
	// see a coherent accordion. In normal flow rebuildRows already ran from
	// the sessionsMsg / toggle path, so this is cheap (linear in counts).
	if len(m.visRows) != m.expectedRowCount() {
		m.rebuildRows()
	}

	// Skip the (relatively expensive) per-row styling when nothing the sidebar
	// depends on has changed since the last render — the screen pane streams far
	// faster than the sidebar's inputs move. sidebarTop is deliberately not in
	// the signature: clampTop is idempotent, so it only changes when the cursor,
	// session set, or pane height does, all of which already invalidate the hash.
	sig := m.sidebarSignature()
	if m.sidebarValid && sig == m.sidebarSig {
		return m.sidebarCache
	}

	var rows []string
	// cursorRow is the index of the highlighted row inside `rows`. It
	// differs from m.cursor (an index into m.visRows) once we interleave
	// spacers, so clampTop can still keep the cursor on-screen.
	cursorRow := 0
	for i, r := range m.visRows {
		gutter := " "
		if i == m.cursor {
			cursorRow = len(rows)
			if m.focus == focusSidebar {
				gutter = selBarStyle.Render("▌")
			} else {
				gutter = selBarDimStyle.Render("▌")
			}
		}
		if r.isScope {
			rows = append(rows, m.renderScopeRow(gutter, r))
		} else {
			rows = append(rows, m.renderSessionRow(gutter, r.session))
		}
		// The inter-group gap belongs to the *expanded* group above it:
		// after the last child of an expanded scope, insert one blank line
		// before the next scope header. With every group collapsed this
		// emits nothing, so the headers stack flush.
		if !r.isScope && i+1 < len(m.visRows) && m.visRows[i+1].isScope {
			rows = append(rows, "")
		}
	}

	if len(rows) == 0 {
		// Pull the actual bindings so rebound keys read correctly in the hint.
		newClaude := m.keyForAction("new_claude")
		newCodex := m.keyForAction("new_codex")
		rows = append(rows,
			helpStyle.Render(" no sessions"),
			"",
			helpStyle.Render(truncate(" prefix+"+newClaude+" claude", sidebarWidth-1)),
			helpStyle.Render(truncate(" prefix+"+newCodex+" codex", sidebarWidth-1)),
		)
	}

	// The sidebar carries the app title at the top, then the accordion, then
	// the global status-tally row at the bottom. errMsg (a daemon problem)
	// rides under the title.
	header := titleStyle.Render("codebridge") + "\n" + m.scopeLabel()
	if m.errMsg != "" {
		header += "\n" + statusStyle["needs_approval"].Render(truncate("daemon: "+m.errMsg, sidebarWidth-1))
	}
	headerH := strings.Count(header, "\n") + 1

	// The list is a window that scrolls to keep the cursor visible. It gets
	// whatever height is left after the header, a blank spacer on each side,
	// and the global status-tally row. The list is padded with blank lines
	// to fill its slot so the counts strip stays pinned to the bottom edge.
	maxRows := maxInt(m.paneH-headerH-3, 1)
	top := clampTop(cursorRow, m.sidebarTop, len(rows), maxRows)
	m.sidebarTop = top
	end := minInt(top+maxRows, len(rows))
	listRows := rows[top:end]
	for len(listRows) < maxRows {
		listRows = append(listRows, "")
	}
	list := strings.Join(listRows, "\n")
	counts := m.renderStatusCounts()
	content := firstLines(lipgloss.JoinVertical(lipgloss.Left, header, "", list, "", counts), maxInt(m.paneH, 1))
	out := lipgloss.NewStyle().Width(sidebarWidth).Height(maxInt(m.paneH, 1)).Render(content)
	m.sidebarSig, m.sidebarValid, m.sidebarCache = sig, true, out
	return out
}

// sidebarSignature hashes everything renderSidebar reads, so an unchanged hash
// means the cached string is still correct and re-styling can be skipped. It
// covers every session's id/status/name/cwd (rows + the bottom status tally +
// the worktree glyph), the accordion's per-scope expansion and counts, and the
// scalar inputs (focus, cursor, pane height, mode, scope, daemon error). The
// spinner frame (m.spin) is folded in only when a session is actually working —
// otherwise an idle sidebar would needlessly miss the cache on every 500ms tick.
func (m *dashboardModel) sidebarSignature() uint64 {
	h := fnv.New64a()
	var b [8]byte
	wu := func(x uint64) { binary.LittleEndian.PutUint64(b[:], x); h.Write(b[:]) }
	wi := func(x int) { wu(uint64(int64(x))) }
	ws := func(s string) { io.WriteString(h, s); h.Write([]byte{0}) }
	wb := func(v bool) {
		if v {
			wu(1)
		} else {
			wu(0)
		}
	}

	wi(int(m.focus))
	wi(m.cursor)
	wi(m.paneH)
	wb(m.accordionMode)
	ws(m.currentScope)
	ws(m.errMsg)

	working := false
	for i := range m.sessions {
		s := &m.sessions[i]
		ws(s.ID)
		ws(s.Status)
		ws(s.Name)
		ws(s.Cwd)
		if s.Status == "working" {
			working = true
		}
	}
	// visRows carries the accordion's expansion + ordering (which scopes are
	// open, header counts) on top of the raw session set above.
	for _, r := range m.visRows {
		if r.isScope {
			ws(r.scopeKey)
			wb(r.expanded)
			wi(r.scopeCount)
		}
	}
	if working {
		wi(m.spin)
	}
	return h.Sum64()
}

// expectedRowCount is what len(visRows) would be after a fresh rebuildRows
// over the current m.sessions and m.expanded. Used as a cheap "is visRows
// out of date?" check so render-time rebuilds skip the work when the cached
// rows still match — and so tests that poke m.sessions directly still pick
// up a fresh accordion without going through the sessionsMsg handler.
func (m *dashboardModel) expectedRowCount() int {
	if !m.accordionMode {
		n := 0
		for _, s := range m.sessions {
			if m.scopeKeyOf(s.Cwd) == m.currentScope {
				n++
			}
		}
		return n
	}
	seen := map[string]bool{}
	n := 0
	for _, s := range m.sessions {
		k := m.scopeKeyOf(s.Cwd)
		if !seen[k] {
			seen[k] = true
			n++ // scope header
		}
		if m.expanded[k] {
			n++ // session row under an expanded header
		}
	}
	if m.currentScope != "" && !seen[m.currentScope] {
		n++ // synthetic empty header for the launch-cwd group
	}
	return n
}

// renderScopeRow is one accordion header: cursor gutter, scope display
// name, a session count, and a far-right chevron that flips between
// collapsed (›) and expanded (⌄). Layout is computed in display columns
// (not bytes) so the chevron stays flush to the right edge regardless of
// ANSI styling in the name.
func (m *dashboardModel) renderScopeRow(gutter string, r visRow) string {
	// ▸/▾ are baseline-centered small triangles. The arrowhead glyphs
	// (›/⌄) render in the upper half of the cell and look pushed up next
	// to the count beside them.
	glyph := "▸"
	if r.expanded {
		glyph = "▾"
	}
	countStr := fmt.Sprintf("%d", r.scopeCount)
	// Budget: gutter(1) + name + pad(>=1) + count + " "(1) + glyph(1)
	nameMax := sidebarWidth - 1 - len(countStr) - 3
	if nameMax < 1 {
		nameMax = 1
	}
	name := truncate(scopeDisplayName(r.scopeKey), nameMax)
	used := 1 + ansi.StringWidth(name) // gutter + name
	pad := sidebarWidth - used - len(countStr) - 2
	if pad < 1 {
		pad = 1
	}
	return gutter +
		scopeNameStyle.Render(name) +
		strings.Repeat(" ", pad) +
		helpStyle.Render(countStr) + " " +
		helpStyle.Render(glyph)
}

// renderSessionRow is one child of an expanded scope: indented by one cell
// to make the hierarchy obvious, then the same status-glyph + name layout
// the original flat sidebar used.
func (m *dashboardModel) renderSessionRow(gutter string, s ipc.SessionInfo) string {
	nameMax := sidebarWidth - 4 // gutter + indent + indicator + space
	suffix := ""
	if m.isWorktreeCached(s.Cwd) {
		suffix = " " + helpStyle.Render("⎇")
		nameMax -= 2
	}
	row := gutter + " " + m.indicator(s.Status) + " " + truncate(displayName(s), nameMax) + suffix
	return rowStyle.Render(row)
}

// renderStatusCounts is a one-row tally of sessions by status, counted across
// every session the daemon knows about — the sidebar is globally scoped, so
// the bottom strip is always a "what's happening across every agent"
// indicator. Order is scan-priority: progressing → needs approval → turn
// complete → idle. Glyphs match the per-row indicators; "working" uses a
// static braille frame (same shape as the animated spinner) so a row of
// "0 working" doesn't flicker an irrelevant animation.
func (m *dashboardModel) renderStatusCounts() string {
	var working, approval, waiting, idle int
	for _, s := range m.sessions {
		switch s.Status {
		case "working":
			working++
		case "needs_approval":
			approval++
		case "waiting_user":
			waiting++
		case "idle":
			idle++
		}
	}
	cell := func(status, glyph string, n int) string {
		return statusStyle[status].Render(glyph) + helpStyle.Render(fmt.Sprintf(" %d", n))
	}
	return " " + cell("working", "⠴", working) +
		" " + cell("needs_approval", "⚑", approval) +
		" " + cell("waiting_user", "●", waiting) +
		" " + cell("idle", "●", idle)
}

// selectionBG is the 256-color background applied to selected cells. We use a
// mid-dark gray rather than SGR reverse so the highlight reads as a translucent
// overlay (text keeps its own colors) instead of inverting to a white block.
// 238 is dark enough to keep bright TUI text readable on most terminals.
const selectionBG = "\x1b[48;5;238m"
const selectionBGReset = "\x1b[49m"

// applySelectionHighlight paints the selected cells of the current screen frame
// with a gray background. We translate from virtual-line space back to the
// visible rows of this frame (lines outside the window become no-ops), splice
// each affected line into [before, selected, after] via ansi.Cut (grapheme- and
// escape-aware), and wrap the middle in set/reset background SGR. Foreground
// colors carry through, so the highlighted text stays legible.
func (m *dashboardModel) applySelectionHighlight(screen string) string {
	if m.selStart == m.selEnd {
		return screen
	}
	start, end := normalizeSel(m.selStart, m.selEnd)
	topV := m.scrollMax - m.scrollOff // virtual line shown on row 0
	lines := strings.Split(screen, "\n")
	for i := range lines {
		v := topV + i
		if v < start.line || v > end.line {
			continue
		}
		lo, hi := 0, m.paneW
		if v == start.line {
			lo = start.col
		}
		if v == end.line {
			hi = end.col
		}
		if hi <= lo {
			continue
		}
		left := ansi.Cut(lines[i], 0, lo)
		mid := ansi.Cut(lines[i], lo, hi)
		right := ansi.Cut(lines[i], hi, m.paneW)
		if ansi.StringWidth(mid) == 0 {
			// Past the printed content on this line: highlight a blank gutter so
			// the user can still see the selected region extending across empty
			// space (mid-paragraph multi-line drags).
			mid = strings.Repeat(" ", hi-lo)
		} else {
			// The mid slice carries the source's own SGR escapes, and any reset
			// inside it (\x1b[0m, \x1b[m) or explicit default-BG (\x1b[49m)
			// would clear the gray background we set at the start — leaving
			// every styled token unhighlighted. Re-emit the BG after each such
			// sequence so the overlay survives nested styling.
			mid = reapplyBGAfterResets(mid, selectionBG)
		}
		lines[i] = left + selectionBG + mid + selectionBGReset + right
	}
	return strings.Join(lines, "\n")
}

// reapplyBGAfterResets scans s for CSI SGR sequences and appends bg after any
// that clears the background — either a full reset (parameter 0 or empty) or
// an explicit default-BG (parameter 49). This keeps a selection highlight
// visible across styled text spans whose own escapes would otherwise wipe the
// BG halfway through.
func reapplyBGAfterResets(s, bg string) string {
	if !strings.Contains(s, "\x1b[") {
		return s
	}
	var out strings.Builder
	out.Grow(len(s) + len(bg)*4)
	for len(s) > 0 {
		i := strings.Index(s, "\x1b[")
		if i < 0 {
			out.WriteString(s)
			break
		}
		out.WriteString(s[:i])
		// Locate the CSI final byte (0x40–0x7E). An unterminated sequence is
		// emitted verbatim; we never invent bytes the source didn't send.
		j := i + 2
		for j < len(s) && (s[j] < 0x40 || s[j] > 0x7e) {
			j++
		}
		if j >= len(s) {
			out.WriteString(s[i:])
			break
		}
		seq := s[i : j+1]
		out.WriteString(seq)
		if seq[len(seq)-1] == 'm' && sgrClearsBG(seq[2:len(seq)-1]) {
			out.WriteString(bg)
		}
		s = s[j+1:]
	}
	return out.String()
}

// sgrClearsBG reports whether the given SGR parameter list contains a code
// that resets the background: empty (treated as 0), 0 (full reset), or 49
// (default BG). Multi-parameter forms like "0;1;38;5;46" are handled by
// splitting on ';'. We don't try to model selector continuations (38;5;n,
// 48;2;r;g;b) — those don't clear BG, and any "0" parameter alone is enough
// to trigger a re-emit.
func sgrClearsBG(params string) bool {
	if params == "" {
		return true
	}
	for _, p := range strings.Split(params, ";") {
		switch p {
		case "", "0", "00", "49":
			return true
		}
	}
	return false
}

// renderScreen draws the right pane: just the selected session's live screen
// (the session is sized to fill this pane). Focus is shown by the border color;
// the session's own status/title lives in the sidebar, so there's no header
// here. Keystrokes are forwarded to the session when this pane has focus.
func (m *dashboardModel) renderScreen() string {
	var screen string
	switch {
	case m.streamID == "":
		screen = helpStyle.Render("no session selected")
	case m.gone:
		screen = helpStyle.Render("(session ended)")
	case m.screen == "":
		screen = helpStyle.Render("loading…")
	default:
		screen = m.screen
		if m.selStart != m.selEnd {
			screen = m.applySelectionHighlight(screen)
		}
	}
	// Bound the screen to the pane height so a tall session render can't overflow
	// the View and clip the top (which would hide the session list).
	screen = lastLines(screen, m.paneH)
	border := screenBorderStyle
	switch {
	case m.scrollMode:
		border = screenScrollBorderStyle
	case m.focus == focusScreen:
		border = screenFocusBorderStyle
	}
	// lipgloss Width is the *total* block width — it includes the border and
	// padding, not just the content. So to give the session content exactly paneW
	// columns (matching the cols the session is sized to), Width must be paneW plus
	// the horizontal frame (left border + left padding). Using the style's own
	// frame size keeps this correct if the border/padding ever change. Getting this
	// wrong by one makes a full-width line (e.g. Claude's input-box rules) wrap onto
	// a stray extra row.
	frame := border.GetHorizontalFrameSize()
	return border.Width(maxInt(m.paneW+frame, 1)).Height(maxInt(m.paneH, 1)).Render(screen)
}

// prefixPanelStyle is the bordered box for the floating command-hints panel.
// The magenta border picks up the same accent used for the scrollback border,
// so the "I'm in a mode" cue is consistent across the UI.
var (
	prefixPanelStyle = lipgloss.NewStyle().
				Border(lipgloss.RoundedBorder()).
				BorderForeground(lipgloss.Color("13")).
				Padding(0, 1)
	kbdStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("12"))
)

// renderPrefixPanel builds the floating command-hints panel: a multi-column
// grid of prefix commands with their current keys highlighted. The panel is
// stretched to the full terminal width passed in, with column widths split
// evenly so every row's cells line up edge-to-edge (which-key style). Bindings
// are read live from cfg.Bindings so a rebind shows up immediately the next
// time the panel opens. The `h`/`?`-style system shortcuts are hardcoded
// because they aren't routed through the rebinding table.
func (m *dashboardModel) renderPrefixPanel(width int) string {
	kbd := func(s string) string { return kbdStyle.Render(s) }
	b := func(action string) string { return kbd(m.keyForAction(action)) }
	type item struct{ key, label string }
	items := []item{
		{b("new_claude"), "new claude"},
		{b("new_codex"), "new codex"},
		{b("new_worktree"), "worktree +agent"},
		{b("task_backlog"), "task backlog"},
		{b("kill"), "kill session"},
		{b("rename"), "rename"},
		{kbd("h"), "focus sidebar"},
		{b("focus_screen"), "focus screen"},
		{b("scroll"), "scrollback"},
		{b("scope_toggle"), "all/this workspace"},
		{b("jump_pending"), "jump pending"},
		{b("newline"), "newline"},
		{b("yank"), "yank selection"},
		{kbd("?"), "toggle hints"},
		{b("config"), "open config"},
		{b("quit"), "quit cb"},
	}
	const cols = 4
	frame := prefixPanelStyle.GetHorizontalFrameSize()
	inner := width - frame
	if inner < cols*8 {
		inner = cols * 8
	}
	cellW := inner / cols
	rows := make([]string, 0, (len(items)+cols-1)/cols+1)
	rows = append(rows, helpStyle.Render(" prefix = "+prefixLabel()+" "))
	for i := 0; i < len(items); i += cols {
		end := i + cols
		if end > len(items) {
			end = len(items)
		}
		cells := make([]string, end-i)
		for j, it := range items[i:end] {
			cells[j] = padDisplayWidth(it.key+" "+it.label, cellW)
		}
		rows = append(rows, strings.Join(cells, ""))
	}
	return prefixPanelStyle.Width(width).Render(strings.Join(rows, "\n"))
}

// padDisplayWidth pads s with trailing spaces so its on-screen column width
// equals n. ANSI escapes don't add to display width, so we can't use plain
// len() — see padRight for the ASCII-only variant.
func padDisplayWidth(s string, n int) string {
	w := ansi.StringWidth(s)
	if w >= n {
		return s
	}
	return s + strings.Repeat(" ", n-w)
}

// chromeLines collects the ephemeral status rows (one toast per line, then
// hooks banner, then rename input) in top-to-bottom order. Returns nil when
// nothing's active so the caller can skip the overlay entirely.
//
// Toasts come first and one-per-line on purpose: each row is independently
// hit-testable in handleMouseClick so the user can click a sticky toast to
// jump straight to the session that raised it.
func (m *dashboardModel) chromeLines() []string {
	var lines []string
	lines = append(lines, m.toastLines()...)
	if !m.hooksOK {
		lines = append(lines, statusStyle["waiting_user"].Render("⚠ hooks not installed — run: cb install-hooks"))
	}
	if m.renaming {
		lines = append(lines, titleStyle.Render("rename: ")+m.renameBuf+"▎  "+helpStyle.Render("enter save · esc cancel"))
	}
	return lines
}

// overlayBottomLeft paints each line in lines over a row at the bottom of
// body, left-aligned at colMin. Unlike overlayBottom this preserves the right
// portion of the underlying row past the chrome line — so a narrow toast only
// covers its own width and the rest of the session row keeps showing through.
func overlayBottomLeft(body string, lines []string, colMin int) string {
	if len(lines) == 0 {
		return body
	}
	bl := strings.Split(body, "\n")
	startRow := len(bl) - len(lines)
	if startRow < 0 {
		lines = lines[-startRow:]
		startRow = 0
	}
	for i, ln := range lines {
		row := startRow + i
		if row < 0 || row >= len(bl) {
			continue
		}
		underlying := bl[row]
		lineW := ansi.StringWidth(underlying)
		panelW := ansi.StringWidth(ln)
		left := ansi.Cut(underlying, 0, colMin)
		if lw := ansi.StringWidth(left); lw < colMin {
			left += strings.Repeat(" ", colMin-lw)
		}
		right := ""
		if colMin+panelW < lineW {
			right = ansi.Cut(underlying, colMin+panelW, lineW)
		}
		bl[row] = left + ln + right
	}
	return strings.Join(bl, "\n")
}

// overlayBottom paints panel onto the bottom of body, horizontally centered
// within [colMin, colMax). Lines are spliced with ansi.Cut so the styling on
// either side of the overlay is preserved. The panel is clipped (rather than
// overflowing) when the available width is too narrow.
func overlayBottom(body, panel string, colMin, colMax int) string {
	if colMax <= colMin {
		return body
	}
	pl := strings.Split(panel, "\n")
	bl := strings.Split(body, "\n")
	panelW := 0
	for _, p := range pl {
		if w := ansi.StringWidth(p); w > panelW {
			panelW = w
		}
	}
	avail := colMax - colMin
	if panelW > avail {
		panelW = avail
		for i, p := range pl {
			pl[i] = ansi.Truncate(p, panelW, "")
		}
	}
	leftCol := colMin + (avail-panelW)/2
	startRow := len(bl) - len(pl)
	if startRow < 0 {
		startRow = 0
		pl = pl[len(pl)-len(bl):]
	}
	for i, p := range pl {
		row := startRow + i
		if row < 0 || row >= len(bl) {
			continue
		}
		line := bl[row]
		lineW := ansi.StringWidth(line)
		left := ansi.Cut(line, 0, leftCol)
		if lw := ansi.StringWidth(left); lw < leftCol {
			left += strings.Repeat(" ", leftCol-lw)
		}
		right := ""
		if leftCol+panelW < lineW {
			right = ansi.Cut(line, leftCol+panelW, lineW)
		}
		bl[row] = left + p + right
	}
	return strings.Join(bl, "\n")
}

// toastLines renders each active toast as its own row (oldest at the top).
// Sticky toasts get a subtle ›-prefix and underline so they read as actionable
// (clickable) rather than passive notifications. Ephemeral toasts render
// plainly so they don't pretend to be interactive.
func (m *dashboardModel) toastLines() []string {
	if len(m.toasts) == 0 {
		return nil
	}
	out := make([]string, len(m.toasts))
	for i, t := range m.toasts {
		style := helpStyle
		if s, ok := statusStyle[t.level]; ok {
			style = s
		}
		if t.sessionID != "" {
			out[i] = style.Underline(true).Render("› "+t.text) + helpStyle.Render(" (click)")
		} else {
			out[i] = style.Render(t.text)
		}
	}
	return out
}

// toastSessionAt returns the sessionID anchored to the sticky toast under
// the click point (x,y in terminal coordinates), or "" when the click misses.
// Toasts are the first len(m.toasts) entries of chromeLines and are overlaid
// at the bottom of the body, so the math is: chromeTop = m.h - len(chrome);
// toast i lives at chromeTop+i and spans its rendered display width.
func (m *dashboardModel) toastSessionAt(x, y int) string {
	if len(m.toasts) == 0 {
		return ""
	}
	chrome := m.chromeLines()
	if len(chrome) == 0 {
		return ""
	}
	chromeTop := m.h - len(chrome)
	if y < chromeTop {
		return ""
	}
	i := y - chromeTop
	if i < 0 || i >= len(m.toasts) {
		return ""
	}
	t := m.toasts[i]
	if t.sessionID == "" {
		return ""
	}
	// Ensure the click landed within the toast's horizontal extent — clicks
	// past the text drop through to whatever's behind it.
	width := ansi.StringWidth(chrome[i])
	if x < 0 || x >= width {
		return ""
	}
	return t.sessionID
}

// focusScreenPane moves focus to the screen pane and immediately drops any
// sticky toast whose session is now under view. The 500ms tick would do this
// anyway, but doing it inline keeps a toast from lingering after a keystroke
// or click that obviously dismisses it.
func (m *dashboardModel) focusScreenPane() {
	m.focus = focusScreen
	m.expireToasts()
}

// focusSession selects the named session and focuses the screen pane on it,
// opening its scope group along the way. Used by sticky-toast clicks so the
// user can act on a prompt without leaving the mouse.
func (m *dashboardModel) focusSession(id string) {
	for _, s := range m.sessions {
		if s.ID != id {
			continue
		}
		key := m.scopeKeyOf(s.Cwd)
		if !m.expanded[key] {
			m.setScopeExpanded(key, true)
		}
		m.selScope = key
		m.selSession = id
		m.rebuildRows()
		m.syncStream()
		m.focusScreenPane()
		return
	}
}

// truncate shortens s to at most n runes, adding an ellipsis when cut. It
// assumes s has no ANSI escapes (true for plain names/ids).
func truncate(s string, n int) string {
	if n < 1 {
		n = 1
	}
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n == 1 {
		return "…"
	}
	return string(r[:n-1]) + "…"
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// clampTop returns the first visible row index for a scrolling list window: it
// keeps `cursor` within the `maxRows`-tall window starting near `top`, without
// scrolling past the end of a list of `count` rows.
func clampTop(cursor, top, count, maxRows int) int {
	if maxRows < 1 {
		maxRows = 1
	}
	if cursor < top {
		top = cursor
	}
	if cursor >= top+maxRows {
		top = cursor - maxRows + 1
	}
	if maxTop := maxInt(0, count-maxRows); top > maxTop {
		top = maxTop
	}
	if top < 0 {
		top = 0
	}
	return top
}

// lastLines keeps at most the final n lines of s (like a terminal showing the
// bottom of the scrollback). Used to bound the live screen so a tall session
// render can't push the whole View past the terminal height and clip the list.
func lastLines(s string, n int) string {
	if n <= 0 {
		return ""
	}
	lines := strings.Split(s, "\n")
	if len(lines) > n {
		lines = lines[len(lines)-n:]
	}
	return strings.Join(lines, "\n")
}

// firstLines keeps at most the first n lines of s.
func firstLines(s string, n int) string {
	if n <= 0 {
		return ""
	}
	lines := strings.Split(s, "\n")
	if len(lines) > n {
		lines = lines[:n]
	}
	return strings.Join(lines, "\n")
}
