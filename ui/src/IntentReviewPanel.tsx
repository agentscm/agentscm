import { useState, type FormEvent } from 'react'
import { Box, Empty, Notice, Pill, Skeleton, type Tone } from './ui.tsx'
import { inline } from './Markdown.tsx'
import { usePoll } from './usePoll.ts'
import { fetchIntentReview, submitIntentDisposition, type Attempt, type Finding, type IntentOutcome, type IntentReview } from './intentReview.ts'

const OUTCOME: Record<IntentOutcome, { label: string; tone: Tone }> = {
  meets_recorded_criteria: { label: 'Meets recorded criteria', tone: 'ok' },
  needs_correction: { label: 'Needs correction', tone: 'bad' }, needs_evidence: { label: 'Needs evidence', tone: 'warn' },
  operational_error: { label: 'Execution problem', tone: 'bad' }, stale: { label: 'Stale assessment', tone: 'warn' },
  interrupted: { label: 'Interrupted', tone: 'warn' }, disputed: { label: 'Disputed by a human', tone: 'warn' }, exception: { label: 'Human exception recorded', tone: 'muted' },
}
const FINDING: Record<Finding['status'], { label: string; tone: Tone }> = {
  supported: { label: 'Supported', tone: 'ok' }, mismatch: { label: 'Mismatch', tone: 'bad' }, unknown: { label: 'Unknown', tone: 'warn' },
}
const NEXT: Record<IntentReview['next'], string> = {
  assess: 'Assess the current candidate', correct: 'Correct the candidate, then reassess', investigate: 'Gather evidence or ask for human input',
  in_progress: 'The agent task is in progress', complete: 'The recorded criteria are supported', human_disposition: 'A human disposition is recorded',
  recovery_required: 'Resolve the interrupted task before continuing', budget_exhausted: 'The review budget is exhausted', cancelled: 'The review was cancelled',
}
const statusPill = (value: IntentOutcome | null) => value ? <Pill tone={OUTCOME[value].tone}>{OUTCOME[value].label}</Pill> : <Pill tone="muted">Not assessed</Pill>
const millis = (value: number) => new Date(value).toLocaleString()

function References({ rows }: { rows: IntentReview['record']['contracts'][number]['request'] }) {
  if (!rows.length) return <p className="small muted mb-0">No available message bodies.</p>
  return <ul className="mb-0">{rows.map((ref) => <li key={ref.id}>
    {ref.text ? <p className="mb-1" style={{ whiteSpace: 'pre-wrap' }}>{ref.text}</p> : <p className="mb-1 muted">Message body unavailable.</p>}
    <p className="small muted">{ref.kind.replaceAll('_', ' ')} · {ref.author} · {ref.source} · {ref.coverage}{ref.reconstructed ? ' · reconstructed reference' : ''}</p>
  </li>)}</ul>
}

function EvidenceList({ attempt, finding }: { attempt: Attempt; finding: Finding }) {
  if (!finding.evidence_ids.length) return <span className="small muted">No evidence cited.</span>
  const evidence = new Map(attempt.subject.evidence.map((item) => [item.id, item]))
  return <ul className="small mb-0">{finding.evidence_ids.map((id) => {
    const item = evidence.get(id)
    return <li key={id}>{item ? <><code>{item.id}</code> · {item.origin} · {item.assurance.replaceAll('_', ' ')} · {item.coverage}{item.available ? '' : ' · unavailable'}{item.candidate_digest ? ` · candidate ${item.candidate_digest.slice(0, 12)}` : ' · candidate binding unknown'}</> : <><code>{id}</code> · evidence reference is missing</>}</li>
  })}</ul>
}

function SourceFacts({ review }: { review: IntentReview }) {
  const source = review.record.subject.source
  const scope = review.record.subject.scope
  const contract = review.record.contracts[review.record.contracts.length - 1]!
  const remainingCriteria = contract.criteria.filter((criterion) => !scope.criteria.includes(criterion.id))
  const sourceGap = source.snapshot_coverage !== 'complete' || ((source.dirty || source.untracked_count > 0) && source.captured_snapshot === null)
  return <>
    <dl className="row mb-2">
      <dt className="col-sm-3">Repository</dt><dd className="col-sm-9"><code>{source.repository}</code></dd>
      <dt className="col-sm-3">Worktree</dt><dd className="col-sm-9"><code>{source.worktree_id}</code></dd>
      <dt className="col-sm-3">HEAD / tree / base</dt><dd className="col-sm-9"><code>{source.head}</code> / <code>{source.tree}</code> / <code>{source.base}</code></dd>
      <dt className="col-sm-3">Working source</dt><dd className="col-sm-9">{source.dirty ? 'Dirty' : 'Clean'} · {source.untracked_count} untracked · snapshot {source.snapshot_coverage}{source.captured_snapshot ? ` · ${source.captured_snapshot.digest}` : ''}</dd>
      <dt className="col-sm-3">Intent scope</dt><dd className="col-sm-9">{scope.coverage}{scope.parent_work_id ? ` · child of ${scope.parent_work_id}` : ''}{scope.checkpoint ? ` · checkpoint: ${scope.checkpoint}` : ''}</dd>
    </dl>
    {remainingCriteria.length > 0 && <div className="mt-3" aria-label="Remaining contract criteria">
      <strong>Remaining contract criteria</strong>
      <p className="small muted mb-2">This assessment covers only the criteria in its current scope. Support here does not establish support for the full Work intent.</p>
      <ul className="mb-0">{remainingCriteria.map((criterion) => <li key={criterion.id} className="mb-1"><strong>{criterion.id}</strong> · {criterion.description} <Pill tone="muted">Not assessed in this scope</Pill></li>)}</ul>
    </div>}
    {(sourceGap || scope.gaps.length > 0 || source.omissions.length > 0 || scope.source_exclusions.length > 0) && <Notice tone="warn" title="Source or scope coverage is incomplete">
      {sourceGap && <p className="mb-1">The captured candidate does not establish complete working-source coverage.</p>}
      {scope.gaps.map((gap) => <p className="mb-1" key={gap}>{gap}</p>)}
      {source.omissions.length > 0 && <p className="mb-1">Candidate omissions: {source.omissions.join(', ')}</p>}
      {scope.source_exclusions.map((item) => <p className="mb-1" key={`${item.omission}:${item.authorization_reference}`}>Excluded {item.omission}: {item.reason} (human reference {item.authorization_reference})</p>)}
    </Notice>}
  </>
}

function Findings({ attempt, review }: { attempt: Attempt; review: IntentReview }) {
  const assessment = attempt.assessment
  if (!assessment) return <Empty icon="question-circle" title="No completed semantic assessment">The latest relevant assessment has no criterion findings.</Empty>
  const contract = review.record.contracts[review.record.contracts.length - 1]!
  const criteria = new Map(contract.criteria.map((criterion) => [criterion.id, criterion]))
  const sameContract = attempt.subject.contract_digest === review.record.subject.contract_digest
  const sameCandidate = attempt.subject.source.head === review.record.subject.source.head && attempt.subject.source.tree === review.record.subject.source.tree && attempt.subject.source.captured_snapshot?.digest === review.record.subject.source.captured_snapshot?.digest
  return <>
    {(!sameContract || !sameCandidate) && <Notice tone="warn" title="These findings describe an earlier intent or candidate">Treat them as history until the current contract and source have a fresh assessment.</Notice>}
    {assessment.coverage !== 'complete' || assessment.gaps.length > 0 ? <Notice tone="warn" title={`Assessment coverage: ${assessment.coverage}`}>
      {assessment.gaps.map((gap) => <p className="mb-1" key={gap}>{gap}</p>)}
    </Notice> : null}
    <div className="d-grid gap-3">{assessment.criteria.map((finding) => {
      const criterion = criteria.get(finding.criterion), state = FINDING[finding.status]
      return <article key={finding.criterion} className="border-top pt-3">
        <div className="d-flex justify-content-between gap-2 mb-2"><strong>{sameContract ? criterion?.description ?? `Unrecognized criterion ${finding.criterion}` : `Historical criterion ${finding.criterion}`}</strong><Pill tone={criterion && sameContract ? state.tone : 'warn'}>{criterion && sameContract ? state.label : 'Historical finding'}</Pill></div>
        <p className="mb-2">{finding.explanation}</p>
        <EvidenceList attempt={attempt} finding={finding} />
        {finding.corrective_outcome && <p className="small mt-2 mb-1"><strong>Correction requested:</strong> {finding.corrective_outcome}</p>}
        {finding.recheck && <p className="small mb-0"><strong>Recheck:</strong> {finding.recheck}</p>}
      </article>
    })}</div>
  </>
}

function AttemptHistory({ attempts }: { attempts: Attempt[] }) {
  if (!attempts.length) return <Empty icon="clock-history" title="No agent attempts yet">The workflow is retained, but no task invocation has started.</Empty>
  return <ol className="mb-0">{attempts.map((attempt) => <li className="mb-3" key={attempt.attempt_id}>
    <strong>{attempt.phase === 'assessment' ? 'Assessment' : 'Correction'}</strong> · {attempt.state} · started {millis(attempt.started_at_ms)}
    <div className="small">Execution: {attempt.execution ?? (attempt.state === 'leased' ? 'running' : 'not recorded')} · candidate <code>{attempt.subject.source.head}</code> · attempt <code>{attempt.attempt_id}</code></div>
    {attempt.observed_model && <div className="small">Model reported by agent: {attempt.observed_model}</div>}
    <div className="small">Source: {attempt.subject.source.dirty ? 'dirty' : 'clean'} · HEAD <code>{attempt.subject.source.head}</code> · tree <code>{attempt.subject.source.tree}</code>{attempt.subject.source.captured_snapshot && <> · snapshot <code>{attempt.subject.source.captured_snapshot.digest}</code></>}</div>
    {attempt.detail && <div className="small">{attempt.detail}</div>}
    {attempt.assessment && <div className="small">Semantic outcome recorded for {attempt.assessment.criteria.length} criteria · coverage {attempt.assessment.coverage}</div>}
  </li>)}</ol>
}

function DispositionForm({ review, onSaved }: { review: IntentReview; onSaved: () => void }) {
  const contract = review.record.contracts[review.record.contracts.length - 1]!
  const [kind, setKind] = useState<'dispute' | 'exception'>('dispute')
  const [criteria, setCriteria] = useState<string[]>([])
  const [reason, setReason] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const eligible = review.current && review.current_subject_digest.length > 0
  const submit = async (event: FormEvent) => {
    event.preventDefault()
    if (!eligible || busy || !criteria.length || !reason.trim()) return
    setBusy(true)
    setError(null)
    setSaved(false)
    try {
      await submitIntentDisposition(review.record.subject.work_id, review, kind, criteria, reason, new AbortController().signal)
      setSaved(true)
      onSaved()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'The disposition could not be recorded.')
    } finally {
      setBusy(false)
    }
  }
  return <Box icon="person-check" title="Human disposition" explain="Saves your objection or exception on this computer. It doesn’t change whether the workflow can land, and it doesn’t check who you are.">
    {!eligible && <Notice tone="warn" title="Disposition requires the current candidate">Reload Work detail to get a fresh subject digest before recording a dispute or exception.</Notice>}
    {error && <Notice tone="bad" title="Disposition was not recorded">{error}</Notice>}
    {saved && <Notice tone="ok" icon="check-circle" title="Disposition recorded">Refreshing the retained assessment.</Notice>}
    <form onSubmit={(event) => void submit(event)}>
      <fieldset disabled={!eligible || busy}>
        <legend className="small">Record a</legend>
        <div className="d-flex gap-3 mb-3">
          <label><input type="radio" name="intent-disposition-kind" value="dispute" checked={kind === 'dispute'} onChange={() => setKind('dispute')} /> Dispute</label>
          <label><input type="radio" name="intent-disposition-kind" value="exception" checked={kind === 'exception'} onChange={() => setKind('exception')} /> Exception</label>
        </div>
        <fieldset className="mb-3">
          <legend className="small">Criteria</legend>
          {review.record.subject.scope.criteria.map((id) => {
            const label = contract.criteria.find((item) => item.id === id)?.description ?? id
            return <label className="d-block" key={id}><input type="checkbox" checked={criteria.includes(id)} onChange={(event) => setCriteria((old) => event.target.checked ? [...old, id] : old.filter((value) => value !== id))} /> {label}</label>
          })}
        </fieldset>
        <label className="form-label" htmlFor="intent-disposition-reason">Reason</label>
        <textarea id="intent-disposition-reason" className="form-control mb-3" rows={3} maxLength={2048} value={reason} onChange={(event) => setReason(event.target.value)} required />
      </fieldset>
      <button className="btn btn-sm btn-outline-primary" type="submit" disabled={!eligible || busy || criteria.length === 0 || !reason.trim()}>{busy ? 'Recording…' : 'Record disposition'}</button>
    </form>
  </Box>
}

function Review({ review, onDispositionSaved }: { review: IntentReview; onDispositionSaved: () => void }) {
  const contract = review.record.contracts[review.record.contracts.length - 1]!
  const lastAssessment = [...review.record.attempts].reverse().find((attempt) => attempt.phase === 'assessment' && attempt.assessment)
  const latestAttempt = review.record.attempts.at(-1)
  return <div className="d-grid gap-3">
    <Box icon="bullseye" title="Intent review" explain="An agent’s review of this work. Treat its findings as advice: they don’t let a workflow land.">
      <p className="d-flex flex-wrap align-items-center gap-2 mb-2">{statusPill(review.outcome)} <span>{review.current ? 'Current source and intent binding' : 'Assessment is stale for the current source or intent'}</span></p>
      <p className="mb-1"><strong>Next:</strong> {NEXT[review.next]}</p>
      <p className="small muted mb-0">Admission: {review.admission.replaceAll('_', ' ')} · skill version <code>{review.record.subject.skill.skill_version_id}</code>, step <code>{review.record.subject.skill.step_id}</code></p>
      {latestAttempt && <p className="small mt-2 mb-0"><strong>Latest attempt:</strong> {latestAttempt.phase} execution {latestAttempt.execution ?? (latestAttempt.state === 'leased' ? 'running' : 'not recorded')}. This execution state is separate from the semantic outcome above.</p>}
    </Box>
    <Box icon="clipboard-check" title={lastAssessment?.attempt_id === latestAttempt?.attempt_id ? 'Latest assessment findings' : 'Most recent assessment findings'} count={lastAssessment?.assessment?.criteria.length ?? 0}>
      {lastAssessment && lastAssessment.attempt_id !== latestAttempt?.attempt_id && <Notice tone="warn" title="These findings are from an earlier attempt">A later task attempt exists. Reassess its resulting candidate before treating these findings as current.</Notice>}
      {lastAssessment ? <Findings attempt={lastAssessment} review={review} /> : <Empty icon="question-circle" title="No completed assessment">No criteria have an assessed result yet.</Empty>}
    </Box>
    <DispositionForm review={review} onSaved={onDispositionSaved} />
    <details className="disclosure">
      <summary>Review details: request, source, budget and {review.record.attempts.length} retained {review.record.attempts.length === 1 ? 'attempt' : 'attempts'}</summary>
      <div className="d-grid gap-3 mt-2">
        <section><h3 className="h6">Original request</h3><References rows={review.record.contracts[0]!.request} /></section>
        {contract.amendments.length > 0 && <section><h3 className="h6">Human amendments</h3><References rows={contract.amendments} /></section>}
        {contract.focus.length > 0 && <section><h3 className="h6">Agent focus</h3><References rows={contract.focus} /></section>}
        <section><h3 className="h6">Candidate and scope</h3><p className="small muted">Which source snapshot this work used, and how much of it was captured.</p><SourceFacts review={review} /></section>
        <section><h3 className="h6">Correction budget</h3><p className="mb-0">{review.root_correction_rounds} of {review.record.limits.max_correction_rounds} correction rounds · {review.root_invocations} of {review.record.limits.max_invocations} agent invocations · {Math.round(review.record.limits.wall_time_ms / 60_000)} minutes total wall-time budget</p></section>
        <section><h3 className="h6">Retained attempts</h3><AttemptHistory attempts={review.record.attempts} /></section>
      </div>
    </details>
  </div>
}

export function IntentReviewPanel({ workId }: { workId: string }) {
  const [refresh, setRefresh] = useState(0)
  const poll = usePoll((signal) => fetchIntentReview(workId, signal), 15_000, `intent-review|${workId}`, refresh)
  if (poll.kind === 'loading') return <Box icon="bullseye" title="Intent review"><Skeleton rows={3} /></Box>
  if (poll.kind === 'error') return <Notice tone="bad" title={poll.status === 401 ? 'Local access needs attention' : poll.status === 403 ? 'This checkout is not available here' : 'Intent review unavailable'} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={poll.retry}>Try again</button>}>{inline(poll.message)}</Notice>
  if (poll.value === null) return <Box icon="bullseye" title="Intent review"><Empty icon="clipboard2-x" title="No retained intent assessment for this Work">Run the intent-review skill inside the implementing agent to create an assessment. This panel reads retained results only.</Empty></Box>
  return <Review review={poll.value} onDispositionSaved={() => setRefresh((value) => value + 1)} />
}
