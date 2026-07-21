# Codebridge repository guide

`codebridge` is a Rust TUI (`cb`) for managing Claude Code, Codex, and other
coding-agent sessions. A durable **conductor** process owns every agent PTY and
its libghostty-vt terminal state; a restartable **broker** process owns the
control plane (tasks, status, hooks, web). A Ratatui client renders the
Codebridge sidebar and one selected live session. It is intentionally an
agent-session manager, not a general-purpose shell or tmux replacement.

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

Use `cb restart` after a broker-protocol-changing rebuild (it cycles the broker
only; the conductor and its PTYs keep running). A stale broker is rejected by
protocol version. To move the conductor itself onto new code without dropping
sessions, use `cb upgrade` (in-place `execve`, sessions preserved). `cb stop`
tears down both processes.

## Architecture

- `src/terminal/` owns the safe libghostty-vt wrapper and generated FFI. It
  handles semantic styled cells, graphemes, Ghostty viewport scrollback,
  reflow, alternate screen, synchronized output, query replies, text
  extraction, paste, focus, and mouse encoding.
- `src/session.rs` owns one child process group under a PTY. Its reader drains
  output continuously even without clients, feeds Ghostty, and notifies dirty
  subscribers. The waiter reaps the child. Killing a session signals the whole
  Unix process group. A session is either spawned fresh or `adopt`ed from a
  surviving PTY master fd + child pid across a conductor hot-upgrade; adopted
  sessions resize via raw `TIOCSWINSZ` and reap via `waitpid` (portable_pty
  cannot rebuild its handles from a bare fd).
- `src/conductor.rs` is the durable session engine on its own socket
  (`conductor.sock`, own `CONDUCTOR_VERSION`). It owns the session map, spawn/
  kill/reap, extract, status/name/harness metadata, the semantic attach frame
  stream, and per-attachment absolute scroll anchors (immediate frame, then only
  changed frames). It survives a broker restart and can hot-upgrade itself in
  place (`cb upgrade`): snapshot each terminal to replayable VT, clear CLOEXEC on
  the master fds, `execve` the new binary in the same pid, then adopt every
  session. The broker reaches it through the `Engine` trait — `ConductorClient`
  over the socket in production, an in-process `Conductor` in tests.
- `src/daemon.rs` is the broker: the control plane on `daemon.sock`. It owns
  tasks, hook→status mapping, watchers, and web spawn, and delegates every
  session fact to the conductor via the `Engine` trait. It is never in the data
  path — clients attach straight to the conductor — so it can restart freely. It
  reaps/parks and refreshes promptly on lifecycle pokes the conductor pushes when
  a session exits (`WatchLifecycle`), with its watch poll as a fallback.
- `src/protocol.rs` is line-delimited JSON between clients and the broker (one-
  shot requests and streaming messages). Bump `VERSION` on every wire-shape or
  semantic wire change. The conductor's socket protocol carries its own
  `CONDUCTOR_VERSION` (in `src/conductor.rs`), deliberately decoupled so a broker
  restart never disturbs a running conductor.
- `src/tui.rs` is client presentation state: sidebar focus and grouping,
  prefix/config/help modals, worktree and task pickers, toasts, selection,
  OSC52, and input forwarding. Rendering is pure; `compute_view` mutates
  geometry before drawing.
- `src/theme.rs` owns Herdr-style semantic UI palettes, built-in dark/light
  themes, terminal-native colors, and per-token overrides. Themes apply to
  Codebridge chrome and never alter agent terminal cells.
- `src/sidebar.rs` implements repo-common-dir scope grouping, flat/accordion
  modes, stable logical selection, and attention jumps.
- `src/task.rs` persists the daemon-owned backlog and multi-run state.
  `src/codex.rs` attributes Codex rollout IDs for precise resume.
- `src/integration.rs` follows Herdr's durable integration model: versioned
  product-owned hook scripts, structural JSON edits, preservation of user
  hooks, current/outdated status, safe uninstall, and Codex's top-level
  `[features] hooks = true` migration.
- `src/web.rs` serves the embedded PWA and one authenticated multiplexed
  WebSocket per browser. It talks to the broker for enriched snapshots (with
  filesystem scope data) and attaches straight to the conductor for the frame
  stream, adapting semantic frames to the PWA's ANSI/xterm contract. Normal phone
  attaches never resize the canonical PTY.
- `src/config.rs`, `src/worktree.rs`, and `src/notify.rs` provide persistent
  bindings, per-launch worktree/agent selection, and delayed/focus-aware
  notification delivery through Codebridge, terminal OSC, or native services.
- `web/` is the Vite/React/xterm.js PWA source. Frames are written directly to
  xterm outside React state.
- `vendor/libghostty-vt` is pinned terminal-engine source. Keep its provenance
  metadata intact when updating it.

## Process model

Two long-lived processes plus short-lived clients:

- **Conductor** (`cb conductor`, `conductor.sock`): the durable data plane — PTYs,
  child process groups, terminal state, and the attach frame stream. Spawned on
  demand by `ensure_conductor`, it outlives broker restarts. A stale conductor is
  never auto-restarted (that would kill live sessions); a version mismatch is
  surfaced for the user to resolve with `cb stop`.
- **Broker** (`cb daemon`, `daemon.sock`): the control plane — tasks, status,
  hooks, watchers, web — talking to the conductor via the `Engine` trait.
- **Clients** (`cb` TUI, `cb web` browsers): use the broker for sidebar/tasks/
  status and attach directly to the conductor for frames/input.

Lifecycle: `cb restart` cycles the broker only (conductor + PTYs survive);
`cb upgrade` hot-upgrades the conductor in place via `execve` (same pid → agents
stay its children → `waitpid` still yields exit codes), snapshotting each
terminal to replayable VT, handing the surviving PTY fds to the successor, and
adopting them, confirmed by the `boot_id` changing while the pid holds; `cb stop`
tears down both.

## Status model

Hooks are no-op observers; never scrape terminal text for status.

- `SessionStart` -> `idle`
- `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolBatch` -> `working`
- `PermissionRequest` -> `needs_approval`
- permission-like `Notification` -> `needs_approval`
- other `Notification`, `Stop`, and `StopFailure` -> `waiting_user`
- `SessionEnd` -> `ended`
- unknown active events -> `working`

Sidebar indicators are a green spinner for working, green `●` for turn
complete, yellow `●` for newly idle, cyan `…` for starting, red `⚑` for
approval, and grey `✗` for ended. Toast and native notification transitions
occur only when a previously observed session enters approval or turn-complete,
and only for sessions in the launch workspace scope unless the accordion (global
view, `prefix a`) is on. Status tracking stays unconditional, so a transition
dropped as out-of-scope never misfires once the accordion is later toggled on.

Interrupting a Claude turn with Escape fires no hook, so a working session
would otherwise spin forever. The TUI sends an `interrupt_check` stream message
just before the Escape byte; the conductor confirms the interrupt against
Claude's own transcript (`transcript_path`, captured from every hook payload)
rather than guessing from the keystroke. It snapshots the transcript length
first, then briefly polls for a `[Request interrupted by user]` marker in the
bytes appended afterwards; only a fresh marker clears the spinner to
`waiting_user`. This is positive-only and scans solely the newly appended
bytes, so a still-running turn or a stale marker never produces a false
turn-complete — the worst case is the unchanged (stuck) spinner. Codex is
unaffected (it may already `Stop` on interrupt; untested).

A session whose agent exits deliberately (`/exit`, a normal quit — exit code
`0`, no signal) is auto-closed: the daemon reaps it exactly like an explicit
`kill` (removed from the map, its run parked and resumable via `prefix m`), and
the attached client advances to a neighbouring session. A session that crashes
(non-zero exit or a terminating signal) is left in place with its grey `✗` row
so the failure stays visible; it can still be killed or resumed. The waiter
captures the child's exit status to make this distinction; `reap_exited_sessions`
runs on every `list`/`watch` snapshot and never signals a reaped (possibly
recycled) pid.

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

Spawning a known agent does not record it immediately; the session is
auto-recorded as an `auto` task carrying one run only on its first user turn
(the first `UserPromptSubmit` hook, via `ensure_auto_session`), so an untouched
spawn never clutters the picker and a session killed before any prompt leaves no
record (there is nothing to resume). Once recorded, any killed session stays
resumable without keeping its process resident. Killing frees the process group
and immediately parks the run as paused, capturing the agent-native resume id
off the session first. The run's `first_message` is captured from that same
first `UserPromptSubmit` hook (or seeded from a task prefill). Prefix `m`
(`session_history`) opens a scope-filtered picker of every session in the
workspace labelled by first message — both live runs and paused ones. Entering a paused run runs the native resume in a fresh
PTY; entering a live run jumps to the already-running session (resuming it would
spawn a duplicate). Auto tasks are hidden from the backlog and surfaced only in
that picker.

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
- Keep the conductor's socket protocol decoupled from the client `VERSION`: a
  broker rebuild must not force a conductor restart. Bump `CONDUCTOR_VERSION`
  only for a real conductor-wire change, and never auto-restart a mismatched
  conductor (it would kill live sessions).
- Conductor hot-upgrade preserves the pid, so it stays the child's parent and
  `waitpid` still yields exit codes; re-parenting to init would lose the clean/
  crash distinction. Clear CLOEXEC on the master fds before `execve`, and snapshot
  the terminal to replayable VT as late as possible — output the old reader
  consumes in the tiny window before `execve` is the only thing that can be lost
  (unread kernel PTY bytes survive for the successor to drain).

## Release

Tag pushes run `.github/workflows/release.yml`: build the PWA, run fmt/clippy/
tests, compile natively for macOS and Linux on x86_64 and arm64, archive `cb`,
and publish checksums. `install.sh` downloads those archives or builds from
source with Rust, Zig, and Node.js.

### Cutting a release

The version lives in exactly one place: `version` in `Cargo.toml`. `Cargo.lock`
picks it up on the next `cargo build`. Nothing else embeds the version (not
`README.md`, `install.sh`, or `web/package.json`), so do not grep for it
elsewhere.

Releases are plain annotated tags on `main`; there is no dedicated "release"
commit. The version bump rides along in the substantive commit, which is then
tagged `vX.Y.Z` (tag name must match `Cargo.toml` exactly, `v`-prefixed).
Pushing the tag is what triggers the workflow. Semver intent: bug-fix releases
bump patch, user-facing features bump minor (pre-1.0, so both stay under `0.`).

Steps to cut a release for a change already committed locally:

```sh
# 1. bump `version` in Cargo.toml (edit), then sync the lockfile
cargo build
# 2. amend the bump into the change commit, or add it as the final commit
git add Cargo.toml Cargo.lock && git commit --amend --no-edit   # or a fresh commit
# 3. tag the tip and push branch + tag together
git tag vX.Y.Z
git push origin main vX.Y.Z
```

CI runs the same fmt/clippy/test gate, so make it green locally first. A
protocol-version bump (`src/protocol.rs`) means users must `cb restart` after
upgrading; call that out in release notes. If the release also changes conductor
code (`src/conductor.rs`, `src/session.rs`, `src/terminal/`), tell users to run
`cb upgrade` to move running sessions onto the new build without losing them —
`cb restart` alone leaves the old conductor (and its sessions) in place.
