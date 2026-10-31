// Inbox: the conversations agents have to have (`#inbox`), as threads with a timeline.
//
// Left, the conversations grouped by who has to move: Needs you, Waiting on others, Between others, Earlier. Right, the
// selected one: the sentence, the two sides, a timeline of everything the Hub recorded (plans, asks, receipts, replies,
// AgentSCM's notices, and a person's own answer), and, where the person's own agent was asked, a reply bar. A person who is
// signed in on the web answers for their OWN agent there; the Hub records that a person did, never that the agent did. With
// nothing to settle the pane shows a small idle scene. Nothing here merges or approves work.
//
// API for other packages:
//   InboxPage({ token, overview, handoff, session, live })   the Inbox page; `handoff` (handoffOf(whoami)) names the viewer and
//                    turns on "Open in" for their agents; `session` (the web sign-in) turns on answering; `live` (agents and
//                    contested files) turns on "Ask … to hand it over"
//   stage(r), pending(r), currentAck(r, t), currentResponse(r, t), RESPONSE_LABEL   a request's state (see requests.ts)
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { AgentMark } from '../AgentMark.tsx'
import { type BrowserSession } from '../devices.ts'
import type { Contested, LiveAgent } from '../derive.ts'
import { HideButton, hideKey, isStale, ShowHidden, useHidden, useHideable } from '../dismiss.tsx'
import { answerHandoff, askHandoff, fetchMyHandoffs, operationId, OUTCOME_SENTENCE, replyToRequest, sendError, type MyHandoff } from '../inboxApi.ts'
import { IdleScene } from '../InboxIdle.tsx'
import { agentName, askOption, buildThreads, GROUP_TITLE, GROUPS, named, REPLY_CHOICES, type Group, type Row, type Say, type SideView, type Thread } from '../inboxThreads.ts'
import { OVERLAP_IS_SIGNAL, READING_CHANGES_NOTHING } from '../copy.ts'
import type { Handoff } from '../launchLink.ts'
import { NextStepView } from '../NextStep.tsx'
import { clean, DATA_NOTE, overlapSteps, requestStep, type NextStep } from '../nextStep.ts'
import { pending, currentAck, currentResponse, RESPONSE_LABEL, stage } from '../requests.ts'
import { fetchCoordination, type CoordinationRequest, type CoordinationResponseKind, type WorkItem, type WorkOverview } from '../standup.ts'
import { agentLabel, Ago, Icon, Notice, PageHead, Pill, relative, Skeleton } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'
import { useNow } from '../useHub.ts'
import { lead, plural, Search, WorkLink, ClaimPill } from './workParts.tsx'
import { Gloss } from '../Gloss.tsx'

export { currentAck, currentResponse, pending, RESPONSE_LABEL, stage }

const signalHideKey = (i: WorkItem) => hideKey(`signal:${i.id}`, Math.max(i.updated_at, i.last_checkin_at))

/** The last good value of a poll, so a refresh after a send replaces the page's data without a skeleton in between. */
function useLast<T>(poll: Poll<T>): T | null {
  const [last, setLast] = useState<T | null>(null)
  if (poll.kind === 'ready' && poll.value !== last) setLast(poll.value)
  return poll.kind === 'ready' ? poll.value : last
}

const nowSeconds = () => Math.floor(Date.now() / 1000)

/* ---------- the pieces of a thread ---------- */

function Sentence({ say }: { say: Say }) {
  return (
    <>
      {say.map((part, i) => (typeof part === 'string' ? part : <code key={i}>{part.code}</code>))}
    </>
  )
}

function Side({ side, children }: { side: SideView; children?: ReactNode }) {
  return (
    <div className="in-side">
      <span className="in-who">
        <AgentMark agent={side.agent} />
        {side.me ? 'You' : side.login} · {agentName(side.agent)}
      </span>
      <span className={`in-task${side.task ? '' : ' is-none'}`}>{side.task ?? 'Task not shown'}</span>
      <span className="in-files">{side.files.length ? side.files.slice(0, 4).join(', ') + (side.files.length > 4 ? ` +${side.files.length - 4} more` : '') : 'No files declared'}</span>
      {children}
    </div>
  )
}

function TimelineRow({ row }: { row: Row }) {
  return (
    <li className={`in-row is-${row.kind}${row.tone ? ` is-${row.tone}` : ''}`}>
      <span className="in-dot" aria-hidden="true">
        {row.kind === 'agent' ? <AgentMark agent={row.agent ?? ''} /> : row.kind === 'notice' ? <Icon name={row.tone === 'ok' ? 'check2' : 'sign-intersection-y'} /> : row.kind === 'receipt' ? <Icon name="check2" /> : row.kind === 'omitted' ? <Icon name="three-dots" /> : null}
      </span>
      <span className="in-rwho">
        {row.who} <span>{row.did}</span>
      </span>
      <span className="in-rtime">{row.at ? <Ago at={row.at} /> : null}</span>
      {row.text && <span className={`in-what${row.quote ? ' is-quote' : ''}`}>{row.text}</span>}
    </li>
  )
}

/** What a person already sent from this page that the Hub's list may not show yet. */
export interface Local {
  /** `sending` while the Hub has it; `sent` once it answered; `error` if it refused. */
  status: 'sending' | 'sent' | 'error'
  message?: string
  /** A sentence that stays beside the timeline once sent. */
  done?: string
  rows?: Row[]
}

/** A prompt to paste, for a handoff request, in the same words as the other next steps. */
function handoffStep(t: Thread): NextStep | null {
  const h = t.handoff
  if (!h || h.state !== 'pending') return null
  const path = clean(h.path)
  return h.holder.mine
    ? { human: `Tell your ${agentName(h.holder.agent)} what to do with ${path}`, decides: [h.holder.login], prompts: [{ for: `your ${agentName(h.holder.agent)}`, login: h.holder.login, agent: h.holder.agent, text: `Another agent asked you to hand over the file ${path}. Call agentscm_handoff with that path and accept true to hand it over, or accept false with a one-line reason to keep it. ${DATA_NOTE}` }] }
    : { human: `Waiting for ${named(h.holder.login, h.holder.agent, h.requester.login)} to answer`, prompts: [{ for: `your ${agentName(h.requester.agent)}`, login: h.requester.login, agent: h.requester.agent, text: `You asked ${h.holder.login}’s agent for the file ${path}. Repeat the edit to see whether it was handed over, or work on something else first. ${DATA_NOTE}` }] }
}

function stepFor(t: Thread): NextStep | null {
  if (t.request) return requestStep(t.request)
  if (t.overlap) return overlapSteps(t.overlap)
  return handoffStep(t)
}

/* ---------- answering ---------- */

interface Draft {
  choice: string | null
  note: string
}

function ReplyBar({ thread, draft, local, onDraft, onSend, canSend }: { thread: Thread; draft: Draft; local: Local | undefined; onDraft: (d: Draft) => void; onSend: () => void; canSend: boolean }) {
  const plan = thread.reply!
  const mineAgent = thread.mine.agent
  const them = thread.theirs ? thread.theirs.login : 'the other'
  const choices = plan.kind === 'coordination' ? REPLY_CHOICES.map((c) => ({ id: c.kind as string, label: c.label })) : [{ id: 'hand', label: 'Hand it over' }, { id: 'keep', label: 'Keep it' }]
  const choice = draft.choice ?? choices[0]!.id
  const sending = local?.status === 'sending'
  if (!canSend)
    return (
      <div className="in-reply">
        <p className="in-foot">
          <Icon name="person-lock" /> Sign in on the web to answer for your {agentName(mineAgent)} from here. Until then, copy a prompt and paste it into the agent.
        </p>
      </div>
    )
  return (
    <form
      className="in-reply"
      onSubmit={(e) => {
        e.preventDefault()
        if (!sending) onSend()
      }}
    >
      <span className="in-lbl">
        <Icon name="send" /> Answer as your {agentName(mineAgent)}
      </span>
      <div className="in-choice" role="group" aria-label="Your answer">
        {choices.map((c) => (
          <button key={c.id} type="button" aria-pressed={c.id === choice} onClick={() => onDraft({ ...draft, choice: c.id })}>
            {c.label}
          </button>
        ))}
      </div>
      <div className="in-send">
        <input aria-label="Note" value={draft.note} maxLength={plan.kind === 'coordination' ? 500 : 200} onChange={(e) => onDraft({ ...draft, note: e.target.value })} placeholder={`Add a note for ${them}’s agent (optional)`} />
        <button type="submit" className="in-go" disabled={sending}>
          <Icon name="send" /> {sending ? 'Sending…' : 'Send'}
        </button>
      </div>
      {local?.status === 'error' && (
        <p className="in-err" role="alert">
          <Icon name="exclamation-triangle" /> {local.message}
        </p>
      )}
      <p className="in-foot">
        It reaches {them}’s agent at its next edit. Nothing here merges or approves work.
      </p>
    </form>
  )
}

/* ---------- the pane ---------- */

function Pane({ thread, local, draft, canSend, handoff, ask, onDraft, onSend, onAsk, hide }: { thread: Thread; local: Local | undefined; draft: Draft; canSend: boolean; handoff?: Handoff; ask: ReturnType<typeof askOption>; onDraft: (d: Draft) => void; onSend: () => void; onAsk: () => void; hide: ReactNode }) {
  const step = stepFor(thread)
  const st = thread.request ? stage(thread.request) : null
  // What was just sent joins the timeline at once; what happens next stays last.
  const everything = [...thread.rows, ...(local?.rows ?? [])]
  const rows = [...everything.filter((r) => r.kind !== 'pending'), ...everything.filter((r) => r.kind === 'pending')]
  return (
    <div className="in-panebody" aria-label={thread.title}>
      <div className="in-phead">
        <p className="in-kind">
          <span className={`rp-chip is-sm${thread.tone ? ` is-${thread.tone}` : ''}`}>
            <Icon name={thread.icon} />
          </span>
          {thread.label}
          {st && (
            <Pill tone={st.tone} icon={st.icon}>
              {st.label}
            </Pill>
          )}
          {hide}
        </p>
        <h2 className="in-say">
          <Sentence say={thread.say} />
        </h2>
        <div className="in-sides">
          <Side side={thread.mine} />
          {thread.theirs && (
            <Side side={thread.theirs}>
              {ask && thread.theirs && !local?.rows?.length && (
                <button type="button" className="in-lnk" onClick={onAsk} disabled={local?.status === 'sending'}>
                  Ask {named(ask.holder.login, ask.holder.agent, null)} to hand it over
                </button>
              )}
            </Side>
          )}
        </div>
      </div>
      {thread.notes.map((n) => (
        <Notice key={n} title={n} />
      ))}
      <ol className="in-tl" aria-label="What has happened">
        {rows.map((r) => (
          <TimelineRow key={r.key} row={r} />
        ))}
      </ol>
      {thread.reply ? (
        <ReplyBar thread={thread} draft={draft} local={local} canSend={canSend} onDraft={onDraft} onSend={onSend} />
      ) : (
        local?.done && (
          <div className="in-reply">
            <p className="in-done" role="status">
              <Icon name="send" /> {local.done}
            </p>
          </div>
        )
      )}
      {local?.status === 'error' && !thread.reply && (
        <p className="in-err in-padded" role="alert">
          <Icon name="exclamation-triangle" /> {local.message}
        </p>
      )}
      {step && (
        <details className="in-prompt" data-tour="inbox-talk">
          <summary>Copy a prompt instead</summary>
          <NextStepView step={step} viewer={handoff?.me} repo={handoff?.repo} originUrl={handoff?.originUrl} />
        </details>
      )}
    </div>
  )
}

/* ---------- the list ---------- */

function ListItem({ thread, selected, onPick, nowMs }: { thread: Thread; selected: boolean; onPick: () => void; nowMs: number }) {
  const sides = [thread.mine.agent, thread.theirs?.agent ?? null]
  return (
    <button type="button" className={thread.group === 'needs' ? 'is-u' : undefined} aria-current={selected} onClick={onPick}>
      <span className="in-t">{thread.title}</span>
      <span className="in-when">{relative(thread.at, nowMs)}</span>
      <span className="in-s">
        {sides.map((a, i) => (a ? <AgentMark key={i} agent={a} /> : null))}
        <span>{thread.status}</span>
      </span>
    </button>
  )
}

function matches(t: Thread, q: string): boolean {
  const n = q.trim().toLocaleLowerCase()
  if (!n) return true
  const sides = [t.mine, t.theirs].flatMap((s) => (s ? [s.login, s.agent, s.task ?? '', ...s.files] : []))
  const r = t.request
  const more = r ? [r.id, r.proposal.request_key ?? '', ...r.proposal.scope, ...r.responses.flatMap((x) => [x.note ?? '', RESPONSE_LABEL[x.kind]])] : []
  return [t.title, t.label, ...t.files, ...sides, ...more].some((v) => v.toLocaleLowerCase().includes(n))
}

/* ---------- the page ---------- */

export interface LiveContext {
  agents: LiveAgent[]
  contested: Contested[]
}

/** Inbox: every declared-scope overlap and directed request, the conversations agents have to have. */
export function InboxPage({ token, overview, handoff, session = null, live = null }: { token: string; overview: Poll<WorkOverview>; handoff?: Handoff; session?: BrowserSession | null; live?: LiveContext | null }) {
  const me = handoff?.me ?? null
  const repo = handoff?.repo ?? null
  const [search, setSearch] = useState('')
  const [picked, setPicked] = useState<string | null>(null)
  const [nonce, setNonce] = useState(0)
  const [drafts, setDrafts] = useState<Record<string, Draft>>({})
  const [locals, setLocals] = useState<Record<string, Local>>({})
  const [overrides, setOverrides] = useState<Record<string, CoordinationRequest>>({})
  const [handoffOverrides, setHandoffOverrides] = useState<Record<string, MyHandoff>>({})
  const ops = useRef<Record<string, string>>({})
  const nowMs = useNow()
  const now = nowMs / 1000
  const hidden = useHidden()
  const [showHidden, setShowHidden] = useState(false)
  const [closed, setClosed] = useState(false)

  const ov = useLast(overview)
  const signals = useHideable((ov?.current ?? []).filter((i) => i.claim_status !== 'active' || i.scope.length === 0), signalHideKey)
  const requestsPoll = usePoll((signal) => fetchCoordination(token, signal, { limit: 100, activeOnly: !closed }), 15_000, `inbox|${token}|${closed}|${nonce}`)
  const handoffsPoll = usePoll((signal) => (session && repo ? fetchMyHandoffs(session, repo, signal) : Promise.resolve([] as MyHandoff[])), 15_000, `inbox-handoffs|${token}|${session?.login ?? ''}|${repo ?? ''}|${nonce}`)
  const requestsNow = useLast(requestsPoll)
  const handoffsNow = useLast(handoffsPoll)

  const requests = useMemo(() => (requestsNow?.requests ?? []).map((r) => (overrides[r.id] && overrides[r.id]!.updated_seq > r.updated_seq ? overrides[r.id]! : r)), [requestsNow, overrides])
  const handoffs = useMemo(() => (handoffsNow ?? []).map((h) => (h.state === 'pending' && handoffOverrides[h.id] ? handoffOverrides[h.id]! : h)), [handoffsNow, handoffOverrides])
  const all = useMemo(() => buildThreads({ me, overview: ov, requests, handoffs }), [me, ov, requests, handoffs])
  const searched = all.filter((t) => matches(t, search))
  const hiddenCount = searched.filter((t) => hidden.has(t.hideKey)).length
  const threads = showHidden ? searched : searched.filter((t) => !hidden.has(t.hideKey))
  const active = threads.filter((t) => t.group !== 'earlier')
  const selected = threads.find((t) => t.id === picked) ?? active[0] ?? null
  const needs = all.filter((t) => t.group === 'needs' && !hidden.has(t.hideKey)).length
  const waiting = all.filter((t) => t.group === 'waiting' && !hidden.has(t.hideKey)).length

  // A reload after a send, so the Hub's own record replaces what was shown at once.
  useEffect(() => {
    if (Object.keys(locals).length === 0) return
    const t = window.setTimeout(() => setNonce((n) => n + 1), 1500)
    return () => window.clearTimeout(t)
  }, [locals])

  // On a phone the conversation sits under the list: bring it into view.
  const pane = useRef<HTMLElement>(null)
  const pick = (id: string) => {
    setPicked(id)
    const el = pane.current
    if (el && typeof el.scrollIntoView === 'function' && window.matchMedia?.('(max-width: 760px)').matches) el.scrollIntoView({ block: 'start', behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' })
  }
  const setLocal = (id: string, local: Local) => setLocals((l) => ({ ...l, [id]: local }))
  const canSend = !!session
  const draftOf = (t: Thread): Draft => drafts[t.id] ?? { choice: null, note: '' }

  const send = async (t: Thread) => {
    if (!session || !t.reply) return
    // Stay on this conversation even when the answer moves it to another group.
    setPicked(t.id)
    const draft = draftOf(t)
    const plan = t.reply
    const note = draft.note.trim()
    setLocal(t.id, { status: 'sending' })
    try {
      if (plan.kind === 'coordination') {
        const kind = (draft.choice ?? REPLY_CHOICES[0]!.kind) as CoordinationResponseKind
        // Sending the same answer again after a failure reuses the key, so the Hub replays it instead of answering twice.
        const key = `${t.id}|${kind}|${note}`
        const op = (ops.current[key] ??= operationId())
        const updated = await replyToRequest(session, { token: plan.target.lead.token, request_id: plan.request.id, operation_id: op, target_work_id: plan.target.work_id, session: plan.target.lead.session, expected_updated_seq: plan.request.updated_seq, kind, ...(note ? { note } : {}) })
        setOverrides((o) => ({ ...o, [updated.id]: updated }))
        const label = REPLY_CHOICES.find((c) => c.kind === kind)!.label
        setLocal(t.id, { status: 'sent', done: `Sent as your ${agentName(plan.target.lead.agent)}: ${label}. ${t.theirs?.login ?? 'The other'}’s agent sees it at its next edit.` })
      } else {
        const accept = (draft.choice ?? 'hand') === 'hand'
        const h = plan.handoff
        const outcome = await answerHandoff(session, { token: h.holder.token!, ...(h.holder.session ? { session: h.holder.session } : {}), path: h.path, accept, ...(note && !accept ? { reason: note } : {}) })
        const done = outcome === 'granted' || outcome === 'declined'
        if (done) setHandoffOverrides((o) => ({ ...o, [h.id]: { ...h, state: outcome, decidedAt: nowSeconds(), reply: note && !accept ? note : null, answeredViaHub: true, requesterTold: false } }))
        setLocal(t.id, { status: done ? 'sent' : 'error', ...(done ? { done: `Sent as your ${agentName(h.holder.agent)}: ${accept ? 'handed it over' : 'kept it'}. ${h.requester.login}’s agent sees it at its next edit.` } : { message: OUTCOME_SENTENCE[outcome] }) })
      }
    } catch (e) {
      setLocal(t.id, { status: 'error', message: sendError(e) })
    }
  }

  const ask = async (t: Thread, option: NonNullable<ReturnType<typeof askOption>>) => {
    if (!session) return
    setPicked(t.id)
    setLocal(t.id, { status: 'sending' })
    try {
      const outcome = await askHandoff(session, { token: option.token, path: option.path })
      const ok = outcome === 'requested' || outcome === 'already_asked'
      const who = named(option.holder.login, option.holder.agent, null)
      const rows: Row[] = ok
        ? [
            { key: 'sent-ask', kind: 'agent', tone: 'me', agent: option.mine.agent, who: `You, as your ${agentName(option.mine.agent)}`, did: `asked ${who} to hand it over · from the Hub`, at: nowSeconds(), text: option.path, quote: false },
            { key: 'sent-wait', kind: 'pending', tone: '', agent: option.holder.agent, who, did: 'sees it at its next edit', at: null, text: null, quote: false },
          ]
        : []
      setLocal(t.id, { status: ok ? 'sent' : 'error', ...(ok ? { rows, done: outcome === 'requested' ? 'Asked. Their agent sees it at its next edit.' : OUTCOME_SENTENCE[outcome] } : { message: OUTCOME_SENTENCE[outcome] }) })
    } catch (e) {
      setLocal(t.id, { status: 'error', message: sendError(e) })
    }
  }

  const ovReady = ov !== null
  const partial = ovReady && !ov.overlaps_complete
  const headline = !ovReady ? (overview.kind === 'error' ? 'The overlap count could not be loaded.' : 'Checking which tasks overlap…') : needs > 0 ? <><b>{plural(needs, 'conversation')}</b> {needs === 1 ? 'needs' : 'need'} you.</> : partial ? 'No overlap in this partial snapshot. An empty list is not an all-clear.' : waiting > 0 ? <>Nothing needs you. <b>{plural(waiting, 'conversation')}</b> {waiting === 1 ? 'is' : 'are'} waiting on others.</> : 'No two tasks plan to touch the same files, and no request is waiting.'

  return (
    <div className="page in-page">
      <PageHead question="Which overlaps need a conversation?" answer={headline} live fact={<>Answers reach the agents’ running sessions at their next edit. Nothing here merges or approves work. {READING_CHANGES_NOTHING} {OVERLAP_IS_SIGNAL} Agents answer <Gloss term="directedRequest">requests</Gloss> themselves too.</>} />
      {overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {!ov && overview.kind === 'loading' && <Skeleton rows={6} />}
      {requestsPoll.kind === 'error' && !requestsNow && (
        <Notice tone="bad" title="Could not load directed requests" action={<button className="btn btn-sm btn-outline-secondary" onClick={requestsPoll.retry}>Retry</button>}>
          {requestsPoll.message}
        </Notice>
      )}
      {requestsPoll.kind === 'ready' && requestsPoll.refreshError && <Notice title="Could not refresh requests">{requestsPoll.refreshError}</Notice>}
      {handoffsPoll.kind === 'error' && session && <Notice title="Could not load file handoffs">{handoffsPoll.message}</Notice>}
      {requestsNow?.retention_gap && <Notice title="Some request history is no longer retained" />}
      {requestsNow && !requestsNow.complete && <Notice title="Only the newest requests are shown">More requests remain in the Hub’s history than this page loads at once.</Notice>}
      {partial && <Notice title="Overlap list is incomplete">The current-work limit was reached, so more overlaps may exist. An empty list is not an all-clear.</Notice>}
      {ov && (
        <>
          <Search value={search} onChange={setSearch} placeholder="Search agents, tasks, or paths" />
          <div className="in-main">
          <div className="in-wrap" id="box-requests">
            <nav className="in-list" id="box-overlaps" aria-label="Conversations">
              {GROUPS.map((g: Group) => {
                const rows = threads.filter((t) => t.group === g)
                if (rows.length === 0) return null
                return (
                  <div key={g} className="in-group">
                    <h2 className="in-gh">{GROUP_TITLE[g]}</h2>
                    {rows.map((t) => (
                      <ListItem key={t.id} thread={t} selected={selected?.id === t.id} onPick={() => pick(t.id)} nowMs={nowMs} />
                    ))}
                  </div>
                )
              })}
              {threads.length === 0 && <p className="in-none">Nothing recorded yet. Agents ask each other with <code>agentscm_ask</code> and declare files with <code>agentscm_start</code>.</p>}
              <ShowHidden count={hiddenCount} show={showHidden} onShow={setShowHidden} />
              <div className="form-check form-switch in-closed">
                <input id="show-closed" className="form-check-input" type="checkbox" role="switch" checked={closed} onChange={(e) => setClosed(e.target.checked)} />
                <label className="form-check-label small" htmlFor="show-closed">
                  Include resolved & withdrawn requests
                </label>
              </div>
            </nav>
            <section className="in-pane" aria-label="Selected conversation" ref={pane}>
              {selected ? (
                <Pane
                  key={selected.id}
                  thread={selected}
                  local={locals[selected.id]}
                  draft={draftOf(selected)}
                  canSend={canSend}
                  handoff={handoff}
                  ask={canSend && live ? askOption(selected, { me, agents: live.agents, contested: live.contested, handoffs }) : null}
                  onDraft={(d) => setDrafts((all) => ({ ...all, [selected.id]: d }))}
                  onSend={() => void send(selected)}
                  onAsk={() => {
                    const option = live ? askOption(selected, { me, agents: live.agents, contested: live.contested, handoffs }) : null
                    if (option) void ask(selected, option)
                  }}
                  hide={(hidden.has(selected.hideKey) || isStale(selected.updatedAt, now)) && <HideButton k={selected.hideKey} hidden={hidden.has(selected.hideKey)} what={`“${selected.title}”`} />}
                />
              ) : (
                <IdleScene partial={partial} />
              )}
            </section>
          </div>
          </div>
          <details className="disclosure in-quiet">
            <summary>Quiet agents and tasks with no stated files · {signals.rows.length - (signals.show ? signals.count : 0)}</summary>
            <p className="small muted mt-2">Overlap checks can’t see these. You can hide an item once it’s a day old.</p>
            {signals.rows.length === 0 ? (
              <p className="small muted">Nothing else needs attention.</p>
            ) : (
              <ul className="plain-list">
                {signals.rows.map((i) => (
                  <li key={i.id} className={signals.isHidden(i) ? 'is-hidden' : undefined}>
                    <WorkLink id={i.id}>{i.title}</WorkLink>
                    <span className="muted small"> · {agentLabel(lead(i).agent)}</span>
                    <div className="pills">
                      {i.claim_status !== 'active' && <ClaimPill item={i} />}
                      {i.scope.length === 0 && <Pill tone="muted" icon="question-circle">Scope unknown</Pill>}
                      {(signals.isHidden(i) || isStale(Math.max(i.updated_at, i.last_checkin_at), now)) && <HideButton k={signalHideKey(i)} hidden={signals.isHidden(i)} what={`“${i.title}”`} />}
                    </div>
                  </li>
                ))}
              </ul>
            )}
            <ShowHidden count={signals.count} show={signals.show} onShow={signals.setShow} />
          </details>
        </>
      )}
    </div>
  )
}
