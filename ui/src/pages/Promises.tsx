import { useEffect, useState, type ReactNode } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import {
  adoptPromise,
  admissionPreflight,
  changeCapabilities,
  changePromise,
  createPromise,
  listPromises,
  promiseHistory,
  promiseReports,
  proposalInputs,
  promiseDefinitionProblems,
  invalidPromiseText,
  InvalidPromiseResponse,
  validPromiseId,
  promiseHref,
  promiseIdFromHash,
  readPromise,
  readPromiseInput,
  type Assurance,
  type EvidenceBasis,
  type EvidenceRequirement,
  type EvidenceStage,
  type PromiseChange,
  type PromiseChangeCapabilities,
  type PromiseAdmissionPreflight,
  type PromiseCriterion,
  type PromiseDefinition,
  type PromiseHistoryPage,
  type PromiseInput,
  type PromiseInputPage,
  type PromiseInputView,
  type PromiseReceipt,
  type PromiseRepositoryAdmission,
  type PromiseVersion,
  type PromiseView,
} from '../promises.ts'
import { workspaceError, WorkspaceError, workspaceRequest, type WorkspaceSession } from '../workspace.ts'
import { Pager } from '../Pager.tsx'
import { Box, Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'

const STAGES: EvidenceStage[] = ['design', 'code', 'review', 'release', 'post_release']
const BASES: EvidenceBasis[] = ['rehearsal', 'observed', 'caller_reported']
const ASSURANCES: Assurance[] = ['caller_asserted', 'observed_trusted_local', 'protected_execution', 'independent_verification']
const label = (value: string) => value.replaceAll('_', ' ')
const emptyRequirement = (criterionId: string, n: number): EvidenceRequirement => ({ id: `requirement_${n}`, criterion_id: criterionId, stage: 'code', basis: 'observed', producer: '', check_key: '', check_digest: '', scenario_digest: null, workflow_digest: null, environment_digest: null, accepted_assurance: ['observed_trusted_local'] })
const emptyDefinition = (): PromiseDefinition => ({ original_request: '', commitment: '', criteria: [{ id: 'criterion_1', text: '', evidence_requirements: [] }], constraints: [], repositories: [{ repository_id: '', relevance: '' }] })
const content = (definition: PromiseDefinition) => ({ commitment: definition.commitment, criteria: definition.criteria, constraints: definition.constraints, repositories: definition.repositories })

function Field({ id, label: text, value, onChange, textarea = false, placeholder, help, mono = false }: { id: string; label: string; value: string; onChange: (value: string) => void; textarea?: boolean; placeholder?: string; help?: string; mono?: boolean }) {
  const control = textarea
    ? <textarea id={id} className={`form-control${mono ? ' font-monospace' : ''}`} rows={3} value={value} placeholder={placeholder} onChange={(event) => onChange(event.target.value)} />
    : <input id={id} className={`form-control${mono ? ' font-monospace' : ''}`} value={value} placeholder={placeholder} onChange={(event) => onChange(event.target.value)} />
  return <div><label className="form-label" htmlFor={id}>{text}</label>{control}{help && <div className="form-text">{help}</div>}</div>
}

function RequirementEditor({ criterion, requirement, index, prefix, onChange, onRemove }: { criterion: PromiseCriterion; requirement: EvidenceRequirement; index: number; prefix: string; onChange: (value: EvidenceRequirement) => void; onRemove: () => void }) {
  const set = (patch: Partial<EvidenceRequirement>) => onChange({ ...requirement, ...patch, criterion_id: criterion.id })
  const optionalDigest = (key: 'scenario_digest' | 'workflow_digest' | 'environment_digest', text: string) => <Field id={`${prefix}-${key}`} label={text} value={requirement[key] ?? ''} mono placeholder="64 lowercase hex characters" onChange={(value) => set({ [key]: value.trim() || null })} />
  return (
    <fieldset className="border rounded p-3 mt-3">
      <legend className="float-none w-auto px-2 h6">Evidence requirement {index + 1}</legend>
      <div className="grid-2 align-start">
        <Field id={`${prefix}-id`} label="Requirement ID" value={requirement.id} mono onChange={(value) => set({ id: value })} />
        <Field id={`${prefix}-producer`} label="Producer" value={requirement.producer} mono placeholder="multiverse" onChange={(value) => set({ producer: value })} />
        <div><label className="form-label" htmlFor={`${prefix}-stage`}>Stage</label><select id={`${prefix}-stage`} className="form-select" value={requirement.stage} onChange={(event) => set({ stage: event.target.value as EvidenceStage })}>{STAGES.map((value) => <option key={value} value={value}>{label(value)}</option>)}</select></div>
        <div><label className="form-label" htmlFor={`${prefix}-basis`}>Basis</label><select id={`${prefix}-basis`} className="form-select" value={requirement.basis} onChange={(event) => set({ basis: event.target.value as EvidenceBasis })}>{BASES.map((value) => <option key={value} value={value}>{label(value)}</option>)}</select></div>
        <Field id={`${prefix}-check`} label="Check key" value={requirement.check_key} mono onChange={(value) => set({ check_key: value })} />
        <Field id={`${prefix}-digest`} label="Check definition digest" value={requirement.check_digest} mono placeholder="64 lowercase hex characters" onChange={(value) => set({ check_digest: value })} />
        {optionalDigest('scenario_digest', 'Scenario digest')}
        {optionalDigest('workflow_digest', 'Workflow digest')}
        {optionalDigest('environment_digest', 'Environment digest')}
      </div>
      <fieldset className="mt-3"><legend className="form-label">Accepted assurance</legend><div className="d-flex flex-wrap gap-3">{ASSURANCES.map((value) => <div key={value} className="form-check"><input id={`${prefix}-${value}`} className="form-check-input" type="checkbox" checked={requirement.accepted_assurance.includes(value)} onChange={(event) => set({ accepted_assurance: event.target.checked ? [...requirement.accepted_assurance, value] : requirement.accepted_assurance.filter((item) => item !== value) })} /><label className="form-check-label" htmlFor={`${prefix}-${value}`}>{label(value)}</label></div>)}</div></fieldset>
      <button type="button" className="btn btn-sm btn-outline-danger mt-3" onClick={onRemove}><Icon name="trash" /> Remove requirement</button>
    </fieldset>
  )
}

function DefinitionEditor({ value, onChange, originalReadOnly = false, prefix = 'promise' }: { value: PromiseDefinition; onChange: (value: PromiseDefinition) => void; originalReadOnly?: boolean; prefix?: string }) {
  const set = (patch: Partial<PromiseDefinition>) => onChange({ ...value, ...patch })
  const setCriterion = (index: number, next: PromiseCriterion) => set({ criteria: value.criteria.map((item, at) => at === index ? next : item) })
  return (
    <div className="stack">
      {originalReadOnly ? <div><div className="small muted">Original owner-confirmed request</div><p className="mb-0">{value.original_request}</p><div className="form-text">Amendments and corrections retain the original request.</div></div> : <Field id={`${prefix}-original`} label="Original request" value={value.original_request} textarea help="Retained exactly as the owner-confirmed request." onChange={(original_request) => set({ original_request })} />}
      <Field id={`${prefix}-commitment`} label="Commitment" value={value.commitment} textarea onChange={(commitment) => set({ commitment })} />
      <fieldset><legend className="h6">Criteria</legend>{value.criteria.map((criterion, index) => <div key={index} className="border rounded p-3 mb-3"><div className="grid-2 align-start"><Field id={`${prefix}-criterion-${index}-id`} label="Criterion ID" value={criterion.id} mono onChange={(criterionId) => setCriterion(index, { ...criterion, id: criterionId, evidence_requirements: criterion.evidence_requirements.map((requirement) => ({ ...requirement, criterion_id: criterionId })) })} /><Field id={`${prefix}-criterion-${index}-text`} label="Required behavior" value={criterion.text} textarea onChange={(text) => setCriterion(index, { ...criterion, text })} /></div>{criterion.evidence_requirements.map((requirement, requirementIndex) => <RequirementEditor key={requirementIndex} criterion={criterion} requirement={requirement} index={requirementIndex} prefix={`${prefix}-criterion-${index}-requirement-${requirementIndex}`} onChange={(next) => setCriterion(index, { ...criterion, evidence_requirements: criterion.evidence_requirements.map((item, at) => at === requirementIndex ? next : item) })} onRemove={() => setCriterion(index, { ...criterion, evidence_requirements: criterion.evidence_requirements.filter((_item, at) => at !== requirementIndex) })} />)}<div className="d-flex flex-wrap gap-2 mt-3"><button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => setCriterion(index, { ...criterion, evidence_requirements: [...criterion.evidence_requirements, emptyRequirement(criterion.id, criterion.evidence_requirements.length + 1)] })}><Icon name="plus" /> Evidence requirement</button><button type="button" className="btn btn-sm btn-outline-danger" disabled={value.criteria.length === 1} onClick={() => set({ criteria: value.criteria.filter((_item, at) => at !== index) })}><Icon name="trash" /> Remove criterion</button></div></div>)}<button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => set({ criteria: [...value.criteria, { id: `criterion_${value.criteria.length + 1}`, text: '', evidence_requirements: [] }] })}><Icon name="plus" /> Criterion</button></fieldset>
      <fieldset><legend className="h6">Constraints</legend>{value.constraints.map((constraint, index) => <div key={index} className="grid-2 align-start mb-3"><Field id={`${prefix}-constraint-${index}-id`} label="Constraint ID" value={constraint.id} mono onChange={(constraintId) => set({ constraints: value.constraints.map((item, at) => at === index ? { ...item, id: constraintId } : item) })} /><div><Field id={`${prefix}-constraint-${index}-text`} label="Constraint" value={constraint.text} onChange={(text) => set({ constraints: value.constraints.map((item, at) => at === index ? { ...item, text } : item) })} /><button type="button" className="btn btn-sm btn-link text-danger px-0" onClick={() => set({ constraints: value.constraints.filter((_item, at) => at !== index) })}>Remove</button></div></div>)}<button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => set({ constraints: [...value.constraints, { id: `constraint_${value.constraints.length + 1}`, text: '' }] })}><Icon name="plus" /> Constraint</button></fieldset>
      <fieldset><legend className="h6">Repository scope</legend>{value.repositories.map((repository, index) => <div key={index} className="grid-2 align-start mb-3"><Field id={`${prefix}-repository-${index}-id`} label="Stable repository ID" value={repository.repository_id} mono onChange={(repository_id) => set({ repositories: value.repositories.map((item, at) => at === index ? { ...item, repository_id } : item) })} /><div><Field id={`${prefix}-repository-${index}-relevance`} label="Why this repository matters" value={repository.relevance} textarea onChange={(relevance) => set({ repositories: value.repositories.map((item, at) => at === index ? { ...item, relevance } : item) })} /><button type="button" className="btn btn-sm btn-link text-danger px-0" disabled={value.repositories.length === 1} onClick={() => set({ repositories: value.repositories.filter((_item, at) => at !== index) })}>Remove</button></div></div>)}<button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => set({ repositories: [...value.repositories, { repository_id: '', relevance: '' }] })}><Icon name="plus" /> Repository</button></fieldset>
    </div>
  )
}

function Fact({ label: text, children }: { label: string; children: ReactNode }) { return <><dt className="small muted">{text}</dt><dd className="mb-3">{children}</dd></> }
function DefinitionSummary({ definition }: { definition: PromiseDefinition }) {
  return <div className="stack"><div><h3 className="h6">Original request</h3><p>{definition.original_request}</p><h3 className="h6">Commitment</h3><p>{definition.commitment}</p></div><div><h3 className="h6">Criteria</h3><ol>{definition.criteria.map((criterion) => <li key={criterion.id}><strong>{criterion.text}</strong> <code>{criterion.id}</code><div className="mt-2"><Pill tone="warn" icon="question-circle">Unknown</Pill> <span className="small muted">No resolved assessment is available from this hub.</span></div>{criterion.evidence_requirements.length > 0 && <ul className="mt-2">{criterion.evidence_requirements.map((requirement) => <li key={requirement.id}><code>{requirement.id}</code> · {label(requirement.stage)} · {label(requirement.basis)} · <code>{requirement.producer}:{requirement.check_key}</code><details><summary>Exact mapping</summary><dl className="tech mt-2"><Fact label="Check digest"><code className="text-break">{requirement.check_digest}</code></Fact>{requirement.scenario_digest && <Fact label="Scenario digest"><code className="text-break">{requirement.scenario_digest}</code></Fact>}{requirement.workflow_digest && <Fact label="Workflow digest"><code className="text-break">{requirement.workflow_digest}</code></Fact>}{requirement.environment_digest && <Fact label="Environment digest"><code className="text-break">{requirement.environment_digest}</code></Fact>}<Fact label="Accepted assurance">{requirement.accepted_assurance.map(label).join(', ')}</Fact></dl></details></li>)}</ul>}</li>)}</ol></div><div><h3 className="h6">Constraints</h3>{definition.constraints.length ? <ul>{definition.constraints.map((constraint) => <li key={constraint.id}>{constraint.text} <code>{constraint.id}</code></li>)}</ul> : <p className="muted">No separate constraints.</p>}</div><div><h3 className="h6">Repositories</h3><ul>{definition.repositories.map((repository) => <li key={repository.repository_id}><code>{repository.repository_id}</code> — {repository.relevance}</li>)}</ul></div></div>
}

function SourceAdmissions({ admissions, reviewed = false }: { admissions: PromiseRepositoryAdmission[]; reviewed?: boolean }) {
  return <div className="mt-3"><h3 className="h6">{reviewed ? 'Source identities at review' : 'Source identities captured with this version'}</h3><ul className="mb-2">{admissions.map((admission) => <li key={admission.repository_id}><code>{admission.repository_id}</code>: {admission.forge === null && admission.native_repository_ids.length === 0 ? 'No source repository identity bound' : <>{admission.forge && <>forge <code>{admission.forge.provider}@{admission.forge.host}#{admission.forge.forge_repository_id}</code></>}{admission.forge && admission.native_repository_ids.length > 0 && '; '}{admission.native_repository_ids.length > 0 && <>native {admission.native_repository_ids.map((native, index) => <span key={native}>{index > 0 && ', '}<code>{native}</code></span>)}</>}</>}</li>)}</ul><p className="small muted mb-0">{reviewed ? 'The hub will compare these identities again at submission. They are not a finding that Work is affected or evidence passed.' : 'These are server-captured scope identities, not a finding that Work is affected or any evidence passed.'}</p></div>
}

interface ReviewedDefinition { definition: PromiseDefinition; preflight: PromiseAdmissionPreflight }

const pollError = (poll: Extract<Poll<unknown>, { kind: 'error' }>) => workspaceError(new WorkspaceError(poll.status ?? 0, poll.message))
const preflightError = (cause: unknown) => cause instanceof InvalidPromiseResponse ? 'The hub returned an invalid admission preflight. No operation was submitted.' : workspaceError(cause)
type Visible<T> = { kind: 'visible'; value: T } | { kind: 'denied' }
const visible = async <T,>(load: () => Promise<T>): Promise<Visible<T>> => { try { return { kind: 'visible', value: await load() } } catch (error) { if (error instanceof WorkspaceError && (error.status === 403 || error.status === 404)) return { kind: 'denied' }; throw error } }

function Receipt({ receipt }: { receipt: PromiseReceipt }) { return <Notice tone="ok" icon="check-circle" title={receipt.replayed ? 'The retained operation was found' : `Promise version ${receipt.ref.version} recorded`}><code>{receipt.ref.promise_id}</code> · digest <code className="text-break">{receipt.ref.digest}</code></Notice> }

function Create({ session, refreshSession }: { session: WorkspaceSession; refreshSession: () => void }) {
  const [definition, setDefinition] = useState(emptyDefinition)
  const [reviewed, setReviewed] = useState<ReviewedDefinition | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [uncertain, setUncertain] = useState(false)
  const [checking, setChecking] = useState(false)
  const issues = promiseDefinitionProblems(definition)
  const review = async () => {
    setChecking(true); setError(null)
    try {
      const preflight = await admissionPreflight(session, definition.repositories.map((repository) => repository.repository_id))
      setReviewed({ definition: structuredClone(definition), preflight })
    } catch (cause) { setError(preflightError(cause)) }
    finally { setChecking(false) }
  }
  const submit = async () => {
    if (!reviewed) return
    setBusy(true); setError(null)
    try { const receipt = await createPromise(session, reviewed.definition, reviewed.preflight.digest); window.location.hash = promiseHref(receipt.ref.promise_id).slice(1) }
    catch (cause) { const status = cause instanceof WorkspaceError ? cause.status : 0; setUncertain(!(cause instanceof WorkspaceError) || status >= 500); setError(workspaceError(cause)); if (status === 409) setReviewed(null); if ([401, 403, 404].includes(status)) { setReviewed(null); refreshSession() } }
    finally { setBusy(false) }
  }
  return <Box icon="plus-circle" title="Create a promise" explain="The signed-in human becomes steward. Review the exact request, scope and evidence mappings before submitting.">{error && <Notice tone="bad" title={error}>{uncertain && 'The outcome may be unknown. Do not edit these fields; retry the exact retained operation.'}</Notice>}{reviewed ? <><DefinitionSummary definition={reviewed.definition} /><SourceAdmissions admissions={reviewed.preflight.admissions} reviewed /><div className="d-flex gap-2"><button type="button" className="btn btn-primary" disabled={busy} onClick={() => void submit()}>{busy ? <span className="spinner-border spinner-border-sm" aria-hidden="true" /> : <Icon name="check2-circle" />} {uncertain ? 'Retry exact create' : 'Create this promise'}</button><button type="button" className="btn btn-outline-secondary" disabled={busy || uncertain} onClick={() => setReviewed(null)}>Back to edit</button></div></> : <><DefinitionEditor value={definition} onChange={setDefinition} />{issues.length > 0 && <ul className="small warn-text mt-3" aria-live="polite">{issues.map((issue) => <li key={issue}>{issue}</li>)}</ul>}<button type="button" className="btn btn-primary mt-3" disabled={issues.length > 0 || checking} onClick={() => void review()}>{checking ? 'Checking current authority…' : 'Review exact promise'}</button></>}</Box>
}

function ProposalEditor({ session, input, proposal, onConclusiveConflict }: { session: WorkspaceSession; input: PromiseInputView; proposal: Extract<PromiseInput, { kind: 'proposal' }>; onConclusiveConflict: () => void }) {
  const [definition, setDefinition] = useState<PromiseDefinition>(() => ({ original_request: proposal.original_request, commitment: proposal.commitment, criteria: structuredClone(proposal.criteria), constraints: structuredClone(proposal.constraints), repositories: [{ repository_id: input.repository_id, relevance: '' }] }))
  const [reviewed, setReviewed] = useState<ReviewedDefinition | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [uncertain, setUncertain] = useState(false)
  const issues = promiseDefinitionProblems(definition)
  const review = async () => {
    setBusy(true); setError(null)
    try {
      if (!definition.repositories.some((repository) => repository.repository_id === input.repository_id)) { setError('The proposal repository must remain in the adopted promise scope.'); return }
      const preflight = await admissionPreflight(session, definition.repositories.map((repository) => repository.repository_id))
      setReviewed({ definition: structuredClone(definition), preflight })
    } catch (cause) { setError(preflightError(cause)) }
    finally { setBusy(false) }
  }
  const submit = async () => {
    if (!reviewed) return
    setBusy(true); setError(null)
    try { const receipt = await adoptPromise(session, input.input_id, reviewed.definition, reviewed.preflight.digest); window.location.hash = promiseHref(receipt.ref.promise_id).slice(1) }
    catch (cause) {
      const status = cause instanceof WorkspaceError ? cause.status : 0
      setUncertain(!(cause instanceof WorkspaceError) || status >= 500)
      setError(workspaceError(cause))
      if ([401, 403, 404, 409].includes(status)) { setReviewed(null); onConclusiveConflict() }
    } finally { setBusy(false) }
  }
  return <Box icon="journal-arrow-up" title="Inspect and adopt proposal" explain="This submitted input is advisory. A signed-in human edits and confirms the actual promise definition.">
    <dl className="tech"><Fact label="Proposal ID"><code>{input.input_id}</code></Fact><Fact label="Submitted by">{input.submitted_by_agent ? 'Agent for ' : 'Human '}<code>{input.actor_user}</code></Fact><Fact label="Repository"><code>{input.repository_id}</code></Fact><Fact label="Recorded">{new Date(input.created_at * 1000).toLocaleString()}</Fact></dl>
    {input.adopted_promise ? <Notice tone="muted" title="Already adopted">Promise <a href={promiseHref(input.adopted_promise)}><code>{input.adopted_promise}</code></a></Notice> : <>
      {error && <Notice tone="bad" title={error}>{uncertain && 'The result may be unknown. Retry the exact retained adoption without editing.'}</Notice>}
      {reviewed ? <>
        <Notice tone="warn" title="Confirm adoption">You are accepting proposal <code>{input.input_id}</code> as this exact owner-controlled definition. The proposal itself is not authority.</Notice>
        <DefinitionSummary definition={reviewed.definition} />
        <SourceAdmissions admissions={reviewed.preflight.admissions} reviewed />
        <div className="d-flex gap-2"><button type="button" className="btn btn-primary" disabled={busy} onClick={() => void submit()}>{uncertain ? 'Retry exact adoption' : 'Adopt this definition'}</button><button type="button" className="btn btn-outline-secondary" disabled={busy || uncertain} onClick={() => setReviewed(null)}>Back to edit</button></div>
      </> : <>
        <DefinitionEditor value={definition} onChange={setDefinition} prefix="adopt" />
        {issues.length > 0 && <ul className="small warn-text mt-3" aria-live="polite">{issues.map((issue) => <li key={issue}>{issue}</li>)}</ul>}
        <button type="button" className="btn btn-primary mt-3" disabled={busy || issues.length > 0} onClick={() => void review()}>{busy ? 'Checking current authority…' : 'Review adoption'}</button>
      </>}
    </>}
  </Box>
}

function ProposalPreview({ session, scope, inputId, onConclusiveConflict }: { session: WorkspaceSession; scope: string; inputId: string; onConclusiveConflict: () => void }) {
  const load = usePoll((signal) => visible(() => readPromiseInput(inputId, signal)), 0, `${scope}:proposal:${inputId}`)
  if (load.kind === 'loading') return <Skeleton rows={3} />
  if (load.kind === 'error') return <Notice tone="bad" title={pollError(load)} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />
  if (load.value.kind === 'denied') return <Notice tone="muted" icon="lock" title="This proposal is no longer readable" />
  const input = load.value.value
  if (input.payload.kind !== 'proposal') return <Notice tone="bad" title="The selected input is not a proposal" />
  return <ProposalEditor key={input.input_id} session={session} input={input} proposal={input.payload} onConclusiveConflict={onConclusiveConflict} />
}

function Proposals({ session, scope, refreshSession }: { session: WorkspaceSession; scope: string; refreshSession: () => void }) {
  const [repository, setRepository] = useState('')
  const [selectedRepository, setSelectedRepository] = useState('')
  const [cursor, setCursor] = useState<string | undefined>()
  const [newer, setNewer] = useState<string[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const load = usePoll((signal) => selectedRepository ? visible(() => proposalInputs(selectedRepository, signal, cursor)) : Promise.resolve({ kind: 'visible' as const, value: { inputs: [], next: null } as PromiseInputPage }), 15_000, `${scope}:proposals:${selectedRepository}:${cursor ?? 'first'}`)
  const page = load.kind === 'ready' && load.value.kind === 'visible' ? load.value.value : null
  return <div className="stack"><Box icon="inboxes" title="Incoming proposals" explain="Choose a configured repository scope key to inspect advisory proposals. They cannot change a promise until a human adopts an edited definition."><form onSubmit={(event) => { event.preventDefault(); setSelectedRepository(repository.trim()); setCursor(undefined); setNewer([]); setSelected(null) }}><div className="d-flex gap-2 align-items-end"><Field id="proposal-repository" label="Workspace repository key" value={repository} mono onChange={setRepository} placeholder="owner/repository or configured project key" /><button type="submit" className="btn btn-outline-primary" disabled={!validPromiseId(repository.trim())}>Load proposals</button></div></form>{selectedRepository && <>{load.kind === 'loading' && <Skeleton rows={2} />}{load.kind === 'error' && <Notice tone="bad" title={pollError(load)} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />}{load.kind === 'ready' && load.value.kind === 'denied' && <Notice tone="muted" icon="lock" title="Proposals are not readable for this repository" />}{page && <>{page.inputs.length ? <ul className="plain-list">{page.inputs.map((input) => <li key={input.input_id}><button type="button" className="btn btn-link px-0 text-start" onClick={() => setSelected(input.input_id)}>{input.payload.kind === 'proposal' ? input.payload.commitment : 'Incoming input'}</button><div className="small muted"><code>{input.input_id}</code> · {input.adopted_promise ? 'adopted' : 'unadopted'} · {input.submitted_by_agent ? 'agent for ' : 'human '}<code>{input.actor_user}</code></div></li>)}</ul> : <Empty icon="inbox" title="No readable proposals on this page" />}<Pager onNewer={newer.length ? () => { const prior = newer.at(-1)!; setNewer(newer.slice(0, -1)); setCursor(prior || undefined) } : undefined} onOlder={page.next ? () => { setNewer([...newer, cursor ?? '']); setCursor(page.next ?? undefined) } : undefined} complete={!page.next} /></>}</>}</Box>{selected && <ProposalPreview key={selected} session={session} scope={scope} inputId={selected} onConclusiveConflict={() => { setSelected(null); if (load.kind !== 'loading') load.retry(); refreshSession() }} />}</div>
}

function PromiseList({ session, scope, refreshSession }: { session: WorkspaceSession; scope: string; refreshSession: () => void }) {
  const [cursor, setCursor] = useState<string | undefined>()
  const [newer, setNewer] = useState<string[]>([])
  const load = usePoll((signal) => visible(() => listPromises(signal, cursor)), 15_000, `${scope}:${cursor ?? 'first'}`)
  if (load.kind === 'loading') return <Skeleton rows={5} />
  if (load.kind === 'error') return <Notice tone="bad" title={pollError(load)} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />
  if (load.value.kind === 'denied') return <Notice tone="muted" icon="lock" title="Promises are not readable with this workspace access">No promise titles, repositories or counts were loaded.</Notice>
  const page = load.value.value
  return <div className="stack"><Create session={session} refreshSession={refreshSession} /><Proposals session={session} scope={scope} refreshSession={refreshSession} /><Box icon="journal-check" title="Promises" count={page.promises.length} explain="Only promises whose complete repository scope this signed-in human may read are returned.">{load.refreshError && <Notice title="Showing the last loaded page">Refresh failed: {load.refreshError}</Notice>}{page.promises.length === 0 ? <Empty icon="journal" title="No readable promises on this page" /> : <ul className="plain-list">{page.promises.map(({ current }) => <li key={current.ref.promise_id}><div className="d-flex flex-wrap align-items-start justify-content-between gap-3"><div><a className="work-link" href={promiseHref(current.ref.promise_id)}>{current.definition.commitment}</a><div className="small muted"><code>{current.ref.promise_id}</code> · version {current.ref.version} · steward <code>{current.steward_user}</code></div><div className="small">{current.definition.repositories.map((repository) => <code key={repository.repository_id} className="me-2">{repository.repository_id}</code>)}</div></div><Pill tone={current.state === 'active' ? 'brand' : 'muted'} icon={current.state === 'active' ? 'bookmark-check' : 'archive'}>{label(current.state)}</Pill></div></li>)}</ul>}<Pager onNewer={newer.length ? () => { const prior = newer.at(-1)!; setNewer(newer.slice(0, -1)); setCursor(prior || undefined) } : undefined} onOlder={page.next ? () => { setNewer([...newer, cursor ?? '']); setCursor(page.next ?? undefined) } : undefined} complete={!page.next} status={`Showing ${page.promises.length} authorized promise${page.promises.length === 1 ? '' : 's'}`} /></Box></div>
}

function History({ id, scope }: { id: string; scope: string }) {
  const [before, setBefore] = useState<number | undefined>()
  const [newer, setNewer] = useState<(number | undefined)[]>([])
  const load = usePoll((signal) => visible(() => promiseHistory(id, signal, before)), 15_000, `${scope}:${id}:${before ?? 'current'}`)
  if (load.kind === 'loading') return <Skeleton rows={4} />
  if (load.kind === 'error') return <Notice tone="bad" title={pollError(load)} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />
  if (load.value.kind === 'denied') return null
  const page: PromiseHistoryPage = load.value.value
  return <Box icon="clock-history" title="Immutable version history" count={page.versions.length}>{page.versions.length === 0 ? <Empty icon="clock" title="No readable versions on this page" /> : <ol className="plain-list">{page.versions.map((version) => <li key={version.ref.version}><div className="d-flex flex-wrap gap-2 align-items-center"><strong>Version {version.ref.version}</strong><Pill tone={version.kind === 'retire' ? 'muted' : 'brand'}>{label(version.kind)}</Pill><Pill tone={version.state === 'active' ? 'brand' : 'muted'}>{label(version.state)}</Pill><span className="small muted">by <code>{version.created_by}</code> · {new Date(version.created_at * 1000).toLocaleString()}</span></div>{version.reason && <p className="mb-1">Reason: {version.reason}</p>}<dl className="tech mt-2 mb-0"><Fact label="Steward"><code>{version.steward_user}</code></Fact><Fact label="Digest"><code className="text-break">{version.ref.digest}</code></Fact><Fact label="Prior digest">{version.prior_digest ? <code className="text-break">{version.prior_digest}</code> : 'Initial version'}</Fact>{version.proposal_id && <Fact label="Adopted proposal"><code>{version.proposal_id}</code></Fact>}</dl><details className="mt-2"><summary>Definition at this version</summary><div className="mt-3"><DefinitionSummary definition={version.definition} /><SourceAdmissions admissions={version.repository_admissions} /></div></details></li>)}</ol>}<Pager onNewer={newer.length ? () => { const prior = newer.at(-1); setNewer(newer.slice(0, -1)); setBefore(prior) } : undefined} onOlder={page.next ? () => { setNewer([...newer, before]); setBefore(page.next ?? undefined) } : undefined} complete={!page.next} /></Box>
}

function Reports({ id, scope }: { id: string; scope: string }) {
  const [cursor, setCursor] = useState<string | undefined>()
  const [newer, setNewer] = useState<string[]>([])
  const load = usePoll((signal) => visible(() => promiseReports(id, signal, cursor)), 15_000, `${scope}:${id}:reports:${cursor ?? 'first'}`)
  if (load.kind === 'loading') return <Skeleton rows={2} />
  if (load.kind === 'error') return <Notice tone="bad" title={pollError(load)} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />
  if (load.value.kind === 'denied') return null
  const page = load.value.value
  return <Box icon="journal-text" title="Advisory reports" count={page.inputs.length} explain="Retained caller-authored observations are advisory. They do not prove any criterion or change this promise.">{page.inputs.length ? <ul className="plain-list">{page.inputs.map((input) => input.payload.kind === 'report' && <li key={input.input_id}><p>{input.payload.summary}</p><div className="small muted"><code>{input.input_id}</code> · from {input.submitted_by_agent ? 'agent for ' : 'human '}<code>{input.actor_user}</code> · repository <code>{input.repository_id}</code> · refers to version {input.payload.promise_version} and digest <code>{input.payload.promise_digest}</code> · {new Date(input.created_at * 1000).toLocaleString()}</div><Pill tone="warn" icon="question-circle">Assessment unknown</Pill></li>)}</ul> : <Empty icon="journal" title="No readable reports on this page" />}<Pager onNewer={newer.length ? () => { const prior = newer.at(-1)!; setNewer(newer.slice(0, -1)); setCursor(prior || undefined) } : undefined} onOlder={page.next ? () => { setNewer([...newer, cursor ?? '']); setCursor(page.next ?? undefined) } : undefined} complete={!page.next} /></Box>
}

type OwnerAction = 'amend' | 'correct' | 'retire' | 'transfer' | 'recovery_transfer'
interface ReviewedChange { action: OwnerAction; definition: PromiseDefinition; reason: string; toUser: string; basis: PromiseVersion; preflight: PromiseAdmissionPreflight }
const ACTIONS: { kind: OwnerAction; title: string }[] = [
  { kind: 'amend', title: 'Amend commitment or scope' }, { kind: 'correct', title: 'Correct a mistaken version' },
  { kind: 'retire', title: 'Retire promise' }, { kind: 'transfer', title: 'Transfer stewardship' },
  { kind: 'recovery_transfer', title: 'Recovery transfer stewardship' },
]
interface ChangeDraft {
  basisDigest: string
  action: OwnerAction
  definition: PromiseDefinition
  reason: string
  toUser: string
  reviewed: ReviewedChange | null
  busy: boolean
  checking: boolean
  error: string | null
  uncertain: boolean
  stale: boolean
}
const freshChangeDraft = (current: PromiseVersion, available: typeof ACTIONS): ChangeDraft => ({
  basisDigest: current.ref.digest,
  action: available[0]?.kind ?? 'amend',
  definition: structuredClone(current.definition),
  reason: '', toUser: '', reviewed: null, busy: false, checking: false, error: null, uncertain: false, stale: false,
})

export function OwnerChange({ session, current, capabilities, onChanged, onRefresh }: { session: WorkspaceSession; current: PromiseVersion; capabilities: PromiseChangeCapabilities; onChanged: (receipt: PromiseReceipt) => void; onRefresh: () => void }) {
  const available = ACTIONS.filter(({ kind }) => capabilities[kind])
  const [saved, setSaved] = useState<ChangeDraft>(() => freshChangeDraft(current, available))
  // A conclusive new version starts an empty editor. An uncertain response keeps its frozen review,
  // including the old digest, until the exact retained operation is resolved.
  const draft = saved.uncertain || saved.basisDigest === current.ref.digest ? saved : freshChangeDraft(current, available)
  const update = (patch: Partial<ChangeDraft>) => setSaved({ ...draft, ...patch })
  const selectedAction = available.some(({ kind }) => kind === draft.action) ? draft.action : available[0]?.kind ?? 'amend'
  const issues = [!draft.reason.trim() && 'Enter a reason.', (new TextEncoder().encode(draft.reason).length > 2048 || invalidPromiseText(draft.reason)) && 'The reason must fit within 2 KiB and cannot contain control characters.', (selectedAction === 'amend' || selectedAction === 'correct') && promiseDefinitionProblems(draft.definition, false), (selectedAction === 'transfer' || selectedAction === 'recovery_transfer') && !validPromiseId(draft.toUser) && 'Enter the eligible target user ID.'].flat().filter((value): value is string => !!value)
  const review = async () => {
    update({ checking: true, error: null })
    try {
      const repositoryIds = (selectedAction === 'amend' || selectedAction === 'correct' ? draft.definition : current.definition).repositories.map((repository) => repository.repository_id)
      const preflight = await admissionPreflight(session, repositoryIds)
      update({ checking: false, reviewed: { action: selectedAction, definition: structuredClone(draft.definition), reason: draft.reason.trim(), toUser: draft.toUser.trim(), basis: structuredClone(current), preflight } })
    } catch (cause) { update({ checking: false, error: preflightError(cause) }) }
  }
  const submit = async () => {
    if (!draft.reviewed) return
    const reviewed = draft.reviewed
    const mutation: PromiseChange = reviewed.action === 'amend' || reviewed.action === 'correct' ? { kind: reviewed.action, content: content(reviewed.definition) } : reviewed.action === 'retire' ? { kind: 'retire' } : { kind: reviewed.action, to_user: reviewed.toUser }
    update({ busy: true, error: null })
    try {
      const receipt = await changePromise(session, reviewed.basis, reviewed.reason, mutation, reviewed.preflight.digest)
      setSaved(freshChangeDraft(current, available))
      onChanged(receipt)
    } catch (cause) {
      const status = cause instanceof WorkspaceError ? cause.status : 0
      setSaved({ ...draft, busy: false, error: workspaceError(cause), uncertain: !(cause instanceof WorkspaceError) || status >= 500, ...(status === 409 ? { reviewed: null, stale: true } : {}) })
      if ([401, 403, 404, 409].includes(status)) onRefresh()
    } finally { setSaved((state) => state.busy ? { ...state, busy: false } : state) }
  }
  return <Box icon="pencil-square" title="Authority change" tone="warn" explain={`Every change appends a version. This form is pinned to version ${current.ref.version} and digest ${current.ref.digest}.`}>
    {draft.error && <Notice tone="bad" title={draft.error}>{draft.uncertain ? 'The outcome may be unknown. Keep this confirmation unchanged and retry the exact retained operation.' : 'The latest session and promise state are being refreshed.'}</Notice>}
    {draft.stale ? <Notice tone="warn" icon="arrow-clockwise" title="Waiting for the current version">Review the refreshed promise before preparing another change.</Notice> : draft.reviewed ? <>
      <Notice tone="warn" title={`Confirm ${label(draft.reviewed.action)}`}>From version {draft.reviewed.basis.ref.version}, exact digest <code className="text-break">{draft.reviewed.basis.ref.digest}</code>. Reason: {draft.reviewed.reason}.{(draft.reviewed.action === 'transfer' || draft.reviewed.action === 'recovery_transfer') && <> New steward: <code>{draft.reviewed.toUser}</code>.</>}</Notice>
      {(draft.reviewed.action === 'amend' || draft.reviewed.action === 'correct') && <DefinitionSummary definition={draft.reviewed.definition} />}
      <SourceAdmissions admissions={draft.reviewed.preflight.admissions} reviewed />
      <div className="d-flex gap-2"><button type="button" className="btn btn-danger" disabled={draft.busy} onClick={() => void submit()}>{draft.busy ? <span className="spinner-border spinner-border-sm" aria-hidden="true" /> : <Icon name="check2-circle" />} {draft.uncertain ? 'Retry exact change' : `Record ${label(draft.reviewed.action)}`}</button><button type="button" className="btn btn-outline-secondary" disabled={draft.busy || draft.uncertain} onClick={() => update({ reviewed: null })}>Back to edit</button></div>
    </> : <>
      <div><label className="form-label" htmlFor="promise-action">Action</label><select id="promise-action" className="form-select" value={selectedAction} onChange={(event) => update({ action: event.target.value as OwnerAction })}>{available.map(({ kind, title }) => <option key={kind} value={kind}>{title}</option>)}</select></div>
      {(selectedAction === 'amend' || selectedAction === 'correct') && <div className="mt-3"><DefinitionEditor value={draft.definition} onChange={(definition) => update({ definition })} originalReadOnly prefix="owner" /></div>}
      {(selectedAction === 'transfer' || selectedAction === 'recovery_transfer') && <div className="mt-3"><Field id="promise-transfer-user" label="Eligible target user ID" value={draft.toUser} mono placeholder="usr_…" onChange={(toUser) => update({ toUser })} /></div>}
      <div className="mt-3"><Field id="promise-change-reason" label="Reason" value={draft.reason} textarea help="The reason is retained on the immutable new version." onChange={(reason) => update({ reason })} /></div>
      {issues.length > 0 && <ul className="small warn-text mt-3" aria-live="polite">{issues.map((issue) => <li key={issue}>{issue}</li>)}</ul>}
      <button type="button" className="btn btn-warning mt-3" disabled={issues.length > 0 || available.length === 0 || draft.checking} onClick={() => void review()}>{draft.checking ? 'Checking current authority…' : 'Review exact change'}</button>
    </>}
  </Box>
}

function Detail({ session, id, scope, refreshSession }: { session: WorkspaceSession; id: string; scope: string; refreshSession: () => void }) {
  const [refresh, setRefresh] = useState(0)
  const [receipt, setReceipt] = useState<PromiseReceipt | null>(null)
  const load = usePoll((signal) => visible(() => readPromise(id, signal)), 15_000, `${scope}:${id}`, refresh)
  const authority = usePoll((signal) => visible(() => changeCapabilities(id, signal)), 15_000, `${scope}:${id}:authority`, refresh)
  const reload = () => { setRefresh((value) => value + 1); refreshSession() }
  if (load.kind === 'loading') return <Skeleton rows={6} />
  if (load.kind === 'error') return <div className="page"><a href="#promises" className="back"><Icon name="arrow-left" /> All promises</a><Notice tone="bad" title={pollError(load)} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} /></div>
  if (load.value.kind === 'denied') return <div className="page"><a href="#promises" className="back"><Icon name="arrow-left" /> All promises</a><Notice tone="muted" icon="lock" title="This promise is not readable">It may not exist, or your current workspace access may not cover its complete repository scope.</Notice></div>
  const view: PromiseView = load.value.value
  const current = view.current
  const capabilities = authority.kind === 'ready' && authority.value.kind === 'visible' && !authority.refreshError ? authority.value.value : null
  const canChange = capabilities && Object.values(capabilities).some(Boolean)
  return <div className="page text-break"><a href="#promises" className="back"><Icon name="arrow-left" /> All promises</a><div className="answer" data-tour="answer"><h1 className="page-question">{current.definition.commitment}</h1><p className="answer-text"><Pill tone={current.state === 'active' ? 'brand' : 'muted'}>{label(current.state)}</Pill> Version {current.ref.version}</p><p className="answer-fact">An immutable owner-confirmed definition. Assessments are separate and are not available from this hub yet.</p></div>{receipt && <Receipt receipt={receipt} />}{load.refreshError && <Notice title="Showing the last loaded version">Refresh failed: {load.refreshError}</Notice>}<div className="grid-2 align-start"><Box icon="bookmark-check" title="Current promise"><DefinitionSummary definition={current.definition} /></Box><Box icon="fingerprint" title="Authority and identity"><dl className="tech mb-0"><Fact label="Promise ID"><code>{current.ref.promise_id}</code></Fact><Fact label="Version">{current.ref.version}</Fact><Fact label="Digest"><code className="text-break">{current.ref.digest}</code></Fact><Fact label="Steward"><code>{current.steward_user}</code></Fact><Fact label="Created by"><code>{current.created_by}</code></Fact><Fact label="Recorded">{new Date(current.created_at * 1000).toLocaleString()}</Fact></dl><SourceAdmissions admissions={current.repository_admissions} />{authority.kind === 'loading' ? <Skeleton rows={1} /> : canChange ? <Notice tone="brand" icon="person-check" title="Current change authority confirmed">The hub will recheck your authority and complete repository scope at submission.</Notice> : <Notice tone="muted" icon="eye" title="Read-only for you">The hub has not granted a current change capability for this promise.</Notice>}</Box></div>{capabilities && canChange && current.state === 'active' && <OwnerChange session={session} current={current} capabilities={capabilities} onChanged={(next) => { setReceipt(next); reload() }} onRefresh={reload} />}{authority.kind === 'error' && <Notice tone="warn" title="Change authority could not be checked" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={authority.retry}>Retry</button>} />}{authority.kind === 'ready' && authority.refreshError && <Notice tone="warn" title="Change authority needs refreshing" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={authority.retry}>Retry</button>} />}<History id={id} scope={scope} /><Reports id={id} scope={scope} /></div>
}

function SignedOut() { return <div className="page"><div className="answer"><h1 className="page-question">What must remain true?</h1><p className="answer-text">Promises use the workspace’s human sign-in, not an agent token.</p></div><SectionTabs /><Notice tone="muted" icon="lock" title="Sign in to read promises">No promise details or counts were requested. <a href="#workspace">Open workspace sign-in</a>, then return here.</Notice></div> }

export function PromisesPage() {
  const [sessionRefresh, setSessionRefresh] = useState(0)
  const [sessionKey] = useState(() => crypto.randomUUID())
  const [route, setRoute] = useState(() => promiseIdFromHash())
  useEffect(() => { const change = () => setRoute(promiseIdFromHash()); window.addEventListener('hashchange', change); return () => window.removeEventListener('hashchange', change) }, [])
  const session = usePoll(async (signal): Promise<{ value: WorkspaceSession } | { denied: number }> => {
    try { return { value: await workspaceRequest<WorkspaceSession>('/session', { signal }) } }
    catch (error) { if (error instanceof WorkspaceError && [401, 403, 404].includes(error.status)) return { denied: error.status }; throw error }
  }, 15_000, sessionKey, sessionRefresh)
  if (session.kind === 'loading') return <div className="page"><Skeleton rows={4} /></div>
  if (session.kind === 'error') return <div className="page"><div className="answer"><h1 className="page-question">What must remain true?</h1><p className="answer-text">Durable owner commitments live in a configured workspace.</p></div><SectionTabs /><Notice tone="muted" icon="gear" title={session.status === 404 || session.status === 503 ? 'Promises are not configured on this hub' : pollError(session)} action={<button className="btn btn-sm btn-outline-secondary" onClick={session.retry}>Retry</button>} /></div>
  if ('denied' in session.value && session.value.denied === 401) return <SignedOut />
  if ('denied' in session.value || session.refreshError) return <div className="page"><div className="answer"><h1 className="page-question">What must remain true?</h1><p className="answer-text">Workspace access must be current before promises can be shown.</p></div><SectionTabs /><Notice tone="muted" icon="lock" title="Promise access needs a fresh session" action={<button className="btn btn-sm btn-outline-secondary" onClick={session.retry}>Retry</button>} /></div>
  const current = session.value.value
  const scope = `${sessionKey}:${current.workspace.id}:${current.workspace.generation}:${current.actor.user.id}:${current.actor.generation}`
  if (route) return <Detail session={current} id={route} scope={scope} refreshSession={() => setSessionRefresh((value) => value + 1)} />
  return <div className="page text-break"><div className="answer" data-tour="answer"><h1 className="page-question">What must remain true?</h1><p className="answer-text">Owner-confirmed commitments that outlive a task, session or agent.</p><p className="answer-fact">You are signed in as {current.actor.user.login}. The hub filters every promise by its complete repository scope.</p></div><SectionTabs /><PromiseList session={current} scope={scope} refreshSession={() => setSessionRefresh((value) => value + 1)} /></div>
}
