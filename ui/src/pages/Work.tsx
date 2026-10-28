import { useEffect, useState, type ReactNode } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import {
  currentLeadParticipant,
  fetchBriefing,
  fetchCoordination,
  fetchStandupWork,
  hasMoreBriefing,
  safeCommitHref,
  safeForgeIssueHref,
  workHref,
  workIdFromHash,
  workMatchesSearch,
  type BriefingQuery,
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
import { agentLabel, Ago, Avatar, Box, Empty, Explain, Icon, Notice, Path, Paths, Pill, Skeleton, Tip, localDate, type Tone } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'
import { useNow } from '../useHub.ts'
import { HideButton, hideKey, isStale, ShowHidden, useHidden, useHideable } from '../dismiss.tsx'
import { hashPath, Pager } from '../Pager.tsx'
import { localCredential } from '../localApi.ts'
import { onFrontDoor } from '../frontDoor.ts'
import { IntentReviewPanel } from '../IntentReviewPanel.tsx'

export const STATE: Record<WorkState, { label: string; tone: Tone; icon: string; tip: string }> = {
  active: { label: 'In progress', tone: 'brand', icon: 'play-circle', tip: 'The lead agent reports it is actively working on this.' },
  blocked: { label: 'Blocked', tone: 'bad', icon: 'sign-stop', tip: 'The lead agent reported a blocker and is waiting on it.' },
  handover: { label: 'Handover', tone: 'brand', icon: 'arrow-left-right', tip: 'The lead reported this is ready for another agent to take over.' },
  paused: { label: 'Paused', tone: 'warn', icon: 'pause-circle', tip: 'Still assigned, but the lead reported it has paused.' },
  finished: { label: 'Reported finished', tone: 'ok', icon: 'check2-circle', tip: 'The agent reported this finished. That does not confirm review, verification or merge.' },
}
const LANES: WorkState[] = ['active', 'blocked', 'handover', 'paused']

const EVENT_LABEL: Record<string, string> = { work_started: 'Started work', work_joined: 'Joined work', work_takeover: 'Took over the claim', work_checkpoint: 'Reported a checkpoint' }
const EVENT_ICON: Record<string, string> = { work_started: 'play-fill', work_joined: 'person-plus', work_takeover: 'arrow-left-right', work_checkpoint: 'flag' }
const RESPONSE_LABEL: Record<CoordinationResponseKind, string> = { propose_split: 'Proposed a scope split', join: 'Offered to join', handover: 'Offered a handover', decline: 'Declined' }

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`
const spanText = (seconds: number) => (seconds <= 48 * 3600 ? plural(Math.max(1, Math.round(seconds / 3600)), 'hour') : plural(Math.round(seconds / 86400), 'day'))

export function lead(item: WorkItem): { agent: string; login: string } {
  try {
    const p = currentLeadParticipant(item)
    return { agent: p.agent, login: p.login }
  } catch {
    return { agent: item.lead_agent, login: item.owner_login }
  }
}

export function StatePill({ state }: { state: WorkState }) {
  const s = STATE[state]
  return (
    <Pill tone={s.tone} icon={s.icon} tip={s.tip}>
      {s.label}
    </Pill>
  )
}

export function ClaimPill({ item }: { item: WorkItem }) {
  if (item.state === 'finished') return null
  if (item.claim_status === 'active')
    return (
      <Pill tone="ok" icon="person-check" tip="The lead checked in recently and still holds this assignment.">
        Assigned
      </Pill>
    )
  return (
    <Pill tone="warn" icon="exclamation-triangle" tip="An old or unknown check-in does not release the assignment. Coordinate with the lead before taking it over.">
      {item.claim_status === 'stale' ? 'No recent check-in' : 'Check-in unknown'} · still assigned
    </Pill>
  )
}

/** The current segment's look follows the reported state; a stale claim is amber whatever the state says. */
function nowClass(i: WorkItem): string {
  if (i.state === 'blocked') return 'lb-fail'
  if (i.claim_status !== 'active' || i.state === 'paused') return 'lb-stale'
  if (i.state === 'handover') return 'lb-ready'
  return ''
}

const NOW_NOTE: Record<WorkState, (i: WorkItem) => string> = {
  active: (i) => i.current?.summary ?? 'Working, no report yet',
  blocked: (i) => i.blocker?.summary ?? 'Blocked',
  handover: (i) => i.next_action ?? 'Ready to hand over',
  paused: (i) => i.current?.summary ?? 'Paused',
  finished: (i) => i.current?.summary ?? 'Reported finished',
}

/**
 * One task as a segmented capsule, read left to right: claimed, where it is now (wide, with the
 * holder's face), its next step, then finished. Every segment opens the task.
 */
export function LaneBar({ item, fresh }: { item: WorkItem; fresh: boolean }) {
  const l = lead(item)
  const href = workHref(item.id)
  const s = STATE[item.state]
  const claimTip = `Claimed by ${agentLabel(l.agent)} (${l.login}) ${localDate(item.created_at)}`
  return (
    <div className="lb-wrap">
      <ol className="lb lb-fit" aria-label={`${item.title}: ${s.label}`}>
        <li className="lb-seg lb-done">
          <Tip text={claimTip} className="lb-tip">
            <a className="lb-hit" href={href}>
              <Icon name="check2" />
              <span className="lb-label">Claimed</span>
            </a>
          </Tip>
        </li>
        <li className={`lb-seg is-here ${nowClass(item)}${fresh ? ' is-new' : ''}`}>
          <a className="lb-hit" href={href}>
            <span className="baton">
              <Avatar name={l.agent} size={24} />
            </span>
            <span className="lb-label">{item.claim_status === 'active' ? s.label : item.claim_status === 'stale' ? 'Quiet' : 'Unknown'}</span>
            <span className="lb-note">{NOW_NOTE[item.state](item)}</span>
          </a>
        </li>
        <li className={`lb-seg ${item.next_action ? 'lb-none' : 'lb-unknown'}`}>
          <Tip text={item.next_action ? `Next: ${item.next_action}` : 'No next step reported'} className="lb-tip">
            <a className="lb-hit" href={href}>
              <Icon name="arrow-right" />
              <span className="lb-label">Next</span>
            </a>
          </Tip>
        </li>
        <li className="lb-seg">
          <Tip text="Not reported finished yet. Even a finished report doesn’t mean it was reviewed or merged." className="lb-tip">
            <a className="lb-hit" href={href}>
              <Icon name="flag" />
              <span className="lb-label">Finished</span>
            </a>
          </Tip>
        </li>
      </ol>
    </div>
  )
}

function WorkLink({ id, children }: { id: string; children?: ReactNode }) {
  return <a href={workHref(id)}>{children ?? <code>{id}</code>}</a>
}

function Source({ item }: { item: WorkItem }) {
  if (item.source.kind === 'native')
    return (
      <Tip text={`Plain Git work request ${item.source.request_key}`} focusable className="muted small">
        <Icon name="git" /> Plain Git
      </Tip>
    )
  const href = safeForgeIssueHref(item.source)
  const text = (
    <>
      <Icon name={item.source.provider === 'gitlab' ? 'gitlab' : 'github'} /> issue {item.source.issue}
    </>
  )
  return href ? (
    <a className="small" href={href} target="_blank" rel="noreferrer noopener">
      {text} <Icon name="box-arrow-up-right" />
    </a>
  ) : (
    <span className="muted small">{text}</span>
  )
}

function Action({ item }: { item: WorkItem }) {
  if (item.state === 'finished' && item.current) return <div className="action"><span className="action-label">Latest report</span>{item.current.summary}</div>
  if (item.blocker) return <div className="action is-blocked"><span className="action-label"><Icon name="sign-stop" /> Blocker</span>{item.blocker.summary}</div>
  if (item.next_action) return <div className="action"><span className="action-label"><Icon name="arrow-right-circle" /> Next</span>{item.next_action}</div>
  return <div className="muted small">No next action reported</div>
}

function WorkCard({ item, outcomeAt }: { item: WorkItem; outcomeAt?: number }) {
  const l = lead(item)
  const helpers = item.participants.filter((p) => p.role !== 'lead')
  return (
    <article className={`work-card is-${item.state}`}>
      <div className="work-owner">
        <Avatar name={l.agent} size={28} />
        <span>
          <strong title={l.agent}>{agentLabel(l.agent)}</strong> <span className="muted small">{l.login}</span>
        </span>
      </div>
      <h3 className="work-title">
        <WorkLink id={item.id}>{item.title}</WorkLink>
      </h3>
      <div className="pills">
        <StatePill state={item.state} />
        {/* In progress, a live claim is the normal case; elsewhere "Assigned" says the lead still holds it. */}
        {(item.state !== 'active' || item.claim_status !== 'active') && <ClaimPill item={item} />}
      </div>
      <Action item={item} />
      {helpers.length > 0 && (
        <div className="small muted">
          <Icon name="people" /> {helpers.slice(0, 2).map((p) => `${agentLabel(p.agent)} (${p.role})`).join(', ')}
          {helpers.length > 2 && <> · <WorkLink id={item.id}>+{helpers.length - 2} more</WorkLink></>}
        </div>
      )}
      <div className="work-foot small muted">
        <Icon name={outcomeAt === undefined ? 'clock' : 'flag'} /> {outcomeAt === undefined ? 'Checked in' : 'Finished'} <Ago at={outcomeAt ?? item.last_checkin_at} />
        <span className="ms-auto">
          <Source item={item} />
        </span>
      </div>
    </article>
  )
}

function Board({ items, showFinished = false }: { items: WorkItem[]; showFinished?: boolean }) {
  const lanes = showFinished ? [...LANES, 'finished' as const] : LANES
  const [mobileLane, setMobileLane] = useState<WorkState>(() => lanes.find((s) => items.some((i) => i.state === s)) ?? 'active')
  return (
    <>
      <div className="lane-picker" role="group" aria-label="Choose a lane">
        {lanes.map((s) => (
          <button key={s} type="button" className={`btn btn-sm ${mobileLane === s ? 'btn-primary' : 'btn-outline-secondary'}`} aria-pressed={mobileLane === s} onClick={() => setMobileLane(s)}>
            {STATE[s].label} · {items.filter((i) => i.state === s).length}
          </button>
        ))}
      </div>
      <div data-tour="work-lanes" className={`board lanes-${lanes.length}`}>
        {lanes.map((s) => {
          const inLane = items.filter((i) => i.state === s)
          return (
            <section key={s} className={`lane tone-${STATE[s].tone}${mobileLane === s ? ' is-picked' : ''}`} aria-label={STATE[s].label}>
              <header className="lane-head">
                <Icon name={STATE[s].icon} />
                <h2 className="lane-title">{STATE[s].label}</h2>
                <span className="lane-count">{inLane.length}</span>
                <Explain text={STATE[s].tip} />
              </header>
              <div className="lane-body">
                {inLane.length === 0 ? <p className="lane-empty">Nothing {STATE[s].label.toLowerCase()}</p> : inLane.map((i) => <WorkCard key={i.id} item={i} />)}
              </div>
            </section>
          )
        })}
      </div>
    </>
  )
}

function Search({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
  return (
    <div className="input-group search">
      <span className="input-group-text">
        <Icon name="search" />
      </span>
      <input className="form-control" aria-label={placeholder} placeholder={placeholder} value={value} onChange={(e) => onChange(e.target.value)} />
      {value && (
        <button type="button" className="btn btn-outline-secondary" onClick={() => onChange('')} aria-label="Clear search">
          <Icon name="x-lg" />
        </button>
      )}
    </div>
  )
}

/* ---------- Needs coordination ---------- */

function currentAck(r: CoordinationRequest, t: CoordinationTarget) {
  return r.acknowledgements.filter((a) => a.target_work_id === t.work_id && a.generation === t.generation && a.through_seq >= r.created_seq).sort((x, y) => y.seq - x.seq)[0]
}
function currentResponse(r: CoordinationRequest, t: CoordinationTarget) {
  return r.responses.filter((x) => x.target_work_id === t.work_id && x.target_generation === t.generation).sort((x, y) => y.seq - x.seq)[0]
}
function stage(r: CoordinationRequest): { label: string; tone: Tone; icon: string } {
  if (r.status === 'resolved') return { label: 'Resolved', tone: 'ok', icon: 'check2-all' }
  if (r.status === 'withdrawn') return { label: 'Withdrawn', tone: 'muted', icon: 'x-circle' }
  if (r.status === 'responded') return { label: 'Replied', tone: 'brand', icon: 'reply' }
  if (!r.acknowledgements_complete || !r.responses_complete) return { label: 'Pending · history incomplete', tone: 'warn', icon: 'question-circle' }
  if (r.targets.every((t) => currentAck(r, t) || currentResponse(r, t))) return { label: 'Awaiting replies', tone: 'warn', icon: 'hourglass-split' }
  return { label: 'Awaiting receipt', tone: 'warn', icon: 'envelope' }
}
function requestMatches(r: CoordinationRequest, q: string) {
  const n = q.trim().toLocaleLowerCase()
  if (!n) return true
  return [r.id, r.requester.agent, r.requester.login, r.proposal.title, r.proposal.request_key, ...r.proposal.scope, ...r.targets.flatMap((t) => [t.work_id, t.lead.agent, t.lead.login, ...t.reasons.flatMap((x) => [x.your_path, x.their_path])]), ...r.responses.flatMap((x) => [x.responder.agent, x.responder.login, RESPONSE_LABEL[x.kind], x.note])].some((v) => v?.toLocaleLowerCase().includes(n))
}

/** Both sides of an overlap reason; a path both sides declared shows once. */
function PathPair({ a, b, joiner = <Icon name="intersect" className="muted" /> }: { a: string; b: string; joiner?: ReactNode }) {
  return a === b ? <Path>{a}</Path> : <><Path>{a}</Path> {joiner} <Path>{b}</Path></>
}

// Browser-only hide keys for the Inbox's lingering rows (see dismiss.tsx).
const requestHideKey = (r: CoordinationRequest) => hideKey(`request:${r.id}`, r.updated_at)
const signalUpdatedAt = (i: WorkItem) => Math.max(i.updated_at, i.last_checkin_at)
const signalHideKey = (i: WorkItem) => hideKey(`signal:${i.id}`, signalUpdatedAt(i))

function Request({ r }: { r: CoordinationRequest }) {
  const st = stage(r)
  const first = r.targets[0]?.reasons[0]
  return (
    <details className="request">
      <summary>
        <Pill tone={st.tone} icon={st.icon}>
          {st.label}
        </Pill>
        <span className="request-main">
          <strong>{r.proposal.title}</strong>
          <span className="muted small">
            <Icon name="person" /> {agentLabel(r.requester.agent)} <Icon name="arrow-right" /> {r.targets.map((t) => agentLabel(t.lead.agent)).join(', ')}
          </span>
        </span>
        {first && (
          <span className="request-reason small">
            <PathPair a={first.your_path} b={first.their_path} />
          </span>
        )}
        <span className="muted small request-time">
          <Ago at={r.updated_at} />
        </span>
        <Icon name="chevron-down" className="request-chevron" />
      </summary>
      <div className="request-body">
        <div className="request-proposal">
          <div className="small muted">
            Requested by {agentLabel(r.requester.agent)} · {r.requester.login} · <code>{r.id}</code>
          </div>
          <div>
            <span className="label">Proposed scope</span> {r.proposal.scope.length ? <Paths items={r.proposal.scope} max={12} /> : <em className="muted">Scope not declared</em>}
          </div>
          {r.proposal.work_id && (
            <div>
              <WorkLink id={r.proposal.work_id}>Open the requesting work</WorkLink>
            </div>
          )}
        </div>
        {(!r.acknowledgements_complete || !r.responses_complete) && (
          <Notice title="Some receipt or reply history is omitted">The request status above is authoritative; the history shown below is incomplete.</Notice>
        )}
        <div className="targets">
          {r.targets.map((t) => {
            const ack = currentAck(r, t)
            const resp = currentResponse(r, t)
            const received = ack?.acknowledged_at ?? resp?.created_at
            return (
              <section key={t.work_id} className="target">
                <div className="target-head">
                  <Avatar name={t.lead.agent} size={28} />
                  <span>
                    <span className="label">Target lead</span>
                    <strong title={t.lead.agent}>{agentLabel(t.lead.agent)}</strong> <span className="muted small">{t.lead.login}</span>
                  </span>
                  <WorkLink id={t.work_id}>Open work</WorkLink>
                </div>
                <div className="pills">
                  <Pill tone={received !== undefined ? 'ok' : 'warn'} icon={received !== undefined ? 'envelope-open' : 'envelope'} tip="Receipt means the target agent’s session has seen this request. Viewing this page never counts as receipt.">
                    {received !== undefined ? `Received ${localDate(received)}` : r.acknowledgements_complete ? 'Awaiting receipt' : 'Receipt history incomplete'}
                  </Pill>
                  <Pill tone={resp ? 'brand' : 'warn'} icon={resp ? 'reply' : 'hourglass'}>
                    {resp ? RESPONSE_LABEL[resp.kind] : r.responses_complete ? 'Awaiting reply' : 'Reply history incomplete'}
                  </Pill>
                </div>
                <ul className="reasons">
                  {t.reasons.map((x, i) => (
                    <li key={i}>
                      <PathPair a={x.your_path} b={x.their_path} joiner={<span className="muted small">overlaps</span>} />
                    </li>
                  ))}
                </ul>
                {!t.reasons_complete && <p className="small warn-text"><Icon name="three-dots" /> More overlapping paths were omitted; check the full declared scopes.</p>}
                {resp && (
                  <blockquote className="response">
                    <strong>
                      {agentLabel(resp.responder.agent)} · {resp.responder.login}
                    </strong>
                    <p>{resp.note ?? 'No note provided.'}</p>
                    <Ago at={resp.created_at} />
                  </blockquote>
                )}
              </section>
            )
          })}
        </div>
      </div>
    </details>
  )
}

function RequestList({ requests, search, complete }: { requests: CoordinationRequest[]; search: string; complete: boolean }) {
  const now = useNow() / 1000
  const { rows, count, show, setShow, isHidden } = useHideable(requests, requestHideKey)
  return (
    <>
      {rows.length === 0 ? (
        <Empty icon="chat-left" title={search ? 'No request matches your search' : count > 0 ? 'Every request here is hidden' : complete ? 'No directed requests' : 'No request on this page'}>
          {!search && count === 0 && (complete ? 'No agent has asked another lead to coordinate in this snapshot.' : 'More requests remain in this fixed snapshot.')}
        </Empty>
      ) : (
        <div className="request-list">
          {rows.map((r) => (
            <div key={r.id} className={`hideable${isHidden(r) ? ' is-hidden' : ''}`}>
              <Request r={r} />
              {(isHidden(r) || isStale(r.updated_at, now)) && <HideButton k={requestHideKey(r)} hidden={isHidden(r)} what={`the request “${r.proposal.title}”`} />}
            </div>
          ))}
        </div>
      )}
      <ShowHidden count={count} show={show} onShow={setShow} />
    </>
  )
}

function RequestQueue({ token, search }: { token: string; search: string }) {
  const [queries, setQueries] = useState<CoordinationQuery[]>([{ limit: 10, activeOnly: true }])
  const [page, setPage] = useState(0)
  const [nonce, setNonce] = useState(0)
  const query = queries[page]!
  const fresh = page === 0 && query.through === undefined
  const key = `${JSON.stringify(query)}|${nonce}`
  const load = usePoll((signal) => fetchCoordination(token, signal, query), fresh ? 15_000 : 0, key)
  const hidden = useHidden()
  const restart = (activeOnly: boolean) => {
    setQueries([{ limit: 10, activeOnly }])
    setPage(0)
    setNonce((n) => n + 1)
  }
  return (
    <Box
      id="box-requests"
      icon="chat-left-dots"
      title="Directed requests"
      explain="When two tasks plan to touch the same files, one agent can ask the other to split the work, join, or hand over. Agents send and answer these themselves; this page only shows them."
      count={load.kind === 'ready' ? load.value.requests.filter((r) => !hidden.has(requestHideKey(r))).length : undefined}
      actions={
        <>
          <div className="form-check form-switch mb-0">
            <input id="show-closed" className="form-check-input" type="checkbox" role="switch" checked={!query.activeOnly} onChange={(e) => restart(!e.target.checked)} />
            <label className="form-check-label small" htmlFor="show-closed">
              Include resolved & withdrawn
            </label>
          </div>
          <Tip text="Return to a fresh first page">
            <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => restart(query.activeOnly ?? true)} aria-label="Refresh requests">
              <Icon name="arrow-clockwise" />
            </button>
          </Tip>
        </>
      }
    >
      {load.kind === 'loading' && <Skeleton rows={3} />}
      {load.kind === 'error' && (
        <Notice tone="bad" title="Could not load directed requests" action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>
          {load.message}
        </Notice>
      )}
      {load.kind === 'ready' && (
        <>
          <p className="small muted mb-2">
            {fresh ? 'Refreshes every 15 seconds.' : `Fixed snapshot through event ${load.value.through}.`} Opening this page does not acknowledge a request.
          </p>
          {load.refreshError && <Notice title="Could not refresh requests">{load.refreshError}</Notice>}
          {load.value.retention_gap && <Notice title="Some request history is no longer retained" />}
          <RequestList requests={load.value.requests.filter((r) => requestMatches(r, search))} search={search} complete={load.value.complete} />
          <Pager
            newerLabel="Previous"
            olderLabel="Next"
            status={`Page ${page + 1}`}
            onNewer={page > 0 ? () => setPage((p) => p - 1) : undefined}
            onOlder={load.value.complete ? undefined : () => {
              const b = load.value
              const next: CoordinationQuery = { after: b.next_after, through: b.through, limit: query.limit, activeOnly: query.activeOnly }
              setQueries((all) => (fresh ? [{ through: b.through, limit: query.limit, activeOnly: query.activeOnly }, next] : [...all.slice(0, page + 1), next]))
              setPage((p) => p + 1)
            }}
          />
        </>
      )}
    </Box>
  )
}

function OverlapRow({ o }: { o: WorkOverlapPair }) {
  const side = (item: WorkItem) => {
    const l = lead(item)
    return (
      <div className="overlap-side">
        <div>
          <Avatar name={l.agent} size={24} /> <strong title={l.agent}>{agentLabel(l.agent)}</strong> <span className="muted small">{l.login}</span>
        </div>
        <WorkLink id={item.id}>{item.title}</WorkLink>
        <div className="small">
          <span className="label">Scope</span> <Paths items={item.scope} max={4} />
        </div>
      </div>
    )
  }
  return (
    <li className="overlap">
      <div className="overlap-why">
        {o.reasons.map((x, i) => (
          <div key={i}>
            <PathPair a={x.left_path} b={x.right_path} />
          </div>
        ))}
        {!o.reasons_complete && <div className="small warn-text">More path pairs omitted — compare full scopes.</div>}
      </div>
      <div className="overlap-sides">
        {side(o.left)}
        <Icon name="arrow-left-right" className="overlap-arrow" />
        {side(o.right)}
      </div>
    </li>
  )
}

export function Coordination({ token, overview, search }: { token: string; overview: WorkOverview; search: string }) {
  const overlaps = overview.overlaps.filter((o) => workMatchesSearch(o.left, search) || workMatchesSearch(o.right, search) || o.reasons.some((r) => `${r.left_path} ${r.right_path}`.toLowerCase().includes(search.trim().toLowerCase())))
  const signals = useHideable(overview.current.filter((i) => workMatchesSearch(i, search) && (i.claim_status !== 'active' || i.scope.length === 0)), signalHideKey)
  const attention = signals.rows
  const now = useNow() / 1000
  const decisions = overview.recent.filter((o) => o.kind === 'decision' && workMatchesSearch(o.item, search))
  return (
    <div className="stack">
      <RequestQueue token={token} search={search} />
      <Box
        id="box-overlaps"
        icon="sign-intersection-y"
        tone={overlaps.length ? 'bad' : 'ok'}
        title="Detected scope overlaps"
        count={overlaps.length}
        explain="The hub compares the files each task said it would touch. An overlap means talk first. It doesn’t mean the code conflicts, and no request is sent."
      >
        {!overview.overlaps_complete && <Notice title="Overlap list is incomplete">The current-work limit was reached, so more overlaps may exist. An empty list is not an all-clear.</Notice>}
        {overlaps.length === 0 ? (
          <Empty icon="check2-circle" title={overview.overlaps_complete ? 'No declared scopes overlap' : 'No overlaps in this partial snapshot'} />
        ) : (
          <ul className="overlap-list">
            {overlaps.map((o) => (
              <OverlapRow key={`${o.left.id}-${o.right.id}`} o={o} />
            ))}
          </ul>
        )}
      </Box>
      <div className="grid-2">
        <Box icon="exclamation-triangle" tone={attention.some((i) => !signals.isHidden(i)) ? 'warn' : 'muted'} title="Other attention signals" count={attention.length - (signals.show ? signals.count : 0)} explain="Tasks whose agent hasn’t checked in lately, or that never said which files they’d touch. Overlap checks can’t see these. You can hide an item once it’s a day old.">
          {attention.length === 0 ? (
            <Empty icon="check2" title="Nothing else needs attention" />
          ) : (
            <ul className="plain-list">
              {attention.map((i) => (
                <li key={i.id} className={signals.isHidden(i) ? 'is-hidden' : undefined}>
                  <WorkLink id={i.id}>{i.title}</WorkLink>
                  <span className="muted small"> · {agentLabel(lead(i).agent)}</span>
                  <div className="pills">
                    {i.claim_status !== 'active' && <ClaimPill item={i} />}
                    {i.scope.length === 0 && <Pill tone="muted" icon="question-circle">Scope unknown</Pill>}
                    {(signals.isHidden(i) || isStale(signalUpdatedAt(i), now)) && <HideButton k={signalHideKey(i)} hidden={signals.isHidden(i)} what={`“${i.title}”`} />}
                  </div>
                </li>
              ))}
            </ul>
          )}
          <ShowHidden count={signals.count} show={signals.show} onShow={signals.setShow} />
        </Box>
        <Box icon="signpost-split" tone="muted" title="Recent decisions" count={decisions.length} explain="Decisions agents reported in the same window, for example agreeing a scope split.">
          {decisions.length === 0 ? (
            <Empty icon="signpost" title="No reported decisions in this window" />
          ) : (
            <ul className="plain-list">
              {decisions.map((o) => (
                <li key={o.seq}>
                  <WorkLink id={o.item.id}>{o.item.title}</WorkLink>
                  <div className="small">{o.item.current?.summary ?? 'No decision summary retained'}</div>
                  <span className="small muted">
                    <Ago at={o.at} />
                  </span>
                </li>
              ))}
            </ul>
          )}
        </Box>
      </div>
    </div>
  )
}

/* ---------- Recently finished ---------- */

function Finished({ overview, search }: { overview: WorkOverview; search: string }) {
  const all = overview.recent.filter((o) => o.kind === 'completion')
  const visible = all.filter((o: WorkOutcome) => workMatchesSearch(o.item, search))
  return (
    <div className="stack">
      <div className="window">
        <Icon name="calendar-range" />
        <span>
          Completions reported in the{' '}
          <Tip text={`${localDate(overview.recent_since)} to ${localDate(overview.until)}`} focusable>
            <strong>last {spanText(overview.until - overview.recent_since)}</strong>
          </Tip>
        </span>
        <Explain text="Finished means the agent said so. It may not be verified, reviewed or merged yet." />
      </div>
      {!overview.recent_complete && <Notice title="This window is incomplete">{overview.history_retention_gap ? 'Some outcomes in this window were compacted from retained history.' : 'The overview limit was reached; more completions exist.'}</Notice>}
      {visible.length === 0 ? (
        <Empty icon="flag" title={all.length ? 'No completion matches your search' : 'Nothing reported finished in this window'} />
      ) : (
        <div className="finished-grid">
          {visible.map((o) => (
            <WorkCard key={o.seq} item={o.item} outcomeAt={o.at} />
          ))}
        </div>
      )}
    </div>
  )
}

/* ---------- Change history ---------- */

function History({ token }: { token: string }) {
  const [queries, setQueries] = useState<BriefingQuery[]>([{ limit: 10 }])
  const [page, setPage] = useState(0)
  const [view, setView] = useState<'changes' | 'current'>('changes')
  const [search, setSearch] = useState('')
  const [filter, setFilter] = useState<'all' | WorkState>('all')
  const query = queries[page]!
  const load = usePoll((signal) => fetchBriefing(token, signal, query), 0, JSON.stringify(query))
  if (load.kind === 'loading') return <Skeleton rows={6} />
  if (load.kind === 'error')
    return (
      <Notice tone="bad" title="Could not load the change history" action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>
        {load.message}
      </Notice>
    )
  const b: StandupBriefing = load.value
  const items = b.items.filter((i) => workMatchesSearch(i, search))
  const events = b.events.filter((e) => (filter === 'all' || e.item.state === filter) && workMatchesSearch(e.item, search))
  const canNext = hasMoreBriefing(b, view)
  return (
    <div className="stack">
      <div className="statusline">
        <span>
          <Icon name="eye" /> Read only · viewing this page does not mark anything as read
        </span>
      </div>
      {b.retention_gap && <Notice title="Some history is no longer retained">The current work snapshot is complete, but this change history has a gap.</Notice>}
      <div className="toolbar">
        <div className="btn-group" role="group" aria-label="History view">
          <button type="button" className={`btn btn-sm ${view === 'changes' ? 'btn-secondary' : 'btn-outline-secondary'}`} onClick={() => setView('changes')} aria-pressed={view === 'changes'}>
            <Icon name="list-ul" /> Recent changes
          </button>
          <button type="button" className={`btn btn-sm ${view === 'current' ? 'btn-secondary' : 'btn-outline-secondary'}`} onClick={() => setView('current')} aria-pressed={view === 'current'}>
            <Icon name="kanban" /> Work on this page
          </button>
        </div>
        <Search value={search} onChange={setSearch} placeholder="Search this page" />
        {view === 'changes' && (
          <select className="form-select form-select-sm w-auto" aria-label="Filter by status" value={filter} onChange={(e) => setFilter(e.target.value as 'all' | WorkState)}>
            <option value="all">All statuses</option>
            {(Object.keys(STATE) as WorkState[]).map((s) => (
              <option key={s} value={s}>
                {STATE[s].label}
              </option>
            ))}
          </select>
        )}
      </div>
      <div className="small muted" aria-live="polite">
        {view === 'current' ? `${items.length} of ${b.items.length} work items on this page` : `${events.length} of ${b.events.length} changes on this page`} · counts describe this page only
      </div>
      {view === 'current' ? (
        <Board items={items} showFinished={b.items.some((i) => i.state === 'finished')} />
      ) : events.length === 0 ? (
        <Empty icon="list" title="No changes on this page" />
      ) : (
        <ol className="change-feed box">
          {events.map((e) => {
            const l = lead(e.item)
            return (
              <li key={e.seq} className="change-row">
                <span className={`change-mark is-${e.kind}`} aria-hidden="true"><Icon name={EVENT_ICON[e.kind] ?? 'dot'} /></span>
                <div className="change-main">
                  <WorkLink id={e.work_id}>{e.item.title}</WorkLink>
                  <div className="change-meta">
                    <span>{EVENT_LABEL[e.kind] ?? e.kind}</span>
                    <span className="change-who"><Avatar name={l.agent} size={18} /> <span title={l.agent}>{agentLabel(l.agent)}</span> <span className="muted">{l.login}</span></span>
                    <StatePill state={e.item.state} />
                  </div>
                  {e.item.current && e.kind === 'work_checkpoint' && <p className="change-quote">“{e.item.current.summary}”</p>}
                </div>
                <span className="change-when"><Ago at={e.at} /></span>
              </li>
            )
          })}
        </ol>
      )}
      <Pager
        newerLabel="Previous"
        olderLabel="Next"
        status={`Page ${page + 1}`}
        onNewer={page > 0 ? () => setPage((p) => p - 1) : undefined}
        onOlder={!canNext ? undefined : () => {
          const next: BriefingQuery = { after: b.next_after, ...(b.next_work_after ? { workAfter: b.next_work_after } : {}), workDone: b.next_work_done, through: b.through, limit: query.limit }
          setQueries((all) => [...all.slice(0, page + 1), next])
          setPage((p) => p + 1)
        }}
      />
    </div>
  )
}

/* ---------- Detail ---------- */

function Ref({ r, item }: { r: WorkSourceRef; item: WorkItem }) {
  if (r.kind === 'work') return <WorkLink id={r.work_id} />
  if (r.kind === 'repository_path') return <Path>{`${r.path}${r.line === null ? '' : `:${r.line}`}`}</Path>
  if (r.kind === 'captured_event') {
    return <span title="Caller-declared metadata reference; any local event body is unavailable here and the Work view does not verify conversation content.">Declared captured event <code>{r.session_id} / {r.event_id}</code></span>
  }
  if (r.kind === 'prior_work_version') {
    return <span title="The exact Work version identity was checked when this checkpoint was recorded; its older snapshot may no longer be retained.">Supersedes version <code>{r.event_seq}</code> of <WorkLink id={r.work_id} /></span>
  }
  const href = safeCommitHref(item.source, r.oid)
  const text = <code>{r.oid.slice(0, 12)}{r.path ? ` · ${r.path}` : ''}</code>
  return href ? <a href={href} target="_blank" rel="noreferrer noopener">{text} <Icon name="box-arrow-up-right" /></a> : text
}

function Report({ c, item, title }: { c: WorkCheckpoint; item: WorkItem; title: string }) {
  return (
    <div className="report">
      <div className="report-head">
        <Pill tone="muted" icon="chat-square-text" tip="Written by the agent. A report does not prove execution, review, acceptance or landing.">
          Agent report
        </Pill>
        <strong>{title}</strong> <span className="muted small">· {c.kind}</span>
      </div>
      <p>{c.summary}</p>
      {c.next_action && (
        <p>
          <span className="label">Next</span> {c.next_action}
        </p>
      )}
      {c.source_refs.length > 0 && (
        <div className="refs">
          <Icon name="link-45deg" /> {c.source_refs.map((r, i) => <Ref key={i} r={r} item={item} />)}
        </div>
      )}
      <div className="small muted">
        Reported by {agentLabel(c.agent)} · {localDate(c.at)}
      </div>
    </div>
  )
}

function Detail({ id, token }: { id: string; token: string }) {
  const load = usePoll((signal) => fetchStandupWork(id, token, signal), 0, id)
  return (
    <div className="page">
      {load.kind === 'loading' && <Skeleton rows={6} />}
      {load.kind === 'error' && (
        <Notice tone="bad" title={load.status === 404 ? 'This work item was not found' : 'Could not load this work item'} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>
          {load.message}
        </Notice>
      )}
      {load.kind === 'ready' &&
        (() => {
          const item = load.value
          const l = lead(item)
          return (
            <div className="stack">
              <header className="detail-head">
                <h1 className="page-question">{item.title}</h1>
                <div className="pills">
                  <StatePill state={item.state} />
                  <ClaimPill item={item} />
                  <Source item={item} />
                </div>
                <a className="btn btn-sm btn-outline-secondary" href={`#change-record/${encodeURIComponent(item.id)}`}>
                  <Icon name="clipboard2-check" /> Change record
                </a>
              </header>
              {item.state !== 'finished' && <LaneBar item={item} fresh={false} />}
              {item.state !== 'finished' && item.claim_status !== 'active' && (
                <Notice title={item.claim_status === 'stale' ? 'No recent check-in' : 'Check-in unknown'}>This task is still assigned. Coordinate with the lead before taking it over.</Notice>
              )}
              <div className="action-grid">
                <div>
                  <span className="label">
                    <Icon name="sign-stop" /> Reported blocker
                  </span>
                  {item.blocker?.summary ?? <span className="muted">None reported</span>}
                </div>
                <div>
                  <span className="label">
                    <Icon name="arrow-right-circle" /> Next action
                  </span>
                  {item.next_action ?? <span className="muted">None reported</span>}
                </div>
                <div>
                  <span className="label">
                    <Icon name="clock" /> Last check-in
                  </span>
                  <Ago at={item.last_checkin_at} />
                </div>
              </div>
              <div className="grid-2">
                <Box icon="people" title="People" count={item.participants.length} explain="The lead owns the task. Helpers and reviewers joined it.">
                  <ul className="plain-list">
                    {item.participants.map((p) => (
                      <li key={`${p.token}-${p.session}`} className="d-flex align-items-center gap-2">
                        <Avatar name={p.agent} size={28} />
                        <span>
                          <strong title={p.agent}>{agentLabel(p.agent)}</strong> <span className="muted small">{p.login}</span>
                        </span>
                        <Pill tone={p.role === 'lead' ? 'brand' : 'muted'} icon={p.role === 'lead' ? 'star' : p.role === 'reviewer' ? 'eye' : 'person'}>
                          {p.role}
                        </Pill>
                        {p.role === 'lead' && p.agent === l.agent && <span className="small muted">current lead</span>}
                      </li>
                    ))}
                  </ul>
                </Box>
                <Box icon="bounding-box" title="Declared scope" count={item.scope.length} explain="The paths this task says it will touch. The hub compares scopes across tasks to find overlaps.">
                  {item.scope.length ? <Paths items={item.scope} max={40} /> : <Empty icon="question-circle" title="No scope declared">Overlap checks cannot cover this task.</Empty>}
                </Box>
              </div>
              <Box icon="chat-square-text" title="Agent reports" explain="The latest checkpoint and any open blocker, as written by the agent.">
                {item.current ? <Report c={item.current} item={item} title="Latest checkpoint" /> : <Empty icon="chat-square" title="No checkpoint yet" />}
                {item.blocker && item.blocker !== item.current && <Report c={item.blocker} item={item} title="Open blocker" />}
              </Box>
              {onFrontDoor() && localCredential() && <IntentReviewPanel workId={item.id} />}
              <details className="disclosure box">
                <summary>
                  <Icon name="code-square" /> Technical details
                </summary>
                <dl className="tech">
                  <dt>Work ID</dt>
                  <dd><code>{item.id}</code></dd>
                  <dt>Branch</dt>
                  <dd>{item.branch ? <code>{item.branch}</code> : '—'}</dd>
                  <dt>Worktree</dt>
                  <dd>{item.worktree ? <code>{item.worktree}</code> : '—'}</dd>
                  <dt>Generation</dt>
                  <dd>{item.generation}</dd>
                  <dt>Lead session</dt>
                  <dd><code>{item.lead_session}</code></dd>
                  <dt>Depends on</dt>
                  <dd>{item.depends_on.length ? item.depends_on.map((d) => <WorkLink key={d} id={d} />) : '—'}</dd>
                  <dt>Related</dt>
                  <dd>{item.related_work.length ? item.related_work.map((d) => <WorkLink key={d} id={d} />) : '—'}</dd>
                  <dt>Created</dt>
                  <dd>{localDate(item.created_at)}</dd>
                  <dt>Updated</dt>
                  <dd>{localDate(item.updated_at)}</dd>
                </dl>
              </details>
            </div>
          )
        })()}
    </div>
  )
}

/* ---------- Pages ---------- */

type Tab = 'working' | 'finished' | 'coordination' | 'history'
const TABS: readonly Tab[] = ['working', 'finished', 'coordination', 'history']
/** `#work/<tab>` (work ids are `wrk_…`, so the two never collide). Working and coordination now live on the Dashboard and in the Inbox. */
export function workTabFromHash(hash = window.location.hash): Tab {
  const t = hashPath(hash).slice(1).split('/')[1] as Tab
  return TABS.includes(t) ? t : 'working'
}
export const workTabHref = (t: Tab) => (t === 'working' ? '#work' : `#work/${t}`)

function useWorkRoute() {
  const [route, setRoute] = useState(() => ({ id: workIdFromHash(window.location.hash), tab: workTabFromHash() }))
  useEffect(() => {
    const on = () => setRoute({ id: workIdFromHash(window.location.hash), tab: workTabFromHash() })
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

/** History › Finished and Changes, and the page of one task. */
export function WorkPage({ token, overview }: { token: string; overview: Poll<WorkOverview> }) {
  const { id, tab } = useWorkRoute()
  const [search, setSearch] = useState('')
  if (id) return <Detail id={id} token={token} />
  const ov = overview.kind === 'ready' ? overview.value : null
  const finished = ov?.recent.filter((o) => o.kind === 'completion').length ?? 0
  const history = tab === 'history'
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">{history ? 'What changed in Work, and when?' : 'What did agents report finished?'}</h1>
        {history ? (
          <>
            <p className="answer-text">Every change to Work, newest first.</p>
            <p className="answer-fact">Starts, joins, takeovers and checkpoints. Reading never marks anything as seen.</p>
          </>
        ) : ov ? (
          <>
            <p className="answer-text">
              {finished === 0 ? 'Nothing was reported finished recently.' : <><b>{plural(finished, 'task')}</b> reported finished since {new Date(ov.recent_since * 1000).toLocaleDateString([], { weekday: 'long' })}.</>}
            </p>
            <p className="answer-fact">Newest first, in each agent’s own words.</p>
          </>
        ) : overview.kind === 'error' ? null : (
          <Skeleton rows={1} />
        )}
      </div>
      <SectionTabs />
      {!history && overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {!history && !ov && overview.kind === 'loading' && <Skeleton rows={6} />}
      {!history && ov && (
        <>
          <Search value={search} onChange={setSearch} placeholder="Search agents, tasks, or paths" />
          <Finished overview={ov} search={search} />
        </>
      )}
      {history && <History token={token} />}
    </div>
  )
}

const pending = (r: CoordinationRequest) => r.status !== 'resolved' && r.status !== 'withdrawn'

/** Inbox: every declared-scope overlap and directed request, the conversations agents have to have. */
export function InboxPage({ token, overview }: { token: string; overview: Poll<WorkOverview> }) {
  const [search, setSearch] = useState('')
  const ov = overview.kind === 'ready' ? overview.value : null
  const requests = usePoll((signal) => fetchCoordination(token, signal, { limit: 10, activeOnly: true }), 15_000, `inbox|${token}`)
  const hidden = useHidden()
  const open = requests.kind === 'ready' ? requests.value.requests.filter((r) => pending(r) && !hidden.has(requestHideKey(r))) : []
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Which overlaps need a conversation?</h1>
        {ov ? (
          <p className="answer-text">
            {ov.overlaps.length === 0 && open.length === 0 ? (
              ov.overlaps_complete ? 'No two tasks plan to touch the same files, and no request is waiting.' : 'No overlap in this partial snapshot. An empty list is not an all-clear.'
            ) : (
              <>
                {ov.overlaps.length > 0 && <><b>{plural(ov.overlaps.length, 'overlap')}</b> {ov.overlaps.length === 1 ? 'needs' : 'need'} a conversation. </>}
                {open.length > 0 && <><b>{plural(open.length, 'request')}</b> {open.length === 1 ? 'is' : 'are'} waiting on another lead.</>}
              </>
            )}
          </p>
        ) : overview.kind === 'error' ? null : (
          <Skeleton rows={1} />
        )}
      </div>
      {overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {!ov && overview.kind === 'loading' && <Skeleton rows={6} />}
      {ov && (
        <>
          <Search value={search} onChange={setSearch} placeholder="Search agents, tasks, or paths" />
          <Coordination token={token} overview={ov} search={search} />
        </>
      )}
    </div>
  )
}
