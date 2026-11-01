// What the Inbox pane shows when nothing needs settling: two little agent cursors politely sidestepping each other around a
// file, next to a "no conflicts" sign. Pure decoration (aria-hidden, no focus) with the real sentence beside it. It loops
// gently, and is completely still under prefers-reduced-motion (see `.in-idle` in twilight.css), where it rests on the
// moment they pass.

export function IdleScene({ partial = false }: { partial?: boolean }) {
  return (
    <div className="in-idle" role="status">
      <svg className="in-idle-art" viewBox="0 -30 440 270" aria-hidden="true" focusable="false">
        <ellipse className="in-shadow" cx="220" cy="216" rx="160" ry="9" />
        <g className="in-file" transform="translate(170 70)">
          <path d="M12 0h62l26 26v98a12 12 0 0 1-12 12H12a12 12 0 0 1-12-12V12A12 12 0 0 1 12 0z" />
          <path className="in-fold" d="M74 0v18a8 8 0 0 0 8 8h18z" />
          <rect className="in-line" x="16" y="48" width="56" height="6" rx="3" />
          <rect className="in-line" x="16" y="64" width="68" height="6" rx="3" />
          <rect className="in-line" x="16" y="80" width="44" height="6" rx="3" />
          <rect className="in-line" x="16" y="96" width="60" height="6" rx="3" />
        </g>
        <g className="in-sign">
          <rect className="in-post" x="372" y="132" width="5" height="84" rx="2.5" />
          <g className="in-board">
            <rect x="338" y="88" width="74" height="46" rx="10" />
            <text x="375" y="108" textAnchor="middle">no</text>
            <text x="375" y="124" textAnchor="middle">conflicts</text>
          </g>
        </g>
        <g className="in-cur in-cur-b">
          <path className="in-arrow" d="M0 0v18l5-4.5 3.6 8.2 3.2-1.4-3.6-8.1H16z" />
          <g className="in-tag">
            <rect x="14" y="14" width="40" height="16" rx="8" />
            <text x="34" y="26" textAnchor="middle">codex</text>
          </g>
          <g className="in-bub in-bub-b">
            <rect x="-30" y="36" width="94" height="24" rx="12" />
            <text x="17" y="52" textAnchor="middle">no, after you!</text>
          </g>
        </g>
        <g className="in-cur in-cur-a">
          <path className="in-arrow" d="M0 0v18l5-4.5 3.6 8.2 3.2-1.4-3.6-8.1H16z" />
          <g className="in-tag">
            <rect x="14" y="14" width="52" height="16" rx="8" />
            <text x="40" y="26" textAnchor="middle">claude</text>
          </g>
          <g className="in-bub in-bub-a">
            <rect x="-42" y="-40" width="68" height="24" rx="12" />
            <text x="-8" y="-24" textAnchor="middle">after you</text>
          </g>
        </g>
      </svg>
      <h2 className="in-idle-t">Waiting for a conflict. None yet.</h2>
      <p className="in-idle-s">{partial ? 'No overlap in this partial snapshot. An empty list is not an all-clear.' : 'When two agents plan the same files, or one asks yours for a file, the conversation shows up here.'}</p>
    </div>
  )
}
