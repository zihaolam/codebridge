// Mobile key strip: the keys a phone keyboard doesn't have, sent as raw
// bytes into the attached session. Two rows: an inverted-T arrow cluster on
// the left (↑ above; ← ↓ → below, like a physical keyboard), the remaining
// keys stacked to its right, with scrollback paging (⇞ ⇟ ⤓) on the far right.
// A vertical finger drag also browses daemon scrollback (see Term.tsx); the
// paging keys step it by a screenful. Term.tsx owns the offset, so ⇞/⇟
// dispatch a window event it clamps.
import type { CbClient } from './ws'
import { ARROWS, KEY_ROWS, type KeyDef } from './keys'

// dir +1 pages back into history, -1 toward live (see Term.tsx setOffset).
const scrollback = (dir: number) =>
  window.dispatchEvent(new CustomEvent('cb-scrollback', { detail: { dir } }))

export default function KeyBar({ client }: { client: CbClient }) {
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
        {key(ARROWS.up, 'k-up')}
        {key(ARROWS.left, 'k-left')}
        {key(ARROWS.down, 'k-down')}
        {key(ARROWS.right, 'k-right')}
      </div>
      <div className="key-rows">
        <div className="key-row">
          {KEY_ROWS[0].map((k) => key(k))}
          <button className="key key-nav" title="scroll back a page" onClick={() => scrollback(1)}>
            ⇞
          </button>
        </div>
        <div className="key-row">
          {KEY_ROWS[1].map((k) => key(k))}
          <button
            className="key key-nav"
            title="scroll forward a page"
            onClick={() => scrollback(-1)}
          >
            ⇟
          </button>
          <button className="key key-live" title="jump to live" onClick={() => client.scroll(0)}>
            ⤓
          </button>
        </div>
      </div>
    </div>
  )
}
