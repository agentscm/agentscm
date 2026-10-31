// Inbox: declared-scope overlaps and directed requests between agents (`#inbox`).
//
// API for other packages:
//   InboxPage({ token, overview, handoff })   the Inbox page; `handoff` (handoffOf(whoami)) turns on "Open in" for the viewer's own agents
//   Coordination({ token, overview, search })   requests, overlaps, other signals and decisions
//   stage(r)       a request's stage: label, tone and icon
//   pending(r)     true while a request is neither resolved nor withdrawn
//   currentAck(r, t), currentResponse(r, t)   a target's current receipt and reply
//   RESPONSE_LABEL a reply kind in words ("Proposed a scope split")
import { useState } from 'react'
import {
  fetchCoordination,
  workMatchesSearch,
  type CoordinationQuery,
  type CoordinationRequest,
  type CoordinationResponseKind,
  type CoordinationTarget,
  type WorkItem,
  type WorkOverlapPair,
  type WorkOverview,
} from '../standup.ts'
import { agentLabel, Ago, Avatar, Box, Empty, Icon, Notice, PageHead, Paths, Pill, Skeleton, Tip, localDate, type Tone } from '../ui.tsx'
import { OVERLAP_IS_SIGNAL, READING_CHANGES_NOTHING } from '../copy.ts'
import { usePoll, type Poll } from '../usePoll.ts'
import { useNow } from '../useHub.ts'
import { HideButton, hideKey, isStale, ShowHidden, useHidden, useHideable } from '../dismiss.tsx'
import { Pager } from '../Pager.tsx'
import { ClaimPill, lead, PathPair, plural, Search, WorkLink } from './workParts.tsx'
// nextStep.ts imports this module's helpers: use these only inside render functions (module cycle).
import { overlapSteps, requestStep, who } from '../nextStep.ts'
import { NextStepView } from '../NextStep.tsx'
import type { Handoff } from '../launchLink.ts'
import { Gloss } from '../Gloss.tsx'

/** A reply kind in words. */
export const RESPONSE_LABEL: Record<CoordinationResponseKind, string> = { propose_split: 'Proposed a scope split', join: 'Offered to join', handover: 'Offered a handover', decline: 'Declined' }

/* ---------- Needs coordination ---------- */

/** The target's receipt for this request at its current generation, newest first. */
export function currentAck(r: CoordinationRequest, t: CoordinationTarget) {
  return r.acknowledgements.filter((a) => a.target_work_id === t.work_id && a.generation === t.generation && a.through_seq >= r.created_seq).sort((x, y) => y.seq - x.seq)[0]
}
/** The target's reply at its current generation, newest first. */
export function currentResponse(r: CoordinationRequest, t: CoordinationTarget) {
  return r.responses.filter((x) => x.target_work_id === t.work_id && x.target_generation === t.generation).sort((x, y) => y.seq - x.seq)[0]
}
/** Where a request is: resolved, withdrawn, replied, awaiting replies or receipt, or history incomplete. */
export function stage(r: CoordinationRequest): { label: string; tone: Tone; icon: string } {
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


// Browser-only hide keys for the Inbox's lingering rows (see dismiss.tsx).
const requestHideKey = (r: CoordinationRequest) => hideKey(`request:${r.id}`, r.updated_at)
const signalUpdatedAt = (i: WorkItem) => Math.max(i.updated_at, i.last_checkin_at)
const signalHideKey = (i: WorkItem) => hideKey(`signal:${i.id}`, signalUpdatedAt(i))

function Request({ r, handoff }: { r: CoordinationRequest; handoff?: Handoff }) {
  const st = stage(r)
  const step = requestStep(r)
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
            <Icon name="person" /> {who(r.requester.login, r.requester.agent)} <Icon name="arrow-right" /> {r.targets.map((t) => who(t.lead.login, t.lead.agent)).join(', ')}
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
        {step && <NextStepView step={step} viewer={handoff?.me} repo={handoff?.repo} originUrl={handoff?.originUrl} />}
        {(!r.acknowledgements_complete || !r.responses_complete) && (
          <Notice title="Some receipt or reply history is omitted">The request status above is authoritative; the history shown below is incomplete.</Notice>
        )}
        <div className="targets">
          {r.targets.map((t) => {
            const ack = currentAck(r, t)
            const resp = currentResponse(r, t)
            const received = ack?.acknowledged_at ?? resp?.created_at
            return (
              <section key={t.work_id} className="target target-plain">
                <div className="target-head">
                  <Avatar name={t.lead.agent} size={28} />
                  <span>
                    <span className="label">Target lead</span>
                    <strong title={t.lead.agent}>{who(t.lead.login, t.lead.agent)}</strong>
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
                      {who(resp.responder.login, resp.responder.agent)}
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

function RequestList({ requests, search, complete, handoff }: { requests: CoordinationRequest[]; search: string; complete: boolean; handoff?: Handoff }) {
  const now = useNow() / 1000
  const { rows, count, show, setShow, isHidden } = useHideable(requests, requestHideKey)
  return (
    <>
      {rows.length === 0 ? (
        <Empty icon="chat-left" title={search ? 'No request matches your search' : count > 0 ? 'Every request here is hidden' : complete ? 'No directed requests' : 'No request on this page'}>
          {!search && count === 0 && (complete ? 'Agents send one when they need a file another agent holds; they use agentscm_ask.' : 'More requests remain in this fixed snapshot.')}
        </Empty>
      ) : (
        <div className="request-list">
          {rows.map((r) => (
            <div key={r.id} className={`hideable${isHidden(r) ? ' is-hidden' : ''}`}>
              <Request r={r} handoff={handoff} />
              {(isHidden(r) || isStale(r.updated_at, now)) && <HideButton k={requestHideKey(r)} hidden={isHidden(r)} what={`the request “${r.proposal.title}”`} />}
            </div>
          ))}
        </div>
      )}
      <ShowHidden count={count} show={show} onShow={setShow} />
    </>
  )
}

function RequestQueue({ token, search, handoff }: { token: string; search: string; handoff?: Handoff }) {
  const [queries, setQueries] = useState<CoordinationQuery[]>([{ limit: 10, activeOnly: true }])
  const [page, setPage] = useState(0)
  const [nonce, setNonce] = useState(0)
  const query = queries[page]!
  const fresh = page === 0 && query.through === undefined
  const key = `${JSON.stringify(query)}|${nonce}`
  const load = usePoll((signal) => fetchCoordination(token, signal, query), fresh ? 15_000 : 0, key)
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
      explain="Agents ask each other to split, join or hand over. They answer themselves; each item says what to tell them if it stalls."
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
          {load.value.requests.length > 0 && (
            <p className="small muted mb-2">
              {fresh ? 'Refreshes every 15 seconds.' : `Fixed snapshot through event ${load.value.through}.`}
            </p>
          )}
          {load.refreshError && <Notice title="Could not refresh requests">{load.refreshError}</Notice>}
          {load.value.retention_gap && <Notice title="Some request history is no longer retained" />}
          <RequestList requests={load.value.requests.filter((r) => requestMatches(r, search))} search={search} complete={load.value.complete} handoff={handoff} />
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

function OverlapRow({ o, handoff }: { o: WorkOverlapPair; handoff?: Handoff }) {
  const side = (item: WorkItem) => {
    const l = lead(item)
    return (
      <div className="overlap-side">
        <div>
          <Avatar name={l.agent} size={24} /> <strong title={l.agent}>{who(l.login, l.agent)}</strong>
        </div>
        <WorkLink id={item.id}>{item.title}</WorkLink>
        <div className="small">
          <span className="label">
            <Gloss term="declaredScope">Plans to change:</Gloss>
          </span>{' '}
          <Paths items={item.scope} max={4} />
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
      <NextStepView step={overlapSteps(o)} tour="inbox-talk" viewer={handoff?.me} repo={handoff?.repo} originUrl={handoff?.originUrl} />
    </li>
  )
}

export function Coordination({ token, overview, search, requestsFirst = true, handoff }: { token: string; overview: WorkOverview; search: string; handoff?: Handoff; /** Directed requests above overlaps; false when none is open. */ requestsFirst?: boolean }) {
  const overlaps = overview.overlaps.filter((o) => workMatchesSearch(o.left, search) || workMatchesSearch(o.right, search) || o.reasons.some((r) => `${r.left_path} ${r.right_path}`.toLowerCase().includes(search.trim().toLowerCase())))
  const signals = useHideable(overview.current.filter((i) => workMatchesSearch(i, search) && (i.claim_status !== 'active' || i.scope.length === 0)), signalHideKey)
  const attention = signals.rows
  const now = useNow() / 1000
  const decisions = overview.recent.filter((o) => o.kind === 'decision' && workMatchesSearch(o.item, search))
  const signalCount = attention.length - (signals.show ? signals.count : 0)
  const requests = <RequestQueue key="requests" token={token} search={search} handoff={handoff} />
  const overlapBox = (
    <Box
      key="overlaps"
      id="box-overlaps"
      icon="sign-intersection-y"
      tone={overlaps.length ? 'bad' : 'ok'}
      title="Two tasks plan the same files"
      explain={`The hub compares the files each task said it would touch. ${OVERLAP_IS_SIGNAL} No request is sent.`}
    >
      {!overview.overlaps_complete && <Notice title="Overlap list is incomplete">The current-work limit was reached, so more overlaps may exist. An empty list is not an all-clear.</Notice>}
      {overlaps.length === 0 ? (
        <Empty icon="check2-circle" title={overview.overlaps_complete ? 'No declared scopes overlap' : 'No overlaps in this partial snapshot'}>{overview.overlaps_complete && !search.trim() && 'Overlaps appear when two tasks declare the same files; agents declare them with agentscm_start.'}</Empty>
      ) : (
        <ul className="overlap-list">
          {overlaps.map((o) => (
            <OverlapRow key={`${o.left.id}-${o.right.id}`} o={o} handoff={handoff} />
          ))}
        </ul>
      )}
    </Box>
  )
  return (
    <div className="stack">
      {/* Keyed so reordering never remounts the request poll. */}
      {requestsFirst ? [requests, overlapBox] : [overlapBox, requests]}
      <div className="grid-2">
        <details className="disclosure">
          <summary>Quiet agents and tasks with no stated files · {signalCount}</summary>
          <p className="small muted mt-2">Overlap checks can’t see these. You can hide an item once it’s a day old.</p>
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
        </details>
        <details className="disclosure">
          <summary>Recent decisions · {decisions.length}</summary>
          <p className="small muted mt-2">Decisions agents reported in the same window, for example agreeing a scope split.</p>
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
        </details>
      </div>
    </div>
  )
}

/** True while a request is neither resolved nor withdrawn. */
export const pending = (r: CoordinationRequest) => r.status !== 'resolved' && r.status !== 'withdrawn'

/** Inbox: every declared-scope overlap and directed request, the conversations agents have to have. */
export function InboxPage({ token, overview, handoff }: { token: string; overview: Poll<WorkOverview>; handoff?: Handoff }) {
  const [search, setSearch] = useState('')
  const ov = overview.kind === 'ready' ? overview.value : null
  const requests = usePoll((signal) => fetchCoordination(token, signal, { limit: 10, activeOnly: true }), 15_000, `inbox|${token}`)
  const hidden = useHidden()
  const open = requests.kind === 'ready' ? requests.value.requests.filter((r) => pending(r) && !hidden.has(requestHideKey(r))) : []
  return (
    <div className="page">
      <PageHead
        question="Which overlaps need a conversation?"
        answer={
          ov ? (
            ov.overlaps.length === 0 && open.length === 0 ? (
              ov.overlaps_complete ? 'No two tasks plan to touch the same files, and no request is waiting.' : 'No overlap in this partial snapshot. An empty list is not an all-clear.'
            ) : (
              <>
                {ov.overlaps.length > 0 && <><b>{plural(ov.overlaps.length, 'overlap')}</b> {ov.overlaps.length === 1 ? 'needs' : 'need'} a conversation. </>}
                {open.length > 0 && <><b>{plural(open.length, 'request')}</b> {open.length === 1 ? 'is' : 'are'} waiting on another lead.</>}
              </>
            )
          ) : overview.kind === 'error' ? 'The overlap count could not be loaded.' : 'Checking which tasks overlap…'
        }
        fact={<>{READING_CHANGES_NOTHING} Agents answer <Gloss term="directedRequest">requests</Gloss> themselves.</>}
      />
      {overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {!ov && overview.kind === 'loading' && <Skeleton rows={6} />}
      {ov && (
        <>
          <Search value={search} onChange={setSearch} placeholder="Search agents, tasks, or paths" />
          <Coordination token={token} overview={ov} search={search} requestsFirst={open.length > 0} handoff={handoff} />
        </>
      )}
    </div>
  )
}
