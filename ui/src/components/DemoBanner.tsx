import { PlayCircleOutlined } from '@ant-design/icons'
import type { CSSProperties } from 'react'

export function DemoBanner() {
  return (
    <aside className="demo-banner rise" style={{ '--i': 0 } as CSSProperties} aria-label="Demo hub">
      <span className="tint-icon tone-amber">
        <PlayCircleOutlined />
      </span>
      <p>
        <strong>Demo hub</strong> — simulated agents working on your repository's file names. Nothing is written to your repository; this page stops when <code>agentscm demo</code> does.
      </p>
    </aside>
  )
}
