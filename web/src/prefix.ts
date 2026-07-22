// Browser side of the TUI's prefix-command system. The bridge's hello carries
// the user's real prefix and bindings (src/config.rs), so one config drives
// both clients; these defaults keep an older bridge working. Key names mirror
// tui.rs `key_name` exactly — a binding saved by the TUI config menu must
// resolve to the same browser keystroke.
export const DEFAULT_PREFIX = 'ctrl+a'

export type WebAction = { id: string; label: string; default: string }

// The subset of config.rs ACTIONS a browser can perform, with the same ids and
// default keys. The rest (rename, yank, scroll mode, focus, config menu, quit,
// scope toggle) are TUI-only concepts; pressing their keys after the prefix is
// swallowed, never forwarded to the agent — same as an unbound key in the TUI.
export const WEB_ACTIONS: WebAction[] = [
  { id: 'new_claude', label: 'new claude session', default: 'n' },
  { id: 'new_codex', label: 'new codex session', default: 'c' },
  { id: 'new_worktree', label: 'new session in worktree', default: 'w' },
  { id: 'kill', label: 'kill session', default: 'x' },
  { id: 'jump_pending', label: 'jump to pending approval', default: 'g' },
  { id: 'resize_pane', label: 'resize session to this screen', default: 'z' },
  { id: 'newline', label: 'insert newline in session', default: 'enter' },
  { id: 'task_backlog', label: 'task backlog', default: 't' },
  { id: 'session_history', label: 'resume past session', default: 'm' },
]

export const DEFAULT_BINDINGS: Record<string, string> = Object.fromEntries(
  WEB_ACTIONS.map((a) => [a.id, a.default]),
)

const SPECIAL: Record<string, string> = {
  Enter: 'enter',
  Tab: 'tab',
  Backspace: 'backspace',
  Escape: 'esc',
  ArrowUp: 'up',
  ArrowDown: 'down',
  ArrowLeft: 'left',
  ArrowRight: 'right',
  Home: 'home',
  End: 'end',
  PageUp: 'pgup',
  PageDown: 'pgdown',
  Delete: 'delete',
  Insert: 'insert',
}

// keyName mirrors tui.rs `key_name`: the base key, prefixed by held modifiers
// in ctrl+alt+super order. Shift is folded into the character itself for char
// keys (crossterm reports shifted chars the same way), named only for special
// keys, and shift+tab short-circuits the other modifiers exactly like the TUI.
// Returns '' for keys the naming scheme doesn't cover.
export function keyName(e: KeyboardEvent): string {
  let base: string
  if (e.key === 'Tab' && e.shiftKey) base = 'shift+tab'
  else if (SPECIAL[e.key]) base = SPECIAL[e.key]
  else if (e.key.length === 1) base = e.key
  else return ''
  const mods: string[] = []
  if (e.ctrlKey) mods.push('ctrl')
  if (e.altKey) mods.push('alt')
  if (e.metaKey) mods.push('super')
  if (e.shiftKey && e.key.length !== 1) mods.push('shift')
  return mods.length === 0 || base.startsWith('shift+') ? base : `${mods.join('+')}+${base}`
}
