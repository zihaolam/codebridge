# CLAUDE.md

Guidance for working in this repo. `command-center` is a Go TUI (`cb`) that manages many
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
`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse` → `working`;
`Notification`/`PermissionRequest` → `needs_approval` (stores the message);
`Stop` → `waiting_user`; `SessionEnd` → `ended`. The hook CLI forwards *any* event name, so
the mapping is the single place to adjust if Claude Code's event names change between
versions.

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
