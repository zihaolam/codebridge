// Shared presentation helpers: session labels, agent names, relative time.
import type { SessionInfo } from './ws'

export function basename(p: string): string {
  return p.replace(/\/+$/, '').split('/').pop() ?? p
}

// The agent binary name ("claude", "codex"), from argv[0].
export function agentOf(s: SessionInfo): string {
  return basename(s.argv[0] ?? '') || '?'
}

// Label precedence, mirroring the TUI sidebar: an explicit rename wins, then
// the agent-summarised conversation title (Claude's ai-title / Codex's
// thread_name, resolved by the broker onto the session's live run), then the
// agent name. `titles` maps cb session id -> run title (built in App from the
// task snapshot; a parked run's cb_session_id is cleared, so only the live
// run for a session can match).
export function sessionLabel(s: SessionInfo, titles?: Map<string, string>): string {
  if (s.name) return s.name
  const title = titles?.get(s.id)
  if (title) return title
  return agentOf(s)
}

// Compact relative time ("now", "5m", "2h", "3d"). Snapshots re-render the
// lists about once a second, so this stays fresh without a timer.
export function ago(unixMs?: number): string {
  if (!unixMs) return ''
  const s = Math.max(0, Math.floor((Date.now() - unixMs) / 1000))
  if (s < 60) return 'now'
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h`
  return `${Math.floor(h / 24)}d`
}
