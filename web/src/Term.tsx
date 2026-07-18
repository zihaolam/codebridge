// Render-only terminal pane. Frames arrive as full ANSI screen repaints from
// the daemon; they are written straight into an xterm.js instance held in a
// ref — deliberately outside React state, so 30fps repaints never touch the
// React render cycle.
//
// FitAddon reports this client's viewport without resizing the shared PTY.
// The top-bar resize button is the only browser action that claims PTY size.
import { useEffect, useRef } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import type { CbClient, Down } from './ws'

const SCROLL_STEP = 3
const RESIZE_DEBOUNCE_MS = 250
// Below these the agent TUIs degrade into garbage; don't claim less.
const MIN_COLS = 40
const MIN_ROWS = 10

// eslint-disable-next-line no-control-regex
const ANSI_RE = /\x1b\[[0-9;:?]*[a-zA-Z]|\x1b\][^\x07]*\x07/g

// Width fallback for frames from a pre-Size daemon (no cols field): estimate
// from the longest visible line so wide sessions don't wrap into garbage.
function measureCols(lines: string[]): number {
  let max = 80
  for (const l of lines) {
    const w = l.replace(ANSI_RE, '').length
    if (w > max) max = w
  }
  return max
}

function writeFrame(term: Terminal, f: Down, lines: string[]) {
  // Home + repaint each line with clear-to-EOL (avoids a full-screen clear,
  // which flickers), clear below, then park the cursor where the frame says.
  const body = lines.join('\x1b[K\r\n')
  const cur = `\x1b[${(f.cursor_y ?? 0) + 1};${(f.cursor_x ?? 0) + 1}H`
  term.write(`\x1b[?25l\x1b[H${body}\x1b[K\x1b[0J${cur}\x1b[?25h`)
}

export default function Term({ client, sessionId }: { client: CbClient; sessionId: string | null }) {
  const holder = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const idRef = useRef<string | null>(null)
  const scrollRef = useRef({ offset: 0, max: 0 })
  const sentRef = useRef({ rows: 0, cols: 0 })

  // proposeGrid asks FitAddon what grid fills the pane; undefined while the
  // holder is hidden or unmeasured (e.g. mobile list view).
  const proposeGrid = () => {
    const d = fitRef.current?.proposeDimensions()
    if (!d || !Number.isFinite(d.cols) || !Number.isFinite(d.rows)) return undefined
    return { cols: Math.max(MIN_COLS, d.cols), rows: Math.max(MIN_ROWS, d.rows) }
  }

  useEffect(() => {
    const term = new Terminal({
      cols: 80,
      rows: 24,
      scrollback: 0, // scrollback lives in the daemon; browse it via scroll offsets
      fontSize: 13,
      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, monospace',
      theme: { background: '#0d1117' },
      cursorBlink: false,
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(holder.current!)
    term.onData((d) => client.input(d))
    // macOS line-editing chords. xterm ignores meta combos and the browser
    // would treat cmd+←/→ as history navigation (leaving the app!), so map
    // them to the readline controls every agent CLI understands: cmd+← →
    // ctrl-a (line start), cmd+→ → ctrl-e (line end), cmd+⌫ → ctrl-u (kill
    // to line start).
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown' || !e.metaKey || e.ctrlKey || e.altKey) return true
      const seq =
        e.key === 'ArrowLeft'
          ? '\x01'
          : e.key === 'ArrowRight'
            ? '\x05'
            : e.key === 'Backspace'
              ? '\x15'
              : null
      if (!seq) return true
      e.preventDefault()
      client.input(seq)
      return false
    })
    termRef.current = term
    fitRef.current = fit

    // Debounced: reflowing the child on every pixel of a drag-resize would
    // spam SIGWINCH redraws through the whole stack.
    let timer: ReturnType<typeof setTimeout> | undefined
    const ro = new ResizeObserver(() => {
      clearTimeout(timer)
      timer = setTimeout(() => {
        const g = proposeGrid()
        const sent = sentRef.current
        if (g && idRef.current && (g.rows !== sent.rows || g.cols !== sent.cols)) {
          sentRef.current = g
          client.viewport(g.rows, g.cols)
        }
      }, RESIZE_DEBOUNCE_MS)
    })
    ro.observe(holder.current!)
    return () => {
      clearTimeout(timer)
      ro.disconnect()
      term.dispose()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client])

  useEffect(() => {
    idRef.current = sessionId
    scrollRef.current = { offset: 0, max: 0 }
    client.onFrame = (f) => {
      const term = termRef.current
      if (!term || f.id !== idRef.current) return
      scrollRef.current = { offset: f.offset ?? 0, max: f.max_offset ?? 0 }
      const lines = (f.screen ?? '').split('\n')
      const cols = f.cols || measureCols(lines)
      const rows = f.rows || lines.length
      if (term.cols !== cols || term.rows !== rows) term.resize(cols, rows)
      writeFrame(term, f, lines)
    }
    if (sessionId) {
      termRef.current?.reset()
      const g = proposeGrid()
      sentRef.current = g ?? { rows: 0, cols: 0 }
      client.attach(sessionId)
      if (g) client.viewport(g.rows, g.cols)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, sessionId])

  useEffect(() => {
    const claim = () => {
      const g = proposeGrid()
      if (g && idRef.current) client.resize(g.rows, g.cols)
    }
    window.addEventListener('cb-resize-session', claim)
    return () => window.removeEventListener('cb-resize-session', claim)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client])

  const lineHeightPx = 17

  const setOffset = (next: number) => {
    const s = scrollRef.current
    next = Math.min(Math.max(next, 0), s.max)
    if (next !== s.offset) {
      s.offset = next
      client.scroll(next)
    }
  }

  // Daemon-side scrollback is now an explicit control (the keybar ↑/↓ buttons),
  // not a drag gesture — a finger drag pans the canonical frame natively (both
  // axes), so overloading vertical drag with scrollback would fight the native
  // pan. A tap pages by roughly one screenful; dir +1 goes back in history
  // (offset up from the live bottom), -1 toward live.
  useEffect(() => {
    const onScrollback = (e: Event) => {
      const dir = (e as CustomEvent<{ dir: number }>).detail?.dir ?? 0
      const visible = Math.floor((holder.current?.clientHeight ?? 0) / lineHeightPx)
      const page = Math.max(1, visible - 1)
      setOffset(scrollRef.current.offset + dir * page)
    }
    window.addEventListener('cb-scrollback', onScrollback)
    return () => window.removeEventListener('cb-scrollback', onScrollback)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client])

  // Wheel browses daemon-side scrollback: offset lines up from live bottom.
  // (Desktop only in practice — the frame fits the pane there, so there is no
  // native scroll to compete with; phones use the keybar buttons.)
  const onWheel = (e: React.WheelEvent) => {
    setOffset(scrollRef.current.offset + (e.deltaY < 0 ? SCROLL_STEP : -SCROLL_STEP))
  }

  return <div className="term-holder" ref={holder} onWheel={onWheel} />
}
