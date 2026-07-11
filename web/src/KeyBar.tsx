// Mobile key strip: the keys a phone keyboard doesn't have, sent as raw
// bytes into the attached session, plus a jump-to-live for scrollback (the
// shift+G of the TUI's scroll mode — untypeable on a phone).
import type { CbClient } from './ws'
import { KEYS } from './keys'

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
      <button className="key key-live" title="jump to live" onClick={() => client.scroll(0)}>
        ⤓ live
      </button>
    </div>
  )
}
