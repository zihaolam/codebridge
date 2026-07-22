// Render-only terminal pane. Frames arrive as full ANSI screen repaints from
// the daemon; they are written straight into an xterm.js instance held in a
// ref — deliberately outside React state, so 30fps repaints never touch the
// React render cycle.
//
// FitAddon reports this client's viewport without resizing the shared PTY.
// Attaching claims the PTY at this browser's grid once per session; the
// top-bar resize button re-claims on demand (e.g. after a desktop `prefix z`).
import { useEffect, useRef } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import type { CbClient, Down } from './ws'

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
  // Scrollback state. `target` is the client's intent and is authoritative:
  // server frames echo the offset they were rendered at, which lags several
  // frames behind during a fast swipe — adopting it would snap the view back.
  // Frames may only update `max` (and clamp the target to it). `sent` dedupes
  // the wire traffic.
  const scrollRef = useRef({ target: 0, sent: 0, max: 0 })
  const sentRef = useRef({ rows: 0, cols: 0 })
  // Whether this client has already claimed the PTY size for the current
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
      theme: { background: '#0b0b0e' },
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
        if (!claimedRef.current) {
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
    scrollRef.current = { target: 0, sent: 0, max: 0 }
    client.onFrame = (f) => {
      const term = termRef.current
      if (!term || f.id !== idRef.current) return
      const s = scrollRef.current
      s.max = f.max_offset ?? 0
      if (s.target > s.max) s.target = s.max
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
        // Claim the PTY at this browser's grid on attach so the agent reflows
        // to fit without a manual resize. One-shot per attach; if the pane
        // isn't measured yet the ResizeObserver claims once it is, and
        // `prefix z` on the host TUI reclaims its size at any time.
        claimedRef.current = true
        client.resize(g.rows, g.cols)
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

  // Track the visual viewport so the software keyboard shrinks the app
  // instead of covering it: `--vvh` drives the .app height, keeping the agent
  // input line and the key bar visible above the keyboard. On shrink, pin the
  // pane to its bottom, where the input line lives.
  useEffect(() => {
    const vv = window.visualViewport
    if (!vv) return
    let lastH = vv.height
    const apply = () => {
      document.documentElement.style.setProperty('--vvh', `${Math.round(vv.height)}px`)
      // iOS sometimes scrolls the page to reveal a focused input; the layout
      // handles the keyboard itself, so undo that.
      if (window.scrollY !== 0) window.scrollTo(0, 0)
      const el = holder.current
      if (el && vv.height < lastH) el.scrollTop = el.scrollHeight
      lastH = vv.height
    }
    apply()
    vv.addEventListener('resize', apply)
    return () => {
      vv.removeEventListener('resize', apply)
      document.documentElement.style.removeProperty('--vvh')
    }
  }, [])

  const lineHeightPx = 17

  // Clamp, record the client's intent, and send only genuine changes.
  // Returns the applied (clamped) offset so callers can detect hitting an
  // edge of the available history.
  const setOffset = (next: number) => {
    const s = scrollRef.current
    const clamped = Math.min(Math.max(Math.round(next), 0), s.max)
    s.target = clamped
    if (clamped !== s.sent) {
      s.sent = clamped
      client.scroll(clamped)
    }
    return clamped
  }

  // A vertical finger drag browses that same daemon scrollback. After the
  // auto-resize the frame fits the pane, so there is no native pan to fight (the
  // reason this used to be button-only) — map drag distance straight to the
  // scroll offset. Drag down (dy>0) reveals earlier output (offset up). A
  // horizontal-dominant drag falls through to native pan for any line still
  // wider than the pane. Non-passive so we can preventDefault the vertical case.
  //
  // Smoothness: sends are rAF-throttled (a 120Hz touch stream would otherwise
  // queue round-trips and lag the finger), and lift-off continues with an
  // iOS-like decaying momentum fling. The keybar's paging/live events share
  // this closure so they can cancel a fling in flight.
  useEffect(() => {
    const el = holder.current
    if (!el) return
    let startY = 0
    let startX = 0
    let startOffset = 0
    let active = false
    let lastY = 0
    let lastT = 0
    let velocity = 0 // px/ms, smoothed; >0 = dragging down = back into history
    let moveRaf = 0
    let pendingOffset = 0
    let flingRaf = 0
    const stopFling = () => {
      if (flingRaf) cancelAnimationFrame(flingRaf)
      flingRaf = 0
    }
    const onStart = (e: TouchEvent) => {
      stopFling()
      active = e.touches.length === 1
      if (!active) return
      startY = lastY = e.touches[0].clientY
      startX = e.touches[0].clientX
      lastT = performance.now()
      velocity = 0
      startOffset = scrollRef.current.target
    }
    const onMove = (e: TouchEvent) => {
      if (!active || e.touches.length !== 1) return
      const y = e.touches[0].clientY
      const dy = y - startY
      const dx = e.touches[0].clientX - startX
      if (Math.abs(dy) <= Math.abs(dx)) return // horizontal → leave native pan
      e.preventDefault()
      const now = performance.now()
      const dt = now - lastT
      if (dt > 0) velocity = 0.6 * ((y - lastY) / dt) + 0.4 * velocity
      lastY = y
      lastT = now
      pendingOffset = startOffset + dy / lineHeightPx
      if (!moveRaf) {
        moveRaf = requestAnimationFrame(() => {
          moveRaf = 0
          setOffset(pendingOffset)
        })
      }
    }
    const onEnd = () => {
      if (!active) return
      active = false
      // A finger held still before lifting means "stop here", not "fling with
      // the speed from half a second ago".
      if (performance.now() - lastT > 100) velocity = 0
      if (Math.abs(velocity) < 0.05) return
      // Momentum: continue from the drag's own target (never the laggy server
      // echo) with iOS-like exponential decay, until it fades or a scrollback
      // edge clamps the offset.
      let acc = scrollRef.current.target
      let last = performance.now()
      const step = (now: number) => {
        flingRaf = 0
        const dt = Math.min(now - last, 64)
        last = now
        velocity *= Math.pow(0.998, dt)
        acc += (velocity * dt) / lineHeightPx
        const applied = setOffset(acc)
        if (Math.abs(velocity) > 0.02 && applied === Math.round(acc)) {
          flingRaf = requestAnimationFrame(step)
        }
      }
      flingRaf = requestAnimationFrame(step)
    }
    // Keybar events: ⇞/⇟ page by a screenful (dir +1 = back into history),
    // ⤓ jumps to live. Routed through here so they cancel a fling and keep
    // the target authoritative.
    const onScrollback = (e: Event) => {
      stopFling()
      const detail = (e as CustomEvent<{ dir?: number; live?: boolean }>).detail ?? {}
      if (detail.live) {
        setOffset(0)
        return
      }
      const visible = Math.floor((holder.current?.clientHeight ?? 0) / lineHeightPx)
      const page = Math.max(1, visible - 1)
      setOffset(scrollRef.current.target + (detail.dir ?? 0) * page)
    }
    window.addEventListener('cb-scrollback', onScrollback)
    el.addEventListener('touchstart', onStart, { passive: true })
    el.addEventListener('touchmove', onMove, { passive: false })
    el.addEventListener('touchend', onEnd, { passive: true })
    el.addEventListener('touchcancel', onEnd, { passive: true })
    return () => {
      stopFling()
      if (moveRaf) cancelAnimationFrame(moveRaf)
      window.removeEventListener('cb-scrollback', onScrollback)
      el.removeEventListener('touchstart', onStart)
      el.removeEventListener('touchmove', onMove)
      el.removeEventListener('touchend', onEnd)
      el.removeEventListener('touchcancel', onEnd)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client])

  // Wheel browses daemon-side scrollback: offset lines up from live bottom.
  // Proportional to the wheel delta (with a fractional accumulator) so a
  // trackpad glides instead of jumping in fixed steps.
  const wheelAcc = useRef(0)
  const onWheel = (e: React.WheelEvent) => {
    const px = e.deltaMode === 1 ? e.deltaY * lineHeightPx : e.deltaY
    wheelAcc.current += px
    const lines = Math.trunc(wheelAcc.current / lineHeightPx)
    if (lines !== 0) {
      wheelAcc.current -= lines * lineHeightPx
      setOffset(scrollRef.current.target - lines)
    }
  }

  return <div className="term-holder" ref={holder} onWheel={onWheel} />
}
