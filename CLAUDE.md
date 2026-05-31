# CLAUDE.md

Guidance for working in this repo. `codebridge` is a Go TUI (`cb`) that manages many
Claude Code / Codex sessions: a long-lived daemon owns each session's PTY, and a Bubble Tea
client renders a single unified view — a session list sidebar plus the selected session's
live, interactive screen. No tmux dependency.

## Build / test / run

```sh
go build -o ./dist/cb .    # single binary (dist/ is gitignored)
go vet ./...
go test ./...              # unit tests live in internal/tui and internal/hook
gofmt -w internal/
```

Requires Go 1.24+ (the `charmbracelet/x/vt` emulator pulls the toolchain to 1.25).

Interactive parts can't be driven from plain stdin in CI — to exercise the client, run it
under a PTY (see the Python `pty` harnesses used during development) or test by hand:

```sh
./dist/cb daemon &            # or let `cb` auto-start it
./dist/cb ctl spawn claude
./dist/cb                     # sidebar + live screen; ^a →/← switch focus
```

## Architecture

- **`internal/session`** — `Session` wraps one child under a PTY (`creack/pty`) plus a
  `charmbracelet/x/vt.SafeEmulator`. Goroutines: `readLoop` (drain PTY → emulator),
  `replyLoop` (forward emulator-generated terminal replies back to the PTY — see Gotchas),
  `wait` (reap + mark ended). Status enum is set by hook events, not by scraping output.
- **`internal/daemon`** — the hub. Owns the session registry and a unix-socket listener
  (`daemon.go`). `stream.go` handles `attach`: a bidirectional stream that pushes deduped
  screen frames (`~30fps`) and accepts input/resize/detach/scroll. A `scroll` message sets
  a per-attach offset; the frame loop renders `Session.RenderScroll(offset)` (scrollback +
  visible grid) and reports the clamped `Offset`/`MaxOffset` back in each frame. Spawns
  sessions with `CB_SESSION=<uuid>` injected so hooks can correlate.
- **`internal/ipc`** — the wire protocol: line-delimited JSON. `Request`/`Response` for
  one-shot calls (`ping`/`spawn`/`list`/`kill`/`hook`/`shutdown`), `StreamUp`/`StreamDown`
  for attached connections. `ProtocolVersion` gates against a stale daemon.
- **`internal/hook`** — `cb hook <event>` is a no-op observer Claude Code invokes; it
  reads stdin JSON + `CB_SESSION` env and POSTs to the daemon, always exiting 0.
  `install.go` merges hook entries into `~/.claude/settings.json` (idempotent, writes a
  `.bak`); `Installed()` detects whether they're present.
- **`internal/tui`** — Bubble Tea client. `dashboard.go` is the whole UI: a sidebar
  (session list, polls `list` every 500ms) beside a live screen pane that opens one attach
  stream to the selected session and forwards input when focused (`focusZone`). The sidebar
  scrolls to keep the cursor visible (`clampTop`); `prefix [` enters a keyboard scroll mode
  that browses the screen pane's scrollback via daemon-rendered frames. The mouse is left
  uncaptured on purpose so the terminal's native text selection keeps working. `keys.go`
  maps Bubble Tea keys → raw bytes for forwarding; the `ctrl+a` prefix (configurable via
  `CB_PREFIX`) switches focus / quits.
- **`internal/cli`** — subcommand router + `ensureDaemon` (auto-start with a readiness wait
  and protocol-version check).

## Status model

Hook event → status mapping lives in `daemon.statusForEvent`:

- `SessionStart` → `idle` (fresh session, no turn has run yet — distinct from
  *turn complete* so the sidebar can colour them differently).
- `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `PostToolBatch` → `working`.
- `PermissionRequest` → `needs_approval` (stores `Message`).
- `Notification` → `needs_approval` when the message looks like a permission
  prompt (`isApprovalMessage`), else `waiting_user` (the generic "waiting for
  input" nudge Claude Code fires at idle).
- `Stop` → `waiting_user` (agent turn finished).
- `SessionEnd` → `ended`.
- Anything else → `working` (tolerant fallback while the session is active).

A spawned session starts at `starting` (set by `session.New`) until the first
hook event fires.

The hook CLI forwards *any* event name, so `statusForEvent` is the single place
to adjust if Claude Code's event names shift between versions.

**Sidebar glyphs** (`dashboardModel.indicator` + `statusStyle`): green spinner
while `working`, **green ●** for `waiting_user` (turn complete), **yellow ●**
for `idle` (just created), cyan `…` for `starting`, red `⚑` for
`needs_approval`, grey `✗` for `ended`. Toasts fire only on transitions *into*
`needs_approval` and *into* `waiting_user` (with `old != ""` guarding against
a first-observation false positive). New colours/icons should be added in both
the style map and the indicator switch.

## Workspace scoping

The sidebar is scoped to the directory `cb` was launched in. In a git repo
(including any linked worktree), scope is the repo's *common directory* — the
shared `.git` — so the main checkout and every linked worktree count as one
scope and their sessions appear together. Outside a git repo, scope is the
launch-dir subtree. `deriveScope` / `gitCommonDir` resolve this with pure
filesystem reads (no `git` subprocess); `repoCache` memoizes `cwd → common dir`
so the 500ms `list` poll doesn't re-walk per session.

Toggle with **prefix `a`**: `m.showAll` flips and `applyScope()` rebuilds the
visible list, re-seeds `prev` (so the next poll doesn't toast sessions that
merely came into view), and re-attaches the screen pane to whatever ends up
under the cursor. The launch-time `--all` flag starts unscoped. `scopeLabel`
renders the current mode in the sidebar header.

`cb` is meant to feel per-repo by default — adding scope-sensitive features
(filters, batch ops) should go through `visibleSessions` / `inScope`, not the
raw `allSessions` list.

## Gotchas (hard-won)

- **Terminal-query replies must be forwarded.** Claude Code (and any full-screen TUI) sends
  device-attribute / cursor-position queries at startup and *blocks* until the terminal
  replies. The emulator computes replies into an internal pipe; `Session.replyLoop` drains
  that pipe back to the child's PTY. Without it, claude hangs: blank screen, no input, no
  hooks. Don't remove it.
- **Backpressure.** Every session's PTY must be drained continuously even with no client
  attached, or the child blocks on write when the PTY buffer fills. `readLoop` always runs.
- **Stale daemon.** The daemon is long-lived and is *not* restarted by rebuilding the
  binary. `ensureDaemon` checks `ProtocolVersion` and refuses to proceed against a stale
  daemon. Bump `ipc.ProtocolVersion` on any wire change. To restart: `cb stop` or
  `pkill -f 'cb daemon'`.
- **Group kill.** Sessions are started with `Setsid` (via `pty.StartWithSize`), so
  `Session.Kill` signals the negative pid to take down claude *and* its subprocess tree.
- **Input fidelity.** `keys.go` re-encodes Bubble Tea `KeyMsg` to bytes. It covers common
  xterm sequences; kitty-keyboard / full mouse fidelity is not implemented. If a key
  misbehaves when typing into a session, that's the place to look.

## Conventions

- Keep the hook CLI a pure no-op observer (always exit 0; never exit 2, which would block
  Claude Code).
- The daemon is the source of truth for screen state; clients render frames it sends.
- Prefer adjusting `statusForEvent` over scraping terminal output for status.
