// Session status → a small colored dot, the app-chrome translation of the
// TUI's glyph language: pulsing green = working, green = turn complete,
// amber = idle, red = needs approval, cyan = starting, grey = ended. The
// pulse is CSS-only and disabled under prefers-reduced-motion.
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
  const pulse = status === 'working' ? ' pulse' : ''
  return <span className={`status-dot ${cls}${pulse}`} />
}
