import {
  ArrowLeftOutlined,
  BranchesOutlined,
  CheckCircleOutlined,
  ClockCircleOutlined,
  ExportOutlined,
  HistoryOutlined,
  LinkOutlined,
  ReloadOutlined,
  SearchOutlined,
  TeamOutlined,
  WarningOutlined,
} from '@ant-design/icons'
import {
  Alert,
  Breadcrumb,
  Checkbox,
  Button,
  Descriptions,
  Empty,
  Input,
  Select,
  Skeleton,
  Space,
  Tag,
  Tabs,
  type DescriptionsProps,
} from 'antd'
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { ApiError } from '../api.ts'
import {
  briefingPath,
  coordinationPath,
  currentLeadParticipant,
  fetchBriefing,
  fetchCoordination,
  fetchOverview,
  fetchStandupWork,
  hasMoreBriefing,
  safeCommitHref,
  safeForgeIssueHref,
  workHref,
  workIdFromHash,
  workMatchesSearch,
  type BriefingQuery,
  type CoordinationBriefing,
  type CoordinationQuery,
  type CoordinationRequest,
  type CoordinationResponseKind,
  type CoordinationTarget,
  type StandupBriefing,
  type WorkCheckpoint,
  type WorkItem,
  type WorkOutcome,
  type WorkOverlapPair,
  type WorkOverview,
  type WorkSourceRef,
  type WorkState,
} from '../standup.ts'
import '../shared-work.css'
import { Chips, ExternalLink } from './bits.tsx'

type Load<T> = { kind: 'loading' } | { kind: 'ready'; value: T; fetchedAt: number } | { kind: 'error'; message: string; missing?: boolean }
type BriefingViewKey = 'current' | 'changes'
type OverviewViewKey = 'working' | 'finished' | 'coordination'
type StateFilter = 'all' | WorkState
type DescriptionItem = NonNullable<DescriptionsProps['items']>[number]

const POLL_INTERVAL_MS = 15_000

const STATE_LABEL: Record<WorkState, string> = {
  active: 'In progress',
  blocked: 'Blocked',
  paused: 'Paused',
  handover: 'Handover',
  finished: 'Reported finished',
}

const EVENT_LABEL: Record<string, string> = {
  work_started: 'Started work',
  work_joined: 'Joined work',
  work_takeover: 'Took over the claim',
  work_checkpoint: 'Reported a checkpoint',
}

const RESPONSE_LABEL: Record<CoordinationResponseKind, string> = {
  propose_split: 'Proposed a scope split',
  join: 'Offered to join',
  handover: 'Offered a handover',
  decline: 'Declined',
}

const FILTER_OPTIONS = [
  { value: 'all', label: 'All statuses' },
  { value: 'active', label: 'In progress' },
  { value: 'blocked', label: 'Blocked' },
  { value: 'paused', label: 'Paused' },
  { value: 'handover', label: 'Handover' },
  { value: 'finished', label: 'Reported finished' },
]

const WORK_LANES: { state: WorkState; label: string; description: string }[] = [
  { state: 'active', label: 'In progress', description: 'Agents reporting active work' },
  { state: 'blocked', label: 'Blocked', description: 'Work waiting on a reported blocker' },
  { state: 'handover', label: 'Handover', description: 'Work ready for another lead' },
  { state: 'paused', label: 'Paused', description: 'Assigned work that is paused' },
]

const localDate = (at: number): string => new Date(at * 1000).toLocaleString()

function WorkLink({ id, children }: { id: string; children?: ReactNode }) {
  return <a className="shared-work-link" href={workHref(id)}>{children ?? <code>{id}</code>}</a>
}

function WorkSource({ item, compact = false }: { item: WorkItem; compact?: boolean }) {
  const href = safeForgeIssueHref(item.source)
  if (item.source.kind === 'native') return compact ? <span>Plain Git</span> : <>Plain Git · <code>{item.source.request_key}</code></>
  const text = compact
    ? <>{item.source.provider} issue {item.source.issue}</>
    : <>{item.source.provider} · {item.source.repository} · issue <code>{item.source.issue}</code></>
  return href ? <ExternalLink href={href}>{text}{!compact && <> <ExportOutlined /></>}</ExternalLink> : text
}

function ClaimTag({ item }: { item: WorkItem }) {
  if (item.state === 'finished') return null
  if (item.claim_status === 'active') return <Tag bordered={false} title="The lead has checked in recently and still holds this assignment." className="shared-work-claim is-active">Assigned</Tag>
  const label = item.claim_status === 'stale' ? 'No recent check-in' : 'Check-in unknown'
  return <Tag bordered={false} icon={<WarningOutlined />} title="An old or unknown check-in does not release the lead's assignment." className="shared-work-claim is-warn">{label} · still assigned</Tag>
}

export function WorkTags({ item, showClaim = true }: { item: WorkItem; showClaim?: boolean }) {
  return (
    <Space size={[6, 6]} wrap>
      <Tag bordered={false} className={`shared-work-state is-${item.state}`}>{STATE_LABEL[item.state]}</Tag>
      {showClaim && <ClaimTag item={item} />}
    </Space>
  )
}

export function WorkClaimWarning({ item }: { item: WorkItem }) {
  if (item.state === 'finished' || item.claim_status === 'active') return null
  return <Alert className="shared-work-claim-alert" showIcon type="warning" message={item.claim_status === 'stale' ? 'No recent check-in' : 'Check-in unknown'} description="This task is still assigned. Coordinate with the lead before taking it over." />
}

function SourceReference({ reference, item }: { reference: WorkSourceRef; item: WorkItem }) {
  if (reference.kind === 'work') return <WorkLink id={reference.work_id} />
  if (reference.kind === 'repository_path') return <code>{reference.path}{reference.line === null ? '' : `:${reference.line}`}</code>
  const href = safeCommitHref(item.source, reference.oid)
  const text = <code>{reference.oid.slice(0, 12)}{reference.path ? ` · ${reference.path}` : ''}</code>
  return href ? <ExternalLink href={href}>{text} <ExportOutlined /></ExternalLink> : text
}

function Checkpoint({ checkpoint, item }: { checkpoint: WorkCheckpoint; item: WorkItem }) {
  return (
    <article className="shared-work-report">
      <div className="shared-work-report-heading"><Tag bordered={false}>Agent report</Tag><strong>{checkpoint.kind}</strong></div>
      <p>{checkpoint.summary}</p>
      {checkpoint.next_action && <p><strong>Next action:</strong> {checkpoint.next_action}</p>}
      {checkpoint.source_refs.length > 0 && (
        <div className="shared-work-references">
          <LinkOutlined aria-hidden="true" />
          {checkpoint.source_refs.map((reference, index) => <SourceReference key={`${reference.kind}-${index}`} reference={reference} item={item} />)}
        </div>
      )}
      <small>Reported by {checkpoint.agent} · {localDate(checkpoint.at)}</small>
    </article>
  )
}

function ReportedAction({ item }: { item: WorkItem }) {
  if (item.state === 'finished' && item.current) return <div className="shared-work-action"><strong>Latest agent report</strong><span>{item.current.summary}</span></div>
  if (item.blocker) return <div className="shared-work-action is-blocked"><strong>Reported blocker</strong><span>{item.blocker.summary}</span></div>
  if (item.next_action) return <div className="shared-work-action"><strong>Reported next action</strong><span>{item.next_action}</span></div>
  return <span className="shared-work-muted">No next action reported</span>
}

function WorkCard({ item, outcomeAt }: { item: WorkItem; outcomeAt?: number }) {
  const lead = currentLeadParticipant(item)
  const teammates = item.participants.filter((participant) => participant.role !== 'lead')
  return (
    <article className={`shared-work-card is-${item.state}`}>
      <div className="shared-work-card-owner">
        <span><strong>{item.lead_agent}</strong><small>{lead.login}</small></span>
        <WorkSource item={item} compact />
      </div>
      <h3><WorkLink id={item.id}>{item.title}</WorkLink></h3>
      <WorkTags item={item} />
      <ReportedAction item={item} />
      {teammates.length > 0 && (
        <div className="shared-work-card-team" aria-label="Other participants">
          {teammates.slice(0, 2).map((participant) => <span key={`${participant.token}-${participant.session}`}><TeamOutlined aria-hidden="true" /> {participant.agent} · {participant.role}</span>)}
          {teammates.length > 2 && <WorkLink id={item.id}>+{teammates.length - 2} more</WorkLink>}
        </div>
      )}
      <time className="shared-work-card-checkin" dateTime={new Date((outcomeAt ?? item.last_checkin_at) * 1000).toISOString()}>{outcomeAt === undefined ? 'Last check-in' : 'Reported finished'} {localDate(outcomeAt ?? item.last_checkin_at)}</time>
    </article>
  )
}

export function WorkBoard({ items, pageItems, snapshotDone, moreHistory, showFinished = false }: { items: WorkItem[]; pageItems: number; snapshotDone: boolean; moreHistory: boolean; showFinished?: boolean }) {
  const lanes = useMemo(() => showFinished ? [...WORK_LANES, { state: 'finished' as const, label: 'Reported finished', description: 'Work reported as finished' }] : WORK_LANES, [showFinished])
  const [selectedLane, setSelectedLane] = useState<WorkState>(() => lanes.find((lane) => items.some((item) => item.state === lane.state))?.state ?? 'active')
  const visibleLane = lanes.some((lane) => lane.state === selectedLane) ? selectedLane : lanes[0]!.state

  if (items.length === 0) {
    const description = pageItems > 0
      ? 'No work on this page matches your search.'
      : snapshotDone && moreHistory
        ? 'No more current work in this snapshot. More history is available in Recent changes.'
        : 'No current work appears on this page.'
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={description} />
  }

  return (
    <>
      <div className="shared-work-lane-picker" aria-label="Choose a progress lane">
        {lanes.map((lane) => {
          const count = items.filter((item) => item.state === lane.state).length
          return <Button key={lane.state} aria-label={`${lane.label}, ${count} on this page`} aria-pressed={visibleLane === lane.state} className={`shared-work-lane-button is-${lane.state}`} onClick={() => setSelectedLane(lane.state)}><span>{lane.label}</span><strong>{count}</strong></Button>
        })}
      </div>
      <div className="shared-work-board-region" tabIndex={0} aria-label="Current work grouped by progress">
        <div className={`shared-work-board has-${lanes.length}-lanes`}>
          {lanes.map((lane) => {
            const laneItems = items.filter((item) => item.state === lane.state)
            return (
              <section key={lane.state} className={`shared-work-lane is-${lane.state}${visibleLane === lane.state ? ' is-selected' : ''}`} aria-labelledby={`shared-work-lane-${lane.state}`}>
                <header className="shared-work-lane-heading">
                  <div><h2 id={`shared-work-lane-${lane.state}`}>{lane.label}</h2><p>{lane.description}</p></div>
                  <span>{laneItems.length} on page</span>
                </header>
                <div className="shared-work-lane-list">
                  {laneItems.length > 0 ? laneItems.map((item) => <WorkCard key={item.id} item={item} />) : <p className="shared-work-lane-empty">No matching work in this lane.</p>}
                </div>
              </section>
            )
          })}
        </div>
      </div>
    </>
  )
}

export function ChangeTable({ events }: { events: StandupBriefing['events'] }) {
  if (events.length === 0) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No recent changes on this page match these controls." />
  return (
    <div className="shared-work-table-region" tabIndex={0} aria-label="Recent changes on this page">
      <table className="shared-work-table shared-work-change-table">
        <thead><tr><th>Change</th><th>Agent + human owner</th><th>Status at this change</th><th>Reported context</th><th>When</th></tr></thead>
        <tbody>
          {events.map((event) => {
            const lead = currentLeadParticipant(event.item)
            return (
              <tr key={event.seq}>
                <td data-label="Change"><WorkLink id={event.work_id}>{event.item.title}</WorkLink><span className="shared-work-source">{EVENT_LABEL[event.kind]}</span></td>
                <td data-label="Agent + human owner"><strong>{event.item.lead_agent}</strong><span>Lead at this change: {lead.login}</span></td>
                <td data-label="Status at this change"><WorkTags item={event.item} showClaim={false} /></td>
                <td data-label="Reported context"><ReportedAction item={event.item} /></td>
                <td data-label="When"><time dateTime={new Date(event.at * 1000).toISOString()}>{localDate(event.at)}</time></td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

export function FinishedList({ outcomes, complete = true, filtered = false }: { outcomes: WorkOutcome[]; complete?: boolean; filtered?: boolean }) {
  if (outcomes.length === 0) {
    const description = filtered
      ? 'No reported completions match your search.'
      : complete
        ? 'No work was reported finished in this time window.'
        : 'No completion is shown, but retained history or the overview limit leaves this window incomplete.'
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={description} />
  }
  return (
    <div className="shared-work-finished-grid" aria-label="Recently reported finished work">
      {outcomes.map((outcome) => <WorkCard key={outcome.seq} item={outcome.item} outcomeAt={outcome.at} />)}
    </div>
  )
}

function CoordinationWork({ label, item }: { label: string; item: WorkItem }) {
  const lead = currentLeadParticipant(item)
  return (
    <div className="shared-work-coordination-work">
      <span>{label}</span>
      <strong>{item.lead_agent}</strong>
      <small>{lead.login}</small>
      <h3><WorkLink id={item.id}>{item.title}</WorkLink></h3>
      <div className="shared-work-coordination-scope">
        <span>Declared scope</span>
        {item.scope.length > 0 ? <Chips items={item.scope} max={6} /> : <em>Unknown · no scope declared</em>}
      </div>
    </div>
  )
}

function overlapMatchesSearch(overlap: WorkOverlapPair, search: string): boolean {
  if (workMatchesSearch(overlap.left, search) || workMatchesSearch(overlap.right, search)) return true
  const normalized = search.trim().toLocaleLowerCase()
  return overlap.reasons.some((reason) => `${reason.left_path} ${reason.right_path}`.toLocaleLowerCase().includes(normalized))
}

export function CoordinationList({ overlaps, complete, filtered = false }: { overlaps: WorkOverlapPair[]; complete: boolean; filtered?: boolean }) {
  if (overlaps.length === 0) {
    const description = filtered
      ? 'No declared-scope overlaps match your search.'
      : complete
      ? 'No declared-scope overlaps need coordination in this snapshot.'
      : 'No overlap is shown, but this snapshot is incomplete. More work may need coordination.'
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={description} />
  }
  return (
    <div className="shared-work-coordination-list" aria-label="Work that needs coordination">
      {overlaps.map((overlap) => (
        <article className="shared-work-coordination-row" key={[overlap.left.id, overlap.right.id].sort().join('-')}>
          <div className="shared-work-coordination-reason">
            <span>Why this needs attention</span>
            <strong>Declared scopes overlap</strong>
            <ul>
              {overlap.reasons.map((reason, index) => <li key={`${reason.left_path}-${reason.right_path}-${index}`}><code>{reason.left_path}</code><span>overlaps</span><code>{reason.right_path}</code></li>)}
            </ul>
            {!overlap.reasons_complete && <p className="shared-work-coordination-omission">Additional overlapping paths omitted; inspect both full declared scopes.</p>}
            <p>This is a coordination signal, not proof of a source conflict.</p>
          </div>
          <div className="shared-work-coordination-agents" aria-label="Involved agents">
            <CoordinationWork label="Agent one" item={overlap.left} />
            <CoordinationWork label="Agent two" item={overlap.right} />
          </div>
        </article>
      ))}
    </div>
  )
}

function currentAcknowledgement(request: CoordinationRequest, target: CoordinationTarget) {
  return request.acknowledgements
    .filter((acknowledgement) => acknowledgement.target_work_id === target.work_id && acknowledgement.generation === target.generation && acknowledgement.through_seq >= request.created_seq)
    .sort((left, right) => right.seq - left.seq)[0]
}

function currentResponse(request: CoordinationRequest, target: CoordinationTarget) {
  return request.responses
    .filter((response) => response.target_work_id === target.work_id && response.target_generation === target.generation)
    .sort((left, right) => right.seq - left.seq)[0]
}

function coordinationStage(request: CoordinationRequest): string {
  if (request.status === 'resolved') return 'Resolved'
  if (request.status === 'withdrawn') return 'Withdrawn'
  if (request.status === 'responded') return 'Replied'
  if (!request.acknowledgements_complete || !request.responses_complete) return 'Pending · history incomplete'
  if (request.targets.every((target) => currentAcknowledgement(request, target) || currentResponse(request, target))) return 'Awaiting replies'
  return 'Awaiting receipt'
}

function coordinationRequestMatchesSearch(request: CoordinationRequest, search: string): boolean {
  const normalized = search.trim().toLocaleLowerCase()
  if (!normalized) return true
  return [
    request.id,
    request.requester.agent,
    request.requester.login,
    request.proposal.title,
    request.proposal.request_key,
    ...request.proposal.scope,
    ...request.targets.flatMap((target) => [target.work_id, target.lead.agent, target.lead.login, ...target.reasons.flatMap((reason) => [reason.your_path, reason.their_path])]),
    ...request.responses.flatMap((response) => [response.responder.agent, response.responder.login, RESPONSE_LABEL[response.kind], response.note]),
  ].some((value) => value?.toLocaleLowerCase().includes(normalized))
}

function ProposalSource({ request }: { request: CoordinationRequest }) {
  if (request.proposal.work_id) return <WorkLink id={request.proposal.work_id}>{request.proposal.title}</WorkLink>
  if (request.proposal.source) {
    const source = { kind: 'forge' as const, ...request.proposal.source }
    const href = safeForgeIssueHref(source)
    const text = <>{request.proposal.title} · {source.provider} issue {source.issue}</>
    return href ? <ExternalLink href={href}>{text} <ExportOutlined /></ExternalLink> : text
  }
  return <>{request.proposal.title}{request.proposal.request_key && <> · <code>{request.proposal.request_key}</code></>}</>
}

export function CoordinationRequestList({ requests, filtered = false, complete = true }: { requests: CoordinationRequest[]; filtered?: boolean; complete?: boolean }) {
  if (requests.length === 0) {
    const description = filtered
      ? 'No directed coordination requests on this page match your search.'
      : complete
        ? 'No directed coordination requests appear in this snapshot.'
        : 'No request appears on this page, but more requests remain in the fixed snapshot.'
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={description} />
  }
  return (
    <div className="shared-work-request-list" aria-label="Directed coordination requests">
      {requests.map((request) => {
        const stage = coordinationStage(request)
        const targetNames = request.targets.map((target) => target.lead.agent).join(', ')
        const firstReason = request.targets[0]!.reasons[0]!
        const reasonCount = request.targets.reduce((count, target) => count + target.reasons.length, 0)
        return (
          <details className={`shared-work-request is-${request.status}`} key={request.id}>
            <summary className="shared-work-request-summary">
              <span className="shared-work-request-summary-layout">
                <span className="shared-work-request-summary-state"><Tag bordered={false} className={`shared-work-request-stage is-${request.status}`}>{stage}</Tag><time dateTime={new Date(request.updated_at * 1000).toISOString()}>Updated {localDate(request.updated_at)}</time></span>
                <span className="shared-work-request-summary-main"><strong>{request.proposal.title}</strong><small>{request.requester.agent} → {targetNames}</small></span>
                <span className="shared-work-request-summary-reason"><code>{firstReason.your_path}</code><span>overlaps</span><code>{firstReason.their_path}</code>{reasonCount > 1 && <small>+{reasonCount - 1} more shown inside</small>}</span>
              </span>
            </summary>
            <div className="shared-work-request-detail">
              <header className="shared-work-request-heading"><time dateTime={new Date(request.updated_at * 1000).toISOString()}>Updated {localDate(request.updated_at)}</time><code>{request.id}</code></header>
              <div className="shared-work-request-proposal">
                <h3><ProposalSource request={request} /></h3>
                <span>Requested by {request.requester.agent} · {request.requester.login}</span>
                <div><strong>Proposed scope</strong>{request.proposal.scope.length > 0 ? <Chips items={request.proposal.scope} max={12} /> : <em>Scope not declared</em>}</div>
              </div>
              {(!request.acknowledgements_complete || !request.responses_complete) && <p className="shared-work-request-history-warning">Some {request.acknowledgements_complete ? 'replies are' : request.responses_complete ? 'receipts are' : 'receipts and replies are'} omitted. The request status is authoritative; displayed target history is incomplete.</p>}
              <div className="shared-work-request-targets">
                {request.targets.map((target) => {
                  const acknowledgement = currentAcknowledgement(request, target)
                  const response = currentResponse(request, target)
                  const receivedAt = acknowledgement?.acknowledged_at ?? response?.created_at
                  return (
                    <section key={target.work_id} aria-labelledby={`coordination-target-${request.id}-${target.work_id}`}>
                      <div className="shared-work-request-target-heading">
                        <div><span>Target lead</span><strong>{target.lead.agent}</strong><small>{target.lead.login}</small></div>
                        <WorkLink id={target.work_id}><span id={`coordination-target-${request.id}-${target.work_id}`}>Open work</span></WorkLink>
                      </div>
                      <div className="shared-work-request-receipt">
                        <span className={receivedAt !== undefined ? 'is-received' : 'is-waiting'}>{receivedAt !== undefined ? `Received ${localDate(receivedAt)}` : request.acknowledgements_complete ? 'Awaiting receipt' : 'Receipt history incomplete'}</span>
                        <span className={response ? 'is-received' : 'is-waiting'}>{response ? RESPONSE_LABEL[response.kind] : request.responses_complete ? 'Awaiting reply' : 'Reply history incomplete'}</span>
                      </div>
                      <ul className="shared-work-request-reasons">
                        {target.reasons.map((reason, index) => <li key={`${reason.your_path}-${reason.their_path}-${index}`}><code>{reason.your_path}</code><span>overlaps</span><code>{reason.their_path}</code></li>)}
                      </ul>
                      {!target.reasons_complete && <p className="shared-work-request-omission">Additional overlapping paths omitted; inspect full declared scope.</p>}
                      {response && <div className="shared-work-request-response"><strong>{response.responder.agent} · {response.responder.login}</strong>{response.note ? <p>{response.note}</p> : <p>No response note provided.</p>}<time dateTime={new Date(response.created_at * 1000).toISOString()}>{localDate(response.created_at)}</time></div>}
                    </section>
                  )
                })}
              </div>
            </div>
          </details>
        )
      })}
    </div>
  )
}

function CoordinationQueue({ token, search }: { token: string; search: string }) {
  const [queries, setQueries] = useState<CoordinationQuery[]>([{ limit: 10, activeOnly: true }])
  const [page, setPage] = useState(0)
  const query = queries[page]!
  const requestPath = useMemo(() => coordinationPath(query), [query])
  const [load, setLoad] = useState<Load<CoordinationBriefing>>({ kind: 'loading' })
  const [attempt, setAttempt] = useState(0)
  const [manualRefresh, setManualRefresh] = useState(0)
  const [refreshError, setRefreshError] = useState<string | null>(null)
  const hasBriefing = useRef(false)
  const isFreshPage = page === 0 && query.through === undefined

  useEffect(() => {
    let disposed = false
    let running = false
    let timer: number | undefined
    let controller: AbortController | undefined
    const schedule = () => {
      if (!disposed && isFreshPage && !document.hidden) timer = window.setTimeout(() => void loadNow(), POLL_INTERVAL_MS)
    }
    const loadNow = async () => {
      if (disposed || running || document.hidden) return
      running = true
      controller = new AbortController()
      try {
        const value = await fetchCoordination(token, controller.signal, query)
        if (disposed) return
        hasBriefing.current = true
        setRefreshError(null)
        setLoad({ kind: 'ready', value, fetchedAt: Date.now() })
      } catch (error: unknown) {
        if (disposed || controller.signal.aborted) return
        const message = error instanceof Error ? error.message : 'Unknown error'
        if (hasBriefing.current) setRefreshError(message)
        else setLoad({ kind: 'error', message })
      } finally {
        running = false
        schedule()
      }
    }
    const visibilityChanged = () => {
      if (document.hidden) {
        if (timer !== undefined) window.clearTimeout(timer)
        timer = undefined
      } else if (isFreshPage && !running) {
        if (timer !== undefined) window.clearTimeout(timer)
        timer = undefined
        void loadNow()
      }
    }
    document.addEventListener('visibilitychange', visibilityChanged)
    void loadNow()
    return () => {
      disposed = true
      if (timer !== undefined) window.clearTimeout(timer)
      controller?.abort()
      document.removeEventListener('visibilitychange', visibilityChanged)
    }
  }, [token, query, requestPath, attempt, manualRefresh, isFreshPage])

  if (load.kind === 'loading') return <div className="shared-work-request-loading" aria-label="Loading coordination requests"><Skeleton active paragraph={{ rows: 5 }} /></div>
  if (load.kind === 'error') return <Alert showIcon type="error" message="Could not load directed coordination requests" description={load.message} action={<Button icon={<ReloadOutlined />} onClick={() => { setLoad({ kind: 'loading' }); setAttempt((value) => value + 1) }}>Retry</Button>} />

  const briefing = load.value
  const fetchedAt = new Date(load.fetchedAt)
  const visible = briefing.requests.filter((request) => coordinationRequestMatchesSearch(request, search))
  const canNext = !briefing.complete
  const next = () => {
    if (!canNext) return
    const nextQuery: CoordinationQuery = { after: briefing.next_after, through: briefing.through, limit: query.limit, activeOnly: query.activeOnly }
    setQueries((all) => isFreshPage
      ? [{ through: briefing.through, limit: query.limit, activeOnly: query.activeOnly }, nextQuery]
      : [...all.slice(0, page + 1), nextQuery])
    hasBriefing.current = false
    setRefreshError(null)
    setLoad({ kind: 'loading' })
    setPage((value) => value + 1)
  }
  const previous = () => {
    if (page === 0) return
    hasBriefing.current = false
    setRefreshError(null)
    setLoad({ kind: 'loading' })
    setPage((value) => value - 1)
  }
  const refreshRequests = () => {
    const resetSnapshot = !isFreshPage
    hasBriefing.current = !resetSnapshot && load.kind === 'ready'
    setRefreshError(null)
    if (resetSnapshot) setLoad({ kind: 'loading' })
    setQueries([{ limit: 10, activeOnly: query.activeOnly }])
    setPage(0)
    setManualRefresh((value) => value + 1)
  }
  return (
    <section className="shared-work-directed" aria-labelledby="shared-work-directed-heading">
      <div className="shared-work-section-heading shared-work-queue-heading"><div><h2 id="shared-work-directed-heading">Requests to agents</h2><p>See who received each request and how they replied.</p></div><Button icon={<ReloadOutlined />} aria-label="Refresh requests" title="Refresh requests" onClick={refreshRequests}><span className="shared-work-refresh-label">Refresh requests</span></Button></div>
      <div className="shared-work-queue-options">
      <Checkbox checked={!query.activeOnly} onChange={(event) => {
        hasBriefing.current = false
        setRefreshError(null)
        setLoad({ kind: 'loading' })
        setQueries([{ limit: 10, activeOnly: !event.target.checked }])
        setPage(0)
      }}>Include resolved and withdrawn</Checkbox>
        <span className="shared-work-count" aria-live="polite">{visible.length} of {briefing.requests.length} requests{briefing.complete ? '' : ' · more pages'}</span>
      </div>
      {refreshError && <Alert className="shared-work-completeness" showIcon type="warning" message="Could not refresh coordination requests" description={<>Showing data loaded at <time dateTime={fetchedAt.toISOString()}>{fetchedAt.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time>. {refreshError}</>} />}
      {briefing.retention_gap && <Alert className="shared-work-completeness" showIcon type="warning" message="Some coordination request history is no longer retained" description="The requests shown remain readable, but this fixed-watermark history has a retention gap." />}
      <CoordinationRequestList requests={visible} filtered={briefing.requests.length > 0 && visible.length === 0} complete={briefing.complete} />
      <div className="shared-work-pagination" aria-label="Directed coordination request pages">
        <Button className="shared-work-page-button" disabled={page === 0} onClick={previous}>Previous</Button>
        <span>Page {page + 1}</span>
        <Button className="shared-work-page-button" disabled={!canNext} onClick={next}>Next</Button>
      </div>
      <details className="shared-work-queue-metadata"><summary>Request delivery details · updated <time dateTime={fetchedAt.toISOString()}>{fetchedAt.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time></summary><p>{isFreshPage ? 'This queue refreshes every 15 seconds.' : `Fixed history snapshot through event ${briefing.through}.`} Agents receive and answer requests through MCP, hooks, or the HTTP inbox. Opening this page does not acknowledge a request.</p></details>
    </section>
  )
}

function AttentionSignals({ items }: { items: WorkItem[] }) {
  const signals = items.filter((item) => item.claim_status !== 'active' || item.scope.length === 0)
  if (signals.length === 0) return null
  return (
    <details className="shared-work-secondary shared-work-attention">
      <summary><span className="shared-work-secondary-summary"><span>Other attention signals</span><strong>{signals.length}</strong></span></summary>
      <div className="shared-work-secondary-body"><p>These do not establish an overlap, but they limit coordination confidence.</p><ul>
          {signals.map((item) => (
            <li key={item.id}>
              <div><WorkLink id={item.id}>{item.title}</WorkLink><span>{item.lead_agent} · {currentLeadParticipant(item).login}</span></div>
              <Space size={[6, 6]} wrap>
                {item.claim_status !== 'active' && <Tag bordered={false} className="shared-work-claim is-warn">{item.claim_status === 'stale' ? 'No recent check-in · still assigned' : 'Check-in unknown · still assigned'}</Tag>}
                {item.scope.length === 0 && <Tag bordered={false}>Scope unknown</Tag>}
              </Space>
            </li>
          ))}
        </ul></div>
    </details>
  )
}

function DecisionList({ outcomes }: { outcomes: WorkOutcome[] }) {
  if (outcomes.length === 0) return null
  return (
    <details className="shared-work-secondary shared-work-decisions">
      <summary><span className="shared-work-secondary-summary"><span>Recent coordination decisions</span><strong>{outcomes.length}</strong></span></summary>
      <div className="shared-work-secondary-body"><p>Agent-reported decisions from the same time window.</p><ul>
          {outcomes.map((outcome) => <li key={outcome.seq}><div><WorkLink id={outcome.item.id}>{outcome.item.title}</WorkLink><span>{outcome.item.current?.summary ?? 'No decision summary retained'}</span></div><time dateTime={new Date(outcome.at * 1000).toISOString()}>{localDate(outcome.at)}</time></li>)}
        </ul></div>
    </details>
  )
}

function OverviewView({ token, refresh }: { token: string; refresh: number }) {
  const [view, setView] = useState<OverviewViewKey>('working')
  const [search, setSearch] = useState('')
  const [load, setLoad] = useState<Load<WorkOverview>>({ kind: 'loading' })
  const [attempt, setAttempt] = useState(0)
  const [refreshError, setRefreshError] = useState<string | null>(null)
  const hasOverview = useRef(false)

  useEffect(() => {
    let disposed = false
    let running = false
    let timer: number | undefined
    let controller: AbortController | undefined
    const schedule = () => {
      if (!disposed && !document.hidden) timer = window.setTimeout(() => void loadNow(), POLL_INTERVAL_MS)
    }
    const loadNow = async () => {
      if (disposed || running || document.hidden) return
      running = true
      controller = new AbortController()
      try {
        const value = await fetchOverview(token, controller.signal)
        if (disposed) return
        hasOverview.current = true
        setRefreshError(null)
        setLoad({ kind: 'ready', value, fetchedAt: Date.now() })
      } catch (error: unknown) {
        if (disposed || controller.signal.aborted) return
        const message = error instanceof Error ? error.message : 'Unknown error'
        if (hasOverview.current) setRefreshError(message)
        else setLoad({ kind: 'error', message })
      } finally {
        running = false
        schedule()
      }
    }
    const visibilityChanged = () => {
      if (document.hidden) {
        if (timer !== undefined) window.clearTimeout(timer)
        timer = undefined
      } else if (!running) {
        if (timer !== undefined) window.clearTimeout(timer)
        timer = undefined
        void loadNow()
      }
    }
    document.addEventListener('visibilitychange', visibilityChanged)
    void loadNow()
    return () => {
      disposed = true
      if (timer !== undefined) window.clearTimeout(timer)
      controller?.abort()
      document.removeEventListener('visibilitychange', visibilityChanged)
    }
  }, [token, attempt, refresh])

  if (load.kind === 'loading') return <div className="shared-work-loading" aria-label="Loading work overview"><Skeleton active paragraph={{ rows: 8 }} /></div>
  if (load.kind === 'error') return <Alert showIcon type="error" message="Could not load the work overview" description={load.message} action={<Button icon={<ReloadOutlined />} onClick={() => { setLoad({ kind: 'loading' }); setAttempt((value) => value + 1) }}>Retry</Button>} />

  const overview = load.value
  const fetchedAt = new Date(load.fetchedAt)
  const finished = overview.recent.filter((outcome) => outcome.kind === 'completion')
  const decisions = overview.recent.filter((outcome) => outcome.kind === 'decision')
  const visibleCurrent = overview.current.filter((item) => workMatchesSearch(item, search))
  const visibleFinished = finished.filter((outcome) => workMatchesSearch(outcome.item, search))
  const visibleOverlaps = overview.overlaps.filter((overlap) => overlapMatchesSearch(overlap, search))
  const visibleAttention = overview.current.filter((item) => workMatchesSearch(item, search))
  const visibleDecisions = decisions.filter((outcome) => workMatchesSearch(outcome.item, search))
  const completeness = view === 'working' ? overview.current_complete : view === 'finished' ? overview.recent_complete : overview.overlaps_complete
  const incompleteDescription = view === 'coordination'
    ? 'The current-work limit was reached, so more declared-scope overlaps may exist. An empty list is not a global all-clear.'
    : view === 'finished' && overview.history_retention_gap
      ? 'Some outcomes in this time window may have been compacted from retained history.'
      : 'The overview limit was reached. More records exist outside this snapshot.'
  const count = view === 'working'
    ? `${visibleCurrent.length} of ${overview.current.length} current work items`
    : view === 'finished'
      ? `${visibleFinished.length} of ${finished.length} reported completions`
      : `${visibleOverlaps.length} of ${overview.overlaps.length} declared-scope overlaps`

  return (
    <div className="shared-work-browser">
      <div className="shared-work-statusbar">
        <span title={`Snapshot through ${localDate(overview.until)}`}>Read only · <ClockCircleOutlined /> Updated <time dateTime={fetchedAt.toISOString()} title={fetchedAt.toLocaleString()}>{fetchedAt.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time> · every 15s</span>
        <span className="shared-work-snapshot">Snapshot through <time dateTime={new Date(overview.until * 1000).toISOString()}>{localDate(overview.until)}</time></span>
      </div>
      {refreshError && <Alert className="shared-work-completeness" showIcon type="warning" message="Could not refresh the work overview" description={<>Showing data loaded at <time dateTime={fetchedAt.toISOString()}>{fetchedAt.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time>. {refreshError}</>} />}

      <div className="shared-work-primary-views" role="tablist" aria-label="Work views">
        {([{ key: 'working', label: 'Working now' }, { key: 'finished', label: 'Recently finished' }, { key: 'coordination', label: 'Needs coordination' }] as const).map(({ key, label }) => (
          <button key={key} type="button" role="tab" id={`shared-work-view-${key}`} data-view={key} aria-selected={view === key} aria-controls="shared-work-overview-panel" tabIndex={view === key ? 0 : -1} onClick={() => setView(key)} onKeyDown={(event) => {
            const keys: OverviewViewKey[] = ['working', 'finished', 'coordination']
            const index = keys.indexOf(key)
            const next = event.key === 'Home' ? keys[0] : event.key === 'End' ? keys[2] : event.key === 'ArrowRight' ? keys[(index + 1) % 3] : event.key === 'ArrowLeft' ? keys[(index + 2) % 3] : undefined
            if (next) {
              event.preventDefault()
              setView(next)
              event.currentTarget.parentElement?.querySelector<HTMLButtonElement>(`[data-view="${next}"]`)?.focus()
            }
          }}>{label}</button>
        ))}
      </div>
      <div id="shared-work-overview-panel" role="tabpanel" aria-labelledby={`shared-work-view-${view}`}>

      {!completeness && <Alert className="shared-work-completeness" showIcon type="warning" message="This section is incomplete" description={incompleteDescription} />}
      {overview.history_retention_gap && view === 'coordination' && <Alert className="shared-work-completeness" showIcon type="warning" message="Some recent work history is no longer retained" description="The declared-scope overlap snapshot is current, but recent coordination decisions may be missing." />}
      {view === 'finished' && <div className="shared-work-window"><strong>Overview window</strong><span><time dateTime={new Date(overview.recent_since * 1000).toISOString()}>{localDate(overview.recent_since)}</time> to <time dateTime={new Date(overview.until * 1000).toISOString()}>{localDate(overview.until)}</time></span><small>Reported finished does not confirm verification, review, or merge.</small></div>}

      <div className="shared-work-controls is-current">
        <label><span>Search this section</span><Input aria-label="Search this section" allowClear prefix={<SearchOutlined />} value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Search agents, work, or scope" /></label>
      </div>
      {view !== 'coordination' && <div className="shared-work-count" aria-live="polite">{count}{completeness ? ' · complete' : ' · incomplete'}</div>}

      {view === 'working' && <WorkBoard items={visibleCurrent} pageItems={overview.current.length} snapshotDone={overview.current_complete} moreHistory={false} />}
      {view === 'finished' && <FinishedList outcomes={visibleFinished} complete={overview.recent_complete} filtered={finished.length > 0 && visibleFinished.length === 0} />}
      {view === 'coordination' && (
        <div className="shared-work-coordination-view">
          <CoordinationQueue token={token} search={search} />
          <details className="shared-work-secondary shared-work-detected">
            <summary><span className="shared-work-secondary-summary"><span>Detected scope overlaps</span><strong>{visibleOverlaps.length}</strong></span></summary>
            <div className="shared-work-secondary-body"><p>Server-detected overlap is a coordination signal, not proof of a source conflict or request delivery.</p><CoordinationList overlaps={visibleOverlaps} complete={overview.overlaps_complete} filtered={overview.overlaps.length > 0 && visibleOverlaps.length === 0} /></div>
          </details>
          <AttentionSignals items={visibleAttention} />
          <DecisionList outcomes={visibleDecisions} />
        </div>
      )}
      </div>
    </div>
  )
}

function BriefingView({ token }: { token: string }) {
  const [queries, setQueries] = useState<BriefingQuery[]>([{ limit: 10 }])
  const [page, setPage] = useState(0)
  const [view, setView] = useState<BriefingViewKey>('changes')
  const [search, setSearch] = useState('')
  const [filter, setFilter] = useState<StateFilter>('all')
  const query = queries[page]!
  const requestPath = useMemo(() => briefingPath(query), [query])
  const [load, setLoad] = useState<Load<StandupBriefing>>({ kind: 'loading' })
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    const controller = new AbortController()
    void fetchBriefing(token, controller.signal, query).then(
      (value) => setLoad({ kind: 'ready', value, fetchedAt: Date.now() }),
      (error: unknown) => {
        if (!controller.signal.aborted) setLoad({ kind: 'error', message: error instanceof Error ? error.message : 'Unknown error' })
      },
    )
    return () => controller.abort()
  }, [token, query, requestPath, attempt])

  if (load.kind === 'loading') return <div className="shared-work-loading" aria-label="Loading shared work"><Skeleton active paragraph={{ rows: 8 }} /></div>
  if (load.kind === 'error') return <Alert showIcon type="error" message="Could not load shared work" description={load.message} action={<Button icon={<ReloadOutlined />} onClick={() => { setLoad({ kind: 'loading' }); setAttempt((value) => value + 1) }}>Retry</Button>} />

  const briefing = load.value
  const fetchedAt = new Date(load.fetchedAt)
  const normalizedSearch = search.trim().toLocaleLowerCase()
  const visibleItems = briefing.items.filter((item) => workMatchesSearch(item, normalizedSearch))
  const visibleEvents = briefing.events.filter((event) => (filter === 'all' || event.item.state === filter) && workMatchesSearch(event.item, normalizedSearch))
  const canNext = hasMoreBriefing(briefing, view)

  const next = () => {
    if (!canNext) return
    const nextQuery: BriefingQuery = {
      after: briefing.next_after,
      ...(briefing.next_work_after ? { workAfter: briefing.next_work_after } : {}),
      workDone: briefing.next_work_done,
      through: briefing.through,
      limit: query.limit,
    }
    setQueries((all) => [...all.slice(0, page + 1), nextQuery])
    setLoad({ kind: 'loading' })
    setPage((value) => value + 1)
  }

  const previous = () => {
    if (page === 0) return
    setLoad({ kind: 'loading' })
    setPage((value) => value - 1)
  }

  return (
    <div className="shared-work-browser">
      <div className="shared-work-statusbar">
        <span>Read only · <ClockCircleOutlined /> Loaded <time dateTime={fetchedAt.toISOString()} title={fetchedAt.toLocaleString()}>{fetchedAt.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time></span>
      </div>
      {briefing.retention_gap && <Alert showIcon type="warning" message="Some acknowledged history is no longer retained" description="The current work snapshot remains available, but this change history has a retention gap." />}

      <Tabs className="shared-work-tabs" activeKey={view} onChange={(key) => setView(key as BriefingViewKey)} items={[
        { key: 'current', label: 'Current work' },
        { key: 'changes', label: 'Recent changes' },
      ]} />
      {view === 'changes' && <p className="shared-work-boundary-note">Reported updates since your last acknowledged briefing. Viewing this page does not mark them as read.</p>}

      <div className={`shared-work-controls is-${view}`}>
        <label><span>Search this page</span><Input aria-label="Search this page" allowClear prefix={<SearchOutlined />} value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Search this page" /></label>
        {view === 'changes' && <label><span>Filter this page</span><Select<StateFilter> aria-label="Filter this page" value={filter} options={FILTER_OPTIONS} onChange={setFilter} /></label>}
      </div>

      <div className="shared-work-count" aria-live="polite">
        {view === 'current' ? `${visibleItems.length} of ${briefing.items.length} work items on this page` : `${visibleEvents.length} of ${briefing.events.length} changes on this page`}
      </div>

      {view === 'current'
        ? <WorkBoard items={visibleItems} pageItems={briefing.items.length} snapshotDone={briefing.next_work_done} moreHistory={!briefing.complete} showFinished={briefing.items.some((item) => item.state === 'finished')} />
        : <ChangeTable events={visibleEvents} />}

      <div className="shared-work-pagination" aria-label="Shared work pages">
        <Button className="shared-work-page-button" disabled={page === 0} onClick={previous}>Previous</Button>
        <span>Page {page + 1}</span>
        <Button className="shared-work-page-button" disabled={!canNext} onClick={next}>Next</Button>
      </div>
      {!canNext && hasMoreBriefing(briefing, view === 'current' ? 'changes' : 'current') && <p className="shared-work-boundary-note">End of {view === 'current' ? 'current work' : 'recent changes'} in this snapshot. More pages are available in {view === 'current' ? 'Recent changes' : 'Current work'}.</p>}
    </div>
  )
}

function relations(key: string, label: string, ids: string[]): DescriptionItem[] {
  if (ids.length === 0) return []
  return [{ key, label, children: <Space size={[8, 4]} wrap>{ids.map((id) => <WorkLink key={id} id={id} />)}</Space> }]
}

export function WorkMetadata({ item }: { item: WorkItem }) {
  const lead = currentLeadParticipant(item)
  const items: DescriptionItem[] = [
    { key: 'work-id', label: 'Work ID', children: <code>{item.id}</code> },
    { key: 'source', label: 'Source', children: <WorkSource item={item} /> },
    { key: 'repository', label: 'Repository', children: <code>{item.repository}</code> },
    { key: 'creator', label: 'Creator', children: item.owner_login },
    { key: 'lead', label: 'Current lead', children: <>{lead.login} · {item.lead_agent} · <code>{item.lead_session}</code></> },
    { key: 'checkin', label: 'Last check-in', children: localDate(item.last_checkin_at) },
    { key: 'generation', label: 'Generation', children: item.generation },
    ...(item.branch ? [{ key: 'branch', label: 'Branch', children: <><BranchesOutlined /> <code>{item.branch}</code></> }] : []),
    ...(item.worktree ? [{ key: 'worktree', label: 'Worktree', children: <code>{item.worktree}</code> }] : []),
    ...(item.next_action ? [{ key: 'next-action', label: 'Agent-reported next action', span: 'filled' as const, children: item.next_action }] : []),
    ...relations('depends-on', 'Depends on', item.depends_on),
    ...relations('related-work', 'Related work', item.related_work),
    { key: 'created', label: 'Created', children: localDate(item.created_at) },
    { key: 'updated', label: 'Updated', children: localDate(item.updated_at) },
  ]
  return <Descriptions bordered size="small" column={{ xs: 1, sm: 1, md: 2, lg: 2, xl: 2, xxl: 2 }} items={items} />
}

function WorkDetail({ id, token }: { id: string; token: string }) {
  const [load, setLoad] = useState<Load<WorkItem>>({ kind: 'loading' })
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    const controller = new AbortController()
    void fetchStandupWork(id, token, controller.signal).then(
      (value) => setLoad({ kind: 'ready', value, fetchedAt: Date.now() }),
      (error: unknown) => {
        if (!controller.signal.aborted) setLoad({ kind: 'error', missing: error instanceof ApiError && error.status === 404, message: error instanceof Error ? error.message : 'Unknown error' })
      },
    )
    return () => controller.abort()
  }, [id, token, attempt])

  if (load.kind === 'loading') return <div className="shared-work-loading" aria-label="Loading work item"><Skeleton active paragraph={{ rows: 10 }} /></div>
  if (load.kind === 'error') return <Alert showIcon type={load.missing ? 'warning' : 'error'} message={load.missing ? 'Work item not found' : 'Could not load this work item'} description={load.missing ? 'It may not exist or this token may no longer be allowed to inspect it.' : load.message} action={<Button onClick={() => { setLoad({ kind: 'loading' }); setAttempt((value) => value + 1) }}>Try again</Button>} />

  const item = load.value
  const lead = currentLeadParticipant(item)
  return (
    <div className="shared-work-detail">
      <div className="shared-work-detail-heading">
        <div><Button className="shared-work-back" type="link" href="#work" icon={<ArrowLeftOutlined />}>All shared work</Button><h1>{item.title}</h1><p><strong>{item.lead_agent}</strong> with {lead.login}</p></div>
        <div className="shared-work-detail-status"><WorkTags item={item} /></div>
      </div>

      <WorkClaimWarning item={item} />

      <section className="shared-work-action-grid" aria-label="Reported work status">
        <div className={item.blocker ? 'shared-work-focus is-blocked' : 'shared-work-focus'}><span>Reported blocker</span><strong>{item.blocker?.summary ?? 'No blocker reported'}</strong></div>
        <div className="shared-work-focus"><span>Reported next action</span><strong>{item.next_action ?? 'No next action reported'}</strong></div>
        <div className="shared-work-focus"><span>Last check-in</span><strong>{localDate(item.last_checkin_at)}</strong></div>
      </section>

      <section className="shared-work-detail-section" aria-labelledby="shared-work-report-heading">
        <div className="shared-work-section-heading"><div><h2 id="shared-work-report-heading"><CheckCircleOutlined /> Latest report</h2><p>An agent’s update. “Reported finished” does not mean verified, reviewed, or merged.</p></div></div>
        {item.current ? <Checkpoint checkpoint={item.current} item={item} /> : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No report has been recorded." />}
      </section>

      <div className="shared-work-detail-grid">
        <section className="shared-work-detail-section" aria-labelledby="shared-work-scope-heading">
          <div className="shared-work-section-heading"><div><h2 id="shared-work-scope-heading">Declared scope</h2><p>A coordination claim, not a filesystem lock.</p></div><span>{item.scope.length}</span></div>
          {item.scope.length ? <Chips items={item.scope} max={12} /> : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No repository paths were declared." />}
        </section>
        <section className="shared-work-detail-section" aria-labelledby="shared-work-participants-heading">
          <div className="shared-work-section-heading"><div><h2 id="shared-work-participants-heading"><TeamOutlined /> Participants</h2></div><span>{item.participants.length}</span></div>
          <ul className="shared-work-participants">
            {item.participants.map((participant) => <li key={`${participant.token}-${participant.session}`}><span><strong>{participant.agent}</strong><small>{participant.login}</small></span><span><Tag bordered={false}>{participant.role}</Tag></span></li>)}
          </ul>
        </section>
      </div>

      <details className="shared-work-technical"><summary>Technical details</summary><div className="shared-work-technical-body"><WorkMetadata item={item} /></div></details>
    </div>
  )
}

export function Standup({ token, repository }: { token: string; repository: string }) {
  const [workId, setWorkId] = useState(() => workIdFromHash(window.location.hash))
  const [refresh, setRefresh] = useState(0)
  const [showHistory, setShowHistory] = useState(false)

  useEffect(() => {
    const changed = () => setWorkId(workIdFromHash(window.location.hash))
    window.addEventListener('hashchange', changed)
    return () => window.removeEventListener('hashchange', changed)
  }, [])

  return (
    <section className="workflow-page shared-work" aria-label="Shared work">
      {workId ? (
        <><Breadcrumb className="shared-work-breadcrumb" items={[
          { title: <a href="#work">Shared work</a> },
          { title: <span className="workflow-breadcrumb-repo">{repository}</span> },
        ]} /><WorkDetail key={`${workId}-${refresh}`} id={workId} token={token} /></>
      ) : (
        <>
          <div className="shared-work-heading">
            <div><h1>{showHistory ? 'Change history' : 'Who’s working on what'}</h1><p>{showHistory ? 'Fixed-watermark history of agent-reported work changes.' : 'Current ownership, recent outcomes, and declared-scope overlaps.'}</p></div>
            <div className="shared-work-heading-actions">
              <Button type="link" icon={<HistoryOutlined />} aria-label={showHistory ? 'Back to overview' : 'Change history'} title={showHistory ? 'Back to overview' : 'Change history'} onClick={() => setShowHistory((value) => !value)}><span className="shared-work-history-label">{showHistory ? 'Back to overview' : 'Change history'}</span></Button>
              <Button className="shared-work-refresh" icon={<ReloadOutlined />} aria-label={showHistory ? 'Refresh change history' : 'Refresh work overview'} title={showHistory ? 'Refresh change history' : 'Refresh work overview'} onClick={() => setRefresh((value) => value + 1)} />
            </div>
          </div>
          {showHistory ? <BriefingView key={refresh} token={token} /> : <OverviewView key={token} token={token} refresh={refresh} />}
        </>
      )}
    </section>
  )
}
