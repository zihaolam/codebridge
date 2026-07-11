// Tiny inline SVG icons (lucide-style paths) — no icon package needed.
type P = { size?: number }

const base = {
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 2.2,
  strokeLinecap: 'round' as const,
  strokeLinejoin: 'round' as const,
}

export const IconX = ({ size = 13 }: P) => (
  <svg width={size} height={size} viewBox="0 0 24 24" {...base}>
    <path d="M18 6 6 18M6 6l12 12" />
  </svg>
)

export const IconPlus = ({ size = 14 }: P) => (
  <svg width={size} height={size} viewBox="0 0 24 24" {...base}>
    <path d="M12 5v14M5 12h14" />
  </svg>
)

export const IconChevronLeft = ({ size = 16 }: P) => (
  <svg width={size} height={size} viewBox="0 0 24 24" {...base}>
    <path d="m15 18-6-6 6-6" />
  </svg>
)
