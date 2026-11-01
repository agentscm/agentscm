import { useEffect, useId, useRef, useState, type ReactNode } from 'react'
import type { BrowserSession, RepositoryRow } from './devices.ts'
import { Ago, Icon, Notice, Pill, Skeleton } from './ui.tsx'
import { fetchValueMetrics, type ValueEvidence, type ValueMetrics } from './valueMetrics.ts'
import './valueMetrics.css'

const number = (v: number | null) => v === null ? 'Unknown' : v.toLocaleString()
// Preserve exact large integers. Formatting does not calculate usage or combine accounting bases.
const quantity = (v: string | null) => v === null ? 'Unknown' : v.replace(/\B(?=(\d{3})+(?!\d))/g, ',')
const words = (v: string) => v.replaceAll('_', ' ')
const date = (v: number) => new Date(v * 1000).toLocaleString()

type Result = { key: string; kind: 'ready'; value: ValueMetrics } | { key: string; kind: 'error'; status?: number } | { key: string; kind: 'loading' }

/** Reads only a server projection; no historical fan-out or local value/savings calculator. */
export function ValueMetricsSection({ session, repositories, onOpen }: { session: BrowserSession; repositories: RepositoryRow[]; onOpen?: (repository: string) => void }) {
  const periodId = useId()
  const [days, setDays] = useState(7)
  const [selected, setSelected] = useState('')
  const [until, setUntil] = useState(() => Math.floor(Date.now() / 1000))
  const [page, setPage] = useState<{ key: string; after: string } | null>(null)
  const [retry, setRetry] = useState(0)
  const [paging, setPaging] = useState(false)
  const windowRef = useRef<{ key: string; since: number; until: number } | null>(null)
  const repository = repositories.some(r => r.repository === selected) ? selected : ''
  // Include session identity and readable scope. Results from an old session/scope never render.
  const access = `${session.provider}:${session.login}:${session.csrf}:${repositories.map(r => r.repository).join(',')}`
  const key = `${access}:${days}:${until}:${repository}:${retry}`
  const cursor = page?.key === key ? page.after : undefined
  const setCursor = (after: string | undefined) => setPage(after ? { key, after } : null)
  const [result, setResult] = useState<Result>({ key, kind: 'loading' })
  const shown = result.key === key ? result : { key, kind: 'loading' } as const
  useEffect(() => {
    const controller = new AbortController()
    let disposed = false
    const bounds = cursor && windowRef.current?.key === key ? windowRef.current : { since: until - days * 86400, until }
    void fetchValueMetrics({ since: bounds.since, until: bounds.until, ...(repository ? { repository } : {}), ...(cursor ? { after: cursor } : {}) }, controller.signal)
      .then(value => {
        if (disposed) return
        windowRef.current = { key, since: value.since, until: value.until }
        setResult(previous => ({ key, kind: 'ready', value: cursor && previous.key === key && previous.kind === 'ready' ? { ...value, evidence: [...previous.value.evidence, ...value.evidence] } : value }))
      })
      .catch((error: unknown) => {
        if (disposed || controller.signal.aborted) return
        // Access can be revoked between any two reads. Do not retain old measurements on error.
        setResult({ key, kind: 'error', status: error && typeof error === 'object' && 'status' in error && typeof error.status === 'number' ? error.status : undefined })
      })
      .finally(() => { if (!disposed) setPaging(false) })
    return () => { disposed = true; controller.abort() }
  }, [key, days, until, repository, cursor, retry])
  useEffect(() => {
    const refresh = () => {
      if (!document.hidden) { setPage(null); setUntil(Math.floor(Date.now() / 1000)); setRetry(n => n + 1) }
    }
    const timer = window.setInterval(refresh, 120_000)
    document.addEventListener('visibilitychange', refresh)
    return () => { window.clearInterval(timer); document.removeEventListener('visibilitychange', refresh) }
  }, [])
  const refresh = () => { setCursor(undefined); setUntil(Math.floor(Date.now() / 1000)); setRetry(n => n + 1) }
  const changeDays = (value: string) => { setCursor(undefined); setDays(Number(value)); setUntil(Math.floor(Date.now() / 1000)) }
  const changeScope = (value: string) => { setCursor(undefined); setSelected(value) }
  const v = shown.kind === 'ready' ? shown.value : null
  return (
    <section id="value-metrics" className="value-metrics" aria-labelledby="value-metrics-title">
      <header className="value-heading"><Icon name="journal-check" /><div><h2 id="value-metrics-title">What AgentSCM recorded</h2><p>Observed by the Hub across the repositories you can read. Unknown stays unknown.</p></div></header>
      <fieldset className="value-period"><legend className="visually-hidden">Period</legend>{[7, 30, 90].map(period => <label key={period}><input className="visually-hidden" type="radio" name={periodId} value={period} checked={days === period} onChange={e => changeDays(e.target.value)} /><span>Last {period} days</span></label>)}</fieldset>
      <div className="value-filters">
        <label>Repository scope<select className="form-select form-select-sm" value={repository} onChange={e => changeScope(e.target.value)}><option value="">All repositories I can read</option>{repositories.map(r => <option key={r.repository} value={r.repository}>{r.repository}</option>)}</select></label>
        <button type="button" className="btn btn-sm btn-outline-secondary" onClick={refresh}>Refresh contributions</button>
      </div>
      {shown.kind === 'loading' ? <Skeleton rows={5} /> : shown.kind === 'error' ? (
        <Notice tone="warn" title="Contributions couldn’t be loaded" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={refresh}>Try contributions again</button>}>
          {shown.status === 401 || shown.status === 403 ? 'Your access could not be confirmed. Sign in again or refresh your repository list.' : 'The Hub did not return a usable projection. Recorded contributions are unknown; try again.'}
        </Notice>
      ) : v && <>
        <p className="value-assessment muted small">{date(v.since)} – {date(v.until)} · Updated <Ago at={v.as_of} /></p>
        <div className="value-cards">
          <ValueCard id="value-issues" icon="shield-exclamation" title="Issues surfaced and resolved" availability={v.issues.availability}>
            <Facts entries={[[v.issues.unresolved, 'Unresolved'], [v.issues.resolved, 'Resolved with evidence'], [v.issues.dismissed, 'Dismissed', 'Not captured']]} />
            <p className="value-card-note">A check or refusal signal. It does not prove a bug was prevented.</p>
            <details className="value-card-extra"><summary>More issue outcomes</summary><Facts entries={[[v.issues.false_trigger, 'False triggers', 'Not captured']]} /></details>
          </ValueCard>
          <ValueCard id="value-followups" icon="arrow-repeat" title="Follow-ups" availability={v.followups.availability}>
            <Facts entries={[[v.followups.current, 'Current'], [v.followups.pending, 'Pending'], [v.followups.failed, 'Failed', 'Not captured']]} />
            <p className="value-card-note">Current evidence applies to its own source, recipe and policy.</p>
            <details className="value-card-extra"><summary>Other follow-up states</summary><Facts entries={[[v.followups.stale, 'Stale'], [v.followups.cancelled, 'Cancelled', 'Not captured'], [v.followups.unknown, 'Unknown']]} /></details>
          </ValueCard>
          <ValueCard id="value-context" icon="people" title="Team context" availability={v.context.availability}>
            <Facts entries={[[v.context.returned, 'Prepared for return'], [v.context.acknowledged, 'Acknowledged'], [v.context.applied, 'Applied']]} />
            <p className="value-card-note">A prepared response does not prove transport receipt or application. Opening this page does not acknowledge anything.</p>
          </ValueCard>
          <ValueCard id="value-resources" icon="database" title="Recorded resources" aside="Bases kept apart">
            <Facts entries={[[v.resources.input_tokens, 'Input tokens'], [v.resources.output_tokens, 'Output tokens']]} />
            {v.resources.costs.length === 0 ? <p className="value-card-note">No cost amounts recorded. Cash cost is unknown.</p> : <ul className="value-costs">{v.resources.costs.map((cost, i) => <li key={i}><b>{cost.provider} · {cost.model}</b><span>{quantity(cost.amount_micros)} {cost.currency} micros · {words(cost.basis)} · {words(cost.kind)} · {number(cost.requests)} requests</span></li>)}</ul>}
            <p className="value-card-note">{number(v.resources.missing_cost_requests)} additive requests have no cost recorded. Unallocated usage records: {number(v.resources.unallocated_usage_records)}. AgentSCM overhead: {v.resources.unallocated_overhead ?? 'Unknown / unallocated'}.</p>
            {v.resources.groups.length > 0 && <details className="value-card-extra"><summary>Usage by provider and category</summary><ul className="value-costs">{v.resources.groups.map((g, i) => <li key={i}><b>{g.provider} · {g.model} · {words(g.category)}</b><span>{words(g.usage_basis)} · {quantity(g.input_tokens)} input · {quantity(g.output_tokens)} output tokens · {number(g.requests)} requests</span><span>Cache read: {quantity(g.cache_read_input_tokens)} · cache creation: {quantity(g.cache_creation_input_tokens)} input tokens</span></li>)}</ul></details>}
          </ValueCard>
        </div>
        <div className="value-coverage" role="note" aria-label="Contribution coverage">
          <div className="value-coverage-line"><Icon name="eye" /><h3>Coverage: {v.coverage.state === 'partial' ? 'partial' : 'unknown'}</h3><p>{number(v.coverage.observed_works)} observed Works · {number(v.coverage.eligible_works)} eligible Works · {number(v.coverage.omitted)} omitted records</p></div>
          <p>Zero observations do not establish zero problems. Work coverage does not establish token-volume coverage.</p>
          <details><summary>Coverage limits</summary>{v.coverage.gaps.length > 0 && <ul>{v.coverage.gaps.map((gap, i) => <li key={i}>{words(gap)}</li>)}</ul>}{!v.coverage.complete && <p>This projection is incomplete. Inspect the evidence and coverage gaps before drawing conclusions.</p>}<p>Recorded amounts do not establish total cost or savings. Accounting bases and currencies stay separate.</p></details>
        </div>
        <details className="value-evidence">
          <summary>Inspect recorded evidence</summary>
          {v.evidence.length === 0 ? <p className="muted">No evidence returned for this scope and period. Contributions remain unobserved.</p> : <ul>{v.evidence.map((e, i) => <Evidence key={`${e.id}-${i}`} evidence={e} onOpen={onOpen} />)}</ul>}
          {v.next_after && <button type="button" className="btn btn-sm btn-outline-secondary" disabled={paging} onClick={() => { setPaging(true); setCursor(v.next_after ?? undefined) }}>{paging ? 'Loading evidence…' : 'Load more evidence'}</button>}
          <p className="muted small">Assessment {v.assessment_id} · {v.method} · {number(v.merge_observations)} provider-observed merges. A merge observation does not establish deployment.</p>
        </details>
      </>}
    </section>
  )
}

function ValueCard({ id, icon, title, availability, aside, children }: { id: string; icon: string; title: string; availability?: string; aside?: string; children: ReactNode }) {
  return <section className="value-card" aria-labelledby={`${id}-title`}><header><Icon name={icon} /><h3 id={`${id}-title`}>{title}</h3>{(availability || aside) && <span>{aside ?? `${words(availability!)} coverage`}</span>}</header>{children}</section>
}
function Facts({ entries }: { entries: [number | string | null, string, string?][] }) {
  return <dl className="value-facts">{entries.map(([value, label, nullLabel]) => <div key={label}><dt>{label}</dt><dd>{value === null ? nullLabel ?? 'Unknown' : typeof value === 'string' ? quantity(value) : number(value)}</dd></div>)}</dl>
}
function Evidence({ evidence: e, onOpen }: { evidence: ValueEvidence; onOpen?: (repository: string) => void }) {
  return <li><details><summary><span>{e.detail || words(e.kind) || 'Recorded observation'}</span><Pill tone={['failed', 'unresolved', 'stale'].includes(e.state) ? 'warn' : 'muted'}>{words(e.state) || 'Unknown'}</Pill></summary><dl className="value-evidence-facts">
    <div><dt>Evidence</dt><dd>{e.id || 'Unknown'}</dd></div><div><dt>Kind</dt><dd>{words(e.kind) || 'Unknown'}</dd></div><div><dt>Observed</dt><dd>{e.observed_at === null ? 'Unknown' : date(e.observed_at)}</dd></div>
    <div><dt>Repository</dt><dd>{e.repository ?? 'Unknown'}</dd></div><div><dt>Work</dt><dd>{e.work_id ?? 'Unknown'}</dd></div><div><dt>Source</dt><dd>{e.source ?? 'Unknown'}</dd></div><div><dt>Recipe / run</dt><dd>{e.recipe ?? 'Unknown'}</dd></div><div><dt>Policy</dt><dd>{e.policy ?? 'Unknown'}</dd></div><div><dt>Basis</dt><dd>{words(e.basis)}</dd></div><div><dt>Assurance</dt><dd>{words(e.assurance)}</dd></div><div><dt>Availability</dt><dd>{words(e.availability)}</dd></div><div><dt>Applicability</dt><dd>{words(e.applicability)}</dd></div>
  </dl>{e.repository && onOpen && <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => onOpen(e.repository!)}>Open {e.repository}</button>}</details></li>
}
