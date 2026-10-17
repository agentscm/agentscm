import { InfoCircleOutlined } from '@ant-design/icons'
import { Skeleton, Tooltip } from 'antd'
import type { Health } from '../types.ts'
import { Section } from './bits.tsx'

const BUDGET_MS = 250
const TICKS = 40

const STORAGE: Record<string, string> = { sqlite: 'SQLite', postgres: 'PostgreSQL' }

const COUNTERS = [
  ['denied', 'Refused first edits', 'Edits refused because another agent holds the file. A session is refused once per file; repeating the same edit is let through.'],
  ['repeated', 'Repeated after refusal', 'Refused edits an agent repeated, which the hub then let through.'],
  ['fail_open', 'Let through when hub was unsure', 'Hook calls where the hub could not decide (an error or timeout). A failure never refuses, so the edit went ahead.'],
  ['rate_limited', 'Rate-limited refusals', 'Edits that passed with only a note, because a session had already been refused 3 times in 10 minutes.'],
] as const

function Meter({ ms }: { ms: number }) {
  const filled = Math.max(ms > 0 ? 1 : 0, Math.round(Math.min(ms / BUDGET_MS, 1) * TICKS))
  return (
    <div className="meter" role="meter" aria-label="Hook p95 latency" aria-valuemin={0} aria-valuemax={BUDGET_MS} aria-valuenow={Math.min(ms, BUDGET_MS)} aria-valuetext={ms > 0 ? `${ms} milliseconds of a ${BUDGET_MS} millisecond budget` : 'No hook calls measured yet'}>
      {Array.from({ length: TICKS }, (_, i) => {
        const zone = i < TICKS * 0.6 ? 'ok' : i < TICKS * 0.85 ? 'warn' : 'bad'
        return <span key={i} className={`tick ${i < filled ? `on tick-${zone}` : ''}`} />
      })}
    </div>
  )
}

export function HealthCard({ health, index }: { health: Health | null; index: number }) {
  if (!health) {
    return (
      <Section title="Hub health" index={index} className="area-health">
        <Skeleton active title={false} paragraph={{ rows: 3 }} />
      </Section>
    )
  }
  const ok = health.status === 'ok'
  const c = health.coordination
  return (
    <Section title="Hub health" index={index} className="area-health">
      <div className="health-top">
        <span className={`status-pill ${ok ? 'is-ok' : 'is-bad'}`}>
          <span className="dot" aria-hidden="true" />
          {ok ? 'Healthy' : 'Unavailable'}
        </span>
        {ok && health.storage && (
          <span className="storage">
            <span className="muted">Storage</span> <strong>{STORAGE[health.storage] ?? health.storage}</strong>
          </span>
        )}
      </div>
      {ok && c && (
        <>
          <dl className="counters">
            {COUNTERS.map(([k, label, tip]) => (
              <div className="counter" key={k}>
                <dt>
                  <Tooltip title={tip}>
                    <span tabIndex={0} className="counter-label">
                      {label} <InfoCircleOutlined aria-hidden="true" />
                      <span className="sr-only">. {tip}</span>
                    </span>
                  </Tooltip>
                </dt>
                <dd className={c[k] === 0 ? 'is-zero' : undefined}>{c[k].toLocaleString()}</dd>
              </div>
            ))}
          </dl>
          <div className="latency">
            <div className="latency-head">
              <Tooltip title="95% of the last 1024 hook calls were answered within this time. The meter fills toward a 250 ms budget.">
                <span tabIndex={0} className="counter-label">
                  Hook p95 latency <InfoCircleOutlined aria-hidden="true" />
                </span>
              </Tooltip>
              <span className="latency-val">
                {c.hook_p95_ms > 0 ? (
                  <>
                    {Math.round(c.hook_p95_ms) || '<1'} <small>ms</small>
                  </>
                ) : (
                  <span className="muted" title="No hook calls measured yet">
                    —
                  </span>
                )}
              </span>
            </div>
            <Meter ms={c.hook_p95_ms} />
          </div>
        </>
      )}
      {!ok && <p className="quiet">The hub reports it is unavailable. This page keeps trying.</p>}
      {ok && <p className="foot">Since the hub started</p>}
    </Section>
  )
}
