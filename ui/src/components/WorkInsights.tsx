import { BarChartOutlined, ReloadOutlined, WarningOutlined } from '@ant-design/icons'
import { Alert, Button, Descriptions, Drawer, Empty, Skeleton, Tag } from 'antd'
import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError } from '../api.ts'
import { fetchInsightPage, fetchWorkInsight, type CostGroup, type TokenGroup, type WorkInsight } from '../insights.ts'
import './work-insights.css'

type Load<T> = { kind: 'loading' } | { kind: 'ready'; value: T } | { kind: 'error'; message: string; missing?: boolean }

const number = new Intl.NumberFormat()
const duration = (seconds: number | null): string => {
  if (seconds === null) return 'Unknown'
  if (seconds < 60) return `${number.format(seconds)}s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${number.format(minutes)}m`
  const hours = Math.floor(minutes / 60)
  const remainder = minutes % 60
  return remainder === 0 ? `${number.format(hours)}h` : `${number.format(hours)}h ${remainder}m`
}
const localDate = (at: number): string => new Date(at * 1000).toLocaleString()
const known = (value: number | null): string => value === null ? 'Unknown' : number.format(value)
const cost = (group: CostGroup): string => `${(group.amount_micros / 1_000_000).toLocaleString(undefined, { maximumFractionDigits: 6 })} ${group.currency}`
const basisLabel = (basis: 'reported' | 'provider_result') => basis === 'provider_result' ? 'Provider result' : 'Reported'

function TokenEvidence({ groups }: { groups: TokenGroup[] }) {
  if (groups.length === 0) return <p className="work-insights-muted">No token usage was reported.</p>
  return (
    <div className="work-insights-table-wrap" tabIndex={0} aria-label="Reported token evidence">
      <table className="work-insights-table">
        <thead><tr><th>Agent / provider / model</th><th>Basis</th><th>Input</th><th>Output</th><th>Reports</th></tr></thead>
        <tbody>{groups.map((group, index) => (
          <tr key={`${group.agent}-${group.provider}-${group.model}-${group.usage_basis}-${index}`}>
            <td><strong>{group.agent}</strong><span>{group.provider} · {group.model}</span></td>
            <td>{basisLabel(group.usage_basis)}</td>
            <td>{known(group.input_tokens)} <small>({group.input_known_observations}/{group.observations} reports)</small></td>
            <td>{known(group.output_tokens)} <small>({group.output_known_observations}/{group.observations} reports)</small></td>
            <td>{number.format(group.observations)}</td>
          </tr>
        ))}</tbody>
      </table>
    </div>
  )
}

function CostEvidence({ groups }: { groups: CostGroup[] }) {
  if (groups.length === 0) return <p className="work-insights-muted">No cost was reported. Missing cost is unknown, not zero.</p>
  return <ul className="work-insights-costs">{groups.map((group, index) => (
    <li key={`${group.agent}-${group.provider}-${group.model}-${group.currency}-${group.basis}-${index}`}>
      <strong>{cost(group)}</strong>
      <span>{group.agent} · {group.provider} · {group.model}</span>
      <small>{group.kind === 'reported_charge' ? 'Reported charge' : 'List-price estimate'} · {group.basis} · {basisLabel(group.usage_basis)} · {number.format(group.observations)} report(s)</small>
    </li>
  ))}</ul>
}

function PublicationEvidence({ item }: { item: WorkInsight }) {
  if (item.publications.length === 0) return <p className="work-insights-muted">No stored publication association appears in this bounded result.</p>
  return <div className="work-insights-publications">{item.publications.map((publication) => (
    <article key={publication.publication_id}>
      <div><strong>{publication.branch_state}</strong><Tag bordered={false}>{publication.association}</Tag></div>
      <code>{publication.commit.slice(0, 12)}</code>
      <span>{publication.pr_number === null ? 'No pull request recorded' : `Pull request ${publication.pr_number}`}</span>
      <span>{publication.merge_commit === null ? 'No merge commit recorded' : `Merge ${publication.merge_commit.slice(0, 12)}`}</span>
      <small>Last stored update {localDate(publication.observed_at)} · {publication.checks.length} stored check(s) for this exact head. Superseded heads’ checks are not retained.</small>
    </article>
  ))}</div>
}

function InsightDetail({ item }: { item: WorkInsight }) {
  return (
    <div className="work-insights-detail">
      <Alert showIcon type="warning" message={item.usage.coverage === 'unknown' ? 'Usage coverage is unknown' : 'Usage coverage is partial'} description="Reports do not prove complete capture. Missing tokens or cost are unknown, not zero." />
      <section>
        <h3>Work report</h3>
        <Descriptions size="small" column={2} items={[
          { key: 'asof', label: 'Measured as of', children: localDate(item.as_of) },
          { key: 'state', label: 'Reported state', children: item.reported_finished ? 'Reported finished' : item.state },
          { key: 'claim', label: 'Claim status', children: item.claim_status },
          { key: 'elapsed', label: 'Elapsed', children: duration(item.timing.elapsed_seconds) },
          { key: 'active', label: 'Reported active', children: duration(item.timing.active_seconds) },
          { key: 'blocked', label: 'Reported blocked', children: duration(item.timing.blocked_seconds) },
          { key: 'paused', label: 'Reported paused', children: duration(item.timing.paused_seconds) },
          { key: 'handover', label: 'Reported handover', children: duration(item.timing.handover_seconds) },
          { key: 'finished', label: 'Reported finished time', children: duration(item.timing.finished_seconds) },
          { key: 'unknown', label: 'Unknown time', children: duration(item.timing.unknown_seconds) },
          { key: 'failed', label: 'Reported failed attempts', children: number.format(item.usage.reported_failed_attempts) },
          { key: 'signals', label: 'Coordination signals', children: `${known(item.signals.takeovers)} takeovers · ${known(item.signals.handovers)} handovers · ${known(item.signals.reopened)} reopened` },
        ]} />
        {(!item.history_complete || item.history_retention_gap) && <Alert showIcon type="warning" message="Timing history is incomplete" description="Durations include unknown time and should not be treated as a complete lifecycle measurement." />}
      </section>
      <section><h3>Token evidence</h3><TokenEvidence groups={item.usage.token_groups} /></section>
      <section><h3>Cost evidence</h3><CostEvidence groups={item.usage.cost_groups} /></section>
      {!item.usage.groups_complete && <Alert showIcon type="warning" message="Some usage groups are omitted" description="Totals and known-report counts include all retained reports, but this grouped breakdown is bounded." />}
      <section>
        <h3>Stored publication state</h3>
        <p className="work-insights-muted">These are claimed source associations with the last retained controller observations. They are separate from the agent's reported finish state and do not establish approval or qualification.</p>
        <PublicationEvidence item={item} />
        {(!item.publications_complete || item.commits_truncated) && <p className="work-insights-omission"><WarningOutlined /> Additional publication or commit evidence may be omitted.</p>}
      </section>
    </div>
  )
}

function InsightCard({ item, onOpen }: { item: WorkInsight; onOpen: () => void }) {
  return (
    <article className="work-insights-card">
      <header><div><span>{item.work_id}</span><h2>{item.title}</h2></div><Tag bordered={false}>{item.reported_finished ? 'Reported finished' : item.state}</Tag></header>
      <dl>
        <div><dt>Elapsed</dt><dd>{duration(item.timing.elapsed_seconds)}</dd></div>
        <div><dt>Blocked / paused</dt><dd>{duration(item.timing.blocked_seconds)} / {duration(item.timing.paused_seconds)}</dd></div>
        <div><dt>Handover / unknown</dt><dd>{duration(item.timing.handover_seconds)} / {duration(item.timing.unknown_seconds)}</dd></div>
        <div><dt>Reported failed attempts</dt><dd>{number.format(item.usage.reported_failed_attempts)}</dd></div>
        <div><dt>Usage reports</dt><dd>{number.format(item.usage.observations)}</dd></div>
      </dl>
      <p>{item.usage.coverage === 'unknown' ? 'No usage reports retained.' : 'Partial reported usage; missing values are unknown.'}</p>
      <Button onClick={onOpen}>View evidence</Button>
    </article>
  )
}

export function WorkInsights({ token }: { token: string }) {
  const [load, setLoad] = useState<Load<Awaited<ReturnType<typeof fetchInsightPage>>>>({ kind: 'loading' })
  const [detail, setDetail] = useState<Load<WorkInsight> | null>(null)
  const listController = useRef<AbortController | null>(null)
  const detailController = useRef<AbortController | null>(null)
  const listRequest = useRef(0)
  const detailRequest = useRef(0)

  const loadPage = useCallback((continuation?: { since: number; until: number; through: number; after: string }) => {
    listController.current?.abort()
    const controller = new AbortController()
    listController.current = controller
    const request = ++listRequest.current
    setLoad({ kind: 'loading' })
    fetchInsightPage(token, controller.signal, continuation)
      .then((value) => { if (request === listRequest.current && !controller.signal.aborted) setLoad({ kind: 'ready', value }) })
      .catch((error: unknown) => {
        if (controller.signal.aborted || request !== listRequest.current) return
        setLoad({ kind: 'error', message: error instanceof Error ? error.message : 'Work insights could not be loaded.' })
      })
    return controller
  }, [token])

  useEffect(() => {
    const controller = new AbortController()
    listController.current = controller
    const request = ++listRequest.current
    fetchInsightPage(token, controller.signal)
      .then((value) => { if (request === listRequest.current && !controller.signal.aborted) setLoad({ kind: 'ready', value }) })
      .catch((error: unknown) => {
        if (controller.signal.aborted || request !== listRequest.current) return
        setLoad({ kind: 'error', message: error instanceof Error ? error.message : 'Work insights could not be loaded.' })
      })
    return () => {
      listController.current?.abort()
      detailController.current?.abort()
      listRequest.current += 1
      detailRequest.current += 1
    }
  }, [token])

  const open = (id: string) => {
    detailController.current?.abort()
    const controller = new AbortController()
    detailController.current = controller
    const request = ++detailRequest.current
    setDetail({ kind: 'loading' })
    fetchWorkInsight(id, token, controller.signal)
      .then((value) => { if (request === detailRequest.current && !controller.signal.aborted) setDetail({ kind: 'ready', value }) })
      .catch((error: unknown) => {
        if (controller.signal.aborted || request !== detailRequest.current) return
        setDetail({ kind: 'error', missing: error instanceof ApiError && error.status === 404, message: error instanceof Error ? error.message : 'Work insight could not be loaded.' })
      })
  }

  const closeDetail = () => {
    detailController.current?.abort()
    detailRequest.current += 1
    setDetail(null)
  }

  if (load.kind === 'loading') return <section className="hub-section hub-section-narrow work-insights" aria-label="Work insights"><Skeleton active /></section>
  if (load.kind === 'error') return <section className="hub-section hub-section-narrow work-insights" aria-label="Work insights"><Alert type="error" showIcon message="Work insights unavailable" description={load.message} action={<Button icon={<ReloadOutlined />} onClick={() => loadPage()}>Retry</Button>} /></section>

  const page = load.value
  return (
    <section className="hub-section work-insights" aria-labelledby="work-insights-title">
      <header className="work-insights-heading"><div><span><BarChartOutlined /> Bounded evidence</span><h1 id="work-insights-title">Work insights</h1><p>Work created in the seven-day cohort ending {localDate(page.until)}. Metrics on this page were read as of {localDate(page.as_of)}. This page contains {page.items.length} item(s); it is not a whole-cohort total.</p></div></header>
      <Alert showIcon type="info" message="Reported work and stored publication state are separate" description="Usage is partial self-reported or provider-result evidence. Historical checks and publication associations do not establish approval, qualification, acceptance, or landing." />
      {page.items.length === 0 ? <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No retained work appears on this page of the selected window." /> : <div className="work-insights-grid">{page.items.map((item) => <InsightCard key={item.work_id} item={item} onOpen={() => open(item.work_id)} />)}</div>}
      <footer className="work-insights-pager">
        <span>{page.complete ? 'End of this fixed cohort.' : 'More work remains in this fixed cohort. Metrics are read again for each page.'}</span>
        {!page.complete && page.next_after && <Button onClick={() => loadPage({ since: page.since, until: page.until, through: page.through, after: page.next_after! })}>Next page</Button>}
      </footer>
      <Drawer width={720} title={detail?.kind === 'ready' ? detail.value.title : 'Work insight'} open={detail !== null} onClose={closeDetail}>
        {detail?.kind === 'loading' && <Skeleton active />}
        {detail?.kind === 'error' && <Alert type={detail.missing ? 'info' : 'error'} showIcon message={detail.missing ? 'Work insight not found' : 'Work insight unavailable'} description={detail.message} />}
        {detail?.kind === 'ready' && <InsightDetail item={detail.value} />}
      </Drawer>
    </section>
  )
}
