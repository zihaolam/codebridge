// Mobile key strip: the keys a phone keyboard doesn't have, sent as raw
// bytes into the attached session. Two rows: the arrow/scroll pad on the
// left — an inverted-T arrow cluster (↑ above; ← ↓ → below) with scrollback
// paging in the top corners (⇞ left of ↑, ⇟ right of ↑) — and the remaining
// keys stacked left-aligned to its right, ending with ⤓ (jump to live).
// A vertical finger drag also browses daemon scrollback (see Term.tsx); the
// paging keys step it by a screenful. Term.tsx owns the offset, so ⇞/⇟/⤓
// dispatch a window event it clamps.
//
// The key rows scroll horizontally without a scrollbar; edge fades appear
// only on a side that actually has more keys hidden beyond it.
import { useEffect, useRef, useState } from 'react'
import type { CbClient } from './ws'
import { ARROWS, KEY_ROWS, type KeyDef } from './keys'

// dir +1 pages back into history, -1 toward live; live jumps to the bottom
// (see the cb-scrollback handler in Term.tsx).
const scrollback = (detail: { dir?: number; live?: boolean }) =>
  window.dispatchEvent(new CustomEvent('cb-scrollback', { detail }))

export default function KeyBar({ client }: { client: CbClient }) {
  const scroller = useRef<HTMLDivElement>(null)
  const [fade, setFade] = useState({ l: false, r: false })

  const update = () => {
    const el = scroller.current
    if (!el) return
    const max = el.scrollWidth - el.clientWidth
    const l = el.scrollLeft > 2
    const r = el.scrollLeft < max - 2
    setFade((prev) => (prev.l === l && prev.r === r ? prev : { l, r }))
  }

  useEffect(() => {
    update()
    const el = scroller.current
    if (!el) return
    // Re-check on any size change (rotation, keyboard, list→term swap).
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])

  const key = (k: KeyDef, cls = '') => (
    <button
      key={k.label}
      className={`key ${cls}`}
      title={k.title}
      onClick={() => client.input(k.seq)}
    >
      {k.label}
    </button>
  )
  return (
    <div className="keybar">
      <div className="arrow-pad">
        <button
          className="key key-nav k-pgup"
          title="scroll back a page"
          onClick={() => scrollback({ dir: 1 })}
        >
          ⇞
        </button>
        {key(ARROWS.up, 'k-up')}
        <button
          className="key key-nav k-pgdn"
          title="scroll forward a page"
          onClick={() => scrollback({ dir: -1 })}
        >
          ⇟
        </button>
        {key(ARROWS.left, 'k-left')}
        {key(ARROWS.down, 'k-down')}
        {key(ARROWS.right, 'k-right')}
      </div>
      <div className={`key-rows-wrap ${fade.l ? 'fade-l' : ''} ${fade.r ? 'fade-r' : ''}`}>
        <div className="key-rows" ref={scroller} onScroll={update}>
          <div className="key-row">{KEY_ROWS[0].map((k) => key(k))}</div>
          <div className="key-row">
            {KEY_ROWS[1].map((k) => key(k))}
            <button
              className="key key-live"
              title="jump to live"
              onClick={() => scrollback({ live: true })}
            >
              ⤓
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}
