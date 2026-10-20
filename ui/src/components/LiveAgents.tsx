import { BranchesOutlined, ExportOutlined, LaptopOutlined } from '@ant-design/icons'
import { Skeleton } from 'antd'
import type { Derived, LiveAgent, OwnerGroup } from '../derive.ts'
import { useNow } from '../useHub.ts'
import { Av, Chips, RelTime, Section, ViaTag } from './bits.tsx'

const MAX_AGENTS = 30
const DOCS_AGENTS = 'https://github.com/counterbranch/agentscm/blob/main/docs/agents.md'

function LastActive({ at }: { at: number }) {
  const now = useNow()
  const fresh = now / 1000 - at < 120
  return (
    <span className="meta-item active">
      <span className={`dot ${fresh ? 'dot-live' : 'dot-idle'}`} aria-hidden="true" />
      <span className="sr-only">{fresh ? 'Active' : 'Idle'}, last active </span>
      <RelTime at={at} />
    </span>
  )
}

function AgentRow({ a }: { a: LiveAgent }) {
  const contested = new Set(a.holdings.filter((h) => h.contested).map((h) => h.spec))
  const specs = [...a.holdings].sort((x, y) => Number(y.contested) - Number(x.contested) || x.spec.localeCompare(y.spec)).map((h) => h.spec)
  return (
    <li className={`agent-row${contested.size > 0 ? ' has-contest' : ''}`}>
      <div className="agent-main">
        {contested.size > 0 && <span className="sr-only">Holds a contested file. </span>}
        <div className="agent-head">
          <span className="agent-name">{a.name}</span>
          {a.via.map((v) => (
            <ViaTag key={v} via={v} />
          ))}
        </div>
        <p className={`agent-task${a.task ? '' : ' is-muted'}`}>{a.task ?? 'No task given'}</p>
        {specs.length > 0 && <Chips items={specs} tone={(s) => (contested.has(s) ? 'amber' : undefined)} />}
      </div>
      <div className="agent-meta">
        {a.lastActive > 0 && <LastActive at={a.lastActive} />}
        {a.machine && (
          <span className="meta-item" title={a.machine}>
            <LaptopOutlined /> <span className="clamp1">{a.machine}</span>
          </span>
        )}
        {a.branch && (
          <span className="meta-item" title={a.branch}>
            <BranchesOutlined /> <span className="clamp1">{a.branch}</span>
          </span>
        )}
      </div>
    </li>
  )
}

export function Onboarding({ index }: { index: number }) {
  return (
    <Section title="Live agents" count={0} index={index} className="area-agents">
      <div className="onboard">
        <h3 className="onboard-title">No agents are connected yet</h3>
        <ol className="steps">
          <li>
            <span className="step-n">1</span>
            <div>
              <p>Choose “Connect an agent” above and copy the setup instructions.</p>
            </div>
          </li>
          <li>
            <span className="step-n">2</span>
            <div>
              <p>Paste setup instructions into Claude Code, Codex, Cursor, Copilot or Gemini CLI in your repository.</p>
            </div>
          </li>
          <li>
            <span className="step-n">3</span>
            <div>
              <p>Your agent guides sign-in and approvals, then verifies MCP and its native edit hook. Live activity alone does not mean ready.</p>
            </div>
          </li>
        </ol>
        <a className="doc-link" href={DOCS_AGENTS} target="_blank" rel="noreferrer noopener">
          Read docs/agents.md <ExportOutlined />
        </a>
      </div>
    </Section>
  )
}

function capGroups(all: OwnerGroup[], max: number) {
  let left = max
  const groups = []
  for (const g of all) {
    const shown = g.agents.slice(0, left)
    left -= shown.length
    if (shown.length > 0) groups.push({ ...g, shown })
  }
  return { groups, hidden: all.reduce((n, g) => n + g.agents.length, 0) - (max - left) }
}

export function LiveAgents({ d, truncated, index }: { d: Derived | null; truncated: boolean; index: number }) {
  if (!d) {
    return (
      <Section title="Live agents" index={index} className="area-agents">
        <Skeleton active avatar={false} paragraph={{ rows: 4 }} />
      </Section>
    )
  }
  if (d.agents.length === 0) return <Onboarding index={index} />
  const { groups, hidden } = capGroups(d.groups, MAX_AGENTS)
  return (
    <Section title="Live agents" count={d.agents.length} index={index} className="area-agents">
      {groups.map((g) => (
        <div className="group" key={g.owner}>
          <div className="group-head">
            <Av name={g.owner} size={28} />
            <span className="group-owner clamp1">{g.owner}</span>
            <span className="group-count">{g.agents.length === 1 ? '1 agent' : `${g.agents.length} agents`}</span>
          </div>
          <ul className="agent-list">
            {g.shown.map((a) => (
              <AgentRow key={a.id} a={a} />
            ))}
          </ul>
        </div>
      ))}
      {hidden > 0 && <p className="more-note">+{hidden} more agents</p>}
      {truncated && <p className="more-note">The hub truncated its list of announced work, so some held files may be missing.</p>}
    </Section>
  )
}

