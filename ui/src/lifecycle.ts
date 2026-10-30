// A task's lifecycle in six steps: Claimed · Working · Coordination · Checks · Merge · Finished.
// Each step uses the hub's own word, links to its evidence when there is some, and carries a proof rung.
// A step whose input is loading says "Checking…"; one whose read failed says "Couldn’t load". Neither is a pass.
// Pure: no DOM, no fetches.
//
// API for other packages:
//   LifecycleStep      publications.ts Step + { href?, proof, proofTip? }
//   LifecycleInput     the task plus each read it uses: null = loading, 'failed' = the read failed, 'none' = 404
//   lifecycle(input)   always six steps, in order
//   Render with <LifecycleStrip steps=… /> from LifecycleStrip.tsx.
import type { ChangeRecordSummaryV1 } from './changeRecord.ts'
import { who } from './nextStep.ts'
import { STATE, lead } from './pages/workParts.tsx'
import { backedTone, proofOfChangeRecord, runProof, type ProofLevel } from './proof.ts'
import { headline, httpsOnly, outcomeStep, type Publication, type Step } from './publications.ts'
import { runStateLabel, runStateTone, type RunView } from './skills.ts'
import type { CoordinationRequest, WorkItem, WorkOverview } from './standup.ts'
import type { PullRequest } from './types.ts'
import { relative } from './ui.tsx'

export interface LifecycleStep extends Step {
  /** Where the evidence is: a hash route, an in-page anchor (`#agent-reports`) or an https link. */
  href?: string
  /** The rung behind the word; null when the hub itself knows the answer (an open task is not finished) and no badge is drawn. */
  proof: ProofLevel | null
  /** Replaces the rung's default tooltip, e.g. "Observed by AgentSCM on the agent’s computer." */
  proofTip?: string
  /** Which system records this step; shown in the step's tooltip. */
  how?: string
}

export interface LifecycleInput {
  item: WorkItem
  overview: WorkOverview | null
  requests: CoordinationRequest[] | null | 'failed'
  skillRuns: RunView[] | null | 'failed'
  changeRecord: ChangeRecordSummaryV1 | 'none' | null | 'failed'
  publications: Publication[] | null | 'failed'
  pulls: PullRequest[] | null | 'failed'
  /** Milliseconds; defaults to now. Only the Claimed step's age reads it. */
  now?: number
}

type Base = Pick<Step, 'key' | 'label' | 'icon'>
const BASE = {
  claimed: { key: 'claimed', label: 'Claimed', icon: 'person-check' },
  working: { key: 'working', label: 'Working', icon: 'play-circle' },
  coordination: { key: 'coordination', label: 'Coordination', icon: 'people' },
  checks: { key: 'checks', label: 'Checks', icon: 'clipboard2-check' },
  merge: { key: 'merge', label: 'Merge', icon: 'sign-merge-right' },
  finished: { key: 'finished', label: 'Finished', icon: 'flag' },
} satisfies Record<string, Base>

const checking = (b: Base): LifecycleStep => ({ ...b, tone: 'muted', word: 'Checking…', proof: 'unknown' })
const failed = (b: Base): LifecycleStep => ({ ...b, tone: 'muted', word: 'Couldn’t load', proof: 'unknown' })
/** null → Checking…, 'failed' → Couldn’t load, otherwise undefined (the value is there). */
const pendingRead = (v: unknown, b: Base): LifecycleStep | undefined => (v === null ? checking(b) : v === 'failed' ? failed(b) : undefined)

const backed = backedTone

const isPending = (r: CoordinationRequest) => r.status !== 'resolved' && r.status !== 'withdrawn'

function coordination(item: WorkItem, overview: WorkOverview | null, requests: LifecycleInput['requests']): LifecycleStep {
  const b = BASE.coordination
  if (!overview) return checking(b)
  const pair = overview.overlaps.find((o) => o.left.id === item.id || o.right.id === item.id)
  if (pair) return { ...b, tone: 'warn', word: `Overlaps “${(pair.left.id === item.id ? pair.right : pair.left).title}”`, proof: 'observed', href: '#inbox' }
  const wait = pendingRead(requests, b)
  if (wait) return wait
  const names = (r: CoordinationRequest) => r.proposal.work_id === item.id || r.targets.some((t) => t.work_id === item.id)
  if ((requests as CoordinationRequest[]).some((r) => isPending(r) && names(r))) return { ...b, tone: 'warn', word: 'Request waiting', proof: 'observed', href: '#inbox' }
  if (item.scope.length === 0) return { ...b, tone: 'muted', word: 'Scope not declared', proof: 'unknown', detail: 'Overlap checks can’t cover a task that didn’t say which files it will change.' }
  if (overview.overlaps_complete) return { ...b, tone: 'ok', word: 'No overlap', proof: 'observed' }
  return { ...b, tone: 'muted', word: 'Unknown', proof: 'unknown' }
}

const CHANGE_WORD = { passed: 'Passed', failed: 'Failed', incomplete: 'Incomplete' } as const
const CHANGE_TONE = { passed: 'ok', failed: 'bad', incomplete: 'warn', unknown: 'muted' } as const

function checks(item: WorkItem, record: LifecycleInput['changeRecord'], runs: LifecycleInput['skillRuns']): LifecycleStep {
  const b = BASE.checks
  const wait = pendingRead(record, b)
  if (wait) return wait
  const action = record !== 'none' ? (record as ChangeRecordSummaryV1).latest_action : null
  if (action) {
    const rec = record as ChangeRecordSummaryV1
    const d = rec.decision
    const proof = proofOfChangeRecord(action.assurance)
    const word = action.result === 'unknown' ? (action.kind === 'execution' ? 'Ran, not verified' : 'Outcome unknown') : CHANGE_WORD[action.result]
    return {
      ...b,
      tone: backed(CHANGE_TONE[action.result], proof.level),
      word,
      proof: proof.level,
      proofTip: proof.tip,
      href: `#change-record/${encodeURIComponent(rec.work_id)}`,
      detail: d.status === 'approved' && d.author_kind === 'human' ? 'Decision: Approved by a person' : d.author_kind === 'agent' ? 'Decision: proposed by an agent, not human approval' : 'No decision recorded',
    }
  }
  if (!item.branch) return { ...b, tone: 'muted', word: 'None recorded', proof: 'unknown', detail: 'No branch reported, so checks can’t be matched.' }
  const waitRuns = pendingRead(runs, b)
  if (waitRuns) return waitRuns
  const run = (runs as RunView[]).filter((r) => r.branch === item.branch).sort((x, y) => y.updated_at - x.updated_at)[0]
  if (run) return { ...b, tone: backed(runStateTone(run), runProof(run.gates)), word: runStateLabel(run), proof: runProof(run.gates), href: `#skills/runs/${encodeURIComponent(run.run_id)}` }
  return { ...b, tone: 'muted', word: 'None recorded', proof: 'unknown', detail: `No check run or change record names ${item.branch}.` }
}

function merge(item: WorkItem, publications: LifecycleInput['publications'], pulls: LifecycleInput['pulls']): LifecycleStep {
  const b = BASE.merge
  // With a branch the hub looked and saw nothing, which it knows; without one it cannot match, so it cannot tell.
  const notSeen = (proof: ProofLevel | null): LifecycleStep => ({ ...b, tone: 'muted', word: 'Not seen', proof, detail: 'The hub hasn’t seen a pull request or published change for this task’s branch.' })
  if (!item.branch) return notSeen('unknown')
  const wait = pendingRead(publications, b)
  if (wait) return wait
  const p = (publications as Publication[]).find((x) => x.branch === item.branch)
  if (p) {
    const n = p.pr.number === null ? '' : ` #${p.pr.number}`
    const word = p.pr.state === 'merged' ? `Merged${n ? ` · PR${n}` : ''}` : p.pr.state === 'opened' ? `PR${n} open` : 'No pull request yet'
    return { ...b, tone: headline(p).tone, word, proof: 'observed', detail: `CI after merge: ${outcomeStep('ci', p).word}`, href: `#landed/${encodeURIComponent(p.id)}` }
  }
  const waitPulls = pendingRead(pulls, b)
  if (waitPulls) return waitPulls
  const pr = (pulls as PullRequest[]).find((x) => x.state === 'open' && x.head_ref === item.branch)
  if (pr) return { ...b, tone: 'brand', word: `PR #${pr.number} open`, proof: 'observed', href: httpsOnly(pr.html_url) ?? undefined }
  return notSeen(null)
}

/** Which system writes each step, from the MCP tools in docs/agents.md and the hub's own reads. */
const HOW: Record<keyof typeof BASE, string> = {
  claimed: 'The hub records it when the agent calls agentscm_start.',
  working: 'The agent reports it with agentscm_work_checkpoint. The hub does not check it.',
  coordination: 'The hub compares the files each task says it will change; requests come from agents (agentscm_check, agentscm_ask).',
  checks: 'A collector on the author’s computer records the change record; skill evidence is reported by the agent. The hub runs no checks.',
  merge: 'The hub reads it from GitHub or GitLab, as of its last sync.',
  finished: 'The agent reports it with agentscm_done or a checkpoint. It is not review, acceptance or a merge.',
}

/** The six steps of one task, always in order: Claimed, Working, Coordination, Checks, Merge, Finished. */
export function lifecycle(i: LifecycleInput): LifecycleStep[] {
  return steps(i).map((s) => ({ ...s, how: HOW[s.key as keyof typeof BASE] }))
}

function steps(i: LifecycleInput): LifecycleStep[] {
  const { item } = i
  const l = lead(item)
  const done = item.state === 'finished'
  // A finished task's old check-in is expected, so only unfinished work reads as quiet.
  const quiet = !done && item.claim_status !== 'active'
  const s = STATE[item.state]
  const claimProof: ProofLevel = item.created_at > 0 ? 'observed' : 'unknown'
  return [
    // The hub records the claim itself, so a claim in its work record is hub-backed; with no claim time there is nothing to back.
    { ...BASE.claimed, tone: backed('ok', claimProof), word: relative(item.created_at, i.now ?? Date.now()), detail: `Claimed by ${who(l.login, l.agent)}`, proof: claimProof, proofTip: claimProof === 'observed' ? 'Recorded by the hub when the task was claimed.' : undefined },
    // A finished task's Working step says the work ended; the Finished step already carries “Reported”.
    { ...BASE.working, tone: done ? 'muted' : quiet ? 'warn' : backed(s.tone, 'reported'), word: done ? 'Ended' : quiet ? 'Quiet' : s.label, detail: item.current?.summary ?? item.blocker?.summary ?? 'No progress report yet', proof: 'reported', href: '#agent-reports' },
    coordination(item, i.overview, i.requests),
    checks(item, i.changeRecord, i.skillRuns),
    merge(item, i.publications, i.pulls),
    done
      ? { ...BASE.finished, tone: backed('ok', 'reported'), word: 'Reported', proof: 'reported', detail: 'Finished means the agent said so. It may not be verified, reviewed or merged yet.' }
      : { ...BASE.finished, tone: 'muted', word: 'Not yet', proof: null },
  ]
}
