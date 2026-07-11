import { useEffect, useMemo, useRef, useState } from 'react'
import { CbClient, type ClientState, type SessionInfo } from './ws'
import SessionList from './SessionList'
import Term from './Term'
import WorktreePicker, { type PickerData } from './WorktreePicker'
import KeyBar from './KeyBar'
import { IconChevronLeft, IconX } from './icons'

const TOKEN_KEY = 'cb-token'

// A `#token=...` fragment (from `cb web qr`) seeds the stored token; the
// fragment never leaves the browser.
function initialToken(): string {
  const m = location.hash.match(/#token=([0-9a-f]+)/)
  if (m) {
    localStorage.setItem(TOKEN_KEY, m[1])
    history.replaceState(null, '', location.pathname)
    return m[1]
  }
  return localStorage.getItem(TOKEN_KEY) ?? ''
}

export default function App() {
  const [token, setToken] = useState(initialToken)
  if (!token) return <TokenGate onSubmit={setToken} />
  return <Dashboard token={token} onAuthFailed={() => setToken('')} />
}

function TokenGate({ onSubmit }: { onSubmit: (t: string) => void }) {
  const [val, setVal] = useState('')
  return (
    <div className="gate">
      <h1>cb</h1>
      <p>
        Paste the pairing token from <code>cb web token</code>
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault()
          const t = val.trim()
          if (!t) return
          localStorage.setItem(TOKEN_KEY, t)
          onSubmit(t)
        }}
      >
        <input
          value={val}
          onChange={(e) => setVal(e.target.value)}
          placeholder="token"
          autoFocus
        />
        <button type="submit">connect</button>
      </form>
    </div>
  )
}

function Dashboard({ token, onAuthFailed }: { token: string; onAuthFailed: () => void }) {
  const [state, setState] = useState<ClientState>('connecting')
  const [sessions, setSessions] = useState<SessionInfo[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [errors, setErrors] = useState<string[]>([])
  // On phones the list and the terminal are two swappable screens; on desktop
  // the CSS ignores this and shows both panes side by side.
  const [view, setView] = useState<'list' | 'term'>('list')
  const [picker, setPicker] = useState<PickerData | null>(null)
  const selectedRef = useRef(selected)
  selectedRef.current = selected

  const client = useMemo(() => new CbClient(token), [token])

  useEffect(() => {
    client.onState = (s) => {
      setState(s)
      if (s === 'auth-failed') {
        localStorage.removeItem(TOKEN_KEY)
        client.close()
        onAuthFailed()
      }
    }
    client.onSessions = (list) => {
      setSessions(list)
      // Keep a valid selection: drop a vanished session, auto-pick the first.
      const cur = selectedRef.current
      if (cur && !list.some((s) => s.id === cur)) setSelected(null)
      if (!selectedRef.current && list.length > 0) setSelected(list[0].id)
    }
    client.onError = (msg) => setErrors((e) => [...e.slice(-4), msg])
    client.onSpawned = (id) => {
      setSelected(id)
      setView('term')
    }
    client.onWorktrees = (cwd, worktrees, agents) =>
      setPicker((p) => (p && p.cwd === cwd ? { ...p, worktrees, agents } : p))
    client.connect()
    return () => client.close()
  }, [client, onAuthFailed])

  const current = sessions.find((s) => s.id === selected)
  const workspace = current
    ? current.scope_name || (current.cwd.replace(/\/+$/, '').split('/').pop() ?? '')
    : ''
  const currentLabel = current ? `${workspace} · ${current.name || current.argv[0]}` : ''

  const kill = (id: string) => {
    const s = sessions.find((x) => x.id === id)
    if (s && confirm(`Kill ${s.name || s.argv.join(' ')}?`)) client.kill(id)
  }

  return (
    <div className={`app view-${view}`}>
      <aside>
        <header>
          <span className="brand">cb</span>
          <span className={`conn-dot conn-${state}`} title={state} />
        </header>
        <SessionList
          sessions={sessions}
          selected={selected}
          onSelect={(id) => {
            setSelected(id)
            setView('term')
          }}
          onKill={kill}
          onSpawn={(cwd) => {
            setPicker({ cwd, worktrees: null, agents: [] })
            client.worktrees(cwd)
          }}
        />
      </aside>
      <main>
        <div className="topbar">
          <button className="icon-btn back" onClick={() => setView('list')}>
            <IconChevronLeft />
          </button>
          <span className="topbar-label">{currentLabel}</span>
          {current && (
            <button
              className="icon-btn topbar-x"
              title="kill this session"
              onClick={() => kill(current.id)}
            >
              <IconX />
            </button>
          )}
        </div>
        {state === 'closed' && (
          <div className="banner">disconnected — retrying… (is Tailscale up? is `cb web` running?)</div>
        )}
        {errors.length > 0 && (
          <div className="banner err" onClick={() => setErrors([])}>
            {errors[errors.length - 1]}
          </div>
        )}
        {selected ? (
          <Term client={client} sessionId={selected} />
        ) : (
          <div className="empty">select a session</div>
        )}
        {selected && <KeyBar client={client} />}
      </main>
      {picker && (
        <WorktreePicker
          data={picker}
          onClose={() => setPicker(null)}
          onLaunch={(agent, path) => {
            client.spawn([agent], path)
            setPicker(null)
          }}
        />
      )}
    </div>
  )
}
