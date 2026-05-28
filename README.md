# command-center

A terminal UI for managing many [Claude Code](https://claude.com/claude-code) (and
[Codex](https://developers.openai.com/codex)) sessions from one place. Instead of one
terminal tab per session — with no central view of which session needs your attention —
`command-center` gives you a single split view: a session list on the left and the
selected session's **live, interactive** screen on the right, with status that updates as
each session works, waits for approval, or finishes its turn. A tiled multi-session grid
is one keypress away.

It is a single self-contained Go binary (`cb`). It owns each session's pseudo-terminal
directly — **no tmux dependency** — and a long-lived daemon keeps your sessions alive even
when the UI is closed.

## How it works

```
            ┌──────────────── cb daemon (long-lived) ────────────────┐
 claude #A ◄┤ each claude runs under a PTY; output is drained into a    │
 claude #B ◄┤ virtual-terminal emulator (the authoritative screen).     │
 claude #C ◄┤ CB_SESSION is injected into each child so Claude Code  │
            │ hooks can report status back.                              │
 hooks ─────┤ unix socket: session list + status, screen streaming,     │
            │ input, control commands. Survives client disconnects.      │
            └───────────────────────────▲───────────────────────────────┘
                                         │ attach / detach
                          ┌──────────────┴───────────────┐
                          │  cb TUI (Bubble Tea)          │
                          │  sidebar + live screen pane,  │
                          │  or a tiled grid of sessions  │
                          └───────────────────────────────┘
```

The dashboard is a two-zone view: a session list (left) and the selected session's live
screen (right). `Ctrl-a` then `→` focuses the screen pane so your keystrokes go straight
to that session; `Ctrl-a` then `←` returns to the list.

Status comes from the **hook** system, not from scraping the terminal: the daemon maps
`SessionStart` → *ready*, `UserPromptSubmit`/`PreToolUse`/`PostToolUse` → *working*,
`Notification`/`PermissionRequest` → *needs approval*, `Stop` → *waiting for you*,
`SessionEnd` → *ended*. Codex uses the same hook shape, so the mapping is shared.

## Install

Requires Go 1.24+.

```sh
./install.sh          # builds cb, installs it to ~/.cb/bin, adds it to your PATH,
                      # and registers hooks for whichever of claude / codex you have
```

`install.sh` is sudo-free (it uses the bun/rustup pattern: its own `~/.cb/bin` dir plus a
`CB_INSTALL`/`PATH` line in your shell rc). To do it by hand:

```sh
go build -o ./cb .
./cb install-hooks    # wires cb into ~/.claude/settings.json (writes a .bak)
./cb install-codex    # wires cb into ~/.codex/hooks.json (leaves config.toml alone)
```

Hooks are required for live status — without them sessions stay at "starting". The merge
into your existing settings is idempotent and re-running heals stale entries (e.g. after
moving the binary).

## Usage

```sh
cb                 # the split view (auto-starts the daemon)
cb tile            # jump straight to the tiled grid of all sessions
cb install-hooks   # install the Claude Code hooks
cb install-codex   # install the Codex hooks
cb stop            # kill all sessions and stop the daemon
cb ctl list|spawn|kill   # scriptable client
cb daemon          # run the hub in the foreground (normally auto-started)
```

### Sidebar keys (list has focus)

| key | action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | move selection (the right pane follows) |
| `enter` or `Ctrl-a` `→` | focus the screen pane (type into the session) |
| `t` | tile all sessions |
| `n` | start a new claude session |
| `c` | start a new codex session |
| `x` | kill the selected session |
| `R` | rename the selected session (defaults to the start folder, e.g. `command-center`) |
| `Ctrl-a` `q` | quit (the daemon and sessions keep running) |

### Screen pane / tiled view

A tmux-style prefix key, **`Ctrl-a`** by default (override with the `CB_PREFIX` env var,
e.g. `CB_PREFIX=ctrl+b`), switches from typing-into-the-session to issuing a command.
`Ctrl-a` `q` always quits, from either zone.

| `Ctrl-a` then… | action |
|----------------|--------|
| `←` | return focus to the sidebar |
| `→` | focus the screen pane |
| `q` | quit cb (sessions keep running) |
| `x` | kill the current session |
| `g` | jump to the session that most recently needs approval |
| `a` | send a literal `Ctrl-a` to the session |
| `h`/`j`/`k`/`l` or arrows | move focus between panes (tiled view) |
| `1`–`9` | focus pane N (tiled view) |
| `d` | detach back to the dashboard (tiled view) |
| `n` | spawn + add a new pane (tiled view) |
| `x` | kill the focused session (tiled view) |

## State & files

- `~/.cb/daemon.sock` — daemon control socket
- `~/.cb/daemon.log` — daemon log

## Status

Complete: session core, hooks (Claude Code + Codex), the unified sidebar + live screen
view, tiled panes, and lifecycle (auto-start, clean shutdown, dead-session reaping). See
`CLAUDE.md` for architecture details and known gotchas.
