# command-center

A terminal UI for managing many [Claude Code](https://claude.com/claude-code) sessions
from one place. Instead of one terminal tab per session — with no central view of which
session needs your attention — `command-center` gives you a live dashboard, a tiled
multi-session view, and status that updates as each session works, waits for approval, or
finishes its turn.

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
                          │  cb TUI (Bubble Tea)       │
                          │  dashboard · tiled grid ·     │
                          │  single-session attach        │
                          └───────────────────────────────┘
```

Status comes from Claude Code's **hook** system, not from scraping the terminal: the daemon
maps `SessionStart`/`PreToolUse` → *working*, `Notification`/`PermissionRequest` →
*needs approval*, `Stop` → *waiting for you*, `SessionEnd` → *ended*.

## Install

Requires Go 1.24+.

```sh
go build -o ./cb .
# optionally put it on your PATH:
#   go install .

./cb install-hooks    # wires cb into ~/.claude/settings.json (writes a .bak)
```

`install-hooks` is required for live status — without it, sessions stay at "starting".
It merges into your existing settings and is idempotent.

## Usage

```sh
cb                 # dashboard (auto-starts the daemon)
cb tile            # jump straight to the tiled grid of all sessions
cb attach [id]     # attach to one session full-screen
cb install-hooks   # install the Claude Code hooks
cb stop            # kill all sessions and stop the daemon
cb ctl list|spawn|kill   # scriptable client
cb daemon          # run the hub in the foreground (normally auto-started)
```

### Dashboard keys

| key | action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | move selection |
| `enter` | attach to the selected session |
| `t` | tile all sessions |
| `n` | start a new claude session |
| `x` | kill the selected session |
| `r` | refresh |
| `q` | quit (the daemon and sessions keep running) |

### Inside a session / tiled view

A tmux-style prefix key, **`Ctrl-a`**, switches from typing-into-the-session to issuing a
command:

| `Ctrl-a` then… | action |
|----------------|--------|
| `d` or `q` | detach back to the dashboard |
| `h`/`j`/`k`/`l` or arrows | move focus between panes (tiled view) |
| `1`–`9` | focus pane N (tiled view) |
| `n` | spawn + add a new pane (tiled view) |
| `x` | kill the focused session (tiled view) |
| `a` | send a literal `Ctrl-a` to the session |

## State & files

- `~/.cb/daemon.sock` — daemon control socket
- `~/.cb/daemon.log` — daemon log

## Status

The six-phase build is complete: session core, hooks, attach, dashboard, tiled panes, and
lifecycle (auto-start, clean shutdown, dead-session reaping). See `CLAUDE.md` for
architecture details and known gotchas.
