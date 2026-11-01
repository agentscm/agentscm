// The Inbox as conversations. Everything the Hub records about two agents needing to talk (declared-scope
// overlaps, directed requests, file handoffs, recent decisions) is turned into threads, each with the two
// sides and a timeline, and sorted into who has to move: the person, someone else, or nobody (earlier).
// Pure: no DOM, no network.
//
// API for other packages:
//   buildThreads({ me, overview, requests, handoffs })   every thread, newest first within a group
//   GROUP_TITLE, GROUPS                                   the list's headings, in order
//   Thread, Row, SideView, ReplyPlan                      what the page draws
//   REPLY_CHOICES                                         the coordination reply kinds, in words, with what each does
//   agentName(agent), named(login, agent, me)             "Claude Code", "dev’s Codex" / "Your Codex"
//   askOption(thread, ctx)                                the person's own agent that could ask the holder of a file to hand it over, or null
import type { MyHandoff } from './inboxApi.ts'
import type { Contested, LiveAgent } from './derive.ts'
import { lead } from './pages/workParts.tsx'
import { currentAck, currentResponse, pending, RESPONSE_LABEL, stage } from './requests.ts'
import type { CoordinationRequest, CoordinationResponseKind, CoordinationTarget, WorkItem, WorkOverlapPair, WorkOverview } from './standup.ts'
import { agentName } from './nextStep.ts'
import { hideKey } from './dismiss.tsx'

export { agentName }

export type Group = 'needs' | 'waiting' | 'others' | 'earlier'
export const GROUPS: Group[] = ['needs', 'waiting', 'others', 'earlier']
export const GROUP_TITLE: Record<Group, string> = { needs: 'Needs you', waiting: 'Waiting on others', others: 'Between others', earlier: 'Earlier' }

/** A sentence in parts; a part with `code` is a file or path. */
export type Say = (string | { code: string })[]

export interface SideView {
  /** The signed-in person's own agent. */
  me: boolean
  login: string
  agent: string
  task: string | null
  files: string[]
}

export interface Row {
  key: string
  /** `agent`: an agent or the person acting for theirs; `notice`: AgentSCM; `receipt`: a receipt; `pending`: what happens next; `omitted`: history the Hub no longer has. */
  kind: 'agent' | 'notice' | 'receipt' | 'pending' | 'omitted'
  tone: '' | 'warn' | 'ok' | 'me'
  agent: string | null
  who: string
  did: string
  at: number | null
  text: string | null
  quote: boolean
}

export type ReplyPlan = { kind: 'coordination'; request: CoordinationRequest; target: CoordinationTarget } | { kind: 'handoff'; handoff: MyHandoff }

export interface Thread {
  id: string
  group: Group
  kind: 'overlap' | 'request' | 'handoff' | 'decision'
  icon: string
  tone: '' | 'warn' | 'ok'
  /** The kind in words, beside its icon. */
  label: string
  title: string
  say: Say
  /** When the conversation began; the list shows its age. */
  at: number
  updatedAt: number
  /** A few words under the title in the list. */
  status: string
  mine: SideView
  theirs: SideView | null
  rows: Row[]
  reply: ReplyPlan | null
  request: CoordinationRequest | null
  overlap: WorkOverlapPair | null
  handoff: MyHandoff | null
  /** Notices about what the Hub could not show, always shown with the thread. */
  notes: string[]
  /** Files named here, for asking their holder. */
  files: string[]
  hideKey: string
}

/** The reply kinds a person can send for their agent, in the order and words the page offers them. */
export const REPLY_CHOICES: { kind: CoordinationResponseKind; label: string; hint: string }[] = [
  { kind: 'propose_split', label: 'Propose a split', hint: 'Suggest each agent takes a different part of the files.' },
  { kind: 'join', label: 'Join their work', hint: 'Offer to work alongside as a helper.' },
  { kind: 'handover', label: 'Hand mine over', hint: 'Offer to hand your task to the other agent.' },
  { kind: 'decline', label: 'Decline', hint: 'Say you will carry on as planned.' },
]

const DID: Record<CoordinationResponseKind, string> = { propose_split: 'proposed a scope split', join: 'offered to join', handover: 'offered a handover', decline: 'declined' }


/** "Your Codex" for the person's own agent, "dev’s Codex" for anyone else's. */
export const named = (login: string, agent: string, me: string | null): string => (me !== null && login === me ? `Your ${agentName(agent)}` : `${login}’s ${agentName(agent)}`)

/** Who an action was done by: the agent, or a person acting for it on the Hub's web pages (never passed off as the agent). */
const actor = (a: { login: string; agent: string }, viaHub: boolean, me: string | null): string =>
  viaHub ? (me !== null && a.login === me ? `You, as your ${agentName(a.agent)}` : `${a.login}, for their ${agentName(a.agent)}`) : named(a.login, a.agent, me)

const uniq = (xs: string[]): string[] => [...new Set(xs)]
const base = (path: string): string => path.replace(/\/+$/, '').split('/').pop() || path
const WILD = /[*?[\]{}]/
const files = (paths: string[], max = 4): string => `${paths.slice(0, max).join(', ')}${paths.length > max ? ` (+${paths.length - max} more)` : ''}`
const codeOrWords = (paths: string[]): { code: string } | string => (paths[0] ? { code: paths[0] } : 'the same files')
const more = (paths: string[]): string => (paths.length > 1 ? ` and ${paths.length - 1} more` : '')

const row = (r: Partial<Row> & Pick<Row, 'key' | 'kind' | 'who' | 'did'>): Row => ({ tone: '', agent: null, at: null, text: null, quote: false, ...r })
const notice = (key: string, did: string, at: number | null, text: string | null, tone: Row['tone'] = ''): Row => row({ key, kind: 'notice', who: 'AgentSCM', did, at, text, tone })
const sortRows = (rows: Row[]): Row[] => {
  // Dated rows in time order (ties keep their order); an undated row sits with the row before it; pending markers close the timeline.
  let last = 0
  const out = rows.map((r, i) => ({ r, i, at: (last = r.at ?? last) }))
  const rank = (r: Row) => (r.kind === 'pending' ? 1 : 0)
  out.sort((a, b) => rank(a.r) - rank(b.r) || a.at - b.at || a.i - b.i)
  return out.map((x) => x.r)
}

function requestThread(r: CoordinationRequest, me: string | null, works: Map<string, WorkItem>): Thread {
  const st = stage(r)
  const open = pending(r)
  const myTargets = me === null ? [] : r.targets.filter((t) => t.lead.login === me)
  const iRequested = me !== null && r.requester.login === me
  const unanswered = open ? myTargets.find((t) => !currentResponse(r, t)) : undefined
  const asTarget = unanswered ?? (!iRequested ? myTargets[0] : undefined)
  const perspective = asTarget ? 'target' : iRequested ? 'requester' : 'other'
  const first = r.targets[0]!
  const targetSide = (t: CoordinationTarget, mine: boolean): SideView => ({ me: mine, login: t.lead.login, agent: t.lead.agent, task: works.get(t.work_id)?.title ?? null, files: uniq(t.reasons.map((x) => x.your_path)) })
  const requesterSide = (mine: boolean): SideView => ({ me: mine, login: r.requester.login, agent: r.requester.agent, task: r.proposal.title, files: r.proposal.scope })
  const mine = perspective === 'target' ? targetSide(asTarget!, true) : perspective === 'requester' ? requesterSide(true) : requesterSide(false)
  const theirs = perspective === 'target' ? requesterSide(false) : targetSide(first, false)
  const group: Group = !open ? 'earlier' : unanswered ? 'needs' : perspective === 'other' ? 'others' : 'waiting'
  const focus = perspective === 'target' ? mine.files : theirs.files
  const where = codeOrWords(focus.length ? focus : [...mine.files, ...theirs.files])
  const say: Say =
    perspective === 'target'
      ? [`${named(r.requester.login, r.requester.agent, me)} wants to start “${r.proposal.title}”, which overlaps your ${agentName(mine.agent)}’s work on `, where, `${more(focus)}.`]
      : perspective === 'requester'
        ? [`Your ${agentName(mine.agent)} asked ${named(first.lead.login, first.lead.agent, me)} to coordinate on `, where, `${more(focus)}.`]
        : [`${named(r.requester.login, r.requester.agent, me)} asked ${named(first.lead.login, first.lead.agent, me)} to coordinate on `, where, `${more(focus)}.`]

  const rows: Row[] = []
  rows.push(row({ key: 'asked', kind: 'agent', tone: iRequested ? 'me' : '', agent: r.requester.agent, who: named(r.requester.login, r.requester.agent, me), did: 'asked to coordinate', at: r.created_at, text: `“${r.proposal.title}”${r.proposal.scope.length ? ` · plans to change ${files(r.proposal.scope)}` : ''}`, quote: true }))
  const pairs = first.reasons.slice(0, 2).map((x) => (x.your_path === x.their_path ? x.your_path : `${x.your_path} and ${x.their_path}`))
  const partial = r.targets.some((t) => !t.reasons_complete)
  rows.push(notice('noticed', 'noticed the overlap', r.created_at, `Both declared ${pairs.join(', ')}. A coordination signal, not a Git conflict. Nothing is blocked yet.${partial ? ' More overlapping paths were omitted; check the full declared scopes.' : ''}`, 'warn'))
  for (const t of r.targets) {
    const ack = currentAck(r, t)
    if (ack) rows.push(row({ key: `ack:${t.work_id}`, kind: 'receipt', tone: 'ok', agent: t.lead.agent, who: named(t.lead.login, t.lead.agent, me), did: 'received it', at: ack.acknowledged_at }))
  }
  const requesterAcks = r.acknowledgements.filter((a) => a.target_work_id === null && a.agent.token === r.requester.token && a.agent.session === r.requester.session).sort((a, b) => b.seq - a.seq)
  const lastSeen = requesterAcks[0]
  if (lastSeen) rows.push(row({ key: 'ack:requester', kind: 'receipt', tone: 'ok', agent: r.requester.agent, who: named(r.requester.login, r.requester.agent, me), did: 'saw the latest update', at: lastSeen.acknowledged_at }))
  const responses = [...r.responses].sort((a, b) => a.seq - b.seq)
  for (const x of responses) {
    const viaHub = x.via === 'hub'
    rows.push(row({ key: `reply:${x.id}`, kind: 'agent', tone: me !== null && x.responder.login === me ? 'me' : '', agent: x.responder.agent, who: actor(x.responder, viaHub, me), did: `${DID[x.kind]}${viaHub ? ' · from the Hub' : ''}`, at: x.created_at, text: x.note ?? null, quote: !!x.note }))
  }
  if (r.status === 'resolved') rows.push(notice('closed', 'closed the request', r.updated_at, 'The requesting agent resolved it.', 'ok'))
  if (r.status === 'withdrawn') rows.push(notice('closed', 'closed the request', r.updated_at, 'The proposal was withdrawn.'))
  const notes: string[] = []
  if (!r.acknowledgements_complete || !r.responses_complete) {
    notes.push('Some receipt or reply history is omitted. The status above is authoritative; the history shown here is incomplete.')
    rows.push(row({ key: 'omitted', kind: 'omitted', who: 'AgentSCM', did: 'omitted some history', text: 'Some receipt or reply history is no longer shown.' }))
  }
  if (open) {
    for (const t of r.targets) {
      if (currentResponse(r, t) || t === asTarget) continue
      const text = !r.acknowledgements_complete ? 'Receipt history is incomplete.' : currentAck(r, t) ? 'It received the request. You’ll see the reply here as soon as it answers.' : 'It has not opened the request yet.'
      rows.push(row({ key: `waiting:${t.work_id}`, kind: 'pending', agent: t.lead.agent, who: named(t.lead.login, t.lead.agent, me), did: 'has not replied yet', text }))
    }
    const latest = responses.at(-1)
    if (latest && (!lastSeen || lastSeen.through_seq < latest.seq)) rows.push(row({ key: 'sees', kind: 'pending', agent: r.requester.agent, who: named(r.requester.login, r.requester.agent, me), did: 'sees it at its next edit' }))
  }
  const status =
    group === 'earlier' ? st.label
    : group === 'needs' ? `${named(r.requester.login, r.requester.agent, me)} is waiting`
    : perspective === 'requester' ? `You asked · ${st.label.toLowerCase()}`
    : st.label
  return {
    id: `request:${r.id}`,
    group,
    kind: 'request',
    icon: !open ? (r.status === 'resolved' ? 'check2-all' : 'x-circle') : perspective === 'target' ? 'chat-left-dots' : perspective === 'requester' ? 'send' : 'chat-left',
    tone: !open ? (r.status === 'resolved' ? 'ok' : '') : perspective === 'target' ? 'warn' : '',
    label: perspective === 'target' ? 'Request for your agent' : perspective === 'requester' ? 'You asked' : 'Request',
    title: r.proposal.title,
    say,
    at: r.created_at,
    updatedAt: r.updated_at,
    status,
    mine,
    theirs,
    rows: sortRows(rows),
    reply: unanswered ? { kind: 'coordination', request: r, target: unanswered } : null,
    request: r,
    overlap: null,
    handoff: null,
    notes,
    files: uniq(r.targets.flatMap((t) => t.reasons.flatMap((x) => [x.your_path, x.their_path]))),
    hideKey: hideKey(`request:${r.id}`, r.updated_at),
  }
}

/** True when a pending request already speaks for this overlap, so it is one conversation, not two. */
function covered(o: WorkOverlapPair, requests: CoordinationRequest[]): boolean {
  const ids = new Set([o.left.id, o.right.id])
  const tokens = new Set([o.left.lead_token, o.right.lead_token])
  return requests.some((r) => pending(r) && r.targets.some((t) => ids.has(t.work_id)) && ((r.proposal.work_id !== null && ids.has(r.proposal.work_id)) || tokens.has(r.requester.token)))
}

function overlapThread(o: WorkOverlapPair, me: string | null): Thread {
  const l = lead(o.left)
  const rr = lead(o.right)
  const iLeft = me !== null && l.login === me
  const iRight = me !== null && rr.login === me
  const [mineItem, theirItem] = iRight && !iLeft ? [o.right, o.left] : [o.left, o.right]
  const [mineLead, theirLead] = mineItem === o.left ? [l, rr] : [rr, l]
  const side = (item: WorkItem, who: { login: string; agent: string }, mine: boolean): SideView => ({ me: mine, login: who.login, agent: who.agent, task: item.title, files: item.scope })
  const shared = uniq(o.reasons.flatMap((x) => [x.left_path, x.right_path]))
  const first = o.reasons[0]?.left_path
  const mine = iLeft || iRight
  const sides = [side(mineItem, mineLead, mine), side(theirItem, theirLead, false)] as const
  const rows: Row[] = [mineItem, theirItem]
    .sort((a, b) => a.created_at - b.created_at)
    .map((item) => {
      const who = item === mineItem ? mineLead : theirLead
      return row({ key: `plan:${item.id}`, kind: 'agent', tone: mine && item === mineItem ? 'me' : '', agent: who.agent, who: named(who.login, who.agent, me), did: `started “${item.title}”`, at: item.created_at, text: item.scope.length ? `Plans to change ${files(item.scope)}` : 'Has not said which files it plans to change' })
    })
  const pair = o.reasons.slice(0, 2).map((x) => (x.left_path === x.right_path ? x.left_path : `${x.left_path} and ${x.right_path}`))
  rows.push(notice('noticed', 'noticed the overlap', Math.max(o.left.created_at, o.right.created_at), `Both declared ${pair.join(', ') || 'overlapping files'}. A coordination signal, not a Git conflict. Nothing is blocked yet.${o.reasons_complete ? '' : ' More path pairs omitted — compare full scopes.'}`, 'warn'))
  const notes = o.reasons_complete ? [] : ['More overlapping path pairs were omitted; compare the full declared scopes.']
  const updatedAt = Math.max(o.left.updated_at, o.right.updated_at, o.left.last_checkin_at, o.right.last_checkin_at)
  const A = mine ? `Your ${agentName(mineLead.agent)}` : named(mineLead.login, mineLead.agent, me)
  const B = named(theirLead.login, theirLead.agent, me)
  return {
    id: `overlap:${o.left.id}:${o.right.id}`,
    group: mine ? 'needs' : 'others',
    kind: 'overlap',
    icon: 'sign-intersection-y',
    tone: 'warn',
    label: 'Overlap',
    title: first ? `Both plan to change ${base(first)}` : 'Two tasks plan the same files',
    say: [`${A} and ${B} both plan to change `, codeOrWords(first ? [first] : []), `${more(shared.length > 2 ? shared : [])}.`],
    at: Math.max(o.left.created_at, o.right.created_at),
    updatedAt,
    status: `${A} and ${B}`,
    mine: sides[0],
    theirs: sides[1],
    rows: sortRows(rows),
    reply: null,
    request: null,
    overlap: o,
    handoff: null,
    notes,
    files: shared,
    hideKey: hideKey(`overlap:${o.left.id}:${o.right.id}`, updatedAt),
  }
}

function handoffThread(h: MyHandoff, me: string | null, works: Map<string, WorkItem>): Thread {
  const open = h.state === 'pending'
  const asHolder = h.holder.mine
  const taskOf = (token: string | undefined): WorkItem | undefined => (token ? [...works.values()].find((w) => w.lead_token === token) : undefined)
  const side = (s: MyHandoff['holder'], isMine: boolean): SideView => {
    const w = taskOf(s.token)
    return { me: isMine, login: s.login, agent: s.agent, task: w?.title ?? null, files: w && s.mine ? w.scope : [h.path] }
  }
  const mine = asHolder ? side(h.holder, true) : side(h.requester, true)
  const theirs = asHolder ? side(h.requester, false) : side(h.holder, false)
  const group: Group = !open ? 'earlier' : asHolder ? 'needs' : 'waiting'
  const requester = named(h.requester.login, h.requester.agent, me)
  const holder = named(h.holder.login, h.holder.agent, me)
  const rows: Row[] = [
    row({ key: 'asked', kind: 'agent', tone: h.requester.mine ? 'me' : '', agent: h.requester.agent, who: actor(h.requester, h.askedViaHub, me), did: 'asked for the file', at: h.createdAt, text: h.note, quote: !!h.note }),
  ]
  if (h.holderTold) rows.push(row({ key: 'told', kind: 'receipt', tone: 'ok', agent: h.holder.agent, who: holder, did: 'received it' }))
  if (h.state === 'granted' || h.state === 'declined') {
    rows.push(row({ key: 'answered', kind: 'agent', tone: h.holder.mine ? 'me' : '', agent: h.holder.agent, who: actor(h.holder, h.answeredViaHub, me), did: `${h.state === 'granted' ? 'handed it over' : 'kept it for now'}${h.answeredViaHub ? ' · from the Hub' : ''}`, at: h.decidedAt, text: h.reply, quote: !!h.reply }))
    if (!h.requesterTold) rows.push(row({ key: 'sees', kind: 'pending', agent: h.requester.agent, who: requester, did: 'sees it at its next edit' }))
  } else if (h.state === 'lapsed') {
    rows.push(notice('lapsed', 'closed the request', h.createdAt + 1800, 'No answer within 30 minutes, so it lapsed.'))
  } else if (!asHolder) {
    rows.push(row({ key: 'waiting', kind: 'pending', agent: h.holder.agent, who: holder, did: 'has not replied yet', text: 'You’ll see the reply here as soon as it answers.' }))
  }
  const closed = { granted: 'Handed over', declined: 'Kept', lapsed: 'Lapsed' } as const
  return {
    id: `handoff:${h.id}`,
    group,
    kind: 'handoff',
    icon: !open ? (h.state === 'granted' ? 'check2-circle' : 'x-circle') : asHolder ? 'arrow-left-right' : 'send',
    tone: open && asHolder ? 'warn' : h.state === 'granted' ? 'ok' : '',
    label: asHolder ? 'Request for your agent' : 'You asked',
    title: `Hand over ${base(h.path)}?`,
    say: asHolder
      ? [`${requester} asks your ${agentName(h.holder.agent)} to hand over `, { code: h.path }, '.']
      : [`Your ${agentName(h.requester.agent)} asked ${holder} to hand over `, { code: h.path }, '.'],
    at: h.createdAt,
    updatedAt: h.decidedAt ?? h.createdAt,
    status: open ? (asHolder ? `${requester} is waiting` : `You asked ${holder}`) : closed[h.state as keyof typeof closed],
    mine,
    theirs,
    rows: sortRows(rows),
    reply: open && asHolder ? { kind: 'handoff', handoff: h } : null,
    request: null,
    overlap: null,
    handoff: h,
    notes: [],
    files: [h.path],
    hideKey: hideKey(`handoff:${h.id}`, h.decidedAt ?? h.createdAt),
  }
}

export interface ThreadInput {
  me: string | null
  overview: WorkOverview | null
  requests: CoordinationRequest[]
  handoffs: MyHandoff[]
}

/** Every conversation the Hub records for this repository, grouped by who has to move, newest first. */
export function buildThreads({ me, overview, requests, handoffs }: ThreadInput): Thread[] {
  const works = new Map<string, WorkItem>()
  for (const o of overview?.recent ?? []) works.set(o.item.id, o.item)
  for (const w of overview?.current ?? []) works.set(w.id, w)
  const threads: Thread[] = [
    ...requests.map((r) => requestThread(r, me, works)),
    ...(overview?.overlaps ?? []).filter((o) => !covered(o, requests)).map((o) => overlapThread(o, me)),
    ...handoffs.map((h) => handoffThread(h, me, works)),
    ...(overview?.recent ?? []).filter((o) => o.kind === 'decision').map((o): Thread => {
      const l = lead(o.item)
      const mine = me !== null && l.login === me
      const summary = o.item.current?.summary ?? 'No decision summary retained'
      return {
        id: `decision:${o.seq}`,
        group: 'earlier',
        kind: 'decision',
        icon: 'signpost',
        tone: '',
        label: 'Decision',
        title: o.item.title,
        say: [`${named(l.login, l.agent, me)} reported a decision on “${o.item.title}”.`],
        at: o.at,
        updatedAt: o.at,
        status: 'Reported a decision',
        mine: { me: mine, login: l.login, agent: l.agent, task: o.item.title, files: o.item.scope },
        theirs: null,
        rows: [row({ key: 'decision', kind: 'agent', tone: mine ? 'me' : '', agent: l.agent, who: named(l.login, l.agent, me), did: 'reported a decision', at: o.at, text: summary, quote: true })],
        reply: null,
        request: null,
        overlap: null,
        handoff: null,
        notes: [],
        files: [],
        hideKey: hideKey(`decision:${o.seq}`, o.at),
      }
    }),
  ]
  return threads.sort((a, b) => GROUPS.indexOf(a.group) - GROUPS.indexOf(b.group) || b.at - a.at || a.id.localeCompare(b.id))
}

/** What the page needs of the live picture to offer "Ask their agent to hand it over". */
export interface AskContext {
  me: string | null
  agents: LiveAgent[]
  contested: Contested[]
  handoffs: MyHandoff[]
}
export interface AskOption {
  /** The person's own agent token; the Hub picks its one live hook session. */
  token: string
  path: string
  /** Whose agent holds it. */
  holder: { login: string; agent: string }
  mine: { agent: string }
}

/**
 * The person's own live agent that wants one of the thread's files, and the file another person's agent holds now: the only
 * case where the Hub can file a handoff request. Null when none, when a request is already open, or when the agent has no session to ask from.
 */
export function askOption(thread: Pick<Thread, 'files'>, { me, agents, contested, handoffs }: AskContext): AskOption | null {
  if (me === null) return null
  for (const path of thread.files.filter((f) => !WILD.test(f))) {
    if (handoffs.some((h) => h.state === 'pending' && h.requester.mine && h.path === path)) continue
    const c = contested.find((x) => x.path === path)
    const theirs = c?.holders.find((h) => h.owner !== me)
    if (!c || !theirs) continue
    const ids = c.holders.filter((h) => h.owner === me).map((h) => h.id.split('\u0000')[1])
    const wanter = agents.find((a) => a.owner === me && ids.includes(a.token) && a.holdings.some((h) => h.spec === path) && !a.id.startsWith('intent:'))
    if (wanter) return { token: wanter.token, path, holder: { login: theirs.owner, agent: theirs.name }, mine: { agent: wanter.name } }
  }
  return null
}

export { RESPONSE_LABEL }
