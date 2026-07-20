# Codebridge repository guide

`codebridge` is a Rust TUI (`cb`) for managing Claude Code, Codex, and other
coding-agent sessions. A long-lived daemon owns every agent PTY and its
libghostty-vt terminal state. A Ratatui client renders the Codebridge sidebar
and one selected live session. It is intentionally an agent-session manager,
not a general-purpose shell or tmux replacement.

## Build, test, and run

```sh
cargo build
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The vendored libghostty-vt build requires Zig 0.15.2. `build.rs` uses `ZIG`
when set, then the matching ignored `.tools/zig-<target>-0.15.2/zig`, then
`zig` on `PATH`.

The phone PWA is embedded at Rust compile time from `internal/web/dist`. Build
it before release builds or after changing `web/`:

```sh
cd web && npm ci && npm run build
```

Interactive checks require a real PTY:

```sh
./target/debug/cb daemon
./target/debug/cb ctl spawn claude
./target/debug/cb
```

Use `cb restart` after a protocol-changing rebuild. A stale daemon is rejected
by protocol version.

## Architecture

- `src/terminal/` owns the safe libghostty-vt wrapper and generated FFI. It
  handles semantic styled cells, graphemes, Ghostty viewport scrollback,
  reflow, alternate screen, synchronized output, query replies, text
  extraction, paste, focus, and mouse encoding.
- `src/session.rs` owns one child process group under a PTY. Its reader drains
  output continuously even without clients, feeds Ghostty, and notifies dirty
  subscribers. The waiter reaps the child. Killing a session signals the whole
  Unix process group.
- `src/daemon.rs` owns sessions, tasks, status, Unix-socket request/watch/attach
  streams, and per-attachment absolute scroll anchors. The daemon sends an
  immediate semantic frame and then only changed frames.
- `src/protocol.rs` is line-delimited JSON for one-shot requests and streaming
  messages. Bump `VERSION` on every wire-shape or semantic wire change.
- `src/tui.rs` is client presentation state: sidebar focus and grouping,
  prefix/config/help modals, worktree and task pickers, toasts, selection,
  OSC52, and input forwarding. Rendering is pure; `compute_view` mutates
  geometry before drawing.
- `src/sidebar.rs` implements repo-common-dir scope grouping, flat/accordion
  modes, stable logical selection, and attention jumps.
- `src/task.rs` persists the daemon-owned backlog and multi-run state.
  `src/codex.rs` attributes Codex rollout IDs for precise resume.
- `src/integration.rs` follows Herdr's durable integration model: versioned
  product-owned hook scripts, structural JSON edits, preservation of user
  hooks, current/outdated status, safe uninstall, and Codex's top-level
  `[features] hooks = true` migration.
- `src/web.rs` is a daemon client that serves the embedded PWA and one
  authenticated multiplexed WebSocket per browser. It enriches snapshots with
  filesystem scope data and adapts semantic frames to the PWA's ANSI/xterm
  contract. Normal phone attaches never resize the canonical PTY.
- `src/config.rs`, `src/worktree.rs`, and `src/notify.rs` provide persistent
  bindings, per-launch worktree/agent selection, and native notifications.
- `web/` is the Vite/React/xterm.js PWA source. Frames are written directly to
  xterm outside React state.
- `vendor/libghostty-vt` is pinned terminal-engine source. Keep its provenance
  metadata intact when updating it.

## Status model

Hooks are no-op observers; never scrape terminal text for status.

- `SessionStart` -> `idle`
- `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolBatch` -> `working`
- `PermissionRequest` -> `needs_approval`
- permission-like `Notification` -> `needs_approval`
- other `Notification` and `Stop` -> `waiting_user`
- `SessionEnd` -> `ended`
- unknown active events -> `working`

Sidebar indicators are a green spinner for working, green `●` for turn
complete, yellow `●` for newly idle, cyan `…` for starting, red `⚑` for
approval, and grey `✗` for ended. Toast and native notification transitions
occur only when a previously observed session enters approval or turn-complete.

## Workspace and tasks

The launch repo is the default flat scope. A repo's main checkout and linked
worktrees share the canonical common `.git` directory as their scope key.
Outside git, the launch directory is the scope. Prefix `a` toggles the global
accordion. Scope-sensitive features must use sidebar/task scope helpers rather
than raw session lists.

Tasks are persisted by the daemon only. Starting creates a fresh run; resuming
reuses a selected paused run and the agent-native resume identity. Claude uses
`--resume`, Codex uses `resume <id>` (or `resume --last` as a fallback), and
OpenCode uses `--continue`. Prefill is queued until the first hook or a bounded
fallback timer.

## Hard-won invariants

- Always forward terminal-query replies from libghostty-vt to the child PTY.
  Full-screen agents can block at startup without them.
- Always drain PTY output. Backpressure can freeze an unattended agent.
- Keep scrollback in Ghostty. An attachment entering history records an
  absolute row so new output cannot move that view; offset zero resumes live
  follow.
- Suppress intermediate frames while synchronized-output mode 2026 is active.
- Preserve semantic cells on the daemon wire. The terminal client draws cells;
  only the web compatibility adapter produces ANSI.
- A phone viewport is presentation-only. Only the explicit resize action may
  resize the shared PTY.
- Mouse reporting belongs to the child when its terminal mode requests it.
  Shift is the local in-app selection override.
- Hooks must be bounded, best-effort observers and always exit successfully.
  `CB_SESSION` correlates events; absence of it makes the hook a no-op.
- Do not disable Codex's shared `features.hooks` flag during uninstall; remove
  only Codebridge's entries and owned script.
- Preserve unrelated user configuration and dirty-worktree changes.

## Release

Tag pushes run `.github/workflows/release.yml`: build the PWA, run fmt/clippy/
tests, compile natively for macOS and Linux on x86_64 and arm64, archive `cb`,
and publish checksums. `install.sh` downloads those archives or builds from
source with Rust, Zig, and Node.js.
