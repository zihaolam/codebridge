// Raw key sequences for the mobile key bar — the keys a phone keyboard can't
// produce. One table on purpose: when an agent TUI changes what it expects,
// this is the only place to touch.
export type KeyDef = {
  label: string
  seq: string
  title: string
}

// Arrow cluster, laid out as an inverted T like a physical keyboard
// (↑ above, ← ↓ → below).
export const ARROWS = {
  up: { label: '↑', seq: '\x1b[A', title: 'up' },
  down: { label: '↓', seq: '\x1b[B', title: 'down' },
  left: { label: '←', seq: '\x1b[D', title: 'left' },
  right: { label: '→', seq: '\x1b[C', title: 'right' },
} satisfies Record<string, KeyDef>

// The remaining keys, split across the key bar's two rows.
export const KEY_ROWS: KeyDef[][] = [
  [
    { label: 'esc', seq: '\x1b', title: 'escape / interrupt' },
    { label: 'tab', seq: '\t', title: 'tab' },
    { label: '⇧tab', seq: '\x1b[Z', title: 'shift+tab (cycle modes)' },
    // Line-editing the way macOS does opt+⌫ / cmd+⌫, sent as the readline
    // controls every agent CLI understands: ctrl-w (delete word back),
    // ctrl-u (delete to line start).
    { label: '⌫word', seq: '\x17', title: 'delete word back (ctrl+w)' },
  ],
  [
    { label: '⏎', seq: '\r', title: 'enter' },
    { label: '^C', seq: '\x03', title: 'ctrl+c' },
    { label: '⌫line', seq: '\x15', title: 'delete to line start (ctrl+u)' },
  ],
]
