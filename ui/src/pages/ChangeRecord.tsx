import { useEffect, useState, type ReactNode } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import {
  fetchChangeRecord,
  fetchConversationBody,
  fetchConversationPage,
  type ConversationAvailability,
  type ConversationEntry,
  type ConversationEventKind,
  type ChangeRecordActionKind,
  type ChangeRecordApplicability,
  type ChangeRecordAssurance,
  type ChangeRecordAvailability,
  type ChangeRecordCoverage,
  type ChangeRecordDecision,
  type ChangeRecordDecisionRef,
  type ChangeRecordDrift,
  type ChangeRecordOmission,
  type ChangeRecordOutcome,
  type ChangeRecordRecipeProvenance,
  type ChangeRecordResult,
  type ChangeRecordSource,
  type ChangeRecordSummaryV1,
  type ChangeRecordUnknown,
} from '../changeRecord.ts'
import { searchStandupWork, validWorkId, workHref, type WorkState } from '../standup.ts'
import { StatePill } from './Work.tsx'
import { agentLabel, Ago, Box, CopyButton, Empty, Explain, Icon, Notice, PageHead, Path, Paths, Pill, Reported, Skeleton, relative, type Tone } from '../ui.tsx'
import { READING_CHANGES_NOTHING } from '../copy.ts'
import { ProofBadge } from '../Proof.tsx'
import { backedTone, proofOfChangeRecord } from '../proof.ts'
import { Combobox, type ComboResult } from '../Combobox.tsx'
import { hashPath, Pager, useHashCursor } from '../Pager.tsx'
import { usePoll } from '../usePoll.ts'

type Status = { label: string; tone: Tone; icon: string; explanation: string }

const ACTION_KIND: Record<ChangeRecordActionKind, Status> = {
  execution: { label: 'Execution', tone: 'muted', icon: 'terminal', explanation: 'This receipt records whether a command ran. It does not establish that a check passed.' },
  verification: { label: 'Verification', tone: 'brand', icon: 'clipboard2-check', explanation: 'This receipt matched a configured verification recipe exactly.' },
}

const OUTCOME: Record<ChangeRecordOutcome, Status> = {
  succeeded: { label: 'Succeeded', tone: 'ok', icon: 'check-circle', explanation: 'The recorded action completed successfully. Verification result is shown separately.' },
  failed: { label: 'Failed', tone: 'bad', icon: 'x-octagon', explanation: 'The recorded action failed.' },
  incomplete: { label: 'Incomplete', tone: 'warn', icon: 'exclamation-triangle', explanation: 'The action did not finish every required step.' },
  unknown: { label: 'Outcome unknown', tone: 'warn', icon: 'question-circle', explanation: 'The retained record does not establish whether the action completed.' },
}

const RESULT: Record<ChangeRecordResult, Status> = {
  passed: { label: 'Passed', tone: 'ok', icon: 'check-circle', explanation: 'The configured verification reported a passing result. Applicability, assurance and coverage remain separate.' },
  failed: { label: 'Failed verification', tone: 'bad', icon: 'x-octagon', explanation: 'The configured verification reported a failing result.' },
  incomplete: { label: 'Verification incomplete', tone: 'warn', icon: 'exclamation-triangle', explanation: 'The configured verification did not complete every required observation.' },
  unknown: { label: 'Result unknown', tone: 'warn', icon: 'question-circle', explanation: 'The retained record does not establish a verification result.' },
}

const RECIPE_PROVENANCE: Record<ChangeRecordRecipeProvenance, Status> = {
  observed_native_input: { label: 'Observed command', tone: 'muted', icon: 'terminal', explanation: 'The collector observed this command or tool input. It did not match an approved verification recipe.' },
  configured_exact_match: { label: 'Configured recipe match', tone: 'brand', icon: 'journal-check', explanation: 'The observed action exactly matched a configured verification recipe. Recipe completeness remains partial because the digest alone is not full provenance.' },
}

const APPLICABILITY: Record<ChangeRecordApplicability, Status> = {
  current: { label: 'Current', tone: 'ok', icon: 'check2-circle', explanation: 'The latest action still applies to the current recorded subject.' },
  stale: { label: 'Stale', tone: 'bad', icon: 'clock-history', explanation: 'The latest action was recorded for an older subject or setup and is historical.' },
  unknown: { label: 'Freshness unknown', tone: 'warn', icon: 'question-circle', explanation: 'The record cannot establish whether the latest action still applies.' },
}

const ASSURANCE: Record<ChangeRecordAssurance, Status> = {
  caller_asserted: { label: 'Caller asserted', tone: 'warn', icon: 'person-check', explanation: 'The caller supplied this claim. It is not independent verification.' },
  observed_trusted_local: { label: 'Observed locally', tone: 'brand', icon: 'laptop', explanation: 'AgentSCM’s collector on the author’s own computer observed the action. The hub did not see it, and this is separate from independent verification.' },
  protected_execution: { label: 'Protected execution', tone: 'brand', icon: 'shield-check', explanation: 'The action ran in the configured protected execution boundary. The record is unsigned and the hub does not authenticate the collector, so it is not independent verification.' },
  independent_verification: { label: 'Independently verified', tone: 'brand', icon: 'patch-check', explanation: 'The record identifies independent verification for this action.' },
}

const AVAILABILITY: Record<ChangeRecordAvailability, Status> = {
  local: { label: 'Local', tone: 'brand', icon: 'laptop', explanation: 'The content is retained locally and may not be readable from this Hub.' },
  shared: { label: 'Shared', tone: 'brand', icon: 'people', explanation: 'The content is retained in the configured shared store.' },
  external: { label: 'External', tone: 'brand', icon: 'box-arrow-up-right', explanation: 'The content is retained by an external system.' },
  mixed: { label: 'Mixed availability', tone: 'warn', icon: 'intersect', explanation: 'The record refers to content with different availability states.' },
  missing: { label: 'Missing', tone: 'bad', icon: 'file-earmark-x', explanation: 'The record points to content that is not available.' },
  expired: { label: 'Expired', tone: 'warn', icon: 'hourglass-bottom', explanation: 'The content is outside its retention window.' },
  deleted: { label: 'Deleted', tone: 'warn', icon: 'trash', explanation: 'The content was deleted under its retention policy.' },
  denied: { label: 'Access denied', tone: 'warn', icon: 'lock', explanation: 'The current viewer is not allowed to read the content.' },
  integrity_failure: { label: 'Integrity failed', tone: 'bad', icon: 'shield-x', explanation: 'The retained content did not pass its integrity check.' },
}

const COVERAGE: Record<ChangeRecordCoverage, Status> = {
  complete: { label: 'Complete', tone: 'ok', icon: 'check2-circle', explanation: 'The record reports complete coverage for this field.' },
  partial: { label: 'Partial', tone: 'warn', icon: 'circle-half', explanation: 'Some expected information is missing.' },
  unknown: { label: 'Unknown', tone: 'warn', icon: 'question-circle', explanation: 'Coverage cannot be established from the retained record.' },
}

const DECISION: Record<ChangeRecordDecision, Status> = {
  missing: { label: 'No decision', tone: 'warn', icon: 'dash-circle', explanation: 'No retained decision is linked to this record.' },
  proposed: { label: 'Decision proposed', tone: 'brand', icon: 'chat-square-text', explanation: 'A decision was proposed. Its author status is shown separately.' },
  approved: { label: 'Approved', tone: 'ok', icon: 'check2-circle', explanation: 'The retained decision has approved status. Its author status is shown separately.' },
  rejected: { label: 'Rejected', tone: 'bad', icon: 'x-circle', explanation: 'The retained decision rejected this subject.' },
  superseded: { label: 'Decision superseded', tone: 'warn', icon: 'arrow-up-right-circle', explanation: 'A newer decision replaced this retained decision.' },
}

const DRIFT: Record<ChangeRecordDrift, Status> = {
  none_observed: { label: 'No drift observed', tone: 'muted', icon: 'check2', explanation: 'The action did not record source, setup or recipe drift. This does not add assurance beyond the stated basis.' },
  source_changed: { label: 'Source changed', tone: 'bad', icon: 'git', explanation: 'The source changed while or after the action ran.' },
  setup_changed: { label: 'Setup changed', tone: 'bad', icon: 'tools', explanation: 'The recorded setup changed relative to the action.' },
  recipe_changed: { label: 'Recipe changed', tone: 'bad', icon: 'journal-code', explanation: 'The verification recipe changed relative to the action.' },
  unknown: { label: 'Drift unknown', tone: 'warn', icon: 'question-circle', explanation: 'The record cannot establish whether relevant inputs drifted.' },
}

const UNKNOWN_LABEL: Record<ChangeRecordUnknown, string> = {
  capture_coverage: 'Capture coverage',
  source_applicability: 'Source applicability',
  setup_identity: 'Setup identity',
  recipe_identity: 'Recipe identity',
  result: 'Verification result',
  usage: 'Usage',
  human_decision: 'Human decision',
}

const OMISSION_LABEL: Record<ChangeRecordOmission, string> = {
  untracked_content: 'Untracked content',
  ignored_content: 'Ignored content',
  working_tree_content: 'Working-tree content',
  index_state: 'Index state',
  submodule_content: 'Submodule content',
  symlink_target: 'Symbolic-link target',
}

const COMPLETENESS_LABEL: Record<keyof ChangeRecordSummaryV1['completeness'], string> = {
  events: 'Captured events',
  source: 'Source identity',
  setup: 'Setup identity',
  recipe: 'Recipe identity',
  result: 'Verification result',
  usage: 'Usage',
}

export const changeRecordHref = (workId: string) => `#change-record/${encodeURIComponent(workId)}`

/** `#change-record/<work-id>` opens one concise record. Invalid or malformed IDs open the lookup. */
export function changeRecordIdFromHash(hash = window.location.hash): string | null {
  const match = /^#change-record\/([^/]+)$/.exec(hashPath(hash))
  if (!match) return null
  try {
    const workId = decodeURIComponent(match[1]!)
    return validWorkId(workId) ? workId : null
  } catch {
    return null
  }
}

function StatusPill({ status }: { status: Status }) {
  return <Pill tone={status.tone} icon={status.icon} tip={status.explanation}>{status.label}</Pill>
}

function Fact({ label, children, explain }: { label: string; children: ReactNode; explain?: string }) {
  return (
    <div>
      <dt className="small muted">{label}{explain ? <> <Explain text={explain} /></> : null}</dt>
      <dd className="mb-3">{children}</dd>
    </div>
  )
}

const WORK_STATE_PILL: Record<WorkState, true> = { active: true, blocked: true, handover: true, paused: true, finished: true }

function Lookup({ token }: { token: string }) {
  return (
    <div className="page">
      <PageHead question="What is ready to hand off?" answer="Pick a task to see what is ready to hand off." fact="Each record shows the source, recorded checks, decision, known gaps and next action." />
      <SectionTabs />
      <Box icon="search" title="Open a change record" explain="Search by title, agent, branch or issue number, or paste a work ID from the Work page or an agent.">
        <div className="lookup">
          <Combobox
            id="change-record-work-id"
            data-tour="change-record-picker"
            label="Find a work item"
            placeholder="Search by title, agent or branch"
            describedBy="change-record-work-id-help"
            parseId={(t) => (validWorkId(t) ? t : null)}
            onSelect={(id) => { window.location.hash = changeRecordHref(id).slice(1) }}
            search={async function page(q, signal, cursor?: string): Promise<ComboResult> {
              const r = await searchStandupWork(token, signal, { q, limit: 10, ...(cursor ? { cursor } : {}) })
              const next = r.next_cursor
              return {
                more: next !== null,
                ...(next !== null ? { loadMore: (s: AbortSignal) => page(q, s, next) } : {}),
                options: r.items.map((w) => ({
                  value: w.id,
                  text: w.title,
                  secondary: w.id,
                  label: (
                    <>
                      {w.title}
                      {w.lead_agent ? <span className="small muted"> · {agentLabel(w.lead_agent)}</span> : null}
                      {w.branch ? <> <code className="branch-name small">{w.branch}</code></> : null}
                    </>
                  ),
                  meta: (
                    <>
                      {w.state in WORK_STATE_PILL ? <StatePill state={w.state as WorkState} /> : w.state ? <Pill tone="muted">{w.state}</Pill> : null}
                      {w.updated_at ? <span className="small muted">{relative(w.updated_at, Date.now())}</span> : null}
                    </>
                  ),
                })),
              }
            }}
          />
        </div>
        <div id="change-record-work-id-help" className="form-text">Or paste a work ID an agent gave you.</div>
      </Box>
    </div>
  )
}

function SourceFacts({ source }: { source: ChangeRecordSource }) {
  if (source.kind === 'native') {
    return (
      <>
      <dl className="tech mb-0">
        <Fact label="Source kind">Native change</Fact>
        <Fact label="Repository"><code className="text-break">{source.repository}</code></Fact>
        <Fact label="Source generation">{source.source_generation}</Fact>
      </dl>
      <details className="disclosure">
        <summary><Icon name="code-square" /> Source identity</summary>
        <dl className="tech mt-3 mb-0">
          <Fact label="Revision"><code className="text-break">{source.revision}</code></Fact>
          <Fact label="Change ID"><code className="text-break">{source.change_id}</code></Fact>
          <Fact label="Context digest"><code className="text-break">{source.context_digest}</code></Fact>
        </dl>
      </details>
      </>
    )
  }
  return (
    <>
      <dl className="tech mb-0">
        <Fact label="Source kind">Git working source</Fact>
        <Fact label="Repository"><code className="text-break">{source.repository}</code></Fact>
        <Fact label="Working source">
          {source.dirty ? 'Dirty' : 'Clean'} · {source.untracked_count} untracked {source.untracked_count === 1 ? 'path' : 'paths'}
        </Fact>
        <Fact label="Snapshot coverage"><StatusPill status={COVERAGE[source.snapshot_coverage]} /></Fact>
        <Fact label="Omitted from snapshot">
          {source.omissions.length === 0
            ? <span className="muted">No omissions reported</span>
            : <ul className="mb-0">{source.omissions.map((item) => <li key={item}>{OMISSION_LABEL[item]}</li>)}</ul>}
        </Fact>
        <Fact label="Captured snapshot">
          {source.captured_snapshot ? (
            <span>
              <code className="text-break">{source.captured_snapshot.digest}</code><br />
              <span className="small muted">
                {source.captured_snapshot.size_bytes.toLocaleString()} bytes · {source.captured_snapshot.media.replace('_', ' ')} · {source.captured_snapshot.retention.replaceAll('_', ' ')}
              </span><br />
              <StatusPill status={AVAILABILITY[source.captured_snapshot.availability]} />
            </span>
          ) : <span className="muted">No content-addressed working snapshot retained</span>}
        </Fact>
      </dl>
      <details className="disclosure">
        <summary><Icon name="code-square" /> Source identity</summary>
        <dl className="tech mt-3 mb-0">
          <Fact label="HEAD"><code className="text-break">{source.head}</code></Fact>
          <Fact label="Tree"><code className="text-break">{source.tree}</code></Fact>
          <Fact label="Base"><code className="text-break">{source.base}</code></Fact>
          <Fact label="Worktree ID"><code className="text-break">{source.worktree_id}</code></Fact>
        </dl>
      </details>
    </>
  )
}

function DecisionRef({ value }: { value: ChangeRecordDecisionRef }) {
  if (value.kind === 'repository_path') return <Path>{`${value.path}${value.line === null ? '' : `:${value.line}`}`}</Path>
  if (value.kind === 'commit') {
    return <code className="text-break">{value.oid}{value.path === null ? '' : ` · ${value.path}`}</code>
  }
  if (value.kind === 'captured_event') {
    return (
      <span title="Caller-declared metadata only. The local event body is unavailable here and this reference has not been verified.">
        Declared captured event <code className="text-break">{value.session_id} / {value.event_id}</code>
      </span>
    )
  }
  if (value.kind === 'prior_work_version') {
    return (
      <span title="This exact Work version identity was checked when the decision checkpoint was recorded. Its historical snapshot may no longer be retained.">
        Supersedes version <code>{value.event_seq}</code> of <a href={`#work/${encodeURIComponent(value.work_id)}`}><code className="text-break">{value.work_id}</code></a>
      </span>
    )
  }
  return <a href={`#work/${encodeURIComponent(value.work_id)}`}><code className="text-break">{value.work_id}</code></a>
}

function DecisionFacts({ summary }: { summary: ChangeRecordSummaryV1 }) {
  const decision = summary.decision
  const author = decision.author_kind === 'agent'
    ? <><Pill tone="warn" icon="robot">Agent proposal</Pill> <span className="small muted">Reported by {decision.author_agent}. This is not human approval.</span></>
    : decision.author_kind === 'human'
      ? <Pill tone="ok" icon="person-check">Human-authored decision</Pill>
      : <Pill tone="warn" icon="question-circle">Decision author unknown</Pill>
  return (
    <>
      <div className="pills mb-3"><StatusPill status={DECISION[decision.status]} /> {author}</div>
      {decision.summary ? <p>{decision.summary}</p> : <p className="muted">No decision summary retained.</p>}
      <dl className="tech mb-0">
        {decision.reference ? <Fact label="Decision reference"><code className="text-break">{decision.reference}</code></Fact> : null}
        {decision.at === null ? null : <Fact label="Recorded"><Reported at={decision.at} /></Fact>}
        {decision.generation === null ? null : <Fact label="Decision generation">{decision.generation}</Fact>}
        {decision.source_refs.length === 0 && decision.source_refs_omitted === 0 ? null : (
          <Fact label="Source references">
            {decision.source_refs.length > 0 ? <ul className="mb-1">{decision.source_refs.map((value, index) => <li key={`${value.kind}-${index}`}><DecisionRef value={value} /></li>)}</ul> : null}
            {decision.source_refs_omitted > 0 ? <span className="small muted">{decision.source_refs_omitted} additional source reference{decision.source_refs_omitted === 1 ? '' : 's'} omitted from this bounded projection.</span> : null}
          </Fact>
        )}
      </dl>
      <div className="action mb-0">
        <span className="action-label"><Icon name="arrow-right-circle" /> Next</span>
        {summary.next_action ?? <span className="muted">No next action reported</span>}
      </div>
    </>
  )
}

function SessionSetups({ summary }: { summary: ChangeRecordSummaryV1 }) {
  const { session_setups: setups, session_setups_omitted: omitted } = summary
  if (setups.length === 0) return <Empty icon="cpu" title="No session setup reported">Model, effort and permission mode are unknown.</Empty>
  return (
    <>
      <dl className="tech mb-0">
        {setups.map((setup) => (
          <Fact key={setup.session_id} label={setup.parent_session_id ? `Session ${setup.session_id} (parent ${setup.parent_session_id})` : `Session ${setup.session_id}`}>
            Model {setup.model ?? 'unknown'} · effort {setup.effort ?? 'unknown'} · permission mode {setup.permission_mode ?? 'unknown'}
          </Fact>
        ))}
      </dl>
      {omitted > 0 ? <p className="small muted mb-0">{omitted} additional session{omitted === 1 ? '' : 's'} omitted from this bounded projection.</p> : null}
    </>
  )
}

function LatestAction({ summary }: { summary: ChangeRecordSummaryV1 }) {
  const action = summary.latest_action
  if (!action) return <Empty icon="clipboard2-x" title="No action captured">The record does not yet contain a source-bound action receipt.</Empty>
  // Green only where the hub or an independent check backs the result; a caller's claim or a collector's own-computer observation is not green.
  const proof = proofOfChangeRecord(action.assurance)
  return (
    <>
      <div className="pills mb-3" data-tour="change-latest" aria-label="Latest action status">
        <StatusPill status={ACTION_KIND[action.kind]} />
        <StatusPill status={{ ...OUTCOME[action.outcome], tone: backedTone(OUTCOME[action.outcome].tone, proof.level) }} />
        {action.kind === 'verification'
          ? <StatusPill status={{ ...RESULT[action.result], tone: backedTone(RESULT[action.result].tone, proof.level) }} />
          : <Pill tone="warn" icon="dash-circle" tip="An execution outcome is not a verification result.">Verification not established</Pill>}
        <StatusPill status={APPLICABILITY[action.applicability]} />
        <StatusPill status={ASSURANCE[action.assurance]} />
        <ProofBadge {...proof} />
        <StatusPill status={{ ...AVAILABILITY[action.evidence_availability], label: `Evidence ${AVAILABILITY[action.evidence_availability].label.toLowerCase()}` }} />
        <StatusPill status={COVERAGE[action.coverage]} />
        <StatusPill status={RECIPE_PROVENANCE[action.recipe_provenance]} />
      </div>
      <dl className="tech mb-0">
        <Fact label="Source identity"><code className="text-break">{action.source_id}</code></Fact>
        <Fact label="Started"><Reported at={action.started_at} /></Fact>
        <Fact label="Finished">{action.finished_at === null ? <span className="muted">No finish time retained</span> : <Reported at={action.finished_at} />}</Fact>
        <Fact label="Drift"><StatusPill status={DRIFT[action.drift]} /></Fact>
      </dl>
      <details className="disclosure mt-3">
        <summary><Icon name="code-square" /> Receipt identities</summary>
        <dl className="tech mt-3 mb-0">
          <Fact label="Action ID"><code className="text-break">{action.action_id}</code></Fact>
          <Fact label="Recipe digest"><code className="text-break">{action.recipe_digest}</code></Fact>
          <Fact label="Setup digest"><code className="text-break">{action.setup_digest}</code></Fact>
        </dl>
      </details>
    </>
  )
}

function HandoffSummary({ summary }: { summary: ChangeRecordSummaryV1 }) {
  const action = summary.latest_action
  if (!action) return <>No source-bound action has been captured. {summary.next_action ?? 'No next action has been reported.'}</>
  if (action.kind === 'execution') {
    return <>Latest recorded execution: <strong>{OUTCOME[action.outcome].label.toLowerCase()}</strong>; no verification result was established. {summary.next_action ?? 'No next action has been reported.'}</>
  }
  return <>Latest recorded verification: <strong>{RESULT[action.result].label.toLowerCase()}</strong>, applicability <strong>{APPLICABILITY[action.applicability].label.toLowerCase()}</strong>. {summary.next_action ?? 'No next action has been reported.'}</>
}

const CONVERSATION_KIND: Record<ConversationEventKind, string> = {
  session_started: 'Session started',
  user_prompt: 'User prompt',
  assistant_message: 'Assistant message',
  tool_started: 'Tool started',
  tool_finished: 'Tool finished',
  check_started: 'Check started',
  check_finished: 'Check finished',
  session_ended: 'Session ended',
  gap: 'Capture gap',
  turn_ended: 'Turn ended',
  compaction: 'Compaction',
  subagent_started: 'Subagent started',
  subagent_stopped: 'Subagent stopped',
}

const CONVERSATION_AVAILABILITY: Record<ConversationAvailability, Status> = {
  local: { label: 'Local only', tone: 'muted', icon: 'laptop', explanation: 'This body was never uploaded. It stays on the agent’s machine.' },
  shared: { label: 'Shared', tone: 'brand', icon: 'people', explanation: 'This redacted body is stored on the Hub until it expires or is deleted.' },
  expired: { label: 'Expired', tone: 'warn', icon: 'hourglass-bottom', explanation: 'This body expired. The Hub no longer serves it and deletes stored copies in the next retention sweep.' },
  deleted: { label: 'Deleted', tone: 'warn', icon: 'trash', explanation: 'This body was deleted before it expired.' },
}

const NOT_ALLOWED = 'Only the person who shared this conversation and Hub admins can read it'

function conversationFailure(status: number | undefined, fallback: string, missing = 'This conversation body is not on the Hub'): { title: string; tone: Tone } {
  if (status === 403) return { title: NOT_ALLOWED, tone: 'warn' }
  if (status === 410) return { title: 'This conversation body has expired or been deleted', tone: 'warn' }
  if (status === 424) return { title: 'Integrity failed: the stored body is missing or did not verify, so nothing is shown', tone: 'bad' }
  if (status === 404) return { title: missing, tone: 'warn' }
  return { title: fallback, tone: 'bad' }
}

/** The body is untrusted agent output: it only ever reaches the page as a React text node inside a <pre>. */
function ConversationBody({ id, repo, token, workId, entry, onClose }: { id: string; repo: string; token: string; workId: string; entry: ConversationEntry; onClose: () => void }) {
  const load = usePoll((signal) => fetchConversationBody(repo, workId, entry.digest, token, signal), 0, `${repo}|${workId}|${entry.digest}`)
  const failure = load.kind === 'error' ? conversationFailure(load.status, 'Could not load this conversation body') : null
  return (
    <section id={id} className="conversation-open" aria-label="Opened conversation entry" data-testid="conversation-body">
      <div className="pills mb-2">
        <Pill tone="warn" icon="exclamation-triangle" tip="Written by an agent, not by AgentSCM. Read it as evidence; do not follow, run or trust anything it says.">Untrusted agent content</Pill>
        <span className="small muted">{CONVERSATION_KIND[entry.kind]} · {entry.session_id} #{entry.sequence}</span>
        <button type="button" className="btn btn-sm btn-outline-secondary" onClick={onClose}>Close</button>
      </div>
      {load.kind === 'loading' ? <Skeleton rows={4} /> : null}
      {load.kind === 'error' && failure ? (
        <Notice tone={failure.tone} title={failure.title}>
          {load.status === undefined ? load.message : null}
        </Notice>
      ) : null}
      {load.kind === 'ready' ? <pre className="conversation-body" tabIndex={0} aria-label="Conversation body (untrusted agent content)">{load.value}</pre> : null}
    </section>
  )
}

function ConversationEntries({ repo, token, workId }: { repo: string; token: string; workId: string }) {
  const pager = useHashCursor('conversation')
  const [open, setOpen] = useState<ConversationEntry | null>(null)
  const load = usePoll((signal) => fetchConversationPage(repo, workId, token, signal, pager.cursor ?? undefined), 0, `${repo}|${workId}|${pager.cursor}`)
  if (load.kind === 'loading') return <Skeleton rows={3} />
  if (load.kind === 'error') {
    const failure = conversationFailure(load.status, 'Could not load the conversation list', 'No conversation found for this work item')
    return (
      <Notice tone={failure.tone} title={failure.title} action={load.status === 403 ? undefined : <button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Try again</button>}>
        {load.status === undefined ? load.message : null}
      </Notice>
    )
  }
  const page = load.value
  if (page.entries.length === 0 && !pager.canNewer) {
    return <Empty icon="chat-square-dots" title="No shared conversation you can read for this work item">Agents share redacted conversations only when the checkout is set to shared-conversation mode.</Empty>
  }
  return (
    <>
      <ul className="conversation-list" aria-label="Shared conversation entries">
        {page.entries.map((entry) => (
          <li key={entry.digest}>
            <strong>{CONVERSATION_KIND[entry.kind]}</strong>
            <span className="small muted">{entry.session_id} #{entry.sequence}</span>
            <Reported at={entry.observed_at} />
            <StatusPill status={CONVERSATION_AVAILABILITY[entry.availability]} />
            <span className="small muted">{entry.size_bytes.toLocaleString()} bytes</span>
            {entry.expires_at === null || (entry.availability !== 'shared' && entry.availability !== 'expired') ? null : (
              <span className="small muted">{entry.availability === 'shared' ? 'expires ' : 'expired '}<Ago at={entry.expires_at} /></span>
            )}
            {entry.availability === 'shared' ? (
              <button type="button" className="btn btn-sm btn-outline-secondary" aria-expanded={open?.digest === entry.digest} aria-controls={open?.digest === entry.digest ? `conversation-body-${entry.digest}` : undefined} onClick={() => setOpen(open?.digest === entry.digest ? null : entry)}>
                {open?.digest === entry.digest ? 'Hide' : 'View'}
              </button>
            ) : null}
            {open?.digest === entry.digest ? <ConversationBody key={entry.digest} id={`conversation-body-${entry.digest}`} repo={repo} token={token} workId={workId} entry={entry} onClose={() => setOpen(null)} /> : null}
          </li>
        ))}
      </ul>
      <Pager newerLabel="Previous" olderLabel="Next" tour="conversation-pager" onNewer={pager.canNewer ? pager.newer : undefined} onOlder={page.next_cursor ? () => pager.older(page.next_cursor!) : undefined} complete={page.next_cursor === null && page.entries.length > 0} />
    </>
  )
}

const CAPTURE_COMMAND = 'agentscm change-capture configure'

function Detail({ repo, token, workId }: { repo: string; token: string; workId: string }) {
  const load = usePoll((signal) => fetchChangeRecord(repo, workId, token, signal), 0, `${repo}|${workId}`)
  if (load.kind === 'loading') return <div className="page"><Skeleton rows={6} /></div>
  if (load.kind === 'error') {
    const title = load.status === 404
      ? 'No change record for this task yet'
      : load.status === 401 || load.status === 403
        ? 'This token cannot read the change record'
        : load.status === 503
          ? 'Change records are temporarily unavailable'
          : 'Could not load this change record'
    return (
      <div className="page">
        <a href="#change-record" className="back"><Icon name="arrow-left" /> Change record lookup</a>
        <Notice tone={load.status === 404 ? 'warn' : 'bad'} title={title} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Try again</button>}>
          {load.status === 404 ? (
            <>
              <p>
                Change capture is experimental and off until you set it up. Run <code>{CAPTURE_COMMAND}</code> once in the agent’s checkout; its next recorded command for this task shows here.
              </p>
              <div className="d-flex flex-wrap gap-2 align-items-center">
                <CopyButton text={CAPTURE_COMMAND} label="Copy command" />
                <a href={workHref(workId)}>Back to the task</a>
              </div>
            </>
          ) : load.message}
        </Notice>
      </div>
    )
  }

  const summary = load.value.summary
  const action = summary.latest_action
  return (
    <div className="page change-record-detail">
      <a href={workHref(summary.work_id)} className="back"><Icon name="arrow-left" /> {summary.intent.title}</a>
      <PageHead question={summary.intent.title} answer={<HandoffSummary summary={summary} />} fact={<>Record updated <Ago at={summary.as_of} />.</>} />

      <div className="grid-2 align-start">
        <Box icon="bullseye" title="Intent and declared scope" explain="Taken from the task record, not pieced together from conversations.">
          <div className="small muted mb-2">Declared scope</div>
          <Paths items={summary.intent.scope} max={12} />
          {summary.intent.scope_omitted > 0 ? <p className="small muted mt-2 mb-0">{summary.intent.scope_omitted} additional scope entr{summary.intent.scope_omitted === 1 ? 'y' : 'ies'} omitted from this bounded projection.</p> : null}
        </Box>

        <Box icon="signpost-split" title="Decision and next action" explain="Who decided what is kept apart from whether it was verified.">
          <DecisionFacts summary={summary} />
        </Box>
      </div>

      <Box icon="camera" title="Action source snapshot" explain="The source the latest action used. How much of it was captured, and whether its content is still available, are shown separately.">
        {summary.subject ? (
          <>
            <SourceFacts source={summary.subject.source} />
            <details className="disclosure mt-3">
              <summary><Icon name="fingerprint" /> Stable source identity</summary>
              <code className="text-break d-block mt-3">{summary.subject.source_id}</code>
            </details>
            <dl className="tech mt-3 mb-0">
              <Fact label="Captured sessions">{summary.sessions.count}</Fact>
              <Fact label="Session coverage">
                <StatusPill status={COVERAGE[summary.sessions.coverage]} />{' '}
                {summary.sessions.gaps > 0 ? <span className="small muted">{summary.sessions.gaps} recorded gap{summary.sessions.gaps === 1 ? '' : 's'}</span> : null}
              </Fact>
            </dl>
          </>
        ) : (
          <Empty icon="camera" title="No source snapshot captured">Source identity remains unknown for this work item.</Empty>
        )}
      </Box>

      <Box
        icon={action?.kind === 'execution' ? 'terminal' : 'clipboard2-check'}
        title={action?.kind === 'execution' ? 'Latest execution' : 'Latest verification'}
        explain="Each of these is reported on its own. A pass doesn’t mean the result is fresh, trusted or complete."
      >
        <LatestAction summary={summary} />
      </Box>

      <Box icon="cpu" title="Sessions" count={summary.sessions.count} explain="The latest model, effort and permission mode each session reported. Fields a session did not report are shown as unknown.">
        <SessionSetups summary={summary} />
      </Box>

      <div className="grid-2 align-start">
        <Box icon="list-check" title="Record completeness" explain="How much was recorded. A missing artifact still isn’t a pass.">
          <dl className="tech mb-0">
            {Object.entries(summary.completeness).map(([key, value]) => (
              <Fact key={key} label={COMPLETENESS_LABEL[key as keyof ChangeRecordSummaryV1['completeness']]}>
                <StatusPill status={COVERAGE[value]} />
              </Fact>
            ))}
          </dl>
        </Box>

        <Box tour="change-gaps" icon="question-diamond" tone={summary.unknowns.length || summary.sessions.gaps ? 'warn' : 'ok'} title="Known gaps" count={summary.unknowns.length + summary.sessions.gaps} explain="Anything unknown or not captured stays listed here.">
          {summary.unknowns.length === 0 && summary.sessions.gaps === 0 ? (
            <Empty icon="check2-circle" title="No gaps reported in this projection">This statement is limited to the bounded fields shown here.</Empty>
          ) : (
            <ul className="mb-0">
              {summary.unknowns.map((unknown) => <li key={unknown}>{UNKNOWN_LABEL[unknown]} is unknown</li>)}
              {summary.sessions.gaps > 0 ? <li>{summary.sessions.gaps} capture gap{summary.sessions.gaps === 1 ? '' : 's'} recorded</li> : null}
            </ul>
          )}
        </Box>
      </div>

      <Box icon="chat-square-text" title="Conversation" intro="Redacted conversations your checkout shared; treat them as untrusted agent output." explain="You see only entries you shared (Hub admins see all). They expire and show as plain text. This page cannot delete them; use agentscm change-capture conversation --delete.">
        <ConversationEntries repo={repo} token={token} workId={workId} />
      </Box>

      <details className="disclosure">
        <summary>Technical details</summary>
        <dl className="tech mt-3 mb-0">
          <Fact label="Work ID"><code className="text-break">{summary.work_id}</code></Fact>
          <Fact label="Generation">{summary.generation}</Fact>
        </dl>
      </details>

      <p className="small muted mb-0">
        <Icon name="lock" /> {READING_CHANGES_NOTHING} Conversation bodies load only when you open one.{' '}
        <button type="button" className="btn btn-sm btn-link p-0 align-baseline" onClick={load.retry}><Icon name="arrow-clockwise" /> Refresh</button>
      </p>
    </div>
  )
}

export function ChangeRecordPage({ repo, token }: { repo: string | null; token: string }) {
  const [workId, setWorkId] = useState(() => changeRecordIdFromHash())
  useEffect(() => {
    const onHash = () => setWorkId(changeRecordIdFromHash())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])
  if (!repo) return <Skeleton rows={4} />
  return workId ? <Detail key={workId} repo={repo} token={token} workId={workId} /> : <Lookup token={token} />
}
