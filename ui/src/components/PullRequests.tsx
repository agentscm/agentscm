import { ArrowRightOutlined, ExportOutlined, PullRequestOutlined, UserOutlined } from '@ant-design/icons'
import { Skeleton, Tag } from 'antd'
import type { Derived, PrOverlap } from '../derive.ts'
import { Chips, ExternalLink, Section, safeHttps } from './bits.tsx'

const MAX = 5
const DOCS_HUB = 'https://github.com/counterbranch/agentscm/blob/main/docs/hub.md'

function PrRow({ o }: { o: PrOverlap }) {
  const { pr } = o
  const url = safeHttps(pr.html_url)
  const title = <span className="pr-title clamp1">{pr.title || '(untitled)'}</span>
  return (
    <li className="pr-row">
      <div className="pr-head">
        <span className="pr-num">#{pr.number}</span>
        {url ? (
          <ExternalLink href={url} className="pr-link">
            {title} <ExportOutlined className="pr-ext" aria-label="opens in a new tab" />
          </ExternalLink>
        ) : (
          title
        )}
        {pr.draft && (
          <Tag bordered={false} className="pill">
            Draft
          </Tag>
        )}
      </div>
      <div className="pr-sub">
        <span className="meta-item">
          <UserOutlined /> <span className="clamp1">{pr.author.login}</span>
        </span>
        <span className="meta-item branches">
          <code className="clamp1">{pr.head_ref}</code> <ArrowRightOutlined /> <code className="clamp1">{pr.base_ref}</code>
        </span>
      </div>
      <div className="pr-overlap">
        <span className="pr-overlap-label">Overlaps live work:</span>
        {o.with.slice(0, 3).map((w) => (
          <div className="pr-with" key={w.agent.id}>
            <strong>
              {w.agent.owner}'s {w.agent.name}
            </strong>
            <Chips items={w.files} max={3} tone={() => 'amber'} />
          </div>
        ))}
        {o.with.length > 3 && <span className="more-note">+{o.with.length - 3} more agents</span>}
        {pr.files_truncated && <span className="more-note">The PR file list was truncated, so more files may overlap.</span>}
      </div>
    </li>
  )
}

export function PullRequests({ d, connected, demo, index }: { d: Derived | null; connected: boolean; demo: boolean; index: number }) {
  if (!d) {
    return (
      <Section title="Open PR overlaps" index={index} className="area-prs">
        <Skeleton active title={false} paragraph={{ rows: 3 }} />
      </Section>
    )
  }
  if (!connected) {
    return (
      <Section title="Open PR overlaps" index={index} className="area-prs">
        <div className="calm-empty">
          <span className="calm-icon">
            <PullRequestOutlined />
          </span>
          <p>{demo ? 'The demo has no GitHub or GitLab connection, so there are no pull requests to compare.' : 'This repository has no GitHub or GitLab connection, so pull requests are not tracked.'}</p>
          <ExternalLink href={DOCS_HUB} className="doc-link">
            {demo ? 'How to connect a repository' : 'See docs/hub.md'} <ExportOutlined />
          </ExternalLink>
        </div>
      </Section>
    )
  }
  const shown = d.prOverlaps.slice(0, MAX)
  return (
    <Section title="Open PR overlaps" count={d.prOverlaps.length} index={index} className="area-prs">
      {shown.length === 0 ? (
        <div className="calm-empty">
          <span className="calm-icon tone-green">
            <PullRequestOutlined />
          </span>
          <p>{d.otherOpenPrs === 0 ? 'No open pull requests.' : `${d.otherOpenPrs} open ${d.otherOpenPrs === 1 ? 'PR does' : 'PRs do'} not touch files agents are working on.`}</p>
        </div>
      ) : (
        <ul className="pr-list">
          {shown.map((o) => (
            <PrRow key={o.pr.number} o={o} />
          ))}
        </ul>
      )}
      {d.prOverlaps.length > MAX && <p className="more-note">+{d.prOverlaps.length - MAX} more overlapping</p>}
      {d.otherOpenPrs > 0 && shown.length > 0 && (
        <p className="more-note">
          {d.otherOpenPrs} other open {d.otherOpenPrs === 1 ? 'PR has' : 'PRs have'} no overlap.
        </p>
      )}
    </Section>
  )
}
