import {
  ApartmentOutlined,
  BranchesOutlined,
  HeartOutlined,
  HistoryOutlined,
  BarChartOutlined,
  TeamOutlined,
  UnorderedListOutlined,
} from '@ant-design/icons'
import { Alert, Tabs } from 'antd'
import { useEffect, useMemo, useRef, useState, type AnimationEvent } from 'react'
import { derive } from '../derive.ts'
import type { HubData } from '../useHub.ts'
import { ActivityCard } from './Activity.tsx'
import { ContestedFiles } from './Contested.tsx'
import { DemoBanner } from './DemoBanner.tsx'
import { HealthCard } from './Health.tsx'
import { Hero } from './Hero.tsx'
import { LiveAgents } from './LiveAgents.tsx'
import { PullRequests } from './PullRequests.tsx'
import { Workspace } from './Workspace.tsx'
import { SkillWorkflow } from './SkillWorkflow.tsx'
import { Standup } from './Standup.tsx'
import { AgentSetup } from './AgentSetup.tsx'
import { WorkInsights } from './WorkInsights.tsx'

type HubSection = 'coordination' | 'work' | 'insights' | 'workflows' | 'activity' | 'health' | 'workspace'

const sectionFromHash = (): HubSection => {
  if (window.location.hash.startsWith('#coordination/work/')) return 'work'
  const section = window.location.hash.slice(1).split('/')[0]
  return section === 'work' || section === 'insights' || section === 'workflows' || section === 'activity' || section === 'health' || section === 'workspace' ? section : 'coordination'
}

const navigation = [
  { key: 'coordination', label: <span><ApartmentOutlined /> Coordination</span> },
  { key: 'work', label: <span><UnorderedListOutlined /> Work</span> },
  { key: 'insights', label: <span><BarChartOutlined /> Insights</span> },
  { key: 'workflows', label: <span><BranchesOutlined /> Workflows</span> },
  { key: 'activity', label: <span><HistoryOutlined /> Activity</span> },
  { key: 'workspace', label: <span><TeamOutlined /> Workspace</span> },
  { key: 'health', label: <span><HeartOutlined /> Health</span> },
]

export function Dashboard({ data, token }: { data: HubData; token: string }) {
  const { work, activity } = data
  const [section, setSection] = useState<HubSection>(sectionFromHash)
  const [intro, setIntro] = useState(true)
  const started = useRef(false)
  const d = useMemo(() => (work && activity ? derive(work, activity) : null), [work, activity])
  const repo = data.whoami?.token.repository ?? 'this repository'

  useEffect(() => {
    const onHashChange = () => setSection(sectionFromHash())
    window.addEventListener('hashchange', onHashChange)
    return () => window.removeEventListener('hashchange', onHashChange)
  }, [])

  const onStart = (event: AnimationEvent<HTMLElement>) => {
    if (started.current || event.animationName !== 'rise') return
    started.current = true
    window.setTimeout(() => setIntro(false), 800)
  }

  const selectSection = (next: string) => {
    if (next === section && window.location.hash === `#${next}`) return
    window.location.hash = next
  }

  return (
    <main className={`page hub-page${intro ? ' intro' : ''}`} onAnimationStart={onStart}>
      {data.health?.demo && <DemoBanner />}
      <AgentSetup token={token} />
      {import.meta.env.DEV && import.meta.env.VITE_SYNTHETIC_PREVIEW === '1' && (
        <Alert className="synthetic-workflow-preview" type="info" showIcon message="Synthetic examples · not live coding sessions" />
      )}
      <nav className="hub-navigation" aria-label="Repository sections">
        <Tabs activeKey={section} items={navigation} onChange={selectSection} />
      </nav>

      {section === 'coordination' && (
        <div className="hub-section" aria-label="Coordination">
          <Hero repo={repo} d={d} failing={data.failing} />
          <div className="grid">
            <div className="col">
              <LiveAgents d={d} truncated={work?.intents_truncated ?? false} index={1} />
              <PullRequests d={d} connected={work?.connected ?? true} demo={data.health?.demo === true} index={3} />
            </div>
            <div className="col col-side">
              <ContestedFiles d={d} index={2} />
            </div>
          </div>
        </div>
      )}

      {section === 'work' && <Standup key={repo} token={token} repository={repo} />}

      {section === 'insights' && <WorkInsights key={repo} token={token} />}

      {section === 'workflows' && (
        <SkillWorkflow
          key={data.whoami?.token.repository ?? 'loading'}
          token={token}
          repository={data.whoami?.token.repository ?? null}
        />
      )}

      {section === 'workspace' && <Workspace />}

      {section === 'activity' && (
        <div className="hub-section hub-section-narrow" aria-label="Activity">
          <ActivityCard d={d} events={activity?.events ?? null} fresh={data.fresh} index={1} />
        </div>
      )}

      {section === 'health' && (
        <div className="hub-section hub-section-narrow" aria-label="Health">
          <HealthCard health={data.health} index={1} />
        </div>
      )}
    </main>
  )
}
