import { useEffect, useState, type ReactNode } from 'react'
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
import { Ago, Avatar, Box, Empty, Explain, Icon, Notice, Path, Paths, Pill, Skeleton, Tip, localDate, type Tone } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'

const STATE: Record<WorkState, { label: string; tone: Tone; icon: string; tip: string }> = {
  active: { label: 'In progress', tone: 'brand', icon: 'play-circle', tip: 'The lead agent reports it is actively working on this.' },
  blocked: { label: 'Blocked', tone: 'bad', icon: 'sign-stop', tip: 'The lead agent reported a blocker and is waiting on it.' },
  handover: { label: 'Handover', tone: 'brand', icon: 'arrow-left-right', tip: 'The lead reported this is ready for another agent to take over.' },
  paused: { label: 'Paused', tone: 'warn', icon: 'pause-circle', tip: 'Still assigned, but the lead reported it has paused.' },
  finished: { label: 'Reported finished', tone: 'ok', icon: 'check2-circle', tip: 'The agent reported this finished. That does not confirm review, verification or merge.' },
}
const LANES: WorkState[] = ['active', 'blocked', 'handover', 'paused']

const EVENT_LABEL: Record<string, string> = { work_started: 'Started work', work_joined: 'Joined work', work_takeover: 'Took over the claim', work_checkpoint: 'Reported a checkpoint' }
const RESPONSE_LABEL: Record<CoordinationResponseKind, string> = { propose_split: 'Proposed a scope split', join: 'Offered to join', handover: 'Offered a handover', decline: 'Declined' }

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

function lead(item: WorkItem): { agent: string; login: string } {
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

function ClaimPill({ item }: { item: WorkItem }) {
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
          <strong>{l.agent}</strong> <span className="muted small">{l.login}</span>
        </span>
      </div>
      <h3 className="work-title">
        <WorkLink id={item.id}>{item.title}</WorkLink>
      </h3>
      <div className="pills">
        <StatePill state={item.state} />
        <ClaimPill item={item} />
      </div>
      <Action item={item} />
      {helpers.length > 0 && (
        <div className="small muted">
          <Icon name="people" /> {helpers.slice(0, 2).map((p) => `${p.agent} (${p.role})`).join(', ')}
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
                <span>{STATE[s].label}</span>
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

function Updated({ fetchedAt, extra, error }: { fetchedAt: number; extra?: ReactNode; error: string | null }) {
  const at = new Date(fetchedAt)
  return (
    <>
      <div className="statusline">
        <span>
          <Icon name="eye" /> Read only
        </span>
        <span>
          <Icon name="arrow-clockwise" /> Updated <time dateTime={at.toISOString()}>{at.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', second: '2-digit' })}</time>
        </span>
        {extra}
      </div>
      {error && (
        <Notice title="Could not refresh">
          Showing data loaded at {at.toLocaleTimeString()}. {error}
        </Notice>
      )}
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
            <Icon name="person" /> {r.requester.agent} <Icon name="arrow-right" /> {r.targets.map((t) => t.lead.agent).join(', ')}
          </span>
        </span>
        {first && (
          <span className="request-reason small">
            <Path>{first.your_path}</Path> <Icon name="intersect" className="muted" /> <Path>{first.their_path}</Path>
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
            Requested by {r.requester.agent} · {r.requester.login} · <code>{r.id}</code>
          </div>
          <div>
            <span className="label">Proposed scope</span> {r.proposal.scope.length ? <Paths items={r.proposal.scope} max={12} /> : <em className="muted">Scope not declared</em>}
          </div>
          {r.proposal.work_id && (
            <div>
              <WorkLink id={r.proposal.work_id}>Open the requesting work <Icon name="arrow-right" /></WorkLink>
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
                    <strong>{t.lead.agent}</strong> <span className="muted small">{t.lead.login}</span>
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
                      <Path>{x.your_path}</Path> <span className="muted small">overlaps</span> <Path>{x.their_path}</Path>
                    </li>
                  ))}
                </ul>
                {!t.reasons_complete && <p className="small warn-text"><Icon name="three-dots" /> More overlapping paths were omitted; check the full declared scopes.</p>}
                {resp && (
                  <blockquote className="response">
                    <strong>
                      {resp.responder.agent} · {resp.responder.login}
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

function RequestQueue({ token, search }: { token: string; search: string }) {
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
      explain="When two tasks declare overlapping scope, an agent can send the other lead a request: split the scope, join, or hand over. Agents receive and answer these through MCP or HTTP; this page only reads them."
      count={load.kind === 'ready' ? load.value.requests.length : undefined}
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
          {(() => {
            const visible = load.value.requests.filter((r) => requestMatches(r, search))
            if (visible.length === 0)
              return (
                <Empty icon="chat-left" title={search ? 'No request matches your search' : load.value.complete ? 'No directed requests' : 'No request on this page'}>
                  {!search && (load.value.complete ? 'No agent has asked another lead to coordinate in this snapshot.' : 'More requests remain in this fixed snapshot.')}
                </Empty>
              )
            return (
              <div className="request-list">
                {visible.map((r) => (
                  <Request key={r.id} r={r} />
                ))}
              </div>
            )
          })()}
          <Pager
            page={page}
            canNext={!load.value.complete}
            onPrev={() => setPage((p) => p - 1)}
            onNext={() => {
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

function Pager({ page, canNext, onPrev, onNext }: { page: number; canNext: boolean; onPrev: () => void; onNext: () => void }) {
  if (page === 0 && !canNext) return null
  return (
    <nav className="pager" aria-label="Pages">
      <button type="button" className="btn btn-sm btn-outline-secondary" disabled={page === 0} onClick={onPrev}>
        <Icon name="chevron-left" /> Previous
      </button>
      <span className="small muted">Page {page + 1}</span>
      <button type="button" className="btn btn-sm btn-outline-secondary" disabled={!canNext} onClick={onNext}>
        Next <Icon name="chevron-right" />
      </button>
    </nav>
  )
}

function OverlapRow({ o }: { o: WorkOverlapPair }) {
  const side = (item: WorkItem) => {
    const l = lead(item)
    return (
      <div className="overlap-side">
        <div>
          <Avatar name={l.agent} size={24} /> <strong>{l.agent}</strong> <span className="muted small">{l.login}</span>
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
            <Path>{x.left_path}</Path> <Icon name="intersect" className="muted" /> <Path>{x.right_path}</Path>
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

function Coordination({ token, overview, search }: { token: string; overview: WorkOverview; search: string }) {
  const overlaps = overview.overlaps.filter((o) => workMatchesSearch(o.left, search) || workMatchesSearch(o.right, search) || o.reasons.some((r) => `${r.left_path} ${r.right_path}`.toLowerCase().includes(search.trim().toLowerCase())))
  const attention = overview.current.filter((i) => workMatchesSearch(i, search) && (i.claim_status !== 'active' || i.scope.length === 0))
  const decisions = overview.recent.filter((o) => o.kind === 'decision' && workMatchesSearch(o.item, search))
  return (
    <div className="stack">
      <RequestQueue token={token} search={search} />
      <Box
        icon="sign-intersection-y"
        tone={overlaps.length ? 'warn' : 'ok'}
        title="Detected scope overlaps"
        count={overlaps.length}
        explain="The hub compares the paths each task declared. An overlap is a signal to talk, not proof the code conflicts, and detecting it does not send a request."
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
        <Box icon="exclamation-triangle" tone={attention.length ? 'warn' : 'muted'} title="Other attention signals" count={attention.length} explain="Tasks whose lead has not checked in recently, or that declared no scope. These limit how confident any overlap check can be.">
          {attention.length === 0 ? (
            <Empty icon="check2" title="Nothing else needs attention" />
          ) : (
            <ul className="plain-list">
              {attention.map((i) => (
                <li key={i.id}>
                  <WorkLink id={i.id}>{i.title}</WorkLink>
                  <span className="muted small"> · {lead(i).agent}</span>
                  <div className="pills">
                    {i.claim_status !== 'active' && <ClaimPill item={i} />}
                    {i.scope.length === 0 && <Pill tone="muted" icon="question-circle">Scope unknown</Pill>}
                  </div>
                </li>
              ))}
            </ul>
          )}
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
          Showing completions from <strong>{localDate(overview.recent_since)}</strong> to <strong>{localDate(overview.until)}</strong>
        </span>
        <Explain text="Finished means the agent reported it finished. It does not confirm verification, review, or merge." />
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
        <div className="table-responsive box">
          <table className="table table-hover align-middle mb-0 history">
            <thead>
              <tr>
                <th scope="col">Work</th>
                <th scope="col">Change</th>
                <th scope="col">Lead</th>
                <th scope="col">Status</th>
                <th scope="col">When</th>
              </tr>
            </thead>
            <tbody>
              {events.map((e) => (
                <tr key={e.seq}>
                  <td>
                    <WorkLink id={e.work_id}>{e.item.title}</WorkLink>
                  </td>
                  <td>
                    {EVENT_LABEL[e.kind] ?? e.kind}
                    {e.item.current && e.kind === 'work_checkpoint' && <div className="small muted text-truncate history-sub">{e.item.current.summary}</div>}
                  </td>
                  <td>{lead(e.item).agent}</td>
                  <td>
                    <StatePill state={e.item.state} />
                  </td>
                  <td className="text-nowrap">{localDate(e.at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <Pager
        page={page}
        canNext={canNext}
        onPrev={() => setPage((p) => p - 1)}
        onNext={() => {
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
        Reported by {c.agent} · {localDate(c.at)}
      </div>
    </div>
  )
}

function Detail({ id, token }: { id: string; token: string }) {
  const load = usePoll((signal) => fetchStandupWork(id, token, signal), 0, id)
  return (
    <div className="page">
      <a href="#work" className="back">
        <Icon name="arrow-left" /> All work
      </a>
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
                          <strong>{p.agent}</strong> <span className="muted small">{p.login}</span>
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

/* ---------- Page ---------- */

type Tab = 'working' | 'finished' | 'coordination' | 'history'
const TABS: readonly Tab[] = ['working', 'finished', 'coordination', 'history']
/** `#work/<tab>` (work ids are `wrk_…`, so the two never collide); breadcrumbs link to these. */
export function workTabFromHash(hash = window.location.hash): Tab {
  const t = hash.slice(1).split('/')[1] as Tab
  return TABS.includes(t) ? t : 'working'
}
export const workTabHref = (t: Tab) => (t === 'working' ? '#work' : `#work/${t}`)

export function WorkPage({ token, overview }: { token: string; overview: Poll<WorkOverview> }) {
  const [workId, setWorkId] = useState(() => workIdFromHash(window.location.hash))
  const [tab, setTab] = useState<Tab>(workTabFromHash)
  const [search, setSearch] = useState('')
  useEffect(() => {
    const on = () => {
      setWorkId(workIdFromHash(window.location.hash))
      setTab(workTabFromHash())
    }
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  if (workId) return <Detail id={workId} token={token} />

  const ov = overview.kind === 'ready' ? overview.value : null
  const count = (s: WorkState) => ov?.current.filter((i) => i.state === s).length ?? 0
  const finished = ov?.recent.filter((o) => o.kind === 'completion').length ?? 0
  const tabs: { key: Tab; icon: string; label: string; n?: number; tone?: string }[] = [
    { key: 'working', icon: 'kanban', label: 'Working now', ...(ov ? { n: ov.current.length } : {}) },
    { key: 'finished', icon: 'flag', label: 'Recently finished', ...(ov ? { n: finished } : {}) },
    { key: 'coordination', icon: 'sign-intersection-y', label: 'Needs coordination', ...(ov && ov.overlaps.length > 0 ? { n: ov.overlaps.length, tone: 'warn' } : {}) },
    { key: 'history', icon: 'clock-history', label: 'Change history' },
  ]
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Who owns which task?</h1>
        {ov ? (
          <p className="answer-text">
            {ov.current.length === 0 ? (
              'No unfinished work is assigned right now.'
            ) : (
              <>
                {plural(count('active'), 'task')} in progress
                {count('blocked') > 0 && <>, <span className="tone-text-bad">{count('blocked')} blocked</span></>}
                {count('handover') > 0 && <>, {count('handover')} waiting for handover</>}
                {count('paused') > 0 && <>, {count('paused')} paused</>}. {finished > 0 && <>{plural(finished, 'task')} reported finished recently.</>}
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
      {overview.kind === 'ready' && (
        <Updated
          fetchedAt={overview.fetchedAt}
          error={overview.refreshError}
          extra={
            <Tip text="The overview is a bounded, repository-wide snapshot. It refreshes every 15 seconds while visible." focusable>
              <Icon name="camera" /> Snapshot through {localDate(overview.value.until)}
            </Tip>
          }
        />
      )}
      <div className="tabbar" data-tour="work-tabs" role="tablist" aria-label="Work views">
        {tabs.map((t) => (
          <button key={t.key} type="button" role="tab" aria-selected={tab === t.key} className={`tab${tab === t.key ? ' active' : ''}`} onClick={() => { window.location.hash = workTabHref(t.key).slice(1) }}>
            <Icon name={t.icon} /> {t.label}
            {t.n !== undefined && <span className={`tab-count ${t.tone ? `tone-${t.tone}` : ''}`}>{t.n}</span>}
          </button>
        ))}
      </div>
      {tab !== 'history' && <Search value={search} onChange={setSearch} placeholder="Search agents, tasks, or paths" />}
      {tab !== 'history' && !ov && overview.kind === 'loading' && <Skeleton rows={6} />}
      {ov && tab === 'working' && (
        <>
          {!ov.current_complete && <Notice title="This snapshot is incomplete">The overview limit was reached. More work exists outside it.</Notice>}
          <Board items={ov.current.filter((i) => workMatchesSearch(i, search))} />
        </>
      )}
      {ov && tab === 'finished' && <Finished overview={ov} search={search} />}
      {ov && tab === 'coordination' && <Coordination token={token} overview={ov} search={search} />}
      {tab === 'history' && <History token={token} />}
    </div>
  )
}
