import type { Health } from '../types.ts'
import { SectionTabs } from '../TopNav.tsx'
import { Box, Explain, Icon, Notice, Pill, Skeleton } from '../ui.tsx'

const BUDGET_MS = 250
const STORAGE: Record<string, string> = { sqlite: 'SQLite', postgres: 'PostgreSQL' }

const COUNTERS = [
  ['denied', 'slash-circle', 'Refused first edits', 'Edits refused because another agent holds the file. A session is refused once per file; repeating the same edit is let through.'],
  ['repeated', 'arrow-repeat', 'Repeated after refusal', 'Refused edits an agent repeated, which the hub then let through.'],
  ['fail_open', 'door-open', 'Let through when unsure', 'Hook calls where the hub could not decide (an error or timeout). A failure never refuses, so the edit went ahead.'],
  ['rate_limited', 'speedometer', 'Rate-limited refusals', 'Edits that passed with only a note, because a session had already been refused 3 times in 10 minutes.'],
] as const

export function HealthPage({ health }: { health: Health | null }) {
  const ok = health?.status === 'ok'
  const c = health?.coordination
  const p95 = c?.hook_p95_ms ?? 0
  const pct = Math.min(p95 / BUDGET_MS, 1) * 100
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Is the hub OK?</h1>
        <p className="answer-text">{!health ? 'Checking…' : ok ? <>Yes. The hub is answering{health.storage ? <> and storing data in {STORAGE[health.storage] ?? health.storage}</> : null}.</> : 'No. The hub reports it is unavailable. This page keeps trying.'}</p>
      </div>
      <SectionTabs />
      {!health ? (
        <Skeleton rows={4} />
      ) : !ok ? (
        <Notice tone="bad" icon="heartbreak" title="Hub unavailable">The database did not answer the last health check. Agents fall back to letting edits through until it recovers.</Notice>
      ) : (
        <div className="stack">
          <Box icon="heart-pulse" tone="ok" title="Status" explain="Polled every 15 seconds from /v1/hub/health.">
            <div className="pills">
              <Pill tone="ok" icon="check-circle">Healthy</Pill>
              {health.storage && <Pill tone="muted" icon="database">{STORAGE[health.storage] ?? health.storage}</Pill>}
              {health.demo && <Pill tone="brand" icon="play-circle">Demo hub</Pill>}
            </div>
          </Box>
          {c && (
            <>
              <Box icon="shield-check" title="Coordination since the hub started" explain="How often the edit hook stepped in. Counters reset when the hub restarts.">
                <dl className="counters" data-tour="health-counters">
                  {COUNTERS.map(([k, icon, label, tip]) => (
                    <div key={k} className={`counter${c[k] === 0 ? ' is-zero' : ''}`}>
                      <dt>
                        <Icon name={icon} /> {label} <Explain text={tip} />
                      </dt>
                      <dd>{c[k].toLocaleString()}</dd>
                    </div>
                  ))}
                </dl>
              </Box>
              <Box tour="health-speed" icon="stopwatch" title="Hook speed" explain="95% of the last 1024 hook calls were answered within this time. The bar fills toward a 250 ms budget.">
                <div className="latency">
                  <span className="latency-value">{p95 > 0 ? <>{Math.round(p95) || '<1'} <small>ms</small></> : <span className="muted">No hook calls measured yet</span>}</span>
                  <span className="muted small">p95 of {BUDGET_MS} ms budget</span>
                </div>
                <div className="progress" role="meter" aria-label="Hook p95 latency" aria-valuemin={0} aria-valuemax={BUDGET_MS} aria-valuenow={Math.min(p95, BUDGET_MS)}>
                  <div className={`progress-bar ${pct < 60 ? 'bg-success' : pct < 85 ? 'bg-warning' : 'bg-danger'}`} style={{ width: `${Math.max(pct, p95 > 0 ? 1 : 0)}%` }} />
                </div>
              </Box>
            </>
          )}
        </div>
      )}
    </div>
  )
}
