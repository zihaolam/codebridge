// Mobile key strip: the keys a phone keyboard doesn't have, sent as raw
// bytes into the attached session, plus explicit scrollback controls. A
// vertical finger drag also browses daemon scrollback (see Term.tsx); these
// buttons page it by a screenful: ↑/↓ step through history and ⤓ jumps back
// to live. Term.tsx owns the offset, so ↑/↓ dispatch a window event it clamps.
import type { CbClient } from './ws'
import { KEYS } from './keys'

// dir +1 pages back into history, -1 toward live (see Term.tsx setOffset).
const scrollback = (dir: number) =>
  window.dispatchEvent(new CustomEvent('cb-scrollback', { detail: { dir } }))

export default function KeyBar({ client }: { client: CbClient }) {
  return (
    <div className="keybar">
      {KEYS.map((k) => (
        <button
          key={k.label}
          className="key"
          title={k.title}
          onClick={() => client.input(k.seq)}
        >
          {k.label}
        </button>
      ))}
      <button className="key key-nav" title="scroll back" onClick={() => scrollback(1)}>
        ↑
      </button>
      <button className="key" title="scroll forward" onClick={() => scrollback(-1)}>
        ↓
      </button>
      <button className="key key-live" title="jump to live" onClick={() => client.scroll(0)}>
        ⤓ live
      </button>
    </div>
  )
}
