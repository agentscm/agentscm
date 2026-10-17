import { BellOutlined, CheckCircleOutlined, ClockCircleOutlined, FlagOutlined, ForkOutlined, LockOutlined, LoginOutlined, PoweroffOutlined, RollbackOutlined, StopOutlined, SwapOutlined, UnlockOutlined, ApiOutlined } from '@ant-design/icons'
import { Button, Skeleton } from 'antd'
import { useState, type ReactNode } from 'react'
import { overlaps, pathsOf, type Derived } from '../derive.ts'
import type { HubEvent } from '../types.ts'
import { Chip, Experimental, RelTime, Section, viaLabel } from './bits.tsx'

const MAX_EVENTS = 30
const INITIAL = 12

const strs = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [])
const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {})

function Paths({ paths }: { paths: string[] }) {
  if (paths.length === 0) return <>files</>
  return (
    <>
      {paths.slice(0, 2).map((p, i) => (
        <span key={p}>
          {i > 0 && ', '}
          <Chip>{p}</Chip>
        </span>
      ))}
      {paths.length > 2 && ` +${paths.length - 2} more`}
    </>
  )
}

interface Row {
  icon: ReactNode
  tone: string
  text: ReactNode
  /** Muted second line (clamped to one line). */
  sub?: ReactNode
  experimental?: boolean
}

function describe(e: HubEvent, d: Derived): Row | null {
  const data = e.data
  switch (e.kind) {
    case 'session_started':
    case 'session_connected': {
      const v = strs(data['via'])
      const label = v.length ? ` via ${v.map(viaLabel).join(', ')}` : ''
      return e.kind === 'session_started'
        ? { icon: <LoginOutlined />, tone: 'green', text: <>started a session{label}</> }
        : { icon: <ApiOutlined />, tone: 'green', text: <>connected{label}</> }
    }
    case 'session_ended':
      return { icon: <PoweroffOutlined />, tone: 'grey', text: <>ended a session</> }
    case 'edit_denied': {
      const holders = [...new Set(strs(data['holders']).map((id) => {
        const i = d.intentById.get(id)
        return i ? `${i.owner.login}'s ${i.agent.name}` : 'another agent'
      }))]
      return {
        icon: <StopOutlined />,
        tone: 'rose',
        text: (
          <>
            was refused an edit to <Paths paths={pathsOf(e)} /> (held by {holders.length ? holders.join(', ') : 'another agent'})
          </>
        ),
      }
    }
    case 'handoff_request': {
      const to = rec(data['to'])
      const note = typeof data['note'] === 'string' && data['note'] ? data['note'] : null
      return {
        icon: <SwapOutlined />,
        tone: 'primary',
        experimental: true,
        text: (
          <>
            asked {String(to['login'])}'s {String(to['agent'])} for <Paths paths={pathsOf(e)} />
          </>
        ),
        sub: note && <>“{note}”</>,
      }
    }
    case 'handoff_grant':
    case 'handoff_decline': {
      const from = rec(data['from'])
      const granted = e.kind === 'handoff_grant'
      const reason = typeof data['reason'] === 'string' && data['reason'] ? data['reason'] : null
      return {
        icon: granted ? <CheckCircleOutlined /> : <LockOutlined />,
        tone: granted ? 'green' : 'amber',
        experimental: true,
        text: granted ? (
          <>
            handed <Paths paths={pathsOf(e)} /> to {String(from['login'])}'s {String(from['agent'])}
          </>
        ) : (
          <>
            kept <Paths paths={pathsOf(e)} />
          </>
        ),
        sub: !granted && reason && <>“{reason}”</>,
      }
    }
    case 'edit_repeated':
      return { icon: <RollbackOutlined />, tone: 'violet', text: <>repeated <Paths paths={pathsOf(e)} /> and was let through</> }
    case 'hold_expired':
      return { icon: <ClockCircleOutlined />, tone: 'grey', text: <>hold expired</> }
    case 'intent_announced': {
      const task = typeof data['task'] === 'string' && data['task'] ? data['task'] : null
      const paths = pathsOf(e)
      return {
        icon: <FlagOutlined />,
        tone: 'primary',
        text: (
          <>
            announced work{paths.length > 0 && <> on <Paths paths={paths} /></>}
            {task && <span className="ev-sub clamp1">{task}</span>}
          </>
        ),
      }
    }
    case 'intent_released':
      return { icon: <UnlockOutlined />, tone: 'grey', text: <>released its files</> }
    case 'intent_notice': {
      const other = rec(data['owner'])['login']
      const agent = typeof data['agent'] === 'string' ? data['agent'] : rec(data['agent'])['name']
      const theirs = strs(data['their_paths'])
      const yours = strs(data['your_paths'])
      const shared = theirs.filter((t) => yours.some((y) => overlaps(t, y)))
      const who = typeof other === 'string' && typeof agent === 'string' ? `${other}'s ${agent}` : 'another agent'
      return {
        icon: <BellOutlined />,
        tone: 'amber',
        text: (
          <>
            was told {who} started on <Paths paths={shared.length ? shared : theirs} />
          </>
        ),
      }
    }
    case 'pr_notice': {
      const n = typeof data['number'] === 'number' ? data['number'] : null
      const title = typeof data['title'] === 'string' ? data['title'] : null
      return {
        icon: <ForkOutlined />,
        tone: 'amber',
        text: (
          <>
            was told {n !== null ? `PR #${n}` : 'a pull request'} touches its files
            {title && <span className="ev-sub clamp1">{title}</span>}
          </>
        ),
      }
    }
    default:
      return null
  }
}

export function ActivityCard({ d, events, fresh, index }: { d: Derived | null; events: HubEvent[] | null; fresh: ReadonlySet<string>; index: number }) {
  const [expanded, setExpanded] = useState(false)
  if (!d || !events) {
    return (
      <Section title="Activity" index={index} className="area-activity">
        <Skeleton active title={false} paragraph={{ rows: 5 }} />
      </Section>
    )
  }
  const rows = events
    .map((e) => ({ e, row: describe(e, d) }))
    .filter((x): x is { e: HubEvent; row: Row } => x.row !== null)
    .slice(0, MAX_EVENTS)
  return (
    <Section title="Activity" count={rows.length} index={index} className="area-activity">
      {rows.length === 0 ? (
        <p className="quiet">Nothing has happened yet. Events show up here as agents start sessions and hit overlaps.</p>
      ) : (
        <ol className="timeline">
          {rows.slice(0, expanded ? MAX_EVENTS : INITIAL).map(({ e, row }) => (
            <li key={e.id} className={`ev${fresh.has(e.id) ? ' is-new' : ''}`}>
              <span className={`tint-icon tone-${row.tone}`}>{row.icon}</span>
              <div className="ev-body">
                <p className="ev-text">
                  <strong>
                    {e.owner.login} · {e.agent.name}
                  </strong>{' '}
                  {row.text}
                  {row.experimental && <Experimental />}
                </p>
                {row.sub && <p className="ev-sub clamp1">{row.sub}</p>}
              </div>
              <RelTime at={e.at} />
            </li>
          ))}
        </ol>
      )}
      {rows.length > INITIAL && (
        <Button type="text" size="small" className="more-btn" onClick={() => setExpanded((v) => !v)} aria-expanded={expanded}>
          {expanded ? 'Show fewer' : `Show ${rows.length - INITIAL} older`}
        </Button>
      )}
    </Section>
  )
}
