import { useEffect, useState } from 'react'

// Braille spinner matching the TUI's working indicator. Frame-swapping in JS
// instead of a CSS rotate: rotating a glyph in place wobbles around its text
// box and reads as broken.
const FRAMES = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']

export default function Spinner() {
  const [i, setI] = useState(0)
  useEffect(() => {
    const t = setInterval(() => setI((x) => (x + 1) % FRAMES.length), 100)
    return () => clearInterval(t)
  }, [])
  return <span className="glyph st-working">{FRAMES[i]}</span>
}
