// Decoder, navigation, and server-rendered metadata checks for the read-only stand-up client.
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { createServer } from 'vite'
import {
  InvalidStandupResponse,
  briefingPath,
  coordinationPath,
  currentLeadParticipant,
  decodeBriefing,
  decodeCoordinationBriefing,
  decodeOverview,
  decodeWorkItem,
  hasMoreBriefing,
  overviewPath,
  safeCommitHref,
  safeForgeIssueHref,
  workHref,
  workIdFromHash,
  workMatchesSearch,
} from '../src/standup.ts'

const workId = `wrk_${'a'.repeat(32)}`
const relatedId = `wrk_${'b'.repeat(32)}`
const otherRelatedId = `wrk_${'d'.repeat(32)}`
const activeId = `wrk_${'e'.repeat(32)}`
const finishedId = `wrk_${'f'.repeat(32)}`
const coordinationId = `crq_${'1'.repeat(32)}`
const malicious = '<img src=x onerror=alert(1)>'
const item = {
  id: workId,
  repository: 'acme/widgets',
  owner: 'github:github.com:101',
  owner_login: 'original-creator',
  source: { kind: 'forge', provider: 'github', host: 'github.com', repository: 'acme/widgets', issue: '42' },
  title: `<script>alert('title')</script>`,
  state: 'paused',
  claim_status: 'stale',
  last_checkin_at: 1_792_440_000,
  generation: 3,
  lead_token: 'tok_lead',
  lead_agent: 'builder',
  lead_session: 'session-1',
  participants: [
    { owner: 'github:github.com:101', login: 'original-creator', token: 'tok_original', agent: 'starter', session: 'session-0', role: 'helper' },
    { owner: 'github:github.com:202', login: 'takeover-human', token: 'tok_lead', agent: 'builder', session: 'session-1', role: 'lead' },
  ],
  scope: ['src/lib.rs'],
  branch: 'feat/widgets',
  worktree: '/retained/display/value',
  next_action: malicious,
  current: {
    kind: 'pause', summary: malicious, next_action: 'Wait for a reviewed dependency',
    source_refs: [
      { kind: 'repository_path', path: 'src/lib.rs', line: 42 },
      { kind: 'commit', oid: 'c'.repeat(40), path: null },
      { kind: 'work', work_id: relatedId },
    ],
    at: 1_792_440_000, token: 'tok_lead', agent: 'builder', session: 'session-1', generation: 3,
  },
  blocker: {
    kind: 'blocker', summary: 'Waiting for dependency review', next_action: null, source_refs: [],
    at: 1_792_439_000, token: 'tok_lead', agent: 'builder', session: 'session-1', generation: 3,
  },
  depends_on: [relatedId],
  related_work: [otherRelatedId],
  created_at: 1_792_430_000,
  updated_at: 1_792_440_000,
  created_seq: 1,
  updated_seq: 12,
}

const envelope = {
  delivery: null,
  from: 1,
  through: 12,
  after: 0,
  work_after: null,
  next_after: 12,
  next_work_after: workId,
  next_work_done: true,
  items: [item],
  events: [{ seq: 12, at: 1_792_440_000, kind: 'work_checkpoint', work_id: workId, item }],
  retention_gap: false,
  complete: true,
}

const decoded = decodeBriefing(envelope)
assert.equal(decoded.items[0].title, item.title, 'untrusted text stays plain data for React to escape')
assert.equal(decoded.items[0].next_action, malicious)
assert.equal(decoded.items[0].owner_login, 'original-creator')
assert.equal(currentLeadParticipant(decoded.items[0]).login, 'takeover-human', 'the current human lead comes from the explicit lead participant')
assert.equal(decoded.items[0].claim_status, 'stale', 'claim liveness remains separate from work progress')
assert.equal(decoded.items[0].state, 'paused')
assert.equal(decoded.items[0].current.source_refs[2].work_id, relatedId)
assert.equal(decoded.items[0].blocker.summary, 'Waiting for dependency review', 'a paused item retains its blocker separately from the latest checkpoint')
assert.equal(decoded.next_work_done, true)
assert.equal(workMatchesSearch(decoded.items[0], ' STARTER '), true, 'helper agents remain discoverable after a takeover')
assert.equal(workMatchesSearch(decoded.items[0], 'takeover-human'), true)
assert.equal(workMatchesSearch(decoded.items[0], 'src/lib.rs'), true, 'declared scope remains searchable')
for (const hidden of ['\u202e', '\u200b', '\u2066', '\u{e0001}', '\u00ad']) {
  assert.throws(() => decodeWorkItem({ ...item, title: `Safe${hidden}txt.exe` }), InvalidStandupResponse)
  assert.throws(() => decodeWorkItem({ ...item, scope: [`src/${hidden}file.rs`] }), InvalidStandupResponse)
}
const reviewedItem = decodeWorkItem({ ...item, participants: [...item.participants, { ...item.participants[0], agent: 'security-review', login: 'review-human', token: 'tok_review', role: 'reviewer' }] })
assert.equal(workMatchesSearch(reviewedItem, 'security-review'), true)
assert.equal(workMatchesSearch(reviewedItem, 'review-human'), true)
assert.equal(workMatchesSearch(reviewedItem, 'not-on-this-task'), false)
assert.equal(hasMoreBriefing({ ...decoded, complete: false, next_after: 10 }, 'current'), false, 'exhausted work does not traverse empty pages while history continues')
assert.equal(hasMoreBriefing({ ...decoded, complete: false, next_after: 10 }, 'changes'), true)
assert.equal(hasMoreBriefing({ ...decoded, complete: false, next_work_done: false }, 'changes'), false, 'exhausted history does not traverse empty pages while work continues')
assert.equal(hasMoreBriefing({ ...decoded, complete: false, next_work_done: false }, 'current'), true)
assert.equal(hasMoreBriefing(decoded, 'current'), false)
assert.equal(hasMoreBriefing(decoded, 'changes'), false)

const activeItem = {
  ...item,
  id: activeId,
  title: 'Refactor shared source loading',
  state: 'active',
  claim_status: 'active',
  scope: ['src/**'],
  depends_on: [],
  related_work: [],
}
const finishedItem = {
  ...item,
  id: finishedId,
  title: 'Document the release boundary',
  state: 'finished',
  claim_status: 'unknown',
  scope: ['docs/release.md'],
  depends_on: [],
  related_work: [],
}
const overviewEnvelope = {
  recent_since: 1_792_430_000,
  until: 1_792_441_000,
  current: [item, activeItem],
  recent: [
    { seq: 14, at: 1_792_440_500, kind: 'decision', item },
    { seq: 13, at: 1_792_440_000, kind: 'completion', item: finishedItem },
  ],
  overlaps: [{ left: item, right: activeItem, reasons: [{ left_path: 'src/lib.rs', right_path: 'src/**' }], reasons_complete: false }],
  current_complete: true,
  recent_complete: true,
  overlaps_complete: true,
  history_retention_gap: false,
}
const overview = decodeOverview(overviewEnvelope)
assert.equal(overview.current.length, 2)
assert.equal(overview.recent[1].item.state, 'finished')
assert.equal(overview.overlaps[0].reasons[0].right_path, 'src/**')
assert.equal(overview.overlaps[0].reasons_complete, false)
assert.equal(overview.overlaps_complete, true)
assert.throws(() => decodeOverview({ ...overviewEnvelope, current: [item, finishedItem] }), InvalidStandupResponse, 'current overview work cannot contain finished items')
assert.throws(() => decodeOverview({ ...overviewEnvelope, recent: [...overviewEnvelope.recent].reverse() }), InvalidStandupResponse, 'recent outcomes must remain newest first')
assert.throws(() => decodeOverview({ ...overviewEnvelope, recent: [{ ...overviewEnvelope.recent[1], item }] }), InvalidStandupResponse, 'completion outcomes must carry a finished snapshot')
assert.throws(() => decodeOverview({ ...overviewEnvelope, recent: [{ ...overviewEnvelope.recent[0], at: overviewEnvelope.recent_since - 1 }] }), InvalidStandupResponse, 'outcomes must stay inside the declared window')
assert.throws(() => decodeOverview({ ...overviewEnvelope, overlaps: [{ left: item, right: finishedItem, reasons: overviewEnvelope.overlaps[0].reasons }] }), InvalidStandupResponse, 'overlap work must belong to the current overview')
assert.throws(() => decodeOverview({ ...overviewEnvelope, overlaps: [{ left: item, right: activeItem, reasons: [] }] }), InvalidStandupResponse, 'an overlap needs a concrete path reason')
assert.throws(() => decodeOverview({ ...overviewEnvelope, overlaps: [{ ...overviewEnvelope.overlaps[0], reasons: Array(5).fill(overviewEnvelope.overlaps[0].reasons[0]) }] }), InvalidStandupResponse, 'detected overlap witnesses are server-bounded')
assert.throws(() => decodeOverview({ ...overviewEnvelope, overlaps: [{ ...overviewEnvelope.overlaps[0], reasons_complete: 'yes' }] }), InvalidStandupResponse)
assert.throws(() => decodeOverview({ ...overviewEnvelope, overlaps_complete: 'yes' }), InvalidStandupResponse)
assert.throws(() => decodeOverview({ ...overviewEnvelope, current_complete: false, overlaps_complete: true }), InvalidStandupResponse, 'an overlap slice cannot be complete when current work was clipped')
assert.throws(() => decodeOverview({ ...overviewEnvelope, history_retention_gap: true, recent_complete: true }), InvalidStandupResponse, 'retained-history gaps make the requested recent window incomplete')

const coordinationRequest = {
  id: coordinationId,
  requester: { token: 'tok_requester', agent: 'planner', session: 'session-requester', owner: 'github:github.com:303', login: 'request-human' },
  proposal: { work_id: null, source: null, request_key: 'native-request-1', title: `Coordinate ${malicious}`, scope: ['src/lib.rs'] },
  targets: [{
    work_id: relatedId,
    lead: { token: 'tok_target', agent: 'maintainer', session: 'session-target', owner: 'github:github.com:404', login: 'target-human' },
    generation: 4,
    reasons: [{ your_path: 'src/lib.rs', their_path: 'src/**' }],
    reasons_complete: false,
  }],
  status: 'responded',
  acknowledgements: [{
    target_work_id: relatedId,
    agent: { token: 'tok_target', agent: 'maintainer', session: 'session-target', owner: 'github:github.com:404', login: 'target-human' },
    generation: 4,
    through_seq: 20,
    acknowledged_at: 1_792_440_100,
    seq: 21,
  }],
  acknowledgements_complete: false,
  responses: [{
    id: `crs_${'2'.repeat(32)}`,
    operation_id: 'operation-1',
    target_work_id: relatedId,
    responder: { token: 'tok_target', agent: 'maintainer', session: 'session-target', owner: 'github:github.com:404', login: 'target-human' },
    target_generation: 4,
    kind: 'propose_split',
    note: malicious,
    created_at: 1_792_440_200,
    seq: 22,
  }],
  responses_complete: false,
  created_at: 1_792_440_000,
  updated_at: 1_792_440_200,
  created_seq: 20,
  updated_seq: 22,
}
const coordinationEnvelope = {
  from: 0,
  through: 22,
  next_after: 22,
  requests: [coordinationRequest],
  complete: true,
  retention_gap: false,
}
const coordination = decodeCoordinationBriefing(coordinationEnvelope)
assert.equal(coordination.requests[0].proposal.request_key, 'native-request-1')
assert.equal(coordination.requests[0].targets[0].reasons[0].their_path, 'src/**')
assert.equal(coordination.requests[0].targets[0].reasons_complete, false)
assert.equal(coordination.requests[0].acknowledgements[0].through_seq, 20)
assert.equal(coordination.requests[0].acknowledgements_complete, false)
assert.equal(coordination.requests[0].responses[0].target_generation, 4)
assert.equal(coordination.requests[0].responses_complete, false)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, status: 'sent' }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, targets: [] }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, targets: [{ ...coordinationRequest.targets[0], reasons_complete: 'yes' }] }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, targets: [{ ...coordinationRequest.targets[0], reasons: Array(5).fill(coordinationRequest.targets[0].reasons[0]) }] }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, acknowledgements_complete: 'yes' }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, responses_complete: 'yes' }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [{ ...coordinationRequest, responses: [{ ...coordinationRequest.responses[0], target_work_id: otherRelatedId }] }] }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, next_after: 23 }), InvalidStandupResponse)
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, next_after: 21 }), InvalidStandupResponse, 'the continuation cursor is bound to the last returned version')
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, requests: [], next_after: 0, complete: false }), InvalidStandupResponse, 'an incomplete page must advance with at least one request')
assert.throws(() => decodeCoordinationBriefing({ ...coordinationEnvelope, retention_gap: 'no' }), InvalidStandupResponse)

assert.equal(briefingPath(), '/v1/hub/standup/briefing?limit=50')
assert.equal(
  briefingPath({ after: 12, workAfter: workId, workDone: false, through: 99, limit: 25 }),
  `/v1/hub/standup/briefing?after=12&work_after=${workId}&work_done=false&through=99&limit=25`,
  'continuation filters have a stable order and preserve the fixed watermark',
)
assert.equal(
  briefingPath({ after: 12, workDone: true, through: 99, limit: 25 }),
  '/v1/hub/standup/briefing?after=12&work_done=true&through=99&limit=25',
  'an exhausted empty work snapshot cannot restart while events continue',
)
assert.throws(() => briefingPath({ limit: 101 }), RangeError)
assert.equal(overviewPath(), '/v1/hub/standup/overview?limit=100')
assert.equal(overviewPath({ recentSince: 1_792_430_000, limit: 25 }), '/v1/hub/standup/overview?recent_since=1792430000&limit=25')
assert.throws(() => overviewPath({ limit: 101 }), RangeError)
assert.equal(coordinationPath(), '/v1/hub/standup/coordination?limit=100')
assert.equal(coordinationPath({ after: 22, through: 40, limit: 10 }), '/v1/hub/standup/coordination?after=22&through=40&limit=10')
assert.equal(coordinationPath({ activeOnly: true, limit: 10 }), '/v1/hub/standup/coordination?active_only=true&limit=10')
assert.throws(() => coordinationPath({ limit: 101 }), RangeError)
assert.equal(workHref(workId), `#work/${workId}`)
assert.equal(workIdFromHash(`#work/${workId}`), workId)
assert.equal(workIdFromHash(`#coordination/work/${workId}`), workId)
assert.equal(workIdFromHash('#coordination/work/%E0%A4%A'), null)
assert.equal(workIdFromHash('#coordination/work/not-an-id'), null)

assert.equal(safeForgeIssueHref(item.source), 'https://github.com/acme/widgets/issues/42')
assert.equal(safeCommitHref(item.source, 'c'.repeat(40)), `https://github.com/acme/widgets/commit/${'c'.repeat(40)}`)
assert.equal(safeForgeIssueHref({ ...item.source, host: 'attacker.example' }), null)
assert.equal(safeForgeIssueHref({ ...item.source, issue: 'https://attacker.example/' }), null)
assert.equal(safeCommitHref({ ...item.source, repository: 'acme/widgets/../../../escape' }, 'c'.repeat(40)), null)

assert.throws(() => decodeWorkItem({ ...item, id: relatedId }, workId), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, claim_status: 'available' }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, participants: item.participants.map((participant) => ({ ...participant, role: 'helper' })) }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, lead_agent: 'not-the-lead-agent' }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, title: 'é'.repeat(251) }), InvalidStandupResponse, 'bounds use UTF-8 bytes')
assert.throws(() => decodeWorkItem({ ...item, scope: Array(257).fill('src/lib.rs') }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, depends_on: [relatedId], related_work: [relatedId] }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, current: { ...item.current, source_refs: Array(101).fill(item.current.source_refs[0]) } }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, scope: ['../outside'] }), InvalidStandupResponse)
assert.throws(() => decodeWorkItem({ ...item, current: { ...item.current, source_refs: [{ kind: 'commit', oid: 'not-a-commit', path: null }] } }), InvalidStandupResponse)
assert.throws(() => decodeBriefing({ ...envelope, events: [{ ...envelope.events[0], seq: 13 }] }), InvalidStandupResponse)
assert.throws(() => decodeBriefing({ ...envelope, next_after: 13 }), InvalidStandupResponse)
assert.throws(() => decodeBriefing({ ...envelope, items: Array(101).fill(item) }), InvalidStandupResponse)
assert.throws(() => decodeBriefing({ ...envelope, next_work_done: 'yes' }), InvalidStandupResponse)
for (const field of ['last_checkin_at', 'created_at', 'updated_at']) {
  assert.throws(() => decodeWorkItem({ ...item, [field]: Number.MAX_SAFE_INTEGER }), InvalidStandupResponse, `${field} must be representable as a Date`)
}
for (const field of ['current', 'blocker']) {
  assert.throws(() => decodeWorkItem({ ...item, [field]: { ...item[field], at: Number.MAX_SAFE_INTEGER } }), InvalidStandupResponse)
}
assert.throws(() => decodeBriefing({ ...envelope, events: [{ ...envelope.events[0], at: Number.MAX_SAFE_INTEGER }] }), InvalidStandupResponse)
assert.equal(decodeWorkItem({ ...item, last_checkin_at: 8_640_000_000_000 }).last_checkin_at, 8_640_000_000_000, 'the Date boundary remains representable')
assert.equal(decodeWorkItem({ ...item, generation: Number.MAX_SAFE_INTEGER }).generation, Number.MAX_SAFE_INTEGER, 'sequence-like values are not restricted to timestamps')

const component = await readFile(new URL('../src/components/Standup.tsx', import.meta.url), 'utf8')
assert.doesNotMatch(component, /dangerouslySetInnerHTML|\binnerHTML\b/, 'the stand-up renders teammate text as React children')
assert.doesNotMatch(component, /\/standup\/(?:ack|start|checkpoint)/, 'the read-only dashboard has no mutation endpoint')
assert.doesNotMatch(component, /\/coordination\/(?:ack|reply|resolve|withdraw)/, 'the coordination queue has no mutation endpoint')
assert.match(component, /Loaded <time/, 'the manually fetched briefing identifies its own freshness')
assert.match(component, /Lead at this change/, 'historical events label their snapshot lead as historical')
for (const label of ['Working now', 'Recently finished', 'Needs coordination']) assert.match(component, new RegExp(label), `${label} remains a primary overview tab`)
assert.match(component, /See who received each request and how they replied/, 'the queue explains request receipt and replies')
assert.match(component, /not proof of a source conflict or request delivery/, 'detected overlap does not imply request delivery')
assert.match(component, /Opening this page does not acknowledge a request/, 'dashboard reads do not imply request receipt')
assert.match(component, /Reported finished does not confirm verification, review, or merge/, 'finished work stays distinct from verification and landing')
assert.match(component, /const POLL_INTERVAL_MS = 15_000/, 'live overview refresh uses the bounded polling interval')
assert.match(component, /document\.hidden/, 'background polling pauses while the document is hidden')
assert.match(component, /window\.setTimeout/, 'polls schedule after prior requests complete instead of overlapping intervals')
assert.doesNotMatch(component, /setInterval/, 'polling cannot queue overlapping interval requests')
assert.match(component, /page === 0 && query\.through === undefined/, 'only the fresh coordination first page polls')
assert.match(component, /Fixed history snapshot through event/, 'paged coordination results disclose their fixed watermark')
assert.match(component, /Showing data loaded at/, 'refresh failures disclose the age of retained data')
const hero = await readFile(new URL('../src/components/Hero.tsx', import.meta.url), 'utf8')
assert.match(hero, /No live file-intent activity/, 'the legacy file-intent hero does not claim durable work is quiet')
assert.doesNotMatch(hero, /All quiet/, 'the legacy file-intent hero does not describe all coordination work')

const uiRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const vite = await createServer({ root: uiRoot, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true } })
try {
  const { WorkMetadata, WorkBoard, ChangeTable, CoordinationList, CoordinationRequestList, FinishedList, WorkTags, WorkClaimWarning } = await vite.ssrLoadModule('/src/components/Standup.tsx')
  const boardItems = ['active', 'blocked', 'handover', 'paused', 'finished'].map((state, index) => ({
    ...reviewedItem, id: `wrk_${String(index + 1).repeat(32)}`, state,
    title: `${state} task ${malicious}`, blocker: state === 'blocked' ? reviewedItem.blocker : null,
  }))
  const boardHtml = renderToStaticMarkup(React.createElement(WorkBoard, { items: boardItems, pageItems: 5, snapshotDone: true, moreHistory: false, showFinished: true }))
  for (const task of boardItems) {
    const lane = boardHtml.match(new RegExp(String.raw`<section[^>]*class="[^"]*shared-work-lane is-${task.state}[^"]*"[^>]*>([\s\S]*?)</section>`))
    assert.ok(lane, `the ${task.state} lane is present`)
    assert.match(lane[1], new RegExp(`href="#work/${task.id}"`), 'each task belongs to its reported-state lane')
    assert.match(lane[1], /builder/)
    assert.match(lane[1], /takeover-human/, 'the human lead remains explicit on task cards')
    assert.match(lane[1], /starter/)
    assert.match(lane[1], /security-review/, 'helpers and reviewers remain visible on task cards')
    if (task.state === 'finished') assert.doesNotMatch(lane[1], /still assigned|>Assigned</)
  }
  assert.doesNotMatch(boardHtml, /<img src=x/, 'task titles and next steps remain escaped in the board')
  for (const claim_status of ['active', 'stale', 'unknown']) {
    const finished = { ...decoded.items[0], state: 'finished', claim_status }
    const statusHtml = renderToStaticMarkup(React.createElement(React.Fragment, null, React.createElement(WorkTags, { item: finished }), React.createElement(WorkClaimWarning, { item: finished })))
    assert.match(statusHtml, /Reported finished/)
    assert.doesNotMatch(statusHtml, /Assigned|still assigned|Check-in unknown|No recent check-in/, 'finished detail must not imply an assignment is still held')
  }
  assert.match(boardHtml, /still assigned/, 'current work keeps stale assignment ownership visible')
  const finishedHtml = renderToStaticMarkup(React.createElement(FinishedList, { outcomes: [overview.recent[1]] }))
  assert.match(finishedHtml, /Reported finished/)
  assert.match(finishedHtml, /Document the release boundary/)
  assert.doesNotMatch(finishedHtml, /still assigned|>Assigned</, 'recently finished work does not imply an active assignment')
  const incompleteFinished = renderToStaticMarkup(React.createElement(FinishedList, { outcomes: [], complete: false }))
  assert.match(incompleteFinished, /leaves this window incomplete/)
  assert.doesNotMatch(incompleteFinished, /No work was reported finished/, 'an incomplete recent window cannot claim an empty result')
  const coordinationHtml = renderToStaticMarkup(React.createElement(CoordinationList, { overlaps: overview.overlaps, complete: true }))
  assert.match(coordinationHtml, /Why this needs attention/)
  assert.match(coordinationHtml, /Declared scopes overlap/)
  assert.match(coordinationHtml, /src\/lib\.rs/)
  assert.match(coordinationHtml, /src\/\*\*/)
  assert.match(coordinationHtml, /Additional overlapping paths omitted; inspect both full declared scopes\./)
  assert.match(coordinationHtml, new RegExp(`href="#work/${workId}"`))
  assert.match(coordinationHtml, new RegExp(`href="#work/${activeId}"`))
  assert.doesNotMatch(coordinationHtml, /<img src=x/, 'coordination reasons and work titles remain escaped')
  const incompleteCoordination = renderToStaticMarkup(React.createElement(CoordinationList, { overlaps: [], complete: false }))
  assert.match(incompleteCoordination, /snapshot is incomplete/)
  assert.doesNotMatch(incompleteCoordination, /No declared-scope overlaps need coordination/)
  const filteredCoordination = renderToStaticMarkup(React.createElement(CoordinationList, { overlaps: [], complete: true, filtered: true }))
  assert.match(filteredCoordination, /match your search/)
  assert.doesNotMatch(filteredCoordination, /need coordination in this snapshot/)
  const requestHtml = renderToStaticMarkup(React.createElement(CoordinationRequestList, { requests: coordination.requests }))
  assert.match(requestHtml, /^<div[^>]*><details/, 'directed requests render as compact native disclosures')
  assert.match(requestHtml, /<summary[^>]*>/, 'request stage and overlap evidence remain scannable before expansion')
  assert.match(requestHtml, /Replied/)
  assert.match(requestHtml, /Requested by planner · request-human/)
  assert.match(requestHtml, /planner → maintainer/)
  assert.match(requestHtml, /Received/)
  assert.match(requestHtml, /Proposed a scope split/)
  assert.match(requestHtml, /Additional overlapping paths omitted; inspect full declared scope\./)
  assert.match(requestHtml, /Some receipts and replies are omitted/)
  assert.match(requestHtml, /displayed target history is incomplete/)
  assert.match(requestHtml, new RegExp(`href="#work/${relatedId}"`))
  assert.doesNotMatch(requestHtml, /<img src=x/, 'request titles and response notes remain escaped')
  const pendingRequest = { ...coordination.requests[0], id: `crq_${'3'.repeat(32)}`, status: 'pending', acknowledgements: [], acknowledgements_complete: true, responses: [], responses_complete: true }
  const awaitingReceiptHtml = renderToStaticMarkup(React.createElement(CoordinationRequestList, { requests: [pendingRequest] }))
  assert.match(awaitingReceiptHtml, /Awaiting receipt/)
  const acknowledgedRequest = { ...pendingRequest, acknowledgements: coordination.requests[0].acknowledgements }
  const awaitingReplyHtml = renderToStaticMarkup(React.createElement(CoordinationRequestList, { requests: [acknowledgedRequest] }))
  assert.match(awaitingReplyHtml, /Awaiting replies/)
  for (const status of ['resolved', 'withdrawn']) {
    const terminalHtml = renderToStaticMarkup(React.createElement(CoordinationRequestList, { requests: [{ ...coordination.requests[0], id: `crq_${status === 'resolved' ? '4' : '5'}`.padEnd(36, status === 'resolved' ? '4' : '5'), status }] }))
    assert.match(terminalHtml, new RegExp(status === 'resolved' ? 'Resolved' : 'Withdrawn'))
  }
  const changesHtml = renderToStaticMarkup(React.createElement(ChangeTable, { events: decoded.events }))
  assert.match(changesHtml, /Paused/)
  assert.match(changesHtml, /Lead at this change: takeover-human/)
  assert.doesNotMatch(changesHtml, /still assigned|Assigned|No recent check-in/, 'history cannot present read-time claim freshness as historical state')
  const warnings = []
  const originalError = console.error
  let html
  try {
    console.error = (...values) => warnings.push(values.map(String).join(' '))
    html = renderToStaticMarkup(React.createElement(WorkMetadata, { item: decoded.items[0] }))
  } finally {
    console.error = originalError
  }
  assert.match(html, /Depends on/)
  assert.match(html, new RegExp(`href="#work/${relatedId}"`))
  assert.match(html, /Related work/)
  assert.match(html, new RegExp(`href="#work/${otherRelatedId}"`))
  assert.match(html, /Creator/)
  assert.match(html, /original-creator/)
  assert.match(html, /Current lead/)
  assert.match(html, /takeover-human/)
  assert.doesNotMatch(html, /original-creator · builder/, 'creator identity is never paired with the takeover agent')
  assert.doesNotMatch(html, /<img src=x/, 'React escapes untrusted next-action text')
  assert.doesNotMatch(warnings.join('\n'), /Sum of column `span`/, 'responsive description spans do not overflow')
} finally {
  await vite.close()
}

console.log('stand-up decoder and navigation checks passed')
