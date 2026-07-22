// Session history picker, mirroring the TUI's prefix+m modal: every recorded
// run — live and paused — labelled by its agent-summarised title or first
// message. Entering a paused run resumes it agent-natively (the daemon
// respawns in the run's origin cwd); entering a live run jumps to the already
// running session, since resuming it would spawn a duplicate. The TUI filters
// to its launch scope, but the browser has no launch scope, so every scope is
// shown and rows are tagged with the workspace name instead.
import { useEffect, useMemo } from 'react'
import type { SessionInfo, TaskInfo } from './ws'
import StatusDot from './StatusDot'
import { ago } from './format'

type Entry = {
  taskId: string
  runId: string
  agent: string
  label: string
  scopeName: string
  live: boolean
  cbSessionId: string
  cwd: string
  updatedAt: number
}

function entries(tasks: TaskInfo[]): Entry[] {
  const out: Entry[] = []
  for (const t of tasks) {
    for (const r of t.runs ?? []) {
      if (r.status !== 'in_progress' && r.status !== 'paused') continue
      const agent = r.agent || t.agent || '?'
      out.push({
        taskId: t.id,
        runId: r.id,
        agent,
        label: r.title?.trim() || r.first_message?.trim() || (t.auto ? agent : t.title),
        scopeName: t.scope_name,
        live: r.status === 'in_progress',
        cbSessionId: r.cb_session_id ?? '',
        cwd: r.cwd || t.cwd || '',
        updatedAt: Date.parse(r.updated_at) || 0,
      })
    }
  }
  return out.sort((a, b) => b.updatedAt - a.updatedAt)
}

export default function HistoryModal({
  tasks,
  sessions,
  onClose,
  onJump,
  onResume,
}: {
  tasks: TaskInfo[]
  sessions: SessionInfo[]
  onClose: () => void
  onJump: (sessionId: string) => void
  onResume: (taskId: string, runId: string, cwd: string) => void
}) {
  const rows = useMemo(() => entries(tasks), [tasks])

  useEffect(() => {
    // Capture phase, event swallowed: focus usually still sits in xterm's
    // hidden textarea (the modal opens from the prefix key), and xterm would
    // otherwise consume the Escape and forward it into the agent.
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      e.preventDefault()
      e.stopPropagation()
      onClose()
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [onClose])

  const liveStatus = (id: string) => sessions.find((s) => s.id === id)?.status ?? 'waiting_user'

  return (
    <div className="overlay" onClick={onClose}>
      <div className="picker" onClick={(e) => e.stopPropagation()}>
        <div className="picker-title">sessions — tap a paused run to resume it</div>
        {rows.length === 0 && <div className="empty">no sessions recorded yet</div>}
        {rows.map((r) => (
          <button
            key={`${r.taskId}/${r.runId}`}
            className="picker-row history-row"
            onClick={() => (r.live ? onJump(r.cbSessionId) : onResume(r.taskId, r.runId, r.cwd))}
          >
            {r.live ? (
              <StatusDot status={liveStatus(r.cbSessionId)} />
            ) : (
              <span className="glyph st-idle">‖</span>
            )}
            <span className="picker-name history-label">{r.label}</span>
            <span className="picker-tag">
              {[r.scopeName, r.agent, r.live ? 'live' : 'paused', ago(r.updatedAt)]
                .filter(Boolean)
                .join(' · ')}
            </span>
          </button>
        ))}
      </div>
    </div>
  )
}
