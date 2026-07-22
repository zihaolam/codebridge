// Two-stage spawn picker, mirroring the TUI's prefix+w dialog: pick one of
// the repo's worktrees, then pick which agent to launch there. The agent is
// chosen every time, never remembered.
import { useEffect, useState } from 'react'
import type { WorktreeEntry } from './ws'
import { basename } from './format'
import { IconChevronLeft } from './icons'

const AGENT_LABELS: Record<string, string> = {
  claude: 'claude code',
  codex: 'codex',
  opencode: 'opencode',
}

export type PickerData = {
  cwd: string
  worktrees: WorktreeEntry[] | null // null = reply not in yet
  agents: string[]
}

function tag(w: WorktreeEntry): string {
  const kind = w.bare ? 'bare' : w.detached ? 'detached' : w.branch || ''
  if (!w.main) return kind
  // Don't render "main · main" when the main worktree is on the main branch.
  return kind && kind !== 'main' && kind !== 'master' ? `${kind} · main` : 'main'
}

export default function WorktreePicker({
  data,
  onClose,
  onLaunch,
}: {
  data: PickerData
  onClose: () => void
  onLaunch: (agent: string, path: string) => void
}) {
  const [chosen, setChosen] = useState<WorktreeEntry | null>(null)

  useEffect(() => {
    // Capture phase, event swallowed: when the picker opens from the prefix
    // key, focus still sits in xterm's hidden textarea, and xterm would
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

  return (
    <div className="overlay" onClick={onClose}>
      <div className="picker" onClick={(e) => e.stopPropagation()}>
        {!chosen ? (
          <>
            <div className="picker-title">new session — pick a worktree</div>
            {data.worktrees === null && <div className="empty">loading…</div>}
            {data.worktrees?.map((w) => (
              <button key={w.path} className="picker-row" onClick={() => setChosen(w)}>
                <span className="picker-name">{basename(w.path)}</span>
                <span className="picker-tag">{tag(w)}</span>
              </button>
            ))}
          </>
        ) : (
          <>
            <div className="picker-title">
              <button className="icon-btn" onClick={() => setChosen(null)} title="back">
                <IconChevronLeft />
              </button>
              {basename(chosen.path)} — pick an agent
            </div>
            {data.agents.length === 0 && (
              <div className="empty">no agent binaries found (claude/codex/opencode)</div>
            )}
            {data.agents.map((a) => (
              <button key={a} className="picker-row" onClick={() => onLaunch(a, chosen.path)}>
                <span className="picker-name">{AGENT_LABELS[a] ?? a}</span>
              </button>
            ))}
          </>
        )}
      </div>
    </div>
  )
}
