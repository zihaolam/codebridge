// WS client for the cb web bridge. One multiplexed socket: session-list
// snapshots, the attached session's frame stream, and upstream input.
// Message shapes mirror the Rust protocol types used by the web bridge.

export type SessionInfo = {
  id: string
  name?: string
  argv: string[]
  cwd: string
  status: string
  last_message?: string
  exited: boolean
  status_since_unix_ms?: number
  // Sidebar grouping, computed bridge-side (repo common-dir semantics).
  scope?: string
  scope_name?: string
}

export type WorktreeEntry = {
  path: string
  branch?: string
  detached?: boolean
  bare?: boolean
  main?: boolean
}

// TaskInfo mirrors internal/web webTask (ipc.Task + scope_name). The daemon
// owns the backlog; the browser never persists tasks, it only sends task_*
// ops and renders the snapshots the bridge pushes.
export type TaskInfo = {
  id: string
  scope: string
  scope_name: string
  title: string
  desc?: string
  status: string
  runs?: TaskRun[]
  // Synthesized to record an ad-hoc agent session rather than authored in the
  // backlog; hidden from the task list, mirroring the TUI.
  auto?: boolean
  agent?: string
  cwd?: string
  cb_session_id?: string
  agent_session_id?: string
  created_at: string
  updated_at: string
}

export type TaskRun = {
  id: string
  agent?: string
  cwd?: string
  cb_session_id?: string
  agent_session_id?: string
  first_message?: string
  // Agent-summarised conversation title (Claude's ai-title / Codex's
  // thread_name), resolved lazily by the broker. Empty until generated.
  title?: string
  status: string
  created_at: string
  updated_at: string
}

export type Down = {
  type: 'hello' | 'sessions' | 'tasks' | 'frame' | 'gone' | 'spawned' | 'worktrees' | 'error'
  protocol?: number
  daemon?: boolean
  // The user's prefix-command config (hello only): the effective prefix key
  // and action -> key bindings, named exactly as tui.rs key_name produces.
  prefix?: string
  bindings?: Record<string, string>
  sessions?: SessionInfo[]
  tasks?: TaskInfo[]
  id?: string
  screen?: string
  cursor_x?: number
  cursor_y?: number
  alt?: boolean
  rows?: number
  cols?: number
  offset?: number
  max_offset?: number
  cwd?: string
  worktrees?: WorktreeEntry[]
  agents?: string[]
  error?: string
}

export type ClientState = 'connecting' | 'open' | 'auth-failed' | 'closed'

// b64 encodes text as base64 bytes (UTF-8), the encoding ipc.StreamUp.Data
// expects. btoa alone can't take non-latin1 input.
export function b64(text: string): string {
  const bytes = new TextEncoder().encode(text)
  let bin = ''
  for (const b of bytes) bin += String.fromCharCode(b)
  return btoa(bin)
}

// Reconnect backoff: start quick so a `cb restart` blip recovers almost
// immediately, then back off exponentially (with jitter) up to a cap so a
// genuinely-down bridge is not hammered. Reset to the base on a clean reconnect.
const BASE_RETRY_MS = 500
const MAX_RETRY_MS = 15_000

export class CbClient {
  onState?: (s: ClientState) => void
  onHello?: (protocol: number, daemon: boolean) => void
  onKeymap?: (prefix: string, bindings: Record<string, string>) => void
  onAgents?: (agents: string[]) => void
  onSessions?: (s: SessionInfo[]) => void
  onTasks?: (t: TaskInfo[]) => void
  onFrame?: (f: Down) => void
  onGone?: (id: string) => void
  onError?: (msg: string) => void
  onSpawned?: (id: string) => void
  onWorktrees?: (cwd: string, worktrees: WorktreeEntry[], agents: string[]) => void

  private ws?: WebSocket
  private token: string
  private stopped = false
  private attachedId: string | null = null
  private viewportSize: { rows: number; cols: number } | null = null
  private retryMs = BASE_RETRY_MS

  constructor(token: string) {
    this.token = token
  }

  connect() {
    this.onState?.('connecting')
    const proto = location.protocol === 'https:' ? 'wss' : 'ws'
    const ws = new WebSocket(`${proto}://${location.host}/ws`)
    this.ws = ws
    ws.onopen = () => {
      ws.send(JSON.stringify({ type: 'auth', token: this.token }))
    }
    ws.onmessage = (ev) => this.handle(JSON.parse(ev.data) as Down)
    ws.onclose = (ev) => {
      if (this.stopped) return
      // Policy violation = bad token; retrying would loop forever.
      if (ev.code === 1008) {
        this.onState?.('auth-failed')
        return
      }
      this.onState?.('closed')
      // ±30% jitter so multiple tabs don't reconnect in lockstep; advance the
      // backoff toward the cap for the next attempt.
      const delay = this.retryMs * (0.7 + Math.random() * 0.6)
      this.retryMs = Math.min(this.retryMs * 2, MAX_RETRY_MS)
      setTimeout(() => {
        if (!this.stopped) this.connect()
      }, delay)
    }
  }

  close() {
    this.stopped = true
    this.ws?.close()
  }

  private handle(m: Down) {
    switch (m.type) {
      case 'hello':
        // A clean handshake resets the backoff so the next drop retries fast.
        this.retryMs = BASE_RETRY_MS
        this.onState?.('open')
        this.onHello?.(m.protocol ?? 0, m.daemon ?? false)
        // Absent from a pre-keymap bridge; the client then keeps its defaults.
        if (m.prefix) this.onKeymap?.(m.prefix, m.bindings ?? {})
        this.onAgents?.(m.agents ?? [])
        // Re-attach after a reconnect so the frame stream resumes.
        if (this.attachedId) {
          this.send({ type: 'attach', id: this.attachedId })
          if (this.viewportSize) this.send({ type: 'viewport', ...this.viewportSize })
        }
        break
      case 'sessions':
        this.onSessions?.(m.sessions ?? [])
        break
      case 'tasks':
        this.onTasks?.(m.tasks ?? [])
        break
      case 'frame':
        this.onFrame?.(m)
        break
      case 'gone':
        if (m.id) this.onGone?.(m.id)
        break
      case 'spawned':
        if (m.id) this.onSpawned?.(m.id)
        break
      case 'worktrees':
        this.onWorktrees?.(m.cwd ?? '', m.worktrees ?? [], m.agents ?? [])
        break
      case 'error':
        if (m.error === 'auth failed') this.onState?.('auth-failed')
        else this.onError?.(m.error ?? 'unknown error')
        break
    }
  }

  private send(m: Record<string, unknown>) {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(m))
  }

  attach(id: string) {
    this.attachedId = id
    this.send({ type: 'attach', id })
  }

  input(text: string) {
    this.send({ type: 'input', data: b64(text) })
  }

  paste(text: string) {
    this.send({ type: 'paste', data: b64(text) })
  }

  scroll(offset: number) {
    this.send({ type: 'scroll', offset })
  }

  interrupt() {
    this.send({ type: 'interrupt' })
  }

  resize(rows: number, cols: number) {
    this.send({ type: 'resize', rows, cols })
  }

  viewport(rows: number, cols: number) {
    this.viewportSize = { rows, cols }
    this.send({ type: 'viewport', rows, cols })
  }

  spawn(argv: string[], cwd: string) {
    this.send({ type: 'spawn', argv, cwd })
  }

  worktrees(cwd: string) {
    this.send({ type: 'worktrees', cwd })
  }

  kill(id: string) {
    this.send({ type: 'kill', id })
  }

  // Backlog ops — all proxied by the bridge to the daemon (the single writer).
  taskAdd(scope: string, title: string, desc = '') {
    this.send({ type: 'task_add', scope, title, desc })
  }

  taskEdit(id: string, title: string, desc: string) {
    this.send({ type: 'task_edit', id, title, desc })
  }

  // status maps to the wire's `task_status` field (see internal/web wsUp).
  taskStatus(id: string, status: string) {
    this.send({ type: 'task_status', id, task_status: status })
  }

  taskDelete(id: string) {
    this.send({ type: 'task_delete', id })
  }

  taskStart(id: string, agent: string, cwd: string) {
    this.send({ type: 'task_start', id, agent, cwd })
  }

  // Resume a paused run (the session-history picker). The daemon respawns in
  // the run's origin cwd — the cwd here is only its fallback — and the bridge
  // answers `spawned` with the fresh session id.
  taskResume(id: string, runId: string, cwd: string) {
    this.send({ type: 'task_resume', id, run_id: runId, cwd })
  }
}
