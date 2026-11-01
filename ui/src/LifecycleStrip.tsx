// API for other packages:
//   <LifecycleStrip steps={lifecycle(input)} />   the six-step strip, each step linked to its evidence with a proof badge
import type { MouseEvent } from 'react'
import type { LifecycleStep } from './lifecycle.ts'
import { ProofBadge } from './Proof.tsx'
import { Tip } from './ui.tsx'
import './plain.css'

/** An in-page anchor (`#agent-reports`) scrolls to its element instead of changing the hash route. */
export function scrollToAnchor(e: MouseEvent<HTMLAnchorElement>, href: string) {
  const el = href.startsWith('#') ? document.getElementById(href.slice(1)) : null
  if (!el) return
  e.preventDefault()
  el.scrollIntoView({ block: 'start' })
}

/** One task's lifecycle, drawn with the After merge track: label (a link when there is evidence), word, proof. */
export function LifecycleStrip({ steps }: { steps: LifecycleStep[] }) {
  return (
    <ol className="track is-lifecycle" aria-label="Where this task is">
      {steps.map((s) => {
        const external = s.href !== undefined && !s.href.startsWith('#')
        return (
          <li key={s.key} className={`track-step tone-${s.tone}`}>
            <span className="track-dot" aria-hidden="true">
              <i className={`bi bi-${s.icon}`} />
            </span>
            <span className="track-text">
              {s.href ? (
                <a className="track-label" href={s.href} onClick={(e) => scrollToAnchor(e, s.href!)} {...(external ? { target: '_blank', rel: 'noreferrer noopener' } : {})}>
                  {s.label}
                </a>
              ) : (
                <span className="track-label">{s.label}</span>
              )}
              {s.detail || s.how ? (
                <Tip text={[s.detail, s.how && `How it gets here: ${s.how}`].filter(Boolean).join(' ')} focusable className="track-word">
                  {s.word}
                </Tip>
              ) : (
                <span className="track-word">{s.word}</span>
              )}
              {s.proof && (
                <span className="track-proof">
                  <ProofBadge level={s.proof} tip={s.proofTip} />
                </span>
              )}
            </span>
          </li>
        )
      })}
    </ol>
  )
}
