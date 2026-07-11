import { useMemo, useState } from 'react'
import type { SessionInfo } from './ws'
import Spinner from './Spinner'
import { IconPlus, IconX } from './icons'

// Status → glyph, mirroring the TUI sidebar's language (dashboard.go
// indicator/statusStyle): working spinner, green ● turn-complete, yellow ●
// fresh, red ⚑ needs approval, cyan … starting, grey ✗ ended.
const GLYPH: Record<string, { g: string; cls: string }> = {
  starting: { g: '…', cls: 'st-starting' },
  idle: { g: '●', cls: 'st-idle' },
  waiting_user: { g: '●', cls: 'st-waiting' },
  needs_approval: { g: '⚑', cls: 'st-approval' },
  ended: { g: '✗', cls: 'st-ended' },
}

function label(s: SessionInfo): string {
  if (s.name) return s.name
  return s.argv[0] ?? '?'
}

function basename(p: string): string {
  return p.replace(/\/+$/, '').split('/').pop() ?? p
}

type Group = { key: string; name: string; sessions: SessionInfo[] }

// Group sessions by bridge-computed scope (repo-common-dir semantics, same as
// the TUI accordion). Sessions from an older bridge without scope fields fall
// back to grouping by literal cwd.
function groupSessions(sessions: SessionInfo[]): Group[] {
  const map = new Map<string, Group>()
  for (const s of sessions) {
    const key = s.scope || s.cwd
    let g = map.get(key)
    if (!g) {
      g = { key, name: s.scope_name || basename(s.cwd), sessions: [] }
      map.set(key, g)
    }
    g.sessions.push(s)
  }
  return [...map.values()].sort((a, b) => a.name.localeCompare(b.name))
}

export default function SessionList({
  sessions,
  selected,
  onSelect,
  onKill,
  onSpawn,
}: {
  sessions: SessionInfo[]
  selected: string | null
  onSelect: (id: string) => void
  onKill: (id: string) => void
  onSpawn: (cwd: string) => void
}) {
  const groups = useMemo(() => groupSessions(sessions), [sessions])
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set())

  const toggle = (key: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })

  if (sessions.length === 0) return <div className="empty">no sessions</div>
  return (
    <ul className="session-list">
      {groups.map((g) => {
        const closed = collapsed.has(g.key)
        return (
          <li key={g.key} className="scope-group">
            <div className="scope-header" onClick={() => toggle(g.key)}>
              <span className="chevron">{closed ? '▸' : '▾'}</span>
              <span className="scope-name">{g.name}</span>
              <span className="scope-count">{g.sessions.length}</span>
              <button
                className="icon-btn scope-add"
                title={`new session in ${g.name}`}
                onClick={(e) => {
                  e.stopPropagation()
                  onSpawn(g.sessions[0].cwd)
                }}
              >
                <IconPlus />
              </button>
            </div>
            {!closed && (
              <ul>
                {g.sessions.map((s) => {
                  const st = GLYPH[s.status] ?? { g: '·', cls: '' }
                  return (
                    <li
                      key={s.id}
                      className={`session-row ${s.id === selected ? 'selected' : ''}`}
                      onClick={() => onSelect(s.id)}
                    >
                      {s.status === 'working' ? (
                        <Spinner />
                      ) : (
                        <span className={`glyph ${st.cls}`}>{st.g}</span>
                      )}
                      <span className="name">{label(s)}</span>
                      <button
                        className="icon-btn row-x"
                        title="kill session"
                        onClick={(e) => {
                          e.stopPropagation()
                          onKill(s.id)
                        }}
                      >
                        <IconX />
                      </button>
                    </li>
                  )
                })}
              </ul>
            )}
          </li>
        )
      })}
    </ul>
  )
}
