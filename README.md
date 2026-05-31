# codebridge

A terminal UI for managing many [Claude Code](https://claude.com/claude-code) (and
[Codex](https://developers.openai.com/codex)) sessions from one place. Instead of one
terminal tab per session — with no central view of which session needs your attention —
`codebridge` gives you a single split view: a session list on the left and the
selected session's **live, interactive** screen on the right, with status that updates as
each session works, waits for approval, or finishes its turn.

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
                          │  sidebar + live screen pane   │
                          └───────────────────────────────┘
```

The dashboard is a two-zone view: a session list (left) and the selected session's live
screen (right). `Ctrl-a` then `→` focuses the screen pane so your keystrokes go straight
to that session; `Ctrl-a` then `←` returns to the list.

Status comes from the **hook** system, not from scraping the terminal: the daemon maps
`SessionStart` → *ready*, `UserPromptSubmit`/`PreToolUse`/`PostToolUse` → *working*,
`Notification`/`PermissionRequest` → *needs approval*, `Stop` → *waiting for you*,
`SessionEnd` → *ended*. Codex uses the same hook shape, so the mapping is shared.

## Terminal support

codebridge is only fully supported on terminals that forward the Kitty keyboard
protocol — currently **Ghostty, WezTerm, Kitty, and iTerm2 (with the Kitty
keyboard extension enabled)**. Other terminals (Terminal.app, stock iTerm2,
Alacritty's default config, GNOME Terminal, etc.) will run codebridge, but a
handful of bindings will be unavailable or fall back to alternatives:

- **Copy with `cmd+c`** (in addition to `ctrl+c` / prefix `y`) — only the
  supported terminals forward Cmd to the app; others intercept it for their own
  selection.
- **Shift+Enter / Shift+Tab** in the focused session — without Kitty keyboard
  reporting, the terminal can't disambiguate the modifier and the session sees
  a plain Enter or Tab. Use the prefix `enter` binding to insert a newline on
  those terminals.
- **Cmd+Arrow / Option+Arrow** for word/line nav — supported terminals send the
  modified arrow; others map it to a different sequence (codebridge falls back
  to the readline-conventional bytes where it can).

If a key isn't doing what you expect, your terminal probably isn't passing it
through.

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
cb install-hooks   # install the Claude Code hooks
cb install-codex   # install the Codex hooks
cb stop            # kill all sessions and stop the daemon
cb ctl list|spawn|kill   # scriptable client
cb daemon          # run the hub in the foreground (normally auto-started)
```

### Sidebar keys (list has focus)

Plain keys only navigate the list — every command goes through the prefix so the same
binding works whether the list or the session has focus.

| key | action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | move selection (the right pane follows) |
| `enter` or `→` or `l` | focus the screen pane (type into the session) |
| any `Ctrl-a` command | see the prefix table below |

The list scrolls automatically to keep the selected session in view, so a long
list of sessions is fully reachable.

### Prefix commands

A tmux-style prefix key, **`Ctrl-a`** by default, switches from typing-into-the-session
to issuing a command. Press `Ctrl-a` then tap any of the keys below. The prefix can be
changed in two ways: persistently from the **config menu** (`Ctrl-a o`, see below), or
per-shell via the `CB_PREFIX` env var (e.g. `CB_PREFIX=ctrl+b`) which always overrides
the config file. `Ctrl-a` then `?` (or `h`) toggles a floating cheat-sheet that lists the
current bindings.

| `Ctrl-a` then… | default key | action |
|----------------|-------------|--------|
| focus sidebar  | `←`         | return focus to the list (not rebindable) |
| focus screen   | `l` or `→`  | focus the screen pane |
| toggle hints   | `h` or `?`  | show/hide the floating prefix cheat-sheet (not rebindable) |
| newline        | `enter`     | insert a newline in the session without submitting (works on any terminal) |
| scroll mode    | `[`         | freeze the screen pane to browse scrollback |
| new claude     | `n`         | start a new claude session |
| new codex      | `c`         | start a new codex session |
| kill           | `x`         | kill the current session |
| rename         | `r`         | rename the selected session (defaults to its start folder) |
| jump pending   | `g`         | jump to the session that most recently needs approval |
| scope toggle   | `a`         | toggle this-repo / all sessions |
| yank           | `y`         | copy the held drag-selection to the system clipboard (OSC52) |
| open config    | `o`         | open the config menu (see below) |
| quit           | `q`         | quit cb (sessions keep running) |

### Config menu (`Ctrl-a o`)

Opens a modal that lets you change the prefix and rebind every prefix command above.
Changes auto-save to `~/.config/cb/config.json` (or `$XDG_CONFIG_HOME/cb/config.json`)
the moment you press them, so closing the modal commits nothing extra and esc-ing out
of capture mode reverts cleanly.

| key | action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | move the cursor |
| `enter` | rebind the highlighted row (next keypress becomes the new binding) |
| `esc` (while capturing) | cancel the rebind |
| `enter` on **reset all to defaults** | restore the factory bindings |
| `esc` or `q` | close the modal |

The menu refuses to rebind a key that another command already uses, and it refuses the
reserved keys (`esc`, `h`, `?`, arrows, `j`/`k`, `Ctrl-c`) that the system layer claims
before dispatch — both with an inline error so you can pick another key. If `CB_PREFIX`
is set in your shell, the prefix row is shown read-only with a "(locked by CB_PREFIX)"
note, since the env override always wins.

### Scroll mode (browsing scrollback)

`Ctrl-a` `[` freezes the screen pane and lets you scroll up through the session's
history (the border turns magenta). cb deliberately does **not** capture the mouse,
so your terminal's native text selection / copy keeps working; scrollback is browsed
with the keyboard.

| key | action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | scroll one line |
| `pgup`/`pgdn` (or `b`/`f`/space) | scroll a page |
| `g` | jump to the oldest line |
| `G` | jump back to the live bottom |
| `esc` or `q` | leave scroll mode (back to the live screen) |

## State & files

- `~/.cb/daemon.sock` — daemon control socket
- `~/.cb/daemon.log` — daemon log
- `~/.config/cb/config.json` (or `$XDG_CONFIG_HOME/cb/config.json`) — prefix and key bindings (managed by the in-app config menu)

## Status

Complete: session core, hooks (Claude Code + Codex), the unified sidebar + live screen
view, and lifecycle (auto-start, clean shutdown, dead-session reaping). See `CLAUDE.md`
for architecture details and known gotchas.
