import { useMemo, useState } from 'react'
import type { SessionInfo, TaskInfo } from './ws'
import Spinner from './Spinner'
import { IconPlus, IconCheck, IconPlay, IconPencil, IconTrash, IconX } from './icons'

// Status → glyph for a task, mirroring the TUI's taskGlyph (internal/tui
// task.go): an in_progress task borrows its linked session's live indicator; the
// resting states get their own marks.
const REST_GLYPH: Record<string, { g: string; cls: string }> = {
  paused: { g: '‖', cls: 'st-idle' },
  completed: { g: '✓', cls: 'st-ended' },
  pending: { g: '○', cls: 'st-pending' },
}

// Session status → glyph, matching SessionList so an in_progress task reads the
// same as its session in the sidebar.
const SESSION_GLYPH: Record<string, { g: string; cls: string }> = {
  starting: { g: '…', cls: 'st-starting' },
  idle: { g: '●', cls: 'st-idle' },
  waiting_user: { g: '●', cls: 'st-waiting' },
  needs_approval: { g: '⚑', cls: 'st-approval' },
  ended: { g: '✗', cls: 'st-ended' },
}

function basename(p: string): string {
  return p.replace(/\/+$/, '').split('/').pop() ?? p
}

// repoCwd resolves a scope key to a real launch directory for task_start /
// task_add: prefer a live session in that scope (the exact worktree the user is
// in), else strip a trailing `/.git` to reach the repo root.
function repoCwd(scope: string, sessions: SessionInfo[]): string {
  const s = sessions.find((x) => (x.scope || x.cwd) === scope)
  if (s) return s.cwd
  return scope.replace(/\/\.git$/, '')
}

type Group = { key: string; name: string; cwd: string; tasks: TaskInfo[] }

// groupTasks buckets the backlog by scope, seeded with every active repo (from
// the session list) so you can add the first task to a repo that has none yet.
function groupTasks(tasks: TaskInfo[], sessions: SessionInfo[]): Group[] {
  const map = new Map<string, Group>()
  const ensure = (key: string, name: string, cwd: string): Group => {
    let g = map.get(key)
    if (!g) {
      g = { key, name, cwd, tasks: [] }
      map.set(key, g)
    }
    return g
  }
  for (const s of sessions) {
    const key = s.scope || s.cwd
    ensure(key, s.scope_name || basename(s.cwd), s.cwd)
  }
  for (const t of tasks) {
    const g = ensure(t.scope, t.scope_name || basename(t.scope), repoCwd(t.scope, sessions))
    g.tasks.push(t)
  }
  return [...map.values()].sort((a, b) => a.name.localeCompare(b.name))
}

// Section order within a group, mirroring the TUI: active work first.
const ORDER: Record<string, number> = { in_progress: 0, paused: 1, pending: 2, completed: 3 }

export default function TaskList({
  tasks,
  sessions,
  agents,
  onJump,
  onAdd,
  onEdit,
  onStatus,
  onDelete,
  onStart,
}: {
  tasks: TaskInfo[]
  sessions: SessionInfo[]
  agents: string[]
  onJump: (sessionId: string) => void
  onAdd: (scope: string, title: string, desc: string) => void
  onEdit: (id: string, title: string, desc: string) => void
  onStatus: (id: string, status: string) => void
  onDelete: (id: string) => void
  onStart: (id: string, agent: string, cwd: string) => void
}) {
  const groups = useMemo(() => groupTasks(tasks, sessions), [tasks, sessions])
  // editor: the new/edit modal ({scope} = new in that scope; task = editing).
  const [editor, setEditor] = useState<{ scope: string; task?: TaskInfo } | null>(null)
  // starting: the task awaiting an agent choice.
  const [starting, setStarting] = useState<TaskInfo | null>(null)

  const sessionById = (id?: string) => (id ? sessions.find((s) => s.id === id) : undefined)

  if (groups.length === 0) {
    return <div className="empty">no repos yet — spawn a session first</div>
  }

  return (
    <>
      <ul className="task-list">
        {groups.map((g) => {
          const sorted = [...g.tasks].sort(
            (a, b) => (ORDER[a.status] ?? 9) - (ORDER[b.status] ?? 9),
          )
          return (
            <li key={g.key} className="scope-group">
              <div className="scope-header">
                <span className="scope-name">{g.name}</span>
                <span className="scope-count">{g.tasks.length}</span>
                <button
                  className="icon-btn scope-add"
                  title={`new task in ${g.name}`}
                  onClick={() => setEditor({ scope: g.key })}
                >
                  <IconPlus />
                </button>
              </div>
              {sorted.length === 0 ? (
                <div className="task-empty">no tasks</div>
              ) : (
                <ul>
                  {sorted.map((t) => {
                    const live = sessionById(t.cb_session_id)
                    const working = t.status === 'in_progress' && live?.status === 'working'
                    const glyph =
                      t.status === 'in_progress'
                        ? live
                          ? (SESSION_GLYPH[live.status] ?? { g: '◐', cls: 'st-waiting' })
                          : { g: '◐', cls: 'st-waiting' }
                        : (REST_GLYPH[t.status] ?? { g: '·', cls: '' })
                    const canStart = t.status === 'pending' || t.status === 'paused'
                    return (
                      <li
                        key={t.id}
                        className={`task-row ${t.status === 'completed' ? 'done' : ''}`}
                      >
                        {working ? (
                          <Spinner />
                        ) : (
                          <span className={`glyph ${glyph.cls}`}>{glyph.g}</span>
                        )}
                        <button
                          className="task-title"
                          title={t.desc || t.title}
                          onClick={() =>
                            t.status === 'in_progress' && t.cb_session_id
                              ? onJump(t.cb_session_id)
                              : setEditor({ scope: t.scope, task: t })
                          }
                        >
                          {t.title}
                        </button>
                        <div className="task-actions">
                          {canStart && (
                            <button
                              className="icon-btn"
                              title="start an agent on this task"
                              onClick={() =>
                                agents.length === 1
                                  ? onStart(t.id, agents[0], repoCwd(t.scope, sessions))
                                  : setStarting(t)
                              }
                            >
                              <IconPlay />
                            </button>
                          )}
                          <button
                            className="icon-btn task-done"
                            title={t.status === 'completed' ? 'mark not done' : 'mark done'}
                            onClick={() =>
                              onStatus(t.id, t.status === 'completed' ? 'pending' : 'completed')
                            }
                          >
                            <IconCheck />
                          </button>
                          <button
                            className="icon-btn"
                            title="edit"
                            onClick={() => setEditor({ scope: t.scope, task: t })}
                          >
                            <IconPencil />
                          </button>
                          <button
                            className="icon-btn task-del"
                            title="delete"
                            onClick={() => onDelete(t.id)}
                          >
                            <IconTrash />
                          </button>
                        </div>
                      </li>
                    )
                  })}
                </ul>
              )}
            </li>
          )
        })}
      </ul>

      {editor && (
        <TaskEditor
          task={editor.task}
          onClose={() => setEditor(null)}
          onSave={(title, desc) => {
            if (editor.task) onEdit(editor.task.id, title, desc)
            else onAdd(editor.scope, title, desc)
            setEditor(null)
          }}
        />
      )}

      {starting && (
        <AgentPicker
          task={starting}
          agents={agents}
          onClose={() => setStarting(null)}
          onPick={(agent) => {
            onStart(starting.id, agent, repoCwd(starting.scope, sessions))
            setStarting(null)
          }}
        />
      )}
    </>
  )
}

// TaskEditor is the new/edit modal: a title line and a multi-line description,
// reused for both creating and editing (create when task is undefined).
function TaskEditor({
  task,
  onClose,
  onSave,
}: {
  task?: TaskInfo
  onClose: () => void
  onSave: (title: string, desc: string) => void
}) {
  const [title, setTitle] = useState(task?.title ?? '')
  const [desc, setDesc] = useState(task?.desc ?? '')
  const submit = () => {
    const t = title.trim()
    if (t) onSave(t, desc)
  }
  return (
    <div className="overlay" onClick={onClose}>
      <div className="picker task-editor" onClick={(e) => e.stopPropagation()}>
        <div className="picker-title">{task ? 'edit task' : 'new task'}</div>
        <input
          className="task-input"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder="title"
          autoFocus
          onKeyDown={(e) => {
            if (e.key === 'Enter') submit()
            if (e.key === 'Escape') onClose()
          }}
        />
        <textarea
          className="task-textarea"
          value={desc}
          onChange={(e) => setDesc(e.target.value)}
          placeholder="description (optional)"
          rows={4}
        />
        <div className="task-editor-row">
          <button className="task-btn" onClick={onClose}>
            cancel
          </button>
          <button className="task-btn primary" onClick={submit} disabled={!title.trim()}>
            {task ? 'save' : 'add'}
          </button>
        </div>
      </div>
    </div>
  )
}

// AgentPicker chooses which agent runs a task, when more than one is installed.
function AgentPicker({
  task,
  agents,
  onClose,
  onPick,
}: {
  task: TaskInfo
  agents: string[]
  onClose: () => void
  onPick: (agent: string) => void
}) {
  const resume = task.status === 'paused'
  return (
    <div className="overlay" onClick={onClose}>
      <div className="picker" onClick={(e) => e.stopPropagation()}>
        <div className="picker-title">
          <IconX />
          {resume ? 'resume' : 'start'}: {task.title}
        </div>
        {agents.map((a) => (
          <button key={a} className="picker-row" onClick={() => onPick(a)}>
            {a}
          </button>
        ))}
        {resume && (
          <div className="picker-tag" style={{ padding: '4px 10px' }}>
            resumes the previous agent session where possible
          </div>
        )}
      </div>
    </div>
  )
}
