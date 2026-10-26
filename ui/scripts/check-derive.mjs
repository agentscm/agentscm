// Zero-dependency self-check for src/derive.ts (Node strips the types).
import assert from 'node:assert/strict'
import { announced, derive, notable, overlaps, specKey } from '../src/derive.ts'
import { InvalidSkillWorkflowResponse, decodeSkillWorkflowEnvelope, decodeSkillWorkflowListEnvelope, validSkillWorkflowRunId, validSkillWorkflowStepId } from '../src/skillWorkflow.ts'
import { fragmentTokenAllowed } from '../src/storage.ts'

// A `#token=` link may replace a stored token only on the demo hub.
assert.ok(fragmentTokenAllowed(null, false), 'nothing stored: adopt')
assert.ok(fragmentTokenAllowed('tok_stored', true), 'demo hub: adopt')
assert.ok(!fragmentTokenAllowed('tok_stored', false), 'a stored token stays outside the demo')

// The workflow decoder keeps historical status, applicability and admission distinct.
const workflow = {
  version: 1,
  availability: 'available',
  projection: {
    version: 1,
    run: {
      version: 1,
      run_id: 'run-1',
      plan_digest: `sha256:${'c'.repeat(64)}`,
      skill_digest: `sha256:${'d'.repeat(64)}`,
      source: { repository: '0123456789abcdef0123456789abcdef', revision: 'a'.repeat(64), policy_epoch: 4 },
      state: 'completed_with_omissions',
      steps: [
        {
          id: 'lint/rust', needs: [], required: true, state: 'succeeded', observations: [{ path: 'src/lib.rs', sha256: 'b'.repeat(64), bytes: 42 }], detail: 'source paths matched',
          execution: { attempt_id: 'attempt-1', generation: 2, state: 'finished', outcome: 'success', record_digest: 'e'.repeat(64) },
          review: {
            model_requested: 'fable', model_observed: 'claude-fable-5-1', status: 'failed', stop: 'findings', covered_paths: ['src/lib.rs'], unresolved: ['error path is untested'],
            findings: [{ path: 'src/lib.rs', start_line: 12, end_line: 13, title: 'Lost error context', detail: 'Preserve the original error before returning.', severity: 'error' }],
          },
        },
        { id: 'verify:source', needs: ['lint/rust'], required: false, state: 'blocked', observations: [], detail: 'capability unavailable' },
      ],
      admission: 'not_assessed',
    },
    present_applicability: { basis: 'current_authority_repository_policy_epoch_and_immutable_revision', state: 'stale', reason: 'policy epoch changed' },
    present_landing_binding: {
      basis: 'current_change_version_review_context_target_and_policy_epoch', state: 'current', reason: null,
      change_id: 'change-1', change_version: 3, context_digest: 'f'.repeat(64), target: 'main',
    },
  },
}
const decodedWorkflow = decodeSkillWorkflowEnvelope(workflow)
assert.equal(decodedWorkflow.availability, 'available')
assert.equal(decodedWorkflow.projection.run.state, 'completed_with_omissions', 'omissions never collapse into success')
assert.equal(decodedWorkflow.projection.run.steps[1].state, 'blocked', 'blocked optional work stays visible')
assert.equal(decodedWorkflow.projection.run.steps[0].execution.outcome, 'success', 'command result remains a distinct controller observation')
assert.equal(decodedWorkflow.projection.run.steps[0].review.model_observed, 'claude-fable-5-1')
assert.equal(decodedWorkflow.projection.run.steps[0].review.findings[0].start_line, 12)
assert.equal(decodedWorkflow.projection.present_applicability.state, 'stale', 'current applicability stays separate from historical state')
assert.equal(decodedWorkflow.projection.present_landing_binding.target, 'main', 'landing binding is decoded separately from source freshness')
assert.equal(decodedWorkflow.projection.run.admission, 'not_assessed', 'the read surface does not manufacture admission')
assert.deepEqual(decodeSkillWorkflowEnvelope({ version: 1, availability: 'unavailable', reason: 'native_workflow_not_configured' }), {
  version: 1,
  availability: 'unavailable',
  reason: 'native_workflow_not_configured',
})
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, admission: 'approved' } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, steps: Array(33).fill(workflow.projection.run.steps[0]) } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, steps: [{ ...workflow.projection.run.steps[0], review: { ...workflow.projection.run.steps[0].review, findings: Array(65).fill(workflow.projection.run.steps[0].review.findings[0]) } }] } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, steps: [{ ...workflow.projection.run.steps[0], review: { ...workflow.projection.run.steps[0].review, findings: [{ ...workflow.projection.run.steps[0].review.findings[0], start_line: 13, end_line: 12 }] } }] } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, steps: [{ ...workflow.projection.run.steps[0], review: { ...workflow.projection.run.steps[0].review, status: 'unknown' } }] } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, steps: [{ ...workflow.projection.run.steps[0], review: { ...workflow.projection.run.steps[0].review, findings: [{ ...workflow.projection.run.steps[0].review.findings[0], severity: 'critical' }] } }] } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, present_applicability: { ...workflow.projection.present_applicability, basis: 'unknown' } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, present_landing_binding: { ...workflow.projection.present_landing_binding, context_digest: 'not-a-digest' } } }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope(workflow, 'another-run'), InvalidSkillWorkflowResponse)
const recentWorkflows = decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: [
  { run_id: 'run-new', revision: 'a'.repeat(64), state: 'failed', name: 'Required review', sequence: '9223372036854775807' },
  { run_id: 'run-old', revision: 'b'.repeat(64), state: 'succeeded', name: 'Required review', sequence: '9223372036854775806' },
], next_cursor: '9223372036854775806' })
assert.equal(recentWorkflows.availability, 'available')
assert.equal(recentWorkflows.runs[0].run_id, 'run-new')
assert.equal(recentWorkflows.next_cursor, '9223372036854775806')
assert.throws(() => decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: Array(17).fill({ run_id: 'run', revision: 'a'.repeat(64), state: 'succeeded', name: 'Review', sequence: '1' }), next_cursor: null }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: [
  { run_id: 'duplicate', revision: 'a'.repeat(64), state: 'succeeded', name: 'Review', sequence: '2' },
  { run_id: 'duplicate', revision: 'b'.repeat(64), state: 'failed', name: 'Review', sequence: '1' },
], next_cursor: null }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: [
  { run_id: 'run', revision: 'a'.repeat(64), state: 'succeeded', name: 'Review', sequence: '01' },
], next_cursor: null }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: [], next_cursor: '0' }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: [], next_cursor: '9223372036854775808' }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowListEnvelope({ version: 1, availability: 'available', runs: [{ run_id: 'run', revision: 'a'.repeat(64), state: 'succeeded', name: 'Review', sequence: '9999999999999999999' }], next_cursor: null }), InvalidSkillWorkflowResponse)
assert.throws(() => decodeSkillWorkflowEnvelope({ ...workflow, projection: { ...workflow.projection, run: { ...workflow.projection.run, steps: [workflow.projection.run.steps[0], { ...workflow.projection.run.steps[1], needs: ['missing'] }] } } }), InvalidSkillWorkflowResponse)
assert.ok(validSkillWorkflowRunId('run-1.ok'))
assert.ok(!validSkillWorkflowRunId('../run'))
assert.ok(!validSkillWorkflowRunId('a'.repeat(65)))
assert.ok(validSkillWorkflowStepId('lint/rust'))
assert.ok(validSkillWorkflowStepId('verify:source'))
assert.ok(!validSkillWorkflowStepId('lint step'))

assert.equal(specKey('src/dir/**'), 'src/dir/')
assert.equal(specKey('src/a.rs'), 'src/a.rs')
assert.ok(overlaps('src/a.rs', 'src/a.rs'))
assert.ok(overlaps('src/**', 'src/a/b.rs'))
assert.ok(overlaps('src/a/b.rs', 'src/**'))
assert.ok(overlaps('src/**', 'src/**'))
assert.ok(!overlaps('src/a.rs', 'src/b.rs'))
assert.ok(!overlaps('src/dir/**', 'src/dir'), 'a directory spec does not match the bare name')
assert.ok(!overlaps('src/dir/**', 'src/dir2/x'), 'prefix must end at a separator')

const agent = (token, name) => ({ token, name })
const intent = (id, login, a, paths, via = ['hook:claude']) => ({
  id, task: `task ${id}`, branch: null, machine: null, paths, via, created_at: 1, updated_at: 100,
  agent: a, owner: { login },
})
const session = (key, a, login, intentId, via, updated = 100) => ({ key, agent: a, owner: { login }, intent: intentId, task: null, via, created_at: 1, updated_at: updated })
const pr = (number, state, files) => ({ number, title: `pr ${number}`, state, draft: false, head_ref: 'h', base_ref: 'main', author: { login: 'z' }, files })
const ev = (id, at, kind, a, login, paths) => ({ seq: at, id, at, kind, agent: a, owner: { login }, data: { paths } })
const run = (intents, sessions, prs = [], events = [], now = 700) =>
  derive({ intents, intents_truncated: false, connected: true, pull_requests: prs }, { sessions, events }, now)

const A = agent('ta', 'claude-code')
const B = agent('tb', 'codex')
const C = agent('tc', 'cursor')

// Main scenario: three agents, one REST-announced intent, PRs and a warn-once story.
{
  const d = run(
    [intent('i1', 'maria', A, ['src/billing.rs', 'src/inv/**']), intent('i2', 'dev', B, ['src/billing.rs'], ['hook:codex']), intent('i3', 'sam', C, ['docs/**'], [])],
    [session('s1', A, 'maria', 'i1', ['hook:claude'], 150), session('s2', B, 'dev', 'i2', ['hook:codex'], 120)],
    [pr(1, 'open', ['src/inv/a.rs']), pr(2, 'open', ['README.md']), pr(3, 'closed', ['src/billing.rs'])],
    [ev('e3', 300, 'edit_repeated', A, 'maria', ['src/billing.rs']), ev('e2', 200, 'edit_denied', A, 'maria', ['src/billing.rs']), ev('e1', 100, 'edit_denied', B, 'dev', ['src/other.rs'])],
  )
  assert.equal(d.agents.length, 3, 'two sessions plus one REST-only intent')
  assert.deepEqual(d.agents.map((x) => x.id).sort(), ['intent:i3', 's1', 's2'])
  assert.deepEqual(d.groups.map((g) => g.owner), ['dev', 'maria', 'sam'])
  assert.equal(d.agents.find((x) => x.id === 's1').lastActive, 150)
  assert.deepEqual(d.contested.map((c) => c.path), ['src/billing.rs', 'src/other.rs'], 'a refused-only file follows the held ones')
  assert.equal(d.contested[0].holders.length, 2)
  assert.equal(d.contested[1].holders.length, 0)
  assert.equal(d.contested[0].refused.at, 200)
  assert.equal(d.contested[0].repeated.at, 300)
  const a = d.agents.find((x) => x.id === 's1')
  assert.ok(a.holdings.find((h) => h.spec === 'src/billing.rs').contested)
  assert.ok(!a.holdings.find((h) => h.spec === 'src/inv/**').contested)
  assert.equal(d.filesHeld, 3)
  assert.equal(d.prOverlaps.length, 1, 'only open PRs count')
  assert.equal(d.prOverlaps[0].pr.number, 1)
  assert.deepEqual(d.prOverlaps[0].with[0].files, ['src/inv/a.rs'])
  assert.equal(d.otherOpenPrs, 1)
}

// Mirror of the hub's same_agent: two HOOK sessions of one token are different agents and do refuse each other.
{
  const d = run(
    [intent('i1', 'maria', A, ['src/billing.rs']), intent('i2', 'maria', A, ['src/billing.rs'])],
    [session('s1', A, 'maria', 'i1', ['hook:claude']), session('s2', A, 'maria', 'i2', ['hook:claude'])],
  )
  assert.equal(d.agents.length, 2, 'one row per session')
  assert.equal(d.contested.length, 1, 'two hook sessions of one token on the same file are contested')
  assert.equal(d.contested[0].holders.length, 1, 'holders are listed once per agent identity')
}
// ...but a hook session and an MCP-only session of one token are the same agent.
{
  const d = run(
    [intent('i1', 'maria', A, ['src/billing.rs']), intent('i2', 'maria', A, ['src/billing.rs'], ['mcp'])],
    [session('s1', A, 'maria', 'i1', ['hook:claude']), session('s2', A, 'maria', 'i2', ['mcp'])],
  )
  assert.equal(d.contested.length, 0, 'hook + mcp-only of one token is one agent')
  assert.ok(d.agents.every((x) => x.holdings.every((h) => !h.contested)))
}
// Only exactly ["mcp"] counts as MCP-only; an intent with no via (REST) is a different agent.
{
  const mixed = run(
    [intent('i1', 'maria', A, ['src/billing.rs']), intent('i2', 'maria', A, ['src/billing.rs'], ['mcp', 'hook:claude'])],
    [session('s1', A, 'maria', 'i1', ['hook:claude']), session('s2', A, 'maria', 'i2', ['mcp', 'hook:claude'])],
  )
  assert.equal(mixed.contested.length, 1)
  const rest = run([intent('i1', 'maria', A, ['src/billing.rs']), intent('i2', 'maria', A, ['src/billing.rs'], [])], [session('s1', A, 'maria', 'i1', ['hook:claude'])])
  assert.equal(rest.contested.length, 1)
  assert.equal(rest.agents.find((x) => x.id === 'intent:i2').owner, 'maria')
}
// An MCP-only session still conflicts with other tokens.
{
  const d = run([intent('i1', 'maria', A, ['src/a.rs'], ['mcp']), intent('i2', 'dev', B, ['src/a.rs'], ['hook:codex'])], [])
  assert.equal(d.contested.length, 1)
}
// A directory spec held by two agents is itself the contested path.
{
  const d = run([intent('i1', 'maria', A, ['src/**']), intent('i2', 'dev', B, ['src/a/b.rs'])], [])
  assert.deepEqual(d.contested.map((c) => c.path), ['src/a/b.rs'])
}

// Handoffs: pending vs settled matched by request id; story-only files appear after the held ones.
{
  const hev = (id, at, kind, a, login, path, request, other) => ({
    seq: at, id, at, kind, agent: a, owner: { login },
    data: { request, paths: [path], note: 'please', ...(kind === 'handoff_request' ? { to: other } : { from: other }) },
  })
  const holder = { agent: 'claude-code', login: 'maria' }
  const req = { agent: 'codex', login: 'dev' }
  const d = run(
    [intent('i1', 'maria', A, ['src/billing.rs']), intent('i2', 'dev', B, ['src/billing.rs'], ['hook:codex'])],
    [session('s1', A, 'maria', 'i1', ['hook:claude']), session('s2', B, 'dev', 'i2', ['hook:codex'])],
    [],
    [
      hev('h6', 600, 'handoff_decline', A, 'maria', 'src/kept.rs', 'r_kept', req),
      hev('h5', 500, 'handoff_request', B, 'dev', 'src/kept.rs', 'r_kept', holder),
      hev('h4', 400, 'handoff_grant', A, 'maria', 'src/given.rs', 'r_other', req), // names another request: must not settle r_new
      hev('h3', 300, 'handoff_request', B, 'dev', 'src/billing.rs', 'r_new', holder),
      hev('h2', 200, 'handoff_grant', A, 'maria', 'src/given.rs', 'r_given', req),
      hev('h1', 100, 'handoff_request', B, 'dev', 'src/given.rs', 'r_given', holder),
      { seq: 50, id: 'd1', at: 50, kind: 'edit_denied', agent: B, owner: { login: 'dev' }, data: { paths: ['src/refused.rs'] } },
    ],
  )
  assert.deepEqual(d.contested.map((c) => c.path), ['src/billing.rs', 'src/given.rs', 'src/kept.rs', 'src/refused.rs'], 'held files first, then story-only files by path')
  const by = (p) => d.contested.find((c) => c.path === p)
  assert.equal(by('src/billing.rs').handoff.request, 'r_new')
  assert.equal(by('src/billing.rs').handoff.answer, undefined, 'a grant naming another request leaves this one pending')
  assert.equal(by('src/billing.rs').handoff.lapsed, undefined, 'a request younger than 1800 s is pending')
  assert.equal(by('src/billing.rs').handoff.from.owner, 'dev')
  assert.equal(by('src/billing.rs').handoff.to.name, 'claude-code')
  assert.equal(by('src/billing.rs').handoff.note, 'please')
  assert.deepEqual(by('src/given.rs').handoff.answer, { kind: 'granted', at: 200 })
  assert.deepEqual(by('src/kept.rs').handoff.answer, { kind: 'declined', at: 600 })
  assert.equal(by('src/kept.rs').holders.length, 0, 'a refused-only file appears with no holder')
  assert.equal(by('src/refused.rs').handoff, undefined)
  assert.equal(by('src/refused.rs').refused.at, 50)
  assert.equal(d.filesHeld, 1)
}
// An unanswered request older than 1800 s shows as lapsed; an answered one does not.
{
  const req = (at, request) => ({ seq: at, id: `r${at}`, at, kind: 'handoff_request', agent: B, owner: { login: 'dev' }, data: { request, paths: ['a.rs'], to: { agent: 'x', login: 'y' } } })
  const grant = { seq: 2, id: 'g', at: 2, kind: 'handoff_grant', agent: A, owner: { login: 'maria' }, data: { request: 'r1', paths: ['a.rs'] } }
  assert.equal(run([], [], [], [req(100, 'r1')], 1900).contested[0].handoff.lapsed, undefined, 'exactly 1800 s old is still pending')
  assert.equal(run([], [], [], [req(100, 'r1')], 1901).contested[0].handoff.lapsed, true)
  assert.equal(run([], [], [], [grant, req(100, 'r1')], 5000).contested[0].handoff.lapsed, undefined)
}
// A request without an id is never settled by an answer without one.
{
  const noId = { seq: 1, id: 'x', at: 1, kind: 'handoff_request', agent: B, owner: { login: 'dev' }, data: { request: '', paths: ['a.rs'], to: { agent: 'x', login: 'y' } } }
  const ans = { seq: 2, id: 'y', at: 2, kind: 'handoff_grant', agent: A, owner: { login: 'maria' }, data: { request: '', paths: ['a.rs'] } }
  assert.equal(run([], [], [], [ans, noId]).contested[0].handoff.answer, undefined)
}
// The decline reason travels with the answer; the hero picks the newest of the refusal and handoff stories.
{
  const D = { seq: 1, id: 'dec', at: 400, kind: 'handoff_decline', agent: A, owner: { login: 'maria' }, data: { request: 'r1', paths: ['a.rs'], reason: 'busy' } }
  const R = { seq: 2, id: 'req', at: 300, kind: 'handoff_request', agent: B, owner: { login: 'dev' }, data: { request: 'r1', paths: ['a.rs'], note: null, to: { agent: 'claude-code', login: 'maria' } } }
  const denied = (at) => ({ seq: at, id: `den${at}`, at, kind: 'edit_denied', agent: B, owner: { login: 'dev' }, data: { paths: ['b.rs'] } })
  assert.deepEqual(run([], [], [], [D, R]).contested[0].handoff.answer, { kind: 'declined', at: 400, reason: 'busy' })
  assert.equal(notable(run([], [], [], []).contested), null)
  assert.equal(notable(run([], [], [], [denied(500), D, R]).contested).kind, 'refusal', 'a newer refusal beats an older handoff answer')
  assert.equal(notable(run([], [], [], [denied(350), D, R]).contested).kind, 'handoff', 'the answer time counts, not the request time')
  assert.equal(notable(run([], [], [], [denied(100), R]).contested).c.path, 'a.rs')
  assert.equal(notable(run([], [], [], [denied(400), D, R]).contested).kind, 'handoff', 'a handoff in the same second as a refusal wins')
}
assert.equal(announced('task unknown'), null, "the hub's placeholder is no task")
assert.equal(announced('Fix invoice rounding'), 'Fix invoice rounding')
console.log('derive checks passed')
