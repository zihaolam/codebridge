# cb v2: Native Remote Agent Control Plane

## 1. Product vision

Build a Rust-native desktop application for two related jobs:

1. daily-drive terminal work: shell, SSH, Neovim, Git, Docker, dev servers;
2. manage many coding-agent sessions, their tasks, projects, worktrees, and
   attention state from one fast workspace.

The application should feel like Zed: native, GPU-rendered, keyboard-first,
pane-based, and calm under sustained terminal output. It is not a web wrapper
and not a replacement code editor.

The important distinction is that a terminal session runs on a **host daemon**:
the machine with the actual repository, Git credentials, agent authentication,
and developer tools. A desktop client attaches to that daemon locally or over a
secure network connection.

## 2. Goals

- Native Rust host, transport, terminal stack, and desktop UI.
- Persistent PTY sessions that survive desktop disconnect/restart.
- A first-class shell terminal in every selected project/worktree.
- First-class agent sessions linked to durable task runs.
- Project scoping that understands Git repositories and linked worktrees.
- One active controller and many read-only observers per terminal.
- Raw PTY byte streaming, not pixel/video or fixed-rate screen-frame streaming.
- Direct remote host access first; optional cloud relay/control plane later.
- Excellent Neovim, shell, and agent-CLI behavior before secondary features.

## 3. Non-goals for the MVP

- Building a general-purpose code editor.
- Simultaneous multi-user typing into the same terminal.
- Cloud-hosted development workspaces, repository cloning, or secret vending.
- Browser/mobile clients.
- Diff, PR, MCP, issue tracker, and managed-process integrations.
- Pixel streaming or screen capture.

## 4. Product model

```text
Host
  └─ Project (Git common-directory identity)
      ├─ Worktree(s)
      ├─ Tasks
      │   └─ Task runs
      │       └─ Agent sessions
      └─ Sessions
          ├─ Shell sessions
          ├─ Agent sessions
          └─ Later: managed process sessions
```

### 4.1 Host

A host is a machine that runs the daemon. It can be the current laptop, a home
machine, a development VM, or a remote workstation. It owns processes and
filesystem access; it is not merely an SSH target.

### 4.2 Project and worktree

A project is identified by its Git common directory, so a main checkout and
linked worktrees are grouped together. A worktree stores its path, branch,
current Git metadata, and lifecycle state.

Outside a Git repository, a user-configured root path is a project.

### 4.3 Task and task run

A task is durable planning/work state: title, description, status, labels, and
project. A task run records one execution attempt: worktree, selected agent,
linked session, timestamps, and outcome. A task can have multiple runs.

### 4.4 Session

A session is one PTY-backed process tree running on a host:

```text
shell:  /bin/zsh -l
shell:  ssh production
shell:  nvim
agent:  codex
agent:  claude
process (later): npm run dev
```

All session types use the same terminal transport and lifecycle API.

## 5. Architecture

```text
┌──────────────────────────── Desktop App ────────────────────────────┐
│ GPUI workspace                                                       │
│ project/task/session panels · terminal tabs/splits · command palette│
│ libghostty-vt state + GPUI terminal renderer                         │
└───────────────────────────────┬─────────────────────────────────────┘
                                │ QUIC + TLS
┌───────────────────────────────┴─────────────────────────────────────┐
│ Host daemon                                                          │
│ project/worktree discovery · task store · PTY/session ownership      │
│ libghostty-vt canonical state · output journal · agent adapters      │
└─────────────────┬───────────────────────┬───────────────────────────┘
                  │                       │
             Shell PTYs               Agent PTYs
           zsh/nvim/ssh            codex/claude/etc.
```

The host daemon is authoritative for PTYs, process lifecycle, terminal state,
scrollback, tasks, and project metadata. The desktop is authoritative for its
layout, fonts, local viewport, tab/split arrangement, and visual selection.

## 6. Terminal architecture

### 6.1 Stack

```text
PTY output bytes
  ↓
libghostty-vt
  - VT parsing
  - terminal grid, scrollback, modes, resize/reflow
  - terminal-query replies
  - keyboard/mouse/focus encoding
  - incremental render state
  ↓
GPUI terminal element
  - GPU cell/glyph rendering
  - glyph atlas and font fallback
  - selection, copy, hyperlinks, hit testing
  - tabs, splits, focus, viewport crop/pan
```

GPUI is the native application and rendering framework. `libghostty-vt` is the
terminal parser/state engine. Neither replaces the other.

Use `libghostty-vt` from the start on both host and desktop. It is a C/Zig
library, so the Rust project must put a small owned C bridge in front of it and
never leak Ghostty internal types into application code.

### 6.2 Pinned native dependency

```text
vendor/ghostty/                    # pinned Git submodule/revision
native/ghostty-vt-bridge/          # owned, intentionally small C ABI wrapper
crates/terminal-engine/            # safe Rust API over the bridge
```

Pin an exact Ghostty revision. `libghostty-vt` is under active development and
its public API may change. Keep upgrade work isolated to the bridge and
`terminal-engine` crate.

The bridge should expose only v2-owned types and functions:

```c
cb_terminal_new(cols, rows, scrollback_limit)
cb_terminal_feed_output(bytes)
cb_terminal_resize(cols, rows)
cb_terminal_encode_key(event)
cb_terminal_encode_mouse(event)
cb_terminal_encode_focus(focused)
cb_terminal_paste(text)
cb_terminal_render_state()
cb_terminal_export_checkpoint()
cb_terminal_import_checkpoint()
```

`cb_terminal_import_checkpoint` is conditional on proving a complete and
correct restore method. Do not assume a visual grid alone restores all terminal
modes correctly.

### 6.3 Terminal protocol

Never stream pixels or periodic full-screen frames. Stream ordered PTY bytes.

```text
Host → client
  TerminalCheckpoint { session_id, sequence, rows, cols, state }
  PtyOutput          { session_id, sequence, bytes }
  TerminalResized    { session_id, rows, cols, controller_id }
  ControlChanged     { session_id, controller_id }

Client → host
  Input              { session_id, bytes }
  Paste              { session_id, text }
  Key / Mouse / Focus
  Resize             { session_id, rows, cols }      # controller only
  SetViewport        { session_id, rows, cols }      # no PTY resize
  AcquireControl     { session_id }
```

The host appends raw bytes with monotonically increasing sequence numbers to a
disk-backed journal. A reconnect receives a complete checkpoint followed by
missing ordered bytes.

The first engineering spike must prove checkpointing. If `libghostty-vt` cannot
fully serialize/restore state, use a tested reconstruction strategy:

1. replay a retained raw-output journal from a checkpoint; or
2. export a full VT reconstruction containing reset/modes/palette/grid/cursor
   and validate it against full-screen applications.

Do not ship a partial snapshot that only looks right for normal shell text but
breaks Neovim or agent TUIs.

### 6.4 Canonical viewport and controller lease

One shared PTY has exactly one canonical rows × columns size. Programs receive
one `SIGWINCH`; allowing every attached device to resize makes full-screen apps
constantly reflow and unusable.

```text
Canonical PTY: 160 × 48
Desktop controller: displays and may resize 160 × 48
Observer:           displays/crops/pans a local 60 × 22 viewport
```

- One client has a renewable controller lease.
- Only the controller sends input, mouse events, and `Resize`.
- Any number of observers can attach read-only.
- Observers use local crop/pan/zoom; `SetViewport` never changes the PTY.
- Control handoff is explicit; the new controller may then resize.

### 6.5 Initial capability policy

Start with `TERM=xterm-256color` plus truecolor. Support bracketed paste,
alternate screen, focus reporting, mouse reporting, OSC 8 links, and
synchronized output.

Kitty keyboard and image protocols are later negotiated capabilities. Do not
advertise a capability that observers or remote desktop clients cannot preserve
correctly.

## 7. Host daemon

### 7.1 Responsibilities

The daemon owns:

- persistent PTYs and their process groups;
- continuous PTY draining, even with no attached client;
- terminal-query replies back to child processes;
- canonical `libghostty-vt` terminal state and scrollback;
- output journal/checkpoints and reconnect replay;
- project/worktree discovery and task persistence;
- agent hook/event normalization;
- connection authorization, controller leases, and client backpressure.

It does not own desktop layouts, font preferences, client viewport offsets, or
client-local selection.

### 7.2 PTY lifecycle

- Spawn children in independent process groups/sessions.
- Interrupt, terminate, and kill target the entire child process group.
- Drain PTY output in a dedicated task with no dependency on client speed.
- Forward emulator-generated terminal replies back to the PTY.
- Store terminal output in segmented compressed files, not SQLite rows.
- Bound in-memory scrollback and implement configurable disk retention.
- On process exit, retain terminal history and mark session terminal-ended.

### 7.3 Agent adapters

Agent adapters know how to launch and observe Claude, Codex, and OpenCode.
They normalize agent-specific hook/notification data into generic events:

```text
starting | working | needs_approval | waiting_user | completed | failed | ended
```

Status must come from official agent hooks/protocols whenever available, not
terminal-text scraping. A safe, bracketed-paste initial prompt can be delivered
once an agent reports readiness, with a bounded fallback for hookless agents.

### 7.4 Storage

SQLite stores structured state:

- host identity and paired devices;
- projects, worktrees, tasks, task runs;
- session metadata and lifecycle events;
- configuration and saved terminal presets.

Terminal journals live outside SQLite:

```text
~/.cb-v2/terminals/<session-id>/
  metadata.json
  checkpoint-000001.zstd
  output-000001.zstd
  output-000002.zstd
```

## 8. Networking and security

### 8.1 MVP: direct host access

The initial remote deployment is direct desktop-to-host QUIC over TLS.

- Host binds loopback-only by default.
- Remote access is enabled explicitly.
- Support Tailscale/WireGuard and SSH forwarding in documentation first.
- Pair devices with host approval and mutual TLS certificates.
- Scope client permissions: observe, control terminal, create sessions, manage
  projects/tasks, administer host.

Avoid an internet-exposed daemon secured solely by a static bearer token.

### 8.2 Later: relay/control plane

Add a cloud relay only after direct remote access is solid.

```text
Desktop ── encrypted connection ── Relay ── outbound encrypted connection ── Host
```

The host keeps an outbound connection so it works behind NAT. The relay handles
account/device registration, host presence, routing, and notifications. The host
still owns source files, credentials, PTYs, and terminal state. Terminal content
should be end-to-end encrypted wherever practical.

### 8.3 Transport

Use `quinn`/QUIC and Protobuf (`prost`):

- a control RPC stream for commands and responses;
- a server-push state stream for projects, tasks, and sessions;
- one bidirectional stream per attached terminal;
- sequence numbers, acknowledgements, resumable output replay;
- bounded queues per client.

Slow observers must be moved to a fresh checkpoint rather than ever blocking
PTY draining or live-controller output.

## 9. Desktop UX

### 9.1 Layout

```text
┌ Activity rail ┬───────── Project / Task / Session panel ────────┬────────┐
│ Projects      │ search · filters · hosts · worktrees             │ details│
│ Tasks         │                                                   │ task   │
│ Sessions      │  project                                          │ run    │
│ Hosts         │   ├─ worktree                                     │ agent  │
│               │   │   ├─ task run                                 │ state  │
│               │   │   └─ shell sessions                           │        │
├───────────────┴──────────────────────────────────────────────────┴────────┤
│ Terminal workspace: tabs · splits · active terminal                         │
├────────────────────────────────────────────────────────────────────────────┤
│ status: host · worktree · controller · latency · terminal dimensions        │
└────────────────────────────────────────────────────────────────────────────┘
```

The center terminal workspace is the primary surface. The left side provides
context and navigation; the right inspector is optional and collapsible.

### 9.2 Core interactions

- Command palette covers every major action.
- New shell opens in selected worktree/project.
- New agent run opens against a task and selected/created worktree.
- Terminal tabs and splits persist locally across desktop restart.
- Task runs display live status and attention state.
- Global attention queue jumps through approval/waiting sessions.
- Native notifications fire for approval, completion, host disconnect, and
  process failure.
- Typical keypresses are forwarded to the active terminal untouched; only
  explicit app commands are intercepted.

Suggested commands:

```text
cmd/ctrl+p        command palette
cmd/ctrl+shift+t  new shell in selected worktree
cmd/ctrl+shift+a  new agent run for selected task
cmd/ctrl+1..9     focus terminal tab
cmd/ctrl+\\        split terminal workspace
cmd/ctrl+w        close terminal view (does not kill session)
cmd/ctrl+shift+k  interrupt active session
cmd/ctrl+shift+x  terminate active session
cmd/ctrl+shift+g  jump to next needs-attention session
```

## 10. Rust workspace

```text
Cargo.toml
proto/v2.proto
vendor/ghostty/
native/ghostty-vt-bridge/
crates/
  protocol/              # DTOs, protocol version, generated protobuf types
  core/                  # domain types and task/session state machines
  transport/             # QUIC, mTLS, multiplexing, reconnect
  pty/                   # platform-specific PTY/process abstraction
  terminal-engine/       # safe libghostty-vt wrapper and checkpoint adapter
  terminal-renderer/     # GPUI terminal Element and GPU glyph/cell renderer
  host-store/            # SQLite and output-journal persistence
  host-git/              # repository/worktree discovery and operations
  host-agents/           # agent launchers, hooks, normalized events
  host/                  # daemon binary
  desktop-ui/            # GPUI views, themes, commands, pane layout
  desktop/               # desktop binary
  cli/                   # daemon-backed administration/scripting CLI
  relay/                 # later optional relay/control-plane binary
docs/
  architecture.md
  protocol.md
  terminal.md
  security.md
  adr/
```

`core` must not depend on GPUI, Tokio, SQLite, PTY APIs, or Ghostty. It is the
unit-testable domain layer.

## 11. Performance requirements

- No WebView, Electron, React, xterm.js, or full-screen JSON repaint loop.
- PTY read, terminal parsing, network forwarding, persistence, and GPUI drawing
  use independent tasks/queues with no blocking lock across stages.
- The terminal renderer renders only dirty rows/cells and coalesces work to the
  display refresh rate.
- Use a glyph atlas and cache shaped terminal glyph runs.
- Bound all client queues and journal memory.
- Never allocate a full terminal string on each redraw.
- Support high-DPI displays, font fallback, wide characters, combining marks,
  emoji, selection, cursor shapes, hyperlinks, and truecolor.

Benchmark before optimizing speculative details:

1. Neovim daily-driver fixture;
2. sustained log flood;
3. Codex/Claude synchronized redraw fixture;
4. 20 live sessions;
5. one controller plus 10 observers;
6. sleep/wake and reconnect replay;
7. large scrollback retention.

## 12. Delivery phases

### Phase 0 — foundation and Ghostty proof

Deliver:

- Rust workspace, CI, formatting, clippy, integration-test harness.
- Protobuf protocol versioning and structured errors.
- Pinned Ghostty source plus owned C bridge.
- Minimal GPUI window rendering a `libghostty-vt` terminal grid.
- Local PTY, input, resize, terminal-query replies, and alternate-screen tests.
- Checkpoint/replay proof with Neovim and an agent CLI.

Acceptance: a minimal native terminal can run Neovim and reconnect without
terminal corruption, lost output, or broken input modes.

### Phase 1 — local persistent terminal vertical slice

Deliver:

- `cb-host` daemon starts persistent shell PTYs.
- Desktop discovers/connects to local host.
- Attach, input, paste, selection/copy, resize, interrupt, terminate.
- Raw output journal and restart/reconnect.
- Controller lease and observer viewport semantics.

Acceptance: use the desktop terminal as the primary local Neovim/shell terminal
for at least one week.

### Phase 2 — projects, worktrees, and workspace UI

Deliver:

- Project discovery and configured roots.
- Git common-directory grouping and worktree list/create/delete.
- Zed-like project/session side panel.
- New shell in a selected worktree.
- Local terminal tabs/splits and workspace restoration.

Acceptance: manage multiple repositories and worktree-scoped shell sessions;
desktop layout restores correctly after restart.

### Phase 3 — task and agent orchestration

Deliver:

- Task CRUD and task-run history.
- Claude and Codex launch adapters.
- Initial prompt prefill and normalized hook-based status.
- Attention queue, status indicators, and notifications.
- Link/unlink task runs and resume ended/paused work.

Acceptance: create a task, launch an agent in a worktree, receive its status,
open a shell beside it, and attach to the task run later.

### Phase 4 — remote host MVP

Deliver:

- QUIC/TLS remote connection and device pairing.
- Host picker, connection state, and remote project navigation.
- Remote shell and agent sessions.
- Reconnect after network changes/laptop sleep.
- Tailscale/WireGuard/SSH-forwarding setup documentation.

Acceptance: daemon runs on another machine containing the repositories; the
desktop can use remote Neovim, create a task, launch an agent, and reconnect
without losing the session.

### Phase 5 — cloud relay/control plane

Deliver:

- Outbound host tunnel and NAT traversal through a relay.
- Account/device registration and host presence.
- Encrypted terminal relay and push-notification fanout.
- Optional replicated task/project metadata.

Acceptance: desktop can attach to an authorized host behind NAT without opening
a public port on that host.

### Phase 6 — hardening and expansion

Deliver:

- Terminal compatibility/torture fixtures.
- Failure recovery, host upgrades, and journal retention policies.
- Signed releases, auto-update, profiling, and optional telemetry.
- Later features: managed processes, diffs, PR context, MCP, browser/mobile
  clients, cloud workspaces.

## 13. Decisions that must not drift

1. **Host owns execution.** Repositories, credentials, PTYs, and agent
   processes stay on the configured host; cloud infrastructure does not execute
   them in the MVP.
2. **Raw bytes plus checkpoints.** Never use video/pixel streaming or a
   fixed-rate full-screen frame protocol.
3. **One controller lease.** A shared terminal has one canonical size and one
   input owner.
4. **GPUI plus libghostty-vt.** GPUI owns application UI; libghostty-vt owns
   terminal behavior; the app owns the bridge and renderer.
5. **Shell first.** Make Neovim and ordinary terminal workflows excellent before
   adding product-surface features that dilute the terminal work.
6. **Security by pairing.** Remote process/filesystem authority requires device
   identity and scoped permissions, not a publicly exposed static token.

## 14. References

- [Ghostty and libghostty](https://github.com/ghostty-org/ghostty)
- [libghostty-vt API reference](https://libghostty.tip.ghostty.org/)
- [Ghostling: minimal libghostty terminal example](https://github.com/ghostty-org/ghostling)
- [GPUI](https://gpui.rs/)
- [Arbor: relevant GPUI/daemon/worktree/terminal reference](https://github.com/penso/arbor)
