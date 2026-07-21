import { useMemo, useState } from 'react'
import type { SessionInfo } from './ws'
import StatusDot from './StatusDot'
import { agentOf, ago, basename, sessionLabel } from './format'
import { IconChevronDown, IconPlus, IconX } from './icons'

// Status → the word shown in a session's meta line.
const STATUS_WORD: Record<string, string> = {
  starting: 'starting',
  working: 'working',
  idle: 'idle',
  waiting_user: 'done',
  needs_approval: 'needs approval',
  ended: 'exited',
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
  titles,
  selected,
  onSelect,
  onKill,
  onSpawn,
}: {
  sessions: SessionInfo[]
  titles: Map<string, string>
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

  if (sessions.length === 0) {
    return (
      <div className="empty">
        <div className="empty-title">No sessions yet</div>
        <div className="empty-sub">Spawn an agent from the desktop TUI to see it here</div>
      </div>
    )
  }
  return (
    <ul className="session-list">
      {groups.map((g) => {
        const closed = collapsed.has(g.key)
        return (
          <li key={g.key} className="scope-group">
            <div className="scope-header" onClick={() => toggle(g.key)}>
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
              <span className={`chevron ${closed ? 'closed' : ''}`}>
                <IconChevronDown />
              </span>
            </div>
            {!closed && (
              <ul>
                {g.sessions.map((s) => {
                  // A worktree checkout is visible as a cwd that isn't the
                  // repo's main directory; surface which one in the meta line.
                  const dir = basename(s.cwd)
                  const meta = [agentOf(s), STATUS_WORD[s.status] ?? s.status]
                  const when = ago(s.status_since_unix_ms)
                  if (when) meta.push(when)
                  return (
                    <li
                      key={s.id}
                      className={`session-row ${s.id === selected ? 'selected' : ''} ${
                        s.status === 'needs_approval' ? 'attn' : ''
                      }`}
                      onClick={() => onSelect(s.id)}
                    >
                      <StatusDot status={s.status} />
                      <div className="session-text">
                        <span className="session-title">{sessionLabel(s, titles)}</span>
                        <span className="session-meta">
                          {meta.join(' · ')}
                          {dir !== g.name && (
                            <>
                              {' · '}
                              <span className="mono">⎇ {dir}</span>
                            </>
                          )}
                        </span>
                      </div>
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
