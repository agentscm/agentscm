import { useMemo, useRef, useState, type AnimationEvent } from 'react'
import { derive } from '../derive.ts'
import type { HubData } from '../useHub.ts'
import { ActivityCard } from './Activity.tsx'
import { DemoBanner } from './DemoBanner.tsx'
import { ContestedFiles } from './Contested.tsx'
import { HealthCard } from './Health.tsx'
import { Hero } from './Hero.tsx'
import { LiveAgents } from './LiveAgents.tsx'
import { PullRequests } from './PullRequests.tsx'
import { SkillWorkflow } from './SkillWorkflow.tsx'

export function Dashboard({ data, token }: { data: HubData; token: string }) {
  const { work, activity } = data
  // Entrance motion runs once per page load: the intro class is dropped shortly after the first card starts
  // animating, so theme switches, polls and re-renders never replay it.
  const [intro, setIntro] = useState(true)
  const started = useRef(false)
  const onStart = (e: AnimationEvent<HTMLElement>) => {
    if (started.current || e.animationName !== 'rise') return
    started.current = true
    window.setTimeout(() => setIntro(false), 800)
  }
  const d = useMemo(() => (work && activity ? derive(work, activity) : null), [work, activity])
  const repo = data.whoami?.token.repository ?? 'this repository'
  return (
    <main className={`page${intro ? ' intro' : ''}`} onAnimationStart={onStart}>
      {data.health?.demo && <DemoBanner />}
      <Hero repo={repo} d={d} failing={data.failing} />
      <div className="grid">
        <div className="col">
          <LiveAgents d={d} truncated={work?.intents_truncated ?? false} index={2} />
          <PullRequests d={d} connected={work?.connected ?? true} demo={data.health?.demo === true} index={3} />
          <ActivityCard d={d} events={activity?.events ?? null} fresh={data.fresh} index={4} />
        </div>
        <div className="col col-side">
          <ContestedFiles d={d} index={1} />
          <SkillWorkflow key={data.whoami?.token.repository ?? 'loading'} token={token} repository={data.whoami?.token.repository ?? null} index={5} />
          <HealthCard health={data.health} index={6} />
        </div>
      </div>
    </main>
  )
}
