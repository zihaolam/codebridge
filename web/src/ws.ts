// WS client for the cb web bridge. One multiplexed socket: session-list
// snapshots, the attached session's frame stream, and upstream input.
// Message shapes mirror internal/web/messages.go.

export type SessionInfo = {
  id: string
  name?: string
  argv: string[]
  cwd: string
  status: string
  last_message?: string
  exited: boolean
  status_since?: number
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

export type Down = {
  type: 'hello' | 'sessions' | 'frame' | 'gone' | 'spawned' | 'worktrees' | 'error'
  protocol?: number
  daemon?: boolean
  sessions?: SessionInfo[]
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

const RETRY_MS = 2000

export class CbClient {
  onState?: (s: ClientState) => void
  onHello?: (protocol: number, daemon: boolean) => void
  onSessions?: (s: SessionInfo[]) => void
  onFrame?: (f: Down) => void
  onGone?: (id: string) => void
  onError?: (msg: string) => void
  onSpawned?: (id: string) => void
  onWorktrees?: (cwd: string, worktrees: WorktreeEntry[], agents: string[]) => void

  private ws?: WebSocket
  private token: string
  private stopped = false
  private attachedId: string | null = null

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
      setTimeout(() => {
        if (!this.stopped) this.connect()
      }, RETRY_MS)
    }
  }

  close() {
    this.stopped = true
    this.ws?.close()
  }

  private handle(m: Down) {
    switch (m.type) {
      case 'hello':
        this.onState?.('open')
        this.onHello?.(m.protocol ?? 0, m.daemon ?? false)
        // Re-attach after a reconnect so the frame stream resumes.
        if (this.attachedId) this.send({ type: 'attach', id: this.attachedId })
        break
      case 'sessions':
        this.onSessions?.(m.sessions ?? [])
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

  attach(id: string, rows?: number, cols?: number) {
    this.attachedId = id
    const m: Record<string, unknown> = { type: 'attach', id }
    // Claim the session size like the TUI does, so the view fills the pane.
    if (rows && cols) {
      m.rows = rows
      m.cols = cols
    }
    this.send(m)
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

  spawn(argv: string[], cwd: string) {
    this.send({ type: 'spawn', argv, cwd })
  }

  worktrees(cwd: string) {
    this.send({ type: 'worktrees', cwd })
  }

  kill(id: string) {
    this.send({ type: 'kill', id })
  }
}
