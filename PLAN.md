# Codebridge Rust rewrite

## Product boundary

Codebridge remains an agent-session multiplexer and manager:

- one left panel for Claude, Codex, and other supported coding-agent sessions;
- one live, interactive selected-agent panel;
- hook-driven attention state, tasks, worktrees, configuration, notifications,
  and the existing phone bridge;
- persistent PTYs owned by a daemon, with no tmux dependency.

It is not becoming a general-purpose terminal multiplexer, shell workspace,
terminal emulator product, desktop IDE, or Herdr clone. Herdr is an
implementation reference for terminal rendering, scrollback, dirty-frame
delivery, daemon/runtime separation, and managed integration registration only.

## Rendering architecture

```text
agent PTY output
  -> continuously drained daemon reader
  -> server-owned libghostty-vt terminal
  -> Ghostty viewport (live or scrollback)
  -> semantic styled-cell frame
  -> Ratatui main-panel cells
```

This replaced the former rendered-ANSI screen protocol. The
libghostty-vt viewport owns scrollback semantics, reflow, alternate-screen
state, graphemes, styles, cursor visibility, synchronized output, and terminal
query replies.

The left panel remains Codebridge-owned and client-local. Layout is computed
before pure rendering. When scrollback exists, the selected-agent panel
reserves one column for a scrollbar, following the useful Herdr behavior.

Each attached client has a scroll anchor. Entering history records an absolute
Ghostty row so new agent output does not move the visible history. Returning to
offset zero resumes live follow. One client's scroll state does not change the
canonical live viewport seen by another client.

## Daemon architecture

The Rust daemon owns:

- the agent process and PTY lifecycle;
- continuous PTY draining independent of attached clients;
- a bounded libghostty-vt scrollback buffer per session;
- terminal-query replies written back to the child PTY;
- session status, names, agent resume IDs, tasks, and watchers;
- event-driven dirty notifications for attached clients;
- Unix-socket request, watch, and attach streams.

Clients own:

- sidebar cursor, focus, accordion expansion, modals, and key-prefix state;
- their selected session and scroll anchor;
- selection and clipboard presentation;
- terminal viewport size for their controlling attachment.

The daemon sends a frame immediately and thereafter only when terminal state or
client viewport state changes. It does not poll and rebuild full ANSI strings at
30 fps.

## Hook and integration architecture

Hooks remain no-op observers and status remains hook-driven rather than scraped
from terminal output.

Following Herdr's durable registration model, Codebridge installs a versioned,
product-owned hook artifact and structurally edits the agent's native JSON:

- Claude: `$CLAUDE_CONFIG_DIR` or `~/.claude`, with `settings.json`;
- Codex: `$CODEX_HOME` or `~/.codex`, with `hooks.json`;
- current/outdated/not-installed integration status;
- idempotent reinstall that repairs stale commands;
- uninstall that removes only Codebridge-owned commands/files;
- preservation of unrelated user hooks and settings;
- a backup plus atomic replacement for edited JSON.

Unlike Herdr's current identity-only registration, Codebridge registers the
lifecycle events required by its existing status model. Hook execution remains
gated by `CB_SESSION`, bounded, best-effort, and always exits successfully.

## Compatibility contract

The rewrite is complete only when the current Codebridge behavior remains:

- sidebar status glyphs, colors, transition toasts, focus, and prefix commands;
- repository-common-directory workspace grouping and accordion behavior;
- spawn, kill, rename, jump-to-attention, and configurable bindings;
- worktree picker with per-launch agent choice;
- task backlog, task runs, prefill, resume IDs, and reconciliation;
- mouse selection, OSC52 copy, scroll-mode navigation, and autoscroll;
- Claude/Codex hook install, status mapping, and Codex resume attribution;
- daemon watch streams, web bridge, authentication, scope grouping, and PWA;
- notification behavior and CLI compatibility;
- session survival across TUI disconnects.

The former implementation was used as a behavioral oracle during the rewrite;
its build path has now been removed after side-by-side protocol, PTY, and UI
checks.

## Current implementation

Implemented:

- Rust package and `cb` binary;
- pinned vendored libghostty-vt with generated Rust FFI;
- safe server-owned terminal wrapper;
- Ghostty cell, color, style, grapheme, cursor, resize, and scrollback render;
- terminal-query reply forwarding;
- portable PTY session runtime with continuous draining;
- dirty-generation subscriptions;
- semantic cell frame protocol;
- Rust Unix-socket daemon with ping/list/spawn/kill/rename/hook/watch/attach;
- per-client absolute scroll anchoring and live-follow restore;
- initial Ratatui sidebar plus selected-agent main panel;
- repository-common-directory scoping, flat/accordion modes, stable logical
  selection, workspace navigation, global status counts, and attention jumps;
- hook-driven approval/turn-complete transition toasts and session rename UI;
- two-stage git worktree launcher with installed-agent filtering and a fresh
  Claude/Codex/OpenCode choice for every launch;
- persisted configurable prefix and named action bindings, including
  `CB_PREFIX` precedence and an in-app rebinding/reset modal;
- modified xterm/Kitty key encoding, host bracketed-paste capture,
  Ghostty-mode-aware paste encoding, and child focus reporting;
- absolute-row mouse drag selection across live/scrollback viewports,
  Ghostty formatter-based extraction, selection highlighting, OSC52 copy, and
  scroll-edge drag movement;
- daemon-owned task persistence/migration, CRUD, parallel runs, agent-native
  resume, hook/fallback prefill, live-run reconciliation, and the complete
  scope-local backlog modal;
- Codex rollout-journal attribution with cwd/time matching and atomic
  claim-once semantics for precise `codex resume <id>` task runs;
- deadlock-free cloned child signaling plus Unix process-group termination;
- characterization tests for alternate-screen restore, synchronized output,
  resize/reflow, wide and combining graphemes, absolute scroll anchors, paste,
  terminal replies, and process-tree kill;
- raw keyboard forwarding, resize, spawn, kill, focus, and scroll mode;
- versioned Claude/Codex hook install/status/uninstall;
- Herdr-style Codex hook feature enablement and legacy-flag migration;
- child mouse reporting through libghostty-vt with Shift as the local
  selection override;
- clickable attention toasts, worktree markers, hooks warning, and a dynamic
  prefix-command panel;
- native macOS/Linux notifications;
- authenticated phone bridge, semantic-to-ANSI PWA adapter, shared watch
  stream, scope enrichment, viewport cropping, tasks, worktrees, and token/QR
  commands;
- daemon auto-start, stale-protocol rejection, stop/restart/version commands;
- Rust-native install and four-platform release packaging;
- unit tests and daemon/PTY/TUI smoke tests.

Cutover audit evidence:

- current Claude Code 2.1.211 and Codex 0.144.5 both produced nonblank
  full-screen semantic frames, answered terminal startup queries, and accepted
  input in disposable daemon-owned PTYs;
- the embedded PWA passed a real loopback HTTP/WebSocket check covering token
  auth, hello/version, scope-enriched snapshots, non-resizing attach, and
  Ghostty-cell-to-ANSI frame delivery;
- disposable daemon lifecycle checks covered auto-start, version ping,
  restart, and stop;
- process-tree tests prove session kill terminates descendants;
- the old Go source and Go release path were removed only after these checks.

## Verification gates

Every migration slice must pass:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Terminal changes also require PTY tests covering:

- startup terminal queries and replies;
- alternate screen and synchronized output;
- resize/reflow;
- wide and combining graphemes;
- live follow versus anchored scrollback under new output;
- input fidelity;
- disconnect and reattach;
- child exit and process-tree kill.

Release CI repeats the full Rust gates, builds the PWA, and compiles native
archives on current GitHub-hosted macOS/Linux runners for x86_64 and arm64.
