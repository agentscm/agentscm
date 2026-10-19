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
  currentLeadParticipant,
  decodeBriefing,
  decodeWorkItem,
  safeCommitHref,
  safeForgeIssueHref,
  workHref,
  workIdFromHash,
} from '../src/standup.ts'

const workId = `wrk_${'a'.repeat(32)}`
const relatedId = `wrk_${'b'.repeat(32)}`
const otherRelatedId = `wrk_${'d'.repeat(32)}`
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
assert.equal(workHref(workId), `#coordination/work/${workId}`)
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

const component = await readFile(new URL('../src/components/Standup.tsx', import.meta.url), 'utf8')
assert.doesNotMatch(component, /dangerouslySetInnerHTML|\binnerHTML\b/, 'the stand-up renders teammate text as React children')
assert.doesNotMatch(component, /\/standup\/(?:ack|start|checkpoint)/, 'the read-only dashboard has no mutation endpoint')
assert.match(component, /Briefing fetched/, 'the manually fetched briefing identifies its own freshness')
assert.match(component, /Lead at this change/, 'historical events label their snapshot lead as historical')
const hero = await readFile(new URL('../src/components/Hero.tsx', import.meta.url), 'utf8')
assert.match(hero, /No live file-intent activity/, 'the legacy file-intent hero does not claim durable work is quiet')
assert.doesNotMatch(hero, /All quiet/, 'the legacy file-intent hero does not describe all coordination work')

const uiRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const vite = await createServer({ root: uiRoot, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true } })
try {
  const { WorkMetadata } = await vite.ssrLoadModule('/src/components/Standup.tsx')
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
  assert.match(html, new RegExp(`href="#coordination/work/${relatedId}"`))
  assert.match(html, /Related work/)
  assert.match(html, new RegExp(`href="#coordination/work/${otherRelatedId}"`))
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
