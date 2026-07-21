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

// Phones use the swappable list/term layout below this width (see the CSS
// `@media (max-width: 768px)` breakpoint). Only there do we auto-claim the PTY
// size — a desktop browser leaves the canonical size to the host TUI.
function isMobile(): boolean {
  return window.matchMedia('(max-width: 768px)').matches
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
  // Whether this phone has already claimed the PTY size for the current
  // session. One-shot per attach so a later desktop `prefix z` reclaim isn't
  // immediately fought back on the next viewport tick.
  const claimedRef = useRef(false)

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
        if (!g || !idRef.current) return
        const sent = sentRef.current
        if (g.rows !== sent.rows || g.cols !== sent.cols) {
          sentRef.current = g
          client.viewport(g.rows, g.cols)
        }
        // Fallback auto-claim for when the pane wasn't measurable at attach
        // time (mobile list→term transition): claim once it has a real size.
        if (isMobile() && !claimedRef.current) {
          claimedRef.current = true
          client.resize(g.rows, g.cols)
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
      claimedRef.current = false
      client.attach(sessionId)
      if (g) {
        client.viewport(g.rows, g.cols)
        // On phones, claim the PTY at this screen's grid on load so the agent
        // reflows to the phone width and vertical history (keybar ↑/↓) pages
        // readably. Desktop stays presentation-only. If the pane isn't measured
        // yet the ResizeObserver claims once it is; `prefix z` on the terminal
        // reclaims the desktop size.
        if (isMobile()) {
          claimedRef.current = true
          client.resize(g.rows, g.cols)
        }
      }
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

  // Keybar ↑/↓ page daemon-side scrollback explicitly; dir +1 goes back in
  // history (offset up from the live bottom), -1 toward live.
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

  // A vertical finger drag browses that same daemon scrollback. After the mobile
  // auto-resize the frame fits the pane, so there is no native pan to fight (the
  // reason this used to be button-only) — map drag distance straight to the
  // scroll offset. Drag down (dy>0) reveals earlier output (offset up). A
  // horizontal-dominant drag falls through to native pan for any line still
  // wider than the pane. Non-passive so we can preventDefault the vertical case.
  useEffect(() => {
    const el = holder.current
    if (!el) return
    let startY = 0
    let startX = 0
    let startOffset = 0
    let active = false
    const onStart = (e: TouchEvent) => {
      active = e.touches.length === 1
      if (!active) return
      startY = e.touches[0].clientY
      startX = e.touches[0].clientX
      startOffset = scrollRef.current.offset
    }
    const onMove = (e: TouchEvent) => {
      if (!active || e.touches.length !== 1) return
      const dy = e.touches[0].clientY - startY
      const dx = e.touches[0].clientX - startX
      if (Math.abs(dy) <= Math.abs(dx)) return // horizontal → leave native pan
      e.preventDefault()
      setOffset(startOffset + Math.round(dy / lineHeightPx))
    }
    const onEnd = () => {
      active = false
    }
    el.addEventListener('touchstart', onStart, { passive: true })
    el.addEventListener('touchmove', onMove, { passive: false })
    el.addEventListener('touchend', onEnd, { passive: true })
    el.addEventListener('touchcancel', onEnd, { passive: true })
    return () => {
      el.removeEventListener('touchstart', onStart)
      el.removeEventListener('touchmove', onMove)
      el.removeEventListener('touchend', onEnd)
      el.removeEventListener('touchcancel', onEnd)
    }
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
