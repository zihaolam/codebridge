// Session status → app-chrome translation of the TUI's glyph language:
// spinning green ring = working, green = turn complete, amber = idle, red =
// needs approval, cyan = starting, grey = ended. The spinner is CSS-only and
// parks as a static ring under prefers-reduced-motion.
const CLS: Record<string, string> = {
  starting: 'st-starting',
  working: 'st-working',
  idle: 'st-idle',
  waiting_user: 'st-waiting',
  needs_approval: 'st-approval',
  ended: 'st-ended',
}

export default function StatusDot({ status }: { status: string }) {
  const cls = CLS[status] ?? 'st-ended'
  if (status === 'working') return <span className={`status-spinner ${cls}`} />
  return <span className={`status-dot ${cls}`} />
}
