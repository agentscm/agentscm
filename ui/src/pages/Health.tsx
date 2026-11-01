import type { ReactNode } from 'react'
import type { Health } from '../types.ts'
import { SectionTabs } from '../TopNav.tsx'
import { Box, Explain, Icon, Notice, PageHead, Skeleton } from '../ui.tsx'

const BUDGET_MS = 250
const STORAGE: Record<string, string> = { sqlite: 'SQLite', postgres: 'PostgreSQL' }

const COUNTERS = [
  ['denied', 'slash-circle', 'Refused first edits', 'Edits refused because another agent holds the file. A session is warned once per file; repeating the same edit goes ahead.'],
  ['repeated', 'arrow-repeat', 'Went ahead after a warning', 'Edits an agent repeated after the one warning. They go ahead by design; the hub never locks files.'],
  ['fail_open', 'door-open', 'Hub could not decide; edit went ahead', 'Hook calls where the hub could not decide (an error or timeout). A failure never refuses, so the edit went ahead (by design).'],
  ['rate_limited', 'speedometer', 'Rate-limited refusals', 'Edits that passed with only a note, because a session had already been refused 3 times in 10 minutes.'],
] as const

const plural = (n: number, one: string) => `${n.toLocaleString()} ${n === 1 ? one : `${one}s`}`

/** The answer: is the Hub OK, and if not quite, what deserves a look. Refusals and warnings are the hook working, so they never count. */
export function healthAnswer(health: Health | null): ReactNode {
  if (!health) return 'Checking…'
  if (health.status !== 'ok') return 'The Hub is not answering, so edits go ahead unchecked.'
  const c = health.coordination
  const notes: ReactNode[] = []
  if (c && c.fail_open > 0) notes.push(<><b>{plural(c.fail_open, 'edit')}</b> went ahead because it could not decide</>)
  if (c && c.hook_p95_ms >= BUDGET_MS * 0.85) notes.push(<>its hook is slow, <b>{Math.round(c.hook_p95_ms)} ms</b> at p95</>)
  if (!notes.length) return 'The Hub is healthy. Nothing needs a look.'
  return <>The Hub is answering, but {notes[0]}{notes[1] ? <> and {notes[1]}</> : null}.</>
}

export function HealthPage({ health }: { health: Health | null }) {
  const ok = health?.status === 'ok'
  const c = health?.coordination
  const p95 = c?.hook_p95_ms ?? 0
  const pct = Math.min(p95 / BUDGET_MS, 1) * 100
  return (
    <div className="page">
      <PageHead
        question="Is the hub OK?"
        answer={healthAnswer(health)}
        fact={health ? `${health.demo ? 'Demo hub. ' : ''}${ok && health.storage ? `Storing data in ${STORAGE[health.storage] ?? health.storage}. ` : ''}Checked every 15 seconds.` : undefined}
      />
      <SectionTabs />
      {!health ? (
        <Skeleton rows={4} />
      ) : !ok ? (
        <Notice tone="bad" icon="heartbreak" title="Hub unavailable">The database did not answer the last health check. This page checks again every 15 seconds.</Notice>
      ) : (
        <div className="stack">
          {!c && <Notice tone="muted" icon="info-circle" title="No coordination counters reported yet">They appear once agents start editing through the hook.</Notice>}
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
              <Box tour="health-speed" icon="stopwatch" title="Hook speed" explain="95% of the last 1024 hook calls were answered this fast. The bar fills toward a 250 ms budget.">
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
