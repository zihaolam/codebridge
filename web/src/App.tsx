import { useEffect, useMemo, useRef, useState } from 'react'
import { CbClient, type ClientState, type SessionInfo, type TaskInfo } from './ws'
import SessionList from './SessionList'
import TaskList from './TaskList'
import Term from './Term'
import WorktreePicker, { type PickerData } from './WorktreePicker'
import HistoryModal from './HistoryModal'
import KeyBar from './KeyBar'
import StatusDot from './StatusDot'
import { basename, sessionLabel } from './format'
import { DEFAULT_BINDINGS, DEFAULT_PREFIX, WEB_ACTIONS, keyName } from './prefix'
import { IconChevronLeft, IconHistory, IconMaximize, IconX } from './icons'

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
      <span className="brand-tile">cb</span>
      <h1>codebridge</h1>
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
  const [tasks, setTasks] = useState<TaskInfo[]>([])
  const [agents, setAgents] = useState<string[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [errors, setErrors] = useState<string[]>([])
  // On phones the list and the terminal are two swappable screens; on desktop
  // the CSS ignores this and shows both panes side by side.
  const [view, setView] = useState<'list' | 'term'>('list')
  // The sidebar shows either the session list or the backlog (list icon toggle).
  const [screen, setScreen] = useState<'sessions' | 'tasks'>('sessions')
  const [picker, setPicker] = useState<PickerData | null>(null)
  // The TUI's prefix-command state: the configured keymap (hello carries the
  // user's real config; defaults until then), whether the prefix is armed,
  // and the help / session-history overlays it opens.
  const [keymap, setKeymap] = useState({ prefix: DEFAULT_PREFIX, bindings: DEFAULT_BINDINGS })
  const [armed, setArmed] = useState(false)
  const [help, setHelp] = useState(false)
  const [historyOpen, setHistoryOpen] = useState(false)
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
    client.onTasks = setTasks
    client.onKeymap = (prefix, bindings) =>
      setKeymap({ prefix, bindings: { ...DEFAULT_BINDINGS, ...bindings } })
    client.onAgents = setAgents
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

  // Agent-summarised titles, joined from the task snapshot: a live run's
  // cb_session_id names its session, its title is the broker-resolved summary
  // (Claude's ai-title / Codex's thread_name). Parked runs have the id
  // cleared, so this only ever labels running sessions — same rule as the TUI
  // sidebar.
  const titles = useMemo(() => {
    const map = new Map<string, string>()
    for (const t of tasks) {
      for (const r of t.runs ?? []) {
        const title = r.title?.trim()
        if (r.cb_session_id && title) map.set(r.cb_session_id, title)
      }
    }
    return map
  }, [tasks])

  const current = sessions.find((s) => s.id === selected)
  const workspace = current ? current.scope_name || basename(current.cwd) : ''

  const kill = (id: string) => {
    const s = sessions.find((x) => x.id === id)
    if (s && confirm(`Kill ${sessionLabel(s, titles)}?`)) client.kill(id)
  }

  const killCurrent = () => {
    if (!current) return
    if (!confirm(`Kill ${sessionLabel(current, titles)}?`)) return
    client.kill(current.id)
    setSelected(null)
    setView('list')
  }

  // Prefix-command dispatch, mirroring tui.rs handle_prefix for the actions a
  // browser can perform. The TUI spawns in its launch cwd; the browser has no
  // launch cwd, so new sessions land next to the current one (or the first
  // listed session as a fallback).
  const runPrefixAction = (action?: string) => {
    const cwd = current?.cwd ?? sessions[0]?.cwd
    switch (action) {
      case 'new_claude':
      case 'new_codex':
        if (cwd) client.spawn([action === 'new_claude' ? 'claude' : 'codex'], cwd)
        break
      case 'new_worktree':
        if (cwd) {
          setPicker({ cwd, worktrees: null, agents: [] })
          client.worktrees(cwd)
        }
        break
      case 'kill':
        killCurrent()
        break
      case 'jump_pending': {
        const s = sessions.find((x) => x.status === 'needs_approval')
        if (s) {
          setSelected(s.id)
          setView('term')
        }
        break
      }
      case 'resize_pane':
        window.dispatchEvent(new Event('cb-resize-session'))
        break
      case 'newline':
        client.input('\n')
        break
      case 'task_backlog':
        setScreen('tasks')
        setView('list')
        break
      case 'session_history':
        setHistoryOpen(true)
        break
    }
  }

  // Snapshot the state the key handler needs; the listener itself registers
  // once (capture phase, so xterm never sees a swallowed key) and reads the
  // latest through this ref, the same pattern as selectedRef above.
  const prefixCtx = useRef({ keymap, armed, help, modal: false, run: runPrefixAction })
  prefixCtx.current = {
    keymap,
    armed,
    help,
    modal: picker !== null || historyOpen,
    run: runPrefixAction,
  }

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const ctx = prefixCtx.current
      if (ctx.modal) return // pickers own their keys
      // Leave real form fields alone (task editor, rename inputs); xterm's
      // hidden textarea is the exception — it IS the terminal.
      const target = e.target as HTMLElement | null
      if (
        target &&
        (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA') &&
        !target.classList.contains('xterm-helper-textarea')
      )
        return
      if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return
      const name = keyName(e)
      if (!ctx.armed && !ctx.help) {
        if (name && name === ctx.keymap.prefix) {
          e.preventDefault()
          e.stopPropagation()
          setArmed(true)
        }
        return
      }
      // Armed (or help open): the keystroke is a command, never terminal
      // input — swallow it whether or not it maps to anything, like the TUI.
      e.preventDefault()
      e.stopPropagation()
      if (name === ctx.keymap.prefix) {
        // Pressing the prefix again just re-arms.
        setHelp(false)
        setArmed(true)
        return
      }
      setArmed(false)
      if (e.key === '?') {
        setHelp(!ctx.help)
        return
      }
      setHelp(false)
      if (e.key === 'Escape') return
      const action = Object.entries(ctx.keymap.bindings).find(([, key]) => key === name)?.[0]
      ctx.run(action)
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [])

  return (
    <div className={`app view-${view}`}>
      <aside>
        <header>
          <span className="brand-tile">cb</span>
          <span className="brand-name">codebridge</span>
          <span className={`conn-dot conn-${state}`} title={state} />
          <button
            className="icon-btn"
            title="session history — resume past sessions (prefix m)"
            onClick={() => setHistoryOpen(true)}
          >
            <IconHistory />
          </button>
        </header>
        <nav className="seg">
          <button
            className={screen === 'sessions' ? 'on' : ''}
            onClick={() => setScreen('sessions')}
          >
            Sessions
          </button>
          <button className={screen === 'tasks' ? 'on' : ''} onClick={() => setScreen('tasks')}>
            Tasks
          </button>
        </nav>
        {screen === 'tasks' ? (
          <TaskList
            tasks={tasks}
            sessions={sessions}
            agents={agents}
            onJump={(id) => {
              setSelected(id)
              setView('term')
            }}
            onAdd={(scope, title, desc) => client.taskAdd(scope, title, desc)}
            onEdit={(id, title, desc) => client.taskEdit(id, title, desc)}
            onStatus={(id, status) => client.taskStatus(id, status)}
            onDelete={(id) => {
              const t = tasks.find((x) => x.id === id)
              if (t && confirm(`Delete "${t.title}"?`)) client.taskDelete(id)
            }}
            onStart={(id, agent, cwd) => client.taskStart(id, agent, cwd)}
          />
        ) : (
          <SessionList
            sessions={sessions}
            titles={titles}
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
        )}
      </aside>
      <main>
        <div className="topbar">
          <button className="icon-btn back" onClick={() => setView('list')}>
            <IconChevronLeft />
          </button>
          {current ? (
            <>
              <StatusDot status={current.status} />
              <div className="topbar-title">
                <span className="topbar-name">{sessionLabel(current, titles)}</span>
                <span className="topbar-scope">{workspace}</span>
              </div>
              <div className="topbar-actions">
                <button
                  className="icon-btn"
                  title="resize session to this screen"
                  onClick={() => window.dispatchEvent(new Event('cb-resize-session'))}
                >
                  <IconMaximize />
                </button>
                <button
                  className="icon-btn topbar-x"
                  title="kill this session"
                  onClick={killCurrent}
                >
                  <IconX />
                </button>
              </div>
            </>
          ) : (
            <span className="topbar-scope">no session</span>
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
          <div className="empty">
            <div className="empty-title">No session selected</div>
            <div className="empty-sub">Pick a session from the list</div>
          </div>
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
      {historyOpen && (
        <HistoryModal
          tasks={tasks}
          sessions={sessions}
          onClose={() => setHistoryOpen(false)}
          onJump={(id) => {
            setSelected(id)
            setView('term')
            setHistoryOpen(false)
          }}
          onResume={(taskId, runId, cwd) => {
            client.taskResume(taskId, runId, cwd)
            setHistoryOpen(false)
          }}
        />
      )}
      {help && (
        <div className="overlay" onClick={() => setHelp(false)}>
          <div className="picker" onClick={(e) => e.stopPropagation()}>
            <div className="picker-title">prefix = {keymap.prefix}</div>
            {WEB_ACTIONS.map((a) => (
              <div key={a.id} className="help-row">
                <kbd>{keymap.bindings[a.id] ?? a.default}</kbd>
                <span>{a.label}</span>
              </div>
            ))}
          </div>
        </div>
      )}
      {armed && <div className="prefix-chip">prefix — ? for help</div>}
    </div>
  )
}
