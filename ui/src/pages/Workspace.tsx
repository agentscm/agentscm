import { useEffect, useRef, useState, type ReactNode } from 'react'
import { decodeSkillWorkflowEnvelope, validSkillWorkflowStepId } from '../skillWorkflow.ts'
import {
  identityKey,
  workspaceError,
  WorkspaceError,
  workspaceMutation,
  workspacePendingMutationEvent,
  workspacePendingMutations,
  workspaceRequest,
  type PendingWorkspaceMutation,
  type WorkspaceMember,
  type WorkspaceProfile,
  type WorkspaceRepository,
  type WorkspaceRun,
  type WorkspaceSession,
  type WorkspaceSettings,
  type WorkspaceSubject,
} from '../workspace.ts'
import { Box, CopyButton, Empty, Explain, Icon, Notice, Pill, Skeleton, Tip, type Tone } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'
import { RunView } from './Workflows.tsx'

// Every write on this page goes through workspaceMutation (ui/src/workspace.ts): CSRF header, request ID,
// a retained retry for unknown outcomes and response checks. Sign-out is the one plain DELETE, as in the hub UI.

const CAPS: { key: keyof WorkspaceSession['capabilities']; label: string; icon: string; tip: string }[] = [
  { key: 'read_repositories', label: 'Read repositories', icon: 'eye', tip: 'See repositories, profiles and runs in this workspace.' },
  { key: 'execute_workflows', label: 'Run workflows', icon: 'play-circle', tip: 'Start and cancel workflow runs on a repository profile.' },
  { key: 'approve_revisions', label: 'Approve revisions', icon: 'patch-check', tip: 'Record approval of an exact revision.' },
  { key: 'integrate_revisions', label: 'Integrate revisions', icon: 'sign-merge-right', tip: 'Land an approved revision on its protected target.' },
  { key: 'manage_workspace', label: 'Manage workspace', icon: 'gear', tip: 'Change members, roles and workspace settings.' },
]
const ROLES: { value: string; tip: string }[] = [
  { value: 'reader', tip: 'Inspect repositories, profiles and runs.' },
  { value: 'contributor', tip: 'Start and cancel workflow runs.' },
  { value: 'reviewer', tip: 'Record native approvals of exact revisions.' },
  { value: 'integrator', tip: 'Land approved revisions on their protected target.' },
  { value: 'administrator', tip: 'Manage members and settings. Does not by itself grant repository read or execution.' },
]

const activeRun = (state: string) => state === 'queued' || state === 'running'
const runTone = (state: string): Tone => (state === 'succeeded' ? 'ok' : ['failed', 'unknown', 'stale', 'blocked'].includes(state) ? 'bad' : activeRun(state) ? 'brand' : state === 'cancelled' ? 'muted' : 'warn')
/** A response we can retry exactly: network failures and 5xx other than "busy" may have reached the hub. */
const outcomeUnknown = (e: unknown) => !(e instanceof WorkspaceError) || (e.status >= 500 && e.code !== 'busy')
const pollError = (p: Extract<Poll<unknown>, { kind: 'error' }>) => workspaceError(new WorkspaceError(p.status ?? 0, p.message))
const enc = encodeURIComponent

// ---------- Routes: #workspace, #workspace/members, #workspace/settings, #workspace/workflow/<repo>/<run>[/steps/<step>]

export type WorkspaceRoute =
  | { tab: 'repositories' | 'members' | 'settings' }
  | { tab: 'workflow'; repositoryId: string; runId: string; stepId?: string }
  | { tab: 'workflow'; invalid: true }

const decode = (v: string | undefined) => {
  try {
    return v ? decodeURIComponent(v) : ''
  } catch {
    return ''
  }
}
export function workspaceRoute(hash = window.location.hash): WorkspaceRoute {
  const parts = hash.slice(1).split('/')
  if (parts[0] !== 'workspace') return { tab: 'repositories' }
  if (parts[1] === 'members' || parts[1] === 'settings') return { tab: parts[1] }
  if (parts[1] !== 'workflow') return { tab: 'repositories' }
  const repositoryId = decode(parts[2])
  const runId = decode(parts[3])
  if (!repositoryId || !runId) return { tab: 'workflow', invalid: true }
  const stepId = parts[4] === 'steps' ? decode(parts[5]) : ''
  return { tab: 'workflow', repositoryId, runId, ...(stepId && validSkillWorkflowStepId(stepId) ? { stepId } : {}) }
}
export const workspaceRunHref = (repositoryId: string, runId: string) => `#workspace/workflow/${enc(repositoryId)}/${enc(runId)}`

function useWorkspaceRoute(): WorkspaceRoute {
  const [route, setRoute] = useState(() => workspaceRoute())
  useEffect(() => {
    const on = () => setRoute(workspaceRoute())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

// ---------- A two-step confirmation that stays in the keyboard flow (no modal)

function Confirm({ label, icon, question, confirm, disabled, busy, onConfirm, tone = 'outline-secondary' }: { label: ReactNode; icon: string; question: string; confirm: string; disabled?: boolean; busy?: boolean; onConfirm: () => void; tone?: string }) {
  const [asking, setAsking] = useState(false)
  const returning = useRef(false)
  const yes = useRef<HTMLButtonElement>(null)
  const trigger = useRef<HTMLButtonElement>(null)
  useEffect(() => {
    if (asking) yes.current?.focus()
    else if (returning.current) trigger.current?.focus()
    returning.current = false
  }, [asking])
  const back = () => {
    returning.current = true
    setAsking(false)
  }
  if (!asking)
    return (
      <button ref={trigger} type="button" className={`btn btn-sm btn-${tone}`} disabled={disabled || busy} onClick={() => setAsking(true)}>
        {busy ? <span className="spinner-border spinner-border-sm" aria-hidden="true" /> : <Icon name={icon} />} {label}
      </button>
    )
  return (
    <span className="confirm-inline" role="group" aria-label={question} onKeyDown={(e) => e.key === 'Escape' && back()}>
      <span className="small">{question}</span>
      <button
        ref={yes}
        type="button"
        className="btn btn-sm btn-primary"
        onClick={() => {
          setAsking(false)
          onConfirm()
        }}
      >
        {confirm}
      </button>
      <button type="button" className="btn btn-sm btn-outline-secondary" onClick={back}>
        Keep
      </button>
    </span>
  )
}

// ---------- Signed out

function Login() {
  const load = usePoll((signal) => workspaceRequest<{ providers: string[] }>('/login', { signal }), 0, 'login')
  return (
    <Box icon="people" title="Sign in to your workspace" explain="The workspace uses your GitHub or GitLab account, not a token. Your operator must add your numeric forge identity first.">
      <p>Manage members and run workflows with your forge account.</p>
      {load.kind === 'loading' && <Skeleton rows={1} />}
      {load.kind === 'error' && <Notice title={pollError(load)} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />}
      {load.kind === 'ready' &&
        (load.value.providers.filter((p) => p === 'github' || p === 'gitlab').length === 0 ? (
          <Notice tone="muted" icon="info-circle" title="Workspace sign-in is not configured on this hub." />
        ) : (
          <div className="d-flex flex-wrap gap-2">
            {load.value.providers
              .filter((p) => p === 'github' || p === 'gitlab')
              .map((p) => (
                <a key={p} className="btn btn-primary" href={`/v1/hub/login/${p}?workspace=1`}>
                  <Icon name={p} /> Continue with {p === 'github' ? 'GitHub' : 'GitLab'}
                </a>
              ))}
          </div>
        ))}
    </Box>
  )
}

// ---------- Pending operation recovery (the retained "unknown outcome" journal)

const pendingAction = (entry: PendingWorkspaceMutation): { label: string; capability: keyof WorkspaceSession['capabilities'] } => {
  if (entry.path.endsWith('/approve')) return { label: 'Native approval', capability: 'approve_revisions' }
  if (entry.path.endsWith('/integrate')) return { label: 'Native landing', capability: 'integrate_revisions' }
  if (entry.path.endsWith('/cancel')) return { label: 'Run cancellation', capability: 'execute_workflows' }
  if (entry.path.endsWith('/runs')) return { label: 'Workflow start', capability: 'execute_workflows' }
  if (entry.path === '/members') return { label: 'Membership change', capability: 'manage_workspace' }
  return { label: 'Workspace settings change', capability: 'manage_workspace' }
}
const pendingSubject = (entry: PendingWorkspaceMutation): WorkspaceSubject | null => {
  const s = entry.body['subject']
  if (!s || typeof s !== 'object' || Array.isArray(s)) return null
  const f = s as Record<string, unknown>
  return typeof f['change_id'] === 'string' && typeof f['change_version'] === 'number' && typeof f['context_digest'] === 'string' && typeof f['revision'] === 'string' && typeof f['requirement_digest'] === 'string' ? (f as unknown as WorkspaceSubject) : null
}

function PendingOperations({ session }: { session: WorkspaceSession }) {
  const [entries, setEntries] = useState(() => workspacePendingMutations(session))
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<{ message: string; returnedId: string | null } | null>(null)
  const result = useRef<HTMLDivElement>(null)
  // The retry button disappears with its entry; move focus to the outcome instead of dropping it.
  useEffect(() => {
    if (done) result.current?.focus()
  }, [done])
  useEffect(() => {
    const load = () => setEntries(workspacePendingMutations(session))
    window.addEventListener(workspacePendingMutationEvent, load)
    return () => window.removeEventListener(workspacePendingMutationEvent, load)
  }, [session])
  const retry = async (entry: PendingWorkspaceMutation) => {
    const action = pendingAction(entry)
    if (!session.capabilities[action.capability]) return
    setBusy(entry.requestId)
    setError(null)
    setDone(null)
    try {
      const result = await workspaceMutation<Record<string, unknown> | undefined>(session, entry.path, entry.body, { method: entry.method, ...(entry.expectation ? { expectation: entry.expectation } : {}) })
      const id = result ? (['approval_id', 'operation_id', 'run_id'].map((k) => result[k]).find((v): v is string => typeof v === 'string') ?? null) : null
      setDone({ message: `${action.label} returned a conclusive result.`, returnedId: id })
    } catch (e) {
      setError(workspaceError(e))
    } finally {
      setBusy(null)
    }
  }
  // Stay mounted while a retry is in flight: its success clears the entry before the outcome is shown.
  if (entries.length === 0 && !done && !error && busy === null) return null
  return (
    <Box icon="arrow-repeat" tone="warn" title="Pending operation recovery" count={entries.length} explain="Requests that may have reached the hub but whose outcome is unknown. Only their non-secret fields and request IDs are kept, in this tab. Nothing is retried automatically.">
      {error && <Notice tone="bad" title={error} />}
      {done && (
        <div ref={result} tabIndex={-1}>
        <Notice tone="ok" icon="check-circle" title={done.message} action={<button type="button" className="btn btn-sm btn-outline-secondary" aria-label="Dismiss" onClick={() => setDone(null)}><Icon name="x-lg" /></button>}>
          {done.returnedId && <>Returned ID <code>{done.returnedId}</code> <CopyButton text={done.returnedId} /></>}
        </Notice>
        </div>
      )}
      <ul className="plain-list">
        {entries.map((entry) => {
          const action = pendingAction(entry)
          const subject = pendingSubject(entry)
          const allowed = session.capabilities[action.capability]
          const generation = typeof entry.body['expected_generation'] === 'number' ? entry.body['expected_generation'] : null
          return (
            <li key={`${entry.method}:${entry.path}`} className="pending-op">
              <div>
                <strong><Icon name="question-circle" /> {action.label} outcome unknown</strong>
                {subject && <div className="small">Exact revision <code>{subject.revision.slice(0, 16)}…</code> · Change version {subject.change_version}</div>}
                {entry.expectation?.kind === 'landing' && <div className="small">Target <code>{entry.expectation.target}</code></div>}
                <div className="small">
                  Request <code>{entry.requestId}</code>
                  {generation !== null && ` · Membership generation ${generation}`}
                </div>
                <div className="small muted">{allowed ? 'Retry sends the exact retained body with your current security token.' : `Your current roles do not allow this ${action.label.toLowerCase()} retry.`}</div>
              </div>
              <button type="button" className="btn btn-sm btn-primary" disabled={!allowed || busy !== null} onClick={() => void retry(entry)}>
                {busy === entry.requestId ? <span className="spinner-border spinner-border-sm" aria-hidden="true" /> : <Icon name="arrow-clockwise" />} Retry exact request
              </button>
            </li>
          )
        })}
      </ul>
    </Box>
  )
}

// ---------- Repositories & execution

interface Page<T> {
  key: string
  items: T[]
  cursor: string | null
}

function Execution({ session }: { session: WorkspaceSession }) {
  const canRead = session.capabilities.read_repositories
  const canExecute = session.capabilities.execute_workflows
  const repos = usePoll((signal) => (canRead ? workspaceRequest<{ repositories: WorkspaceRepository[] }>('/repositories', { signal }) : Promise.resolve({ repositories: [] })), 0, String(canRead))
  const repositories = repos.kind === 'ready' ? repos.value.repositories : null
  const [chosenRepo, setChosenRepo] = useState<string | null>(null)
  const repositoryId = chosenRepo ?? repositories?.[0]?.repository_id ?? null
  const repository = repositories?.find((r) => r.repository_id === repositoryId)
  const [runCursors, setRunCursors] = useState<string[]>([''])
  const runCursor = runCursors.at(-1) ?? ''
  const [refresh, setRefresh] = useState(0)
  const runs = usePoll(
    (signal) => (repositoryId ? workspaceRequest<{ runs: WorkspaceRun[]; next_cursor: string | null }>(`/repositories/${enc(repositoryId)}/runs?limit=20${runCursor ? `&before=${enc(runCursor)}` : ''}`, { signal }) : Promise.resolve({ runs: [], next_cursor: null })),
    5_000,
    `${repositoryId}|${runCursor}|${refresh}`,
  )
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const pendingCancels = useRef(new Map<string, Record<string, unknown>>())

  const cancel = async (run: WorkspaceRun) => {
    setBusy(run.run_id)
    setError(null)
    const body = pendingCancels.current.get(run.run_id) ?? { expected_generation: run.generation }
    try {
      await workspaceMutation(session, `/repositories/${enc(run.repository_id)}/runs/${enc(run.run_id)}/cancel`, body)
      pendingCancels.current.delete(run.run_id)
      setNotice(`Cancellation requested for ${run.run_id}. Refreshing its durable state.`)
      setRefresh((n) => n + 1)
    } catch (e) {
      if (outcomeUnknown(e)) pendingCancels.current.set(run.run_id, body)
      else pendingCancels.current.delete(run.run_id)
      setError(workspaceError(e))
    } finally {
      setBusy(null)
    }
  }

  if (!canRead) return <Notice tone="muted" icon="lock" title="Your membership does not include repository read access.">Ask an administrator to add the reader role for the repositories you need.</Notice>
  if (repos.kind === 'loading') return <Skeleton rows={4} />
  if (repos.kind === 'error') return <Notice title={pollError(repos)} action={<button className="btn btn-sm btn-outline-secondary" onClick={repos.retry}>Retry</button>} />
  if (!repositories || repositories.length === 0) return <Empty icon="folder" title="No execution repositories are configured">Ask the operator to map a native repository and workflow profile.</Empty>

  return (
    <div className="stack">
      {error && <Notice tone="bad" title={error} />}
      {notice && <Notice tone="brand" icon="info-circle" title={notice} />}
      {!canExecute && <Notice tone="muted" icon="info-circle" title="You can inspect these repositories. Starting or cancelling runs requires the contributor role." />}
      <Box
        icon="folder2-open"
        title="Recent runs"
        explain="Durable workspace execution state, newest first, 20 per page. Unknown and stale runs need operator review before a new attempt."
        actions={
          <>
            <select
              className="form-select form-select-sm w-auto"
              aria-label="Repository"
              value={repositoryId ?? ''}
              disabled={busy !== null}
              onChange={(e) => {
                setChosenRepo(e.target.value)
                setRunCursors([''])
              }}
            >
              {repositories.map((r) => <option key={r.repository_id} value={r.repository_id}>{r.repository_id}</option>)}
            </select>
            <Tip text="Refresh runs">
              <button type="button" className="btn btn-sm btn-outline-secondary" aria-label="Refresh runs" onClick={() => setRefresh((n) => n + 1)}><Icon name="arrow-clockwise" /></button>
            </Tip>
          </>
        }
      >
        {runs.kind === 'loading' && <Skeleton rows={3} />}
        {runs.kind === 'error' && <Notice title={pollError(runs)} />}
        {runs.kind === 'ready' && (runs.value.runs.length === 0 ? (
          <Empty icon="inbox" title="No workspace runs yet">Open New run below to load a Change and start its configured workflow.</Empty>
        ) : (
          <ul className="run-list">
            {runs.value.runs.map((r) => (
              <li key={r.run_id} className="run-row ws-run">
                <span className={`run-icon tone-${runTone(r.state)}`}><Icon name="diagram-3" /></span>
                <span className="run-main">
                  <strong>{r.profile_id}</strong>
                  <span className="small muted">
                    <code>{r.run_id}</code> · change {r.subject.change_id} v{r.subject.change_version} · rev <Tip text={r.subject.revision} focusable><code>{r.subject.revision.slice(0, 10)}</code></Tip>
                  </span>
                  {r.detail && <span className="small">{r.detail}</span>}
                </span>
                <Pill tone={runTone(r.state)}>{r.state}</Pill>
                <span className="ws-actions">
                  {r.native_run_id && <a className="btn btn-sm btn-outline-secondary" href={workspaceRunHref(r.repository_id, r.run_id)}><Icon name="search" /> Inspect</a>}
                  {canExecute && activeRun(r.state) && (
                    <Confirm label="Cancel" icon="stop-circle" question={`Cancel run ${r.run_id}?`} confirm="Cancel run" busy={busy === r.run_id} disabled={busy !== null} onConfirm={() => void cancel(r)} />
                  )}
                </span>
              </li>
            ))}
          </ul>
        ))}
        <nav className="pager" aria-label="Run pages">
          <button type="button" className="btn btn-sm btn-outline-secondary" disabled={runCursors.length <= 1} onClick={() => setRunCursors((c) => c.slice(0, -1))}><Icon name="chevron-left" /> Newer runs</button>
          <button type="button" className="btn btn-sm btn-outline-secondary" disabled={runs.kind !== 'ready' || !runs.value.next_cursor} onClick={() => runs.kind === 'ready' && runs.value.next_cursor && setRunCursors((c) => [...c, runs.value.next_cursor!])}>Older runs <Icon name="chevron-right" /></button>
        </nav>
        <p className="small muted mb-0">Configuration readiness does not prove runtime availability or a passing admission receipt.</p>
      </Box>
      {repository && <Composer key={repository.repository_id} session={session} repository={repository} onStarted={() => { setRunCursors(['']); setRefresh((n) => n + 1) }} />}
    </div>
  )
}

function Composer({ session, repository, onStarted }: { session: WorkspaceSession; repository: WorkspaceRepository; onStarted: () => void }) {
  const canExecute = session.capabilities.execute_workflows
  const [profileId, setProfileId] = useState('')
  const profile = repository.profiles.find((p) => p.profile_id === profileId)
  const base = `/repositories/${enc(repository.repository_id)}/profiles/${enc(profileId)}`
  const firstSubjects = usePoll(
    (signal) => (profileId ? workspaceRequest<{ subjects: WorkspaceSubject[]; next_cursor: string | null }>(`${base}/subjects?limit=20`, { signal }) : Promise.resolve({ subjects: [], next_cursor: null })),
    0,
    profileId,
  )
  const [more, setMore] = useState<Page<WorkspaceSubject> | null>(null)
  const extra = more?.key === profileId ? more : null
  const subjects = [...(firstSubjects.kind === 'ready' ? firstSubjects.value.subjects : []), ...(extra?.items ?? [])]
  const subjectCursor = extra ? extra.cursor : firstSubjects.kind === 'ready' ? firstSubjects.value.next_cursor : null
  const [changeId, setChangeId] = useState('')
  const [subject, setSubject] = useState<WorkspaceSubject | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [pendingStart, setPendingStart] = useState<Record<string, unknown> | null>(null)
  const selection = useRef(0)
  const locked = pendingStart !== null || busy !== null

  const clearSubject = () => {
    selection.current += 1
    setSubject(null)
    setNotice(null)
  }
  const loadMore = async () => {
    if (!subjectCursor) return
    const current = selection.current
    setBusy('more')
    try {
      const r = await workspaceRequest<{ subjects: WorkspaceSubject[]; next_cursor: string | null }>(`${base}/subjects?limit=20&after=${enc(subjectCursor)}`)
      if (selection.current === current) setMore({ key: profileId, items: [...(extra?.items ?? []), ...r.subjects], cursor: r.next_cursor })
    } catch (e) {
      if (selection.current === current) setError(workspaceError(e))
    } finally {
      setBusy(null)
    }
  }
  const loadSubject = async () => {
    if (!profile || !changeId.trim()) return
    const current = selection.current
    setBusy('subject')
    setError(null)
    setSubject(null)
    try {
      const r = await workspaceRequest<{ subject: WorkspaceSubject }>(`${base}/subject?change_id=${enc(changeId.trim())}`)
      if (selection.current === current) setSubject(r.subject)
    } catch (e) {
      if (selection.current === current) setError(workspaceError(e))
    } finally {
      setBusy(null)
    }
  }
  const start = async () => {
    if (!profile || !subject || !canExecute) return
    setBusy('start')
    setError(null)
    setNotice(null)
    const body = pendingStart ?? { subject, profile_id: profile.profile_id, expected_generation: session.actor.generation }
    const expectation = { kind: 'start' as const, repositoryId: repository.repository_id, profileId: body['profile_id'] as string, subject: body['subject'] as WorkspaceSubject }
    try {
      const run = await workspaceMutation<WorkspaceRun>(session, `/repositories/${enc(repository.repository_id)}/runs`, body, { expectation })
      setNotice(`Run ${run.run_id} accepted as ${run.state}.`)
      setPendingStart(null)
      setSubject(null)
      onStarted()
    } catch (e) {
      setPendingStart(outcomeUnknown(e) ? body : null)
      setError(workspaceError(e))
    } finally {
      setBusy(null)
    }
  }

  return (
    <details className="box ws-composer" open={pendingStart !== null || undefined}>
      <summary className="box-head">
        <span className="box-badge tone-brand"><Icon name="play-circle" /></span>
        <span className="box-title">New run & native admission{pendingStart ? ' · request outcome unknown' : ''}</span>
        <Explain text="Choose an operator-configured profile and load an existing native Change. Execution binds to its exact immutable revision." />
      </summary>
      <div className="box-body stack">
        {pendingStart && <Notice title="Start request outcome unknown">The original revision and generation are retained. Inspect recent runs, or retry the same request; changing the form is disabled until its outcome is confirmed.</Notice>}
        {error && <Notice tone="bad" title={error} />}
        {notice && <Notice tone="ok" icon="check-circle" title={notice} />}
        {repository.profiles.length === 0 ? (
          <Notice title="This repository has no execution profiles.">Ask the operator to configure one.</Notice>
        ) : (
          <div className="ws-grid">
            <label className="form-label" htmlFor="ws-profile">Workflow profile</label>
            <select
              id="ws-profile"
              className="form-select"
              value={profileId}
              disabled={locked}
              onChange={(e) => {
                setProfileId(e.target.value)
                setChangeId('')
                clearSubject()
              }}
            >
              <option value="">Choose profile</option>
              {repository.profiles.map((p: WorkspaceProfile) => <option key={p.profile_id} value={p.profile_id}>{p.profile_id} · {p.target}</option>)}
            </select>
          </div>
        )}
        {profile && (
          <div className="pills">
            <Pill tone={profile.readiness === 'configured' ? 'ok' : 'warn'} icon="sliders" tip="Operator configuration state. Only configured profiles can start runs.">{profile.readiness}</Pill>
            <Pill tone="muted" icon="bullseye" tip="The protected native target this profile lands on.">target {profile.target}</Pill>
            {profile.limitations && <span className="small muted">{profile.limitations}</span>}
          </div>
        )}
        {profile && (
          <div className="ws-grid">
            <label className="form-label" htmlFor="ws-known-change">Current native Changes</label>
            <div className="d-flex gap-2 flex-wrap">
              <select
                id="ws-known-change"
                className="form-select"
                value={subjects.some((s) => s.change_id === changeId) ? changeId : ''}
                disabled={locked}
                onChange={(e) => {
                  setChangeId(e.target.value)
                  clearSubject()
                }}
              >
                <option value="">{firstSubjects.kind === 'loading' ? 'Loading Changes…' : subjects.length ? 'Choose a current Change' : 'No current Changes found on this page'}</option>
                {subjects.map((s) => <option key={`${s.change_id}:${s.change_version}`} value={s.change_id}>{s.change_id} · version {s.change_version} · {s.revision.slice(0, 12)}</option>)}
              </select>
              {subjectCursor && <button type="button" className="btn btn-outline-secondary" disabled={busy !== null} onClick={() => void loadMore()}>Load more Changes</button>}
            </div>
            {firstSubjects.kind === 'error' && <Notice title={pollError(firstSubjects)} />}
          </div>
        )}
        <form
          className="ws-grid"
          onSubmit={(e) => {
            e.preventDefault()
            void loadSubject()
          }}
        >
          <label className="form-label" htmlFor="ws-change">Native Change ID <Explain text="The ID of an existing native Change. Loading it shows the exact revision, review context and requirement the run will bind to." /></label>
          <div className="input-group">
            <input
              id="ws-change"
              className="form-control"
              value={changeId}
              placeholder="Existing Change ID"
              disabled={locked}
              onChange={(e) => {
                setChangeId(e.target.value)
                clearSubject()
              }}
            />
            <button type="submit" className="btn btn-outline-secondary" disabled={!profile || !changeId.trim() || locked}>Load revision</button>
          </div>
        </form>
        {subject && profile && (
          <section className="ws-subject" aria-label="Revision to execute">
            <h3 className="h6">Revision to execute</h3>
            <dl className="tech">
              <dt>Revision</dt><dd><code>{subject.revision}</code></dd>
              <dt>Change version</dt><dd>{subject.change_version}</dd>
              <dt>Review context <Explain text="Digest of the review context the approval and landing are bound to." /></dt><dd><code>{subject.context_digest}</code></dd>
              <dt>Requirement <Explain text="Digest of the profile requirement this run must satisfy." /></dt><dd><code>{subject.requirement_digest}</code></dd>
            </dl>
            {canExecute ? (
              <button type="button" className="btn btn-primary" disabled={busy !== null || profile.readiness !== 'configured'} onClick={() => void start()}>
                {busy === 'start' ? <span className="spinner-border spinner-border-sm" aria-hidden="true" /> : <Icon name="play-fill" />} {pendingStart ? 'Retry same start request' : 'Start workflow'}
              </button>
            ) : (
              <p className="small muted">Starting runs requires the contributor role.</p>
            )}
            <NativeAdmission key={`${repository.repository_id}/${profile.profile_id}/${JSON.stringify(subject)}`} session={session} repositoryId={repository.repository_id} profile={profile} subject={subject} blocked={locked} />
          </section>
        )}
      </div>
    </details>
  )
}

type Approval = { approval_id: string; revision: string; context_digest: string; actor_id: string }
type Landing = { operation_id: string; target: string; after: { revision: string; version: number } }

function NativeAdmission({ session, repositoryId, profile, subject, blocked }: { session: WorkspaceSession; repositoryId: string; profile: WorkspaceProfile; subject: WorkspaceSubject; blocked: boolean }) {
  const canApprove = session.capabilities.approve_revisions === true
  const canIntegrate = session.capabilities.integrate_revisions === true
  const [approvalId, setApprovalId] = useState('')
  const [busy, setBusy] = useState<'approve' | 'integrate' | null>(null)
  const [pending, setPending] = useState<{ action: 'approve' | 'integrate'; body: Record<string, unknown> } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [approval, setApproval] = useState<Approval | null>(null)
  const [landing, setLanding] = useState<Landing | null>(null)
  const submit = async (action: 'approve' | 'integrate') => {
    if ((action === 'approve' && !canApprove) || (action === 'integrate' && (!canIntegrate || !approvalId.trim()))) return
    setBusy(action)
    setError(null)
    const request = pending ?? { action, body: { subject, expected_generation: session.actor.generation, ...(action === 'integrate' ? { approval_id: approvalId.trim() } : {}) } }
    const expectation = request.action === 'approve' ? { kind: 'approval' as const, revision: subject.revision, contextDigest: subject.context_digest } : { kind: 'landing' as const, revision: subject.revision, contextDigest: subject.context_digest, target: profile.target }
    try {
      const result = await workspaceMutation<Approval | Landing>(session, `/repositories/${enc(repositoryId)}/profiles/${enc(profile.profile_id)}/${request.action}`, request.body, { expectation })
      if (request.action === 'approve') {
        setApproval(result as Approval)
        setApprovalId((result as Approval).approval_id)
      } else setLanding(result as Landing)
      setPending(null)
    } catch (e) {
      setPending(outcomeUnknown(e) ? request : null)
      setError(workspaceError(e))
    } finally {
      setBusy(null)
    }
  }
  return (
    <section className="ws-admission" aria-label="Native approval and landing">
      <h3 className="h6">Approve and land this revision <Explain text="Approval and landing are separate actions. The server revalidates membership, the exact subject and required workflow evidence for each." /></h3>
      {error && <Notice tone="bad" title={error} />}
      {pending && <Notice title={`${pending.action === 'approve' ? 'Approval' : 'Landing'} outcome unknown`}>Retry keeps the original subject, generation and request ID. Inspect the native state before making a different change.</Notice>}
      {approval && <Notice tone="ok" icon="patch-check" title="Native approval recorded">Approval <code>{approval.approval_id}</code> is bound to the displayed revision and review context.</Notice>}
      {landing && <Notice tone="ok" icon="sign-merge-right" title={`Landed to ${landing.target}`}>Native target version {landing.after.version}. Operation <code>{landing.operation_id}</code></Notice>}
      <div className="ws-admission-grid">
        <div>
          {canApprove ? (
            <Confirm
              label={pending?.action === 'approve' ? 'Retry same approval' : 'Approve revision'}
              icon="patch-check"
              question={`Approve revision ${subject.revision.slice(0, 12)} in this review context?`}
              confirm="Approve"
              busy={busy === 'approve'}
              disabled={blocked || busy !== null || (pending !== null && pending.action !== 'approve') || landing !== null}
              onConfirm={() => void submit('approve')}
            />
          ) : null}
          <p className="small muted">{canApprove ? 'Records a native approval for this revision and context.' : 'Reviewer role required to approve.'}</p>
        </div>
        <div>
          <label className="form-label" htmlFor="ws-approval">Native approval ID</label>
          <input
            id="ws-approval"
            className="form-control mb-2"
            value={approvalId}
            placeholder="Approval from the reviewer"
            disabled={blocked || busy !== null || pending !== null || !canIntegrate}
            onChange={(e) => {
              setApprovalId(e.target.value)
              setLanding(null)
            }}
          />
          {canIntegrate ? (
            <Confirm
              tone="primary"
              label={pending?.action === 'integrate' ? 'Retry same landing' : `Land revision to ${profile.target}`}
              icon="sign-merge-right"
              question={`Land revision ${subject.revision.slice(0, 12)} on ${profile.target}? This updates the protected target.`}
              confirm={`Land to ${profile.target}`}
              busy={busy === 'integrate'}
              disabled={blocked || !approvalId.trim() || busy !== null || (pending !== null && pending.action !== 'integrate') || landing !== null}
              onConfirm={() => void submit('integrate')}
            />
          ) : null}
          <p className="small muted">{canIntegrate ? 'Updates the native target only after its current gate passes. Use the approval above or paste one issued for this exact subject.' : 'Integrator role required to land.'}</p>
        </div>
      </div>
    </section>
  )
}

// ---------- Members

interface MemberForm {
  provider: string
  host: string
  userId: string
  roles: string[]
  repositories: string
  enabled: boolean
}
const validUserId = (v: string) => /^[1-9][0-9]*$/.test(v) && Number.isSafeInteger(Number(v))

function Members({ session, onChanged }: { session: WorkspaceSession; onChanged: () => void }) {
  const canManage = session.capabilities.manage_workspace
  const first = usePoll((signal) => (canManage ? workspaceRequest<{ members: WorkspaceMember[]; next_cursor: string | null; generation: number }>('/members?limit=50', { signal }) : Promise.resolve(null)), 0, String(canManage))
  const [more, setMore] = useState<Page<WorkspaceMember> | null>(null)
  const [editing, setEditing] = useState<{ existing: WorkspaceMember | null; form: MemberForm } | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  if (!canManage) return <Notice tone="muted" icon="lock" title="Administrator access is required to view or change workspace members." />
  if (first.kind === 'loading') return <Skeleton rows={3} />
  if (first.kind === 'error') return <Notice title={pollError(first)} action={<button className="btn btn-sm btn-outline-secondary" onClick={first.retry}>Retry</button>} />
  if (!first.value) return null
  const generation = first.value.generation
  const members = [...first.value.members, ...(more?.items ?? [])]
  const cursor = more ? more.cursor : first.value.next_cursor

  const edit = (m: WorkspaceMember | null) => {
    setError(null)
    setEditing({ existing: m, form: m ? { provider: m.identity.provider, host: m.identity.host, userId: String(m.identity.user_id), roles: m.roles, repositories: m.repository_ids.join(', '), enabled: m.enabled } : { provider: 'github', host: 'github.com', userId: '', roles: ['reader'], repositories: '*', enabled: true } })
  }
  const loadMore = async () => {
    if (!cursor) return
    setBusy(true)
    try {
      const r = await workspaceRequest<{ members: WorkspaceMember[]; next_cursor: string | null; generation: number }>(`/members?limit=50&after=${enc(cursor)}`)
      if (r.generation !== generation) setError('Membership changed while loading. Refresh the workspace to see the current list.')
      else setMore({ key: '', items: [...(more?.items ?? []), ...r.members], cursor: r.next_cursor })
    } catch (e) {
      setError(workspaceError(e))
    } finally {
      setBusy(false)
    }
  }
  const f = editing?.form
  const set = (patch: Partial<MemberForm>) => editing && setEditing({ ...editing, form: { ...editing.form, ...patch } })
  const problems = f ? [!f.host.trim() && 'Enter the forge host.', !validUserId(f.userId) && 'Enter a positive, safe numeric user ID.', f.roles.length === 0 && 'Choose at least one role.', !f.repositories.trim() && 'Enter repository IDs, or * for all.'].filter((x): x is string => !!x) : []
  const save = async () => {
    if (!f || problems.length) return
    setBusy(true)
    setError(null)
    try {
      await workspaceMutation(
        session,
        '/members',
        { expected_generation: generation, identity: { provider: f.provider, host: f.host.trim(), user_id: Number(f.userId) }, roles: f.roles, repository_ids: f.repositories.split(',').map((v) => v.trim()).filter(Boolean), enabled: f.enabled },
        { method: 'PUT' },
      )
      setEditing(null)
      onChanged()
    } catch (e) {
      setError(workspaceError(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Box
      icon="people"
      title="Members"
      count={members.length}
      explain="Roles grant separate permissions. Administrator access alone does not grant repository read or execution access."
      actions={<button type="button" className="btn btn-sm btn-primary" disabled={editing !== null} onClick={() => edit(null)}><Icon name="person-plus" /> Add member</button>}
    >
      {error && <Notice tone="bad" title={error} />}
      {f && editing && (
        <form
          className="ws-member-form"
          aria-label={editing.existing ? 'Edit member' : 'Add forge member'}
          noValidate
          onSubmit={(e) => {
            e.preventDefault()
            void save()
          }}
        >
          <h3 className="h6">{editing.existing ? 'Edit member' : 'Add forge member'}</h3>
          <div className="ws-form-grid">
            <div>
              <label className="form-label" htmlFor="ws-m-provider">Forge</label>
              <select id="ws-m-provider" className="form-select" value={f.provider} disabled={!!editing.existing || busy} onChange={(e) => set({ provider: e.target.value })}>
                <option value="github">GitHub</option>
                <option value="gitlab">GitLab</option>
              </select>
            </div>
            <div>
              <label className="form-label" htmlFor="ws-m-host">Forge host</label>
              <input id="ws-m-host" className="form-control" value={f.host} disabled={!!editing.existing || busy} placeholder="github.com" onChange={(e) => set({ host: e.target.value })} />
            </div>
            <div>
              <label className="form-label" htmlFor="ws-m-user">Numeric forge user ID <Explain text="The forge's numeric account ID, which does not change when someone renames their login." /></label>
              <input id="ws-m-user" className={`form-control${f.userId && !validUserId(f.userId) ? ' is-invalid' : ''}`} inputMode="numeric" value={f.userId} disabled={!!editing.existing || busy} placeholder="12345" onChange={(e) => set({ userId: e.target.value })} />
            </div>
          </div>
          <fieldset className="mt-3">
            <legend className="form-label">Explicit roles</legend>
            <div className="d-flex flex-wrap gap-3">
              {ROLES.map((r) => (
                <div key={r.value} className="form-check">
                  <input id={`ws-role-${r.value}`} className="form-check-input" type="checkbox" checked={f.roles.includes(r.value)} disabled={busy} onChange={(e) => set({ roles: e.target.checked ? [...f.roles, r.value] : f.roles.filter((x) => x !== r.value) })} />
                  <label className="form-check-label" htmlFor={`ws-role-${r.value}`}>{r.value}</label> <Explain text={r.tip} />
                </div>
              ))}
            </div>
          </fieldset>
          <div className="mt-3">
            <label className="form-label" htmlFor="ws-m-repos">Repository IDs</label>
            <input id="ws-m-repos" className="form-control" value={f.repositories} disabled={busy} placeholder="owner/repository" aria-describedby="ws-m-repos-help" onChange={(e) => set({ repositories: e.target.value })} />
            <div id="ws-m-repos-help" className="form-text">Comma-separated repository IDs, or * for all configured repositories.</div>
          </div>
          <div className="form-check mt-3">
            <input id="ws-m-enabled" className="form-check-input" type="checkbox" checked={f.enabled} disabled={busy} onChange={(e) => set({ enabled: e.target.checked })} />
            <label className="form-check-label" htmlFor="ws-m-enabled">Membership enabled</label>
          </div>
          {problems.length > 0 && <ul className="small warn-text mt-2 mb-0" aria-live="polite">{problems.map((p) => <li key={p}>{p}</li>)}</ul>}
          <div className="d-flex gap-2 mt-3">
            <button type="submit" className="btn btn-primary" disabled={busy || problems.length > 0}>Save member</button>
            <button type="button" className="btn btn-outline-secondary" disabled={busy} onClick={() => setEditing(null)}>Cancel</button>
          </div>
        </form>
      )}
      {members.length === 0 ? (
        <Empty icon="people" title="No members available to display." />
      ) : (
        <ul className="plain-list" aria-label="Workspace members">
          {members.map((m) => (
            <li key={identityKey(m.identity)} className="ws-member">
              <Icon name={m.identity.provider === 'gitlab' ? 'gitlab' : 'github'} />
              <span className="ws-member-main">
                <strong>{m.identity.host} · user {m.identity.user_id}</strong>
                <span className="small muted">Repositories: {m.repository_ids.join(', ')}</span>
              </span>
              <span className="pills">
                {m.roles.map((r) => <Pill key={r} tone="muted">{r}</Pill>)}
                <Pill tone={m.enabled ? 'ok' : 'warn'} icon={m.enabled ? 'check-circle' : 'pause-circle'}>{m.enabled ? 'Enabled' : 'Disabled'}</Pill>
              </span>
              <button type="button" className="btn btn-sm btn-outline-secondary" aria-label={`Edit member ${m.identity.user_id}`} disabled={editing !== null} onClick={() => edit(m)}><Icon name="pencil" /> Edit</button>
            </li>
          ))}
        </ul>
      )}
      {cursor && <button type="button" className="btn btn-sm btn-outline-secondary mt-2" disabled={busy} onClick={() => void loadMore()}>Load more members</button>}
      <p className="small muted mt-3 mb-0">Disable a member to revoke workspace access. The last enabled administrator must remain.</p>
    </Box>
  )
}

// ---------- Settings

function Settings({ session, onChanged }: { session: WorkspaceSession; onChanged: () => void }) {
  const canManage = session.capabilities.manage_workspace
  const load = usePoll((signal) => (canManage ? workspaceRequest<WorkspaceSettings>('/settings', { signal }) : Promise.resolve(null)), 0, String(canManage))
  if (!canManage) return <Notice tone="muted" icon="lock" title="Administrator access is required to view or change workspace settings." />
  if (load.kind === 'loading') return <Skeleton rows={2} />
  if (load.kind === 'error') return <Notice title={pollError(load)} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>} />
  if (!load.value) return null
  return <SettingsForm key={load.value.generation} session={session} settings={load.value} onChanged={onChanged} />
}

function SettingsForm({ session, settings, onChanged }: { session: WorkspaceSession; settings: WorkspaceSettings; onChanged: () => void }) {
  const [name, setName] = useState(settings.name)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const problem = !name.trim() ? 'Enter a workspace name.' : name.length > 120 ? 'Keep the name to 120 characters.' : null
  const save = async () => {
    if (problem) return
    setBusy(true)
    setError(null)
    try {
      await workspaceMutation(session, '/settings', { name: name.trim(), max_concurrent_runs: settings.max_concurrent_runs, expected_generation: settings.generation }, { method: 'PUT' })
      onChanged()
    } catch (e) {
      setError(workspaceError(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Box icon="gear" title="Workspace settings" explain="The shared name, and the limit on concurrent workflow runs on this hub.">
      {error && <Notice tone="bad" title={error} />}
      <form
        className="ws-settings"
        aria-label="Workspace settings"
        noValidate
        onSubmit={(e) => {
          e.preventDefault()
          void save()
        }}
      >
        <label className="form-label" htmlFor="ws-name">Workspace name</label>
        <input id="ws-name" className={`form-control${problem ? ' is-invalid' : ''}`} maxLength={120} value={name} disabled={busy} aria-describedby="ws-name-help" onChange={(e) => setName(e.target.value)} />
        <div id="ws-name-help" className={problem ? 'invalid-feedback d-block' : 'form-text'}>{problem ?? 'Up to 120 characters.'}</div>
        <label className="form-label mt-3" htmlFor="ws-max-runs">Maximum concurrent runs <Explain text="This hub currently runs one workflow at a time. Additional runs remain queued." /></label>
        <input id="ws-max-runs" className="form-control" type="number" value={settings.max_concurrent_runs} min={1} max={1} disabled readOnly />
        <button type="submit" className="btn btn-primary mt-3" disabled={busy || !!problem || name.trim() === settings.name}>Save settings</button>
      </form>
    </Box>
  )
}

// ---------- Workspace workflow run (deep link)

function WorkspaceWorkflow({ repositoryId, runId, stepId }: { repositoryId: string; runId: string; stepId?: string }) {
  const [n, setN] = useState(0)
  const path = `/repositories/${enc(repositoryId)}/runs/${enc(runId)}`
  const load = usePoll(
    async (signal) => {
      const run = await workspaceRequest<WorkspaceRun>(path, { signal })
      return decodeSkillWorkflowEnvelope(await workspaceRequest<unknown>(`${path}/workflow`, { signal }), run.native_run_id ?? '')
    },
    0,
    `${path}|${n}`,
  )
  const reload = <button className="btn btn-sm btn-outline-secondary" onClick={() => setN((x) => x + 1)}>Refresh</button>
  return (
    <div className="stack">
      <a href="#workspace" className="back"><Icon name="arrow-left" /> Repositories & runs</a>
      {load.kind === 'loading' && <Skeleton rows={6} />}
      {load.kind === 'error' && <Notice title="Retained workflow is not available" action={reload}>{pollError(load)}</Notice>}
      {load.kind === 'ready' && load.value.availability === 'unavailable' && <Notice title="Retained workflow is not available yet" action={reload}>A queued run may not have written its native journal. Refresh after execution starts.</Notice>}
      {load.kind === 'ready' && load.value.availability === 'available' && (
        stepId && !load.value.projection.run.steps.some((s) => s.id === stepId) ? (
          <Notice tone="bad" title="This retained step was not found." />
        ) : (
          <RunView projection={load.value.projection} {...(stepId ? { stepId } : {})} stepHref={(id) => `${workspaceRunHref(repositoryId, runId)}/steps/${enc(id)}`} />
        )
      )}
    </div>
  )
}

// ---------- Page

const TABS = [
  { key: 'repositories', label: 'Repositories & runs', icon: 'folder2-open', href: '#workspace' },
  { key: 'members', label: 'Members', icon: 'people', href: '#workspace/members' },
  { key: 'settings', label: 'Settings', icon: 'gear', href: '#workspace/settings' },
] as const

export function WorkspacePage() {
  const route = useWorkspaceRoute()
  const [nonce, setNonce] = useState(0)
  const [signOutError, setSignOutError] = useState<string | null>(null)
  const session = usePoll((signal) => workspaceRequest<WorkspaceSession>('/session', { signal }), 15_000, String(nonce))
  const s = session.kind === 'ready' ? session.value : null
  const reload = () => setNonce((n) => n + 1)
  const signedOut = session.kind === 'error' && session.status === 401
  const activeTab = route.tab === 'workflow' ? 'repositories' : route.tab
  return (
    <div className="page">
      <div className="answer">
        <h1 className="page-question">Who can run what?</h1>
        <p className="answer-text">{s ? <>You’re signed in to <strong>{s.workspace.name}</strong> as {s.actor.user.login}.</> : 'The workspace holds shared repositories, members and workflow execution.'}</p>
      </div>
      {s?.synthetic_fixture && <Notice tone="brand" icon="flask" title="Synthetic workspace fixture">These records are examples, not live coding sessions.</Notice>}
      {signOutError && <Notice title={signOutError} />}
      {session.kind === 'loading' && <Skeleton rows={4} />}
      {signedOut && <Login />}
      {session.kind === 'error' && !signedOut && <Notice title={pollError(session)} action={<button className="btn btn-sm btn-outline-secondary" onClick={session.retry}>Retry</button>} />}
      {s && (
        <div className="stack">
          <Box
            icon="person-badge"
            title="You"
            explain="Your roles decide what you can do in this workspace."
            actions={
              <>
                <Tip text="Refresh workspace">
                  <button className="btn btn-sm btn-outline-secondary" onClick={reload} aria-label="Refresh workspace"><Icon name="arrow-clockwise" /></button>
                </Tip>
                <button
                  className="btn btn-sm btn-outline-secondary"
                  onClick={() => {
                    workspaceRequest('/session', { method: 'DELETE', csrf: s.csrf_token }).then(reload, (e: unknown) => setSignOutError(workspaceError(e)))
                  }}
                >
                  <Icon name="box-arrow-right" /> Sign out
                </button>
              </>
            }
          >
            <div className="d-flex flex-wrap gap-2 align-items-center mb-3">
              <Icon name={s.actor.user.identity.provider === 'gitlab' ? 'gitlab' : 'github'} /> <strong>{s.actor.user.login}</strong>
              {s.actor.roles.map((r) => <Pill key={r} tone="brand" icon="award">{r}</Pill>)}
            </div>
            <ul className="caps">
              {CAPS.map((c) => (
                <li key={c.key} className={s.capabilities[c.key] ? 'is-on' : 'is-off'}>
                  <Icon name={s.capabilities[c.key] ? 'check-circle-fill' : 'dash-circle'} /> <Icon name={c.icon} /> {c.label} <Explain text={c.tip} />
                  <span className="visually-hidden">{s.capabilities[c.key] ? 'allowed' : 'not allowed'}</span>
                </li>
              ))}
            </ul>
          </Box>
          <PendingOperations session={s} />
          <nav className="tabbar" aria-label="Workspace sections">
            {TABS.map((t) => (
              <a key={t.key} href={t.href} className={`tab${activeTab === t.key ? ' active' : ''}`} aria-current={activeTab === t.key ? 'page' : undefined}>
                <Icon name={t.icon} /> {t.label}
              </a>
            ))}
          </nav>
          {route.tab === 'workflow' && ('invalid' in route ? (
            <Notice tone="bad" title="Invalid workspace run link.">Open Repositories & runs to choose a run.</Notice>
          ) : (
            <WorkspaceWorkflow key={`${route.repositoryId}/${route.runId}`} repositoryId={route.repositoryId} runId={route.runId} {...(route.stepId ? { stepId: route.stepId } : {})} />
          ))}
          {route.tab === 'repositories' && <Execution key={nonce} session={s} />}
          {route.tab === 'members' && <Members key={nonce} session={s} onChanged={reload} />}
          {route.tab === 'settings' && <Settings key={nonce} session={s} onChanged={reload} />}
        </div>
      )}
    </div>
  )
}
