import { CheckCircleOutlined, ClockCircleOutlined, LockOutlined, RollbackOutlined, StopOutlined, SwapOutlined } from '@ant-design/icons'
import { Skeleton, Tooltip } from 'antd'
import type { Contested as ContestedFile, Derived, Handoff } from '../derive.ts'
import { Av, Experimental, Section, clock } from './bits.tsx'

const MAX = 8

function HandoffAnswer({ h }: { h: Handoff }) {
  const a = h.answer
  if (a) {
    const granted = a.kind === 'granted'
    return (
      <li className={`story-step tone-${granted ? 'green' : 'amber'}`}>
        <span className="tint-icon">{granted ? <CheckCircleOutlined /> : <LockOutlined />}</span>
        <span className="story-text">
          <strong>
            {granted ? 'Handed over' : 'Kept'} · {clock(a.at)}
          </strong>
          {a.reason && <span className="story-sub clamp1">“{a.reason}”</span>}
        </span>
      </li>
    )
  }
  if (h.lapsed) {
    return (
      <li className="story-step tone-grey">
        <span className="tint-icon">
          <ClockCircleOutlined />
        </span>
        <span className="story-text">
          <strong>Lapsed</strong>
          <span className="story-sub">No answer within 30 minutes</span>
        </span>
      </li>
    )
  }
  return (
    <li className="story-step tone-primary">
      <span className="tint-icon">
        <span className="wait-dot" aria-hidden="true" />
      </span>
      <span className="story-text">
        <strong>Waiting for an answer</strong>
      </span>
    </li>
  )
}

function Story({ c }: { c: ContestedFile }) {
  const h = c.handoff
  if (!c.refused && !h) return null
  return (
    <ol className="story">
      {c.refused && (
        <li className="story-step tone-rose">
          <span className="tint-icon">
            <StopOutlined />
          </span>
          <span className="story-text">
            <strong>Refused · {clock(c.refused.at)}</strong>
            <span className="clamp1">
              {c.refused.owner}'s {c.refused.name}
            </span>
          </span>
        </li>
      )}
      {c.repeated && (
        <li className="story-step tone-violet">
          <span className="tint-icon">
            <RollbackOutlined />
          </span>
          <span className="story-text">
            <strong>Repeated, let through · {clock(c.repeated.at)}</strong>
            <span className="clamp1">
              {c.repeated.owner}'s {c.repeated.name}
            </span>
          </span>
        </li>
      )}
      {h && (
        <li className="story-step tone-primary">
          <span className="tint-icon">
            <SwapOutlined />
          </span>
          <span className="story-text">
            <span className="story-line">
              <strong>
                {h.from.owner}'s {h.from.name}
              </strong>{' '}
              asked{' '}
              <strong>
                {h.to.owner}'s {h.to.name}
              </strong>{' '}
              for it
              <span className="story-when"> · {clock(h.at)}</span>
              <Experimental />
            </span>
            {h.note && <span className="story-sub clamp1">“{h.note}”</span>}
          </span>
        </li>
      )}
      {h && <HandoffAnswer h={h} />}
    </ol>
  )
}

export function ContestedFiles({ d, index }: { d: Derived | null; index: number }) {
  if (!d) {
    return (
      <Section title="Contested files" index={index} className="area-contested">
        <Skeleton active title={false} paragraph={{ rows: 3 }} />
      </Section>
    )
  }
  const shown = d.contested.slice(0, MAX)
  return (
    <Section title="Contested files" count={d.contested.length} index={index} className="area-contested">
      <p className="card-sub">Files or folders more than one agent holds, and files with a recent refusal or handoff</p>
      {shown.length === 0 ? (
        <div className="calm-empty">
          <span className="calm-icon tone-green">
            <CheckCircleOutlined />
          </span>
          <p>
            {d.agents.length > 0 ? 'No overlaps. Agents are working on separate files.' : 'No overlaps. Nothing is held right now.'}
          </p>
        </div>
      ) : (
        <ul className="contested-list">
          {shown.map((c) => (
            <li key={c.path} className="contested-item">
              <code className="contested-path">{c.path}</code>
              <div className="holders">
                <span className="avs" role="group" aria-label="Agents holding this file">
                  {c.holders.slice(0, 4).map((h) => (
                    <Tooltip key={h.id} title={`${h.owner} · ${h.name}`}>
                      <span>
                        <Av name={h.owner} size={24} title={`${h.owner} · ${h.name}`} />
                      </span>
                    </Tooltip>
                  ))}
                  {c.holders.length > 4 && <span className="av-more">+{c.holders.length - 4}</span>}
                </span>
                <span className="holders-text">{c.holders.length ? c.holders.map((h) => `${h.owner}'s ${h.name}`).join(' · ') : 'No one holds it now'}</span>
              </div>
              <Story c={c} />
            </li>
          ))}
        </ul>
      )}
      {d.contested.length > MAX && <p className="more-note">+{d.contested.length - MAX} more</p>}
    </Section>
  )
}
