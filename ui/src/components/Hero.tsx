import { FileTextOutlined, PullRequestOutlined, TeamOutlined, WarningOutlined } from '@ant-design/icons'
import type { CSSProperties, ReactNode } from 'react'
import { notable, type Derived } from '../derive.ts'
import { clock } from './bits.tsx'

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`

function headline(repo: string, d: Derived): string {
  if (d.agents.length === 0) return `All quiet in ${repo}.`
  const verb = d.agents.length === 1 ? 'is' : 'are'
  const c = d.contested.length
  const tail = c === 0 ? 'No files are contested.' : `${plural(c, 'file', 'files')} ${c === 1 ? 'is' : 'are'} contested.`
  return `${plural(d.agents.length, 'agent', 'agents')} ${verb} working in ${repo}. ${tail}`
}

function keyFact(d: Derived): string {
  const n = notable(d.contested)
  if (n?.kind === 'handoff' && n.c.handoff) {
    const h = n.c.handoff
    const state = h.answer ? (h.answer.kind === 'granted' ? 'handed over' : 'kept') : h.lapsed ? 'lapsed' : 'waiting'
    return `${h.from.owner}'s ${h.from.name} asked ${h.to.owner}'s ${h.to.name} for ${n.c.path} · ${state}.`
  }
  if (n?.c.refused) {
    const { c } = n
    const r = n.c.refused
    return `${r.owner}'s ${r.name} was refused on ${c.path} at ${clock(r.at)}${c.repeated ? ', then repeated.' : '.'}`
  }
  const c = d.contested[0]
  if (c) return `${c.holders.map((h) => `${h.owner}'s ${h.name}`).join(' and ')} both hold ${c.path}.`
  const p = d.prOverlaps[0]
  const w = p?.with[0]
  if (p && w) return `PR #${p.pr.number} touches files that ${w.agent.owner}'s ${w.agent.name} is working on.`
  if (d.agents.length > 0) return 'Agents are working on separate files.'
  return 'Agents that connect to this repository will appear here within seconds.'
}

function Sparkle() {
  return (
    <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" fill="currentColor">
      <path d="M8 0C8.6 4.5 11.5 7.4 16 8C11.5 8.6 8.6 11.5 8 16C7.4 11.5 4.5 8.6 0 8C4.5 7.4 7.4 4.5 8 0Z" />
    </svg>
  )
}

export function Hero({ repo, d, failing }: { repo: string; d: Derived | null; failing: boolean }) {
  const tiles: { label: string; n: number | null; icon: ReactNode; warm?: boolean }[] = [
    { label: 'Agents live', n: d?.agents.length ?? null, icon: <TeamOutlined /> },
    { label: 'Files held', n: d?.filesHeld ?? null, icon: <FileTextOutlined /> },
    { label: 'Contested', n: d?.contested.length ?? null, icon: <WarningOutlined />, warm: true },
    { label: 'PR overlaps', n: d?.prOverlaps.length ?? null, icon: <PullRequestOutlined />, warm: true },
  ]
  return (
    <section className="hero rise" style={{ '--i': 0 } as CSSProperties} aria-label="Brief">
      <span className="hero-tex" aria-hidden="true" />
      <div className="hero-copy">
        <p className="eyebrow">
          <Sparkle /> Right now
        </p>
        {d ? (
          <>
            <p className="hero-sentence">{headline(repo, d)}</p>
            <p className="hero-fact">{keyFact(d)}</p>
          </>
        ) : failing ? (
          <>
            <p className="hero-sentence">Waiting for the hub.</p>
            <p className="hero-fact">The hub is not answering right now. This page keeps trying and fills in as soon as it does.</p>
          </>
        ) : (
          <>
            <span className="hero-skel hero-skel-a" />
            <span className="hero-skel hero-skel-b" />
          </>
        )}
      </div>
      <dl className="hero-stats">
        {tiles.map(({ label, n, icon, warm }) => (
          <div className={`stat${warm && n ? ' is-warm' : ''}`} key={label}>
            <span className="stat-icon" aria-hidden="true">
              {icon}
            </span>
            <dt>{label}</dt>
            <dd>{n ?? '–'}</dd>
          </div>
        ))}
      </dl>
    </section>
  )
}
