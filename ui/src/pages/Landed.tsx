import { useEffect, useState } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import { dateSpan, fetchPublication, headline, listPublications, listTrack, loadRecent, recentLabels, rememberRecent, track, validPublicationId, type ExternalRun, type ExternalState, type Outcome, type Publication, type Step } from '../publications.ts'
import { Ago, Box, Empty, Explain, Icon, Notice, PageHead, Pill, Skeleton, Tip, relative, type Tone } from '../ui.tsx'
import { Combobox, type ComboResult } from '../Combobox.tsx'
import { hashPath, Pager, useHashCursor } from '../Pager.tsx'
import { usePoll } from '../usePoll.ts'
import { who } from '../nextStep.ts'
import { ProofBadge } from '../Proof.tsx'
import { proofOfExternal } from '../proof.ts'
import type { WorkItem } from '../standup.ts'
import { useTaskIndex } from '../taskIndex.ts'
import { lead, WorkLink } from './workParts.tsx'

export const landedHref = (id: string) => `#landed/${encodeURIComponent(id)}`

/** `#landed` is the list; `#landed/<id>` one change. A malformed id is just the list. */
export function landedIdFromHash(hash = window.location.hash): string | null {
  const m = /^#landed\/([^/]+)$/.exec(hashPath(hash))
  if (!m) return null
  try {
    const id = decodeURIComponent(m[1]!)
    return validPublicationId(id) ? id : null
  } catch {
    return null
  }
}

const OUTCOME: Record<Outcome, { tone: Tone; icon: string; word: string }> = {
  succeeded: { tone: 'ok', icon: 'check-circle', word: 'Succeeded' },
  failed: { tone: 'bad', icon: 'x-octagon', word: 'Failed' },
  running: { tone: 'brand', icon: 'arrow-repeat', word: 'Running' },
  pending: { tone: 'brand', icon: 'hourglass-split', word: 'Waiting' },
  cancelled: { tone: 'warn', icon: 'slash-circle', word: 'Cancelled' },
  superseded: { tone: 'warn', icon: 'arrow-up-right-circle', word: 'Replaced' },
  unknown: { tone: 'warn', icon: 'question-circle', word: 'Unclear' },
}

/** The signature: five plain steps from branch to deploys, each with an icon, a word and a reason. */
export function Track({ steps, compact = false }: { steps: Step[]; compact?: boolean }) {
  return (
    <ol className={`track${compact ? ' is-compact' : ''}`} aria-label="How far this change got">
      {steps.map((s) => (
        <li key={s.key} className={`track-step tone-${s.tone}`}>
          <span className="track-dot" aria-hidden="true">
            <i className={`bi bi-${s.icon}`} />
          </span>
          <span className="track-text">
            <span className="track-label">{s.label}</span>
            {s.detail && !compact ? (
              <Tip text={s.detail} focusable className="track-word">{s.word}</Tip>
            ) : (
              <span className="track-word">{s.word}</span>
            )}
          </span>
        </li>
      ))}
    </ol>
  )
}

/** "Task: <title> · maria’s Claude Code", the title linking to the task. Keep it outside any row link so anchors don't nest. */
export function TaskLine({ item, className = '' }: { item: WorkItem; className?: string }) {
  return <p className={`small mb-0 ${className}`}><TaskInline item={item} /></p>
}
/** The same line without a paragraph, for places that already sit inside one. */
export function TaskInline({ item }: { item: WorkItem }) {
  const l = lead(item)
  return (
    <>
      Task:{' '}
      <span className="task-line-link"><WorkLink id={item.id}>{item.title}</WorkLink></span>
      {' '}· {who(l.login, l.agent)}
    </>
  )
}

const HONESTY: Record<ExternalState, { tone: Tone; icon: string; title: string; body: string }> = {
  observed: { tone: 'ok', icon: 'eye', title: 'Checked', body: 'The hub read GitHub for this merge in full.' },
  incomplete: { tone: 'warn', icon: 'exclamation-triangle', title: 'Partly checked', body: 'The hub could not read everything, so this list may be missing runs.' },
  unobserved: { tone: 'muted', icon: 'hourglass', title: 'Not checked yet', body: 'The hub has not read this merge yet. An empty list does not mean success.' },
}

function RunList({ runs, deployment, state }: { runs: ExternalRun[]; deployment: boolean; state: ExternalState }) {
  if (runs.length === 0)
    return (
      <Empty icon={deployment ? 'cloud' : 'gear'} title={state === 'observed' ? (deployment ? 'No deployments' : 'No CI runs') : 'Nothing to show yet'}>
        {state === 'observed' ? `GitHub showed no ${deployment ? 'deployments' : 'CI runs on the base branch'} for this merge.` : 'This is not a pass: the hub has not finished reading this merge.'}
      </Empty>
    )
  return (
    <ul className="run-list">
      {runs.map((r) => {
        const o = OUTCOME[r.outcome]
        const title = deployment ? r.environment ?? r.name : r.name
        return (
          <li key={`${r.runId}:${r.attempt}`} className="run-row">
            <span className={`run-icon tone-${o.tone}`}><Icon name={o.icon} /></span>
            <span className="run-main">
              <strong>{title || (deployment ? 'Deployment' : 'Workflow run')}</strong>
              <span className="small muted">
                {deployment && r.name && r.name !== title ? <>{r.name} · </> : null}
                {!deployment && r.branch ? <>on <code>{r.branch}</code> · </> : null}
                {r.attempt > 1 ? <>attempt {r.attempt} · </> : null}
                {r.updatedAt > 0 ? <>updated <Ago at={r.updatedAt} /></> : 'no update time'}
              </span>
            </span>
            <Pill tone={o.tone}>{o.word}</Pill>
            {r.url ? (
              <a href={r.url} target="_blank" rel="noreferrer noopener" className="btn btn-sm btn-outline-secondary" aria-label={`Open ${title || 'run'} on GitHub`}>
                <Icon name="box-arrow-up-right" />
              </a>
            ) : null}
          </li>
        )
      })}
    </ul>
  )
}

function Detail({ repo, id, token }: { repo: string; id: string; token: string }) {
  const load = usePoll((signal) => fetchPublication(repo, id, token, signal), 15_000, `${repo}|${id}`)
  const tasks = useTaskIndex(token)
  useEffect(() => {
    if (load.kind === 'ready' && load.value.supported) rememberRecent(id, load.value.publication.title ?? load.value.publication.branch)
  }, [load, id])
  if (load.kind === 'loading') return <Skeleton rows={6} />
  if (load.kind === 'error')
    return (
      <div className="page">
        <a href="#landed" className="back"><Icon name="arrow-left" /> All changes</a>
        <Notice tone="bad" title={load.status === 404 ? 'No change with this ID in this repository' : 'Could not load this change'} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Try again</button>}>
          {load.status === 404 ? 'Check the ID. Your token only sees changes published to its own repository.' : load.message}
        </Notice>
      </div>
    )
  if (!load.value.supported)
    return (
      <div className="page">
        <a href="#landed" className="back"><Icon name="arrow-left" /> All changes</a>
        <Notice tone="muted" icon="lock" title="Published changes aren’t readable here yet">This hub doesn’t let the status page read published changes yet — it needs a newer hub. The page uses a read-only token and never asks for an agent token.</Notice>
      </div>
    )
  const p = load.value.publication
  const x = p.external
  const h = headline(p)
  const task = p.branch ? tasks?.byBranch.get(p.branch) : undefined
  const route = <><code className="branch-name">{p.branch || p.id}</code> into <code className="branch-name">{p.base}</code></>
  return (
    <div className="page">
      <a href="#landed" className="back"><Icon name="arrow-left" /> All changes</a>
      <PageHead question={p.title ?? `${p.branch || p.id} into ${p.base}`} answer={<Pill tone={h.tone}>{h.text}</Pill>} fact={task ? <TaskInline item={task} /> : p.title ? route : undefined} />
      {load.refreshError && <Notice title="Showing the last loaded state">The latest refresh failed: {load.refreshError}</Notice>}
      {p.error && <Notice tone="bad" title="The hub recorded a problem with this change">{p.error}</Notice>}
      <Box icon="signpost-split" title="How far it got" explain="Each step is what the hub recorded. Hover a step for why it shows that word.">
        <Track steps={track(p)} />
        {p.branchState === 'rejected' && p.branchMessage && <p className="small mt-3 mb-0"><Icon name="info-circle" /> GitHub said: {p.branchMessage}</p>}
        <div className="mt-3"><SyncNote updatedAt={p.updatedAt} /></div>
      </Box>
      {p.pr.state === 'merged' &&
        (x ? (
          <>
            <div className={`honesty tone-${HONESTY[x.state].tone}`} role="status">
              <Icon name={HONESTY[x.state].icon} />
              <span>
                <strong>{HONESTY[x.state].title}</strong>
                {x.state === 'observed' && x.reconciledAt ? <> <Ago at={x.reconciledAt} /></> : null}. {HONESTY[x.state].body}
                {x.state === 'incomplete' && x.reconciledAt ? <> Last full read attempt <Ago at={x.reconciledAt} />.</> : null}
              </span>
              <ProofBadge {...proofOfExternal(x)} />
            </div>
            <div className="grid-2 align-start">
              <Box icon="gear-wide-connected" title="CI after merge" count={x.ci.length} explain="GitHub Actions runs for the exact merge commit on the base branch. Runs for pull requests or other branches are left out.">
                <RunList runs={x.ci} deployment={false} state={x.state} />
              </Box>
              <Box icon="cloud-upload" title="Deployments" count={x.deployments.length} explain="Each deployment of the merge commit and its latest status. An environment can have more than one.">
                <RunList runs={x.deployments} deployment state={x.state} />
              </Box>
            </div>
            <p className="small muted"><Icon name="info-circle" /> {x.note} Nothing here decides whether a change can merge.</p>
          </>
        ) : (
          <Notice tone="muted" icon="eye-slash" title="CI and deployments are not tracked here">This hub is not set to watch GitHub CI and deployments for this repository, so nothing is shown. It does not mean they passed.</Notice>
        ))}
      <details className="box disclosure">
        <summary>Technical details</summary>
        <dl className="tech">
          <dt>Change ID</dt><dd><code>{p.id}</code></dd>
          <dt>Branch</dt><dd>{route}</dd>
          <dt>Revision</dt><dd><code>{p.revision}</code></dd>
          <dt>Branch commit</dt><dd><code>{p.commit}</code></dd>
          {p.pr.mergeCommit && <><dt>Merge commit</dt><dd><code>{p.pr.mergeCommit}</code></dd></>}
          {p.pr.number !== null && (
            <>
              <dt>Pull request</dt>
              <dd>{p.pr.url ? <a href={p.pr.url} target="_blank" rel="noreferrer noopener">#{p.pr.number} <Icon name="box-arrow-up-right" /></a> : `#${p.pr.number}`}</dd>
            </>
          )}
          <dt>Recorded locally as landed</dt><dd>{p.nativeLanded ? <>Yes, by the publishing agent <ProofBadge level="reported" /></> : 'No'} <Explain text="The publishing agent says so. The hub has nothing to check it against." /></dd>
          {p.historical && <><dt>Kind</dt><dd>Historical candidate, not re-qualified</dd></>}
        </dl>
      </details>
    </div>
  )
}

function Lookup({ repo, token, recent, onOpen }: { repo: string; token: string; recent: string[]; onOpen: (id: string) => void }) {
  const labels = recentLabels()
  return (
    <div className="mb-3">
      <div className="lookup">
        <Combobox
          id="landed-id"
          data-tour="landed-picker"
          label="Find a change"
          placeholder="Find a change by branch, title or PR number, or paste a pub_ ID"
          describedBy="landed-id-help"
          parseId={(t) => (validPublicationId(t) ? t : null)}
          onSelect={(id) => onOpen(id)}
          search={async function page(q, signal, before?: string): Promise<ComboResult> {
            const r = await listPublications(repo, token, signal, before, { q, limit: 10 })
            if (!r.supported) return { options: [] }
            const next = r.nextCursor
            return {
              more: next !== null,
              ...(next !== null ? { loadMore: (s: AbortSignal) => page(q, s, next) } : {}),
              options: r.publications.map((p) => {
                const h = headline(p)
                return {
                  value: p.id,
                  text: p.branch || p.id,
                  secondary: p.id,
                  label: (
                    <>
                      <code className="branch-name">{p.branch || p.id}</code>
                      {p.pr.number !== null ? <span className="small muted"> · PR #{p.pr.number}</span> : null}
                      {p.title ? <span className="small"> · {p.title}</span> : null}
                    </>
                  ),
                  meta: (
                    <>
                      <Pill tone={h.tone}>{h.text}</Pill>
                      {p.updatedAt ? <span className="small muted">{relative(p.updatedAt, Date.now())}</span> : null}
                    </>
                  ),
                }
              }),
            }
          }}
        />
      </div>
      <div id="landed-id-help" className="form-text">Change IDs start with pub_; the publishing agent prints them and the hub posts them on the PR.</div>
      {recent.length > 0 && (
        <div className="recent">
          <span className="small muted">Opened in this browser</span>
          {recent.map((r) => (
            <a key={r} href={landedHref(r)} className="recent-chip" title={r}>{labels[r] ?? <code>{r.slice(0, 12)}…</code>}</a>
          ))}
        </div>
      )}
    </div>
  )
}

/** Read-only viewers see merge state as the hub last recorded it; the hub doesn't ask the forge for them. */
function SyncNote({ updatedAt }: { updatedAt?: number | null }) {
  return (
    <p className="small muted mb-0 sync-note">
      <Icon name="arrow-repeat" /> Merge state as of the hub’s last sync{updatedAt ? <> (<Ago at={updatedAt} />)</> : null}{' '}
      <Explain text="This page reads with a read-only token, so the hub doesn’t ask GitHub on your behalf. A merge shows up after the hub’s own sync or an agent’s next look records it." />
    </p>
  )
}

const DETAIL_LIMIT = 25
const DETAIL_WORKERS = 4
const DETAIL_EVERY_MS = 60_000

/** CI and deployments for the merged changes on this page: at most 25, four at a time, once a minute. */
function useDetails(repo: string, token: string, ids: string[]): Record<string, Publication | 'failed'> {
  const [details, setDetails] = useState<Record<string, Publication | 'failed'>>({})
  const key = ids.slice(0, DETAIL_LIMIT).join(',')
  useEffect(() => {
    if (!key) return
    const ctrl = new AbortController()
    let running = false
    const run = async () => {
      // Skip ticks while the tab is hidden or the previous run is still going.
      if (running || document.hidden) return
      running = true
      const queue = key.split(',')
      const worker = async () => {
        for (let id = queue.shift(); id !== undefined; id = queue.shift()) {
          try {
            const r = await fetchPublication(repo, id, token, ctrl.signal)
            setDetails((d) => ({ ...d, [id]: r.supported ? r.publication : 'failed' }))
          } catch {
            if (!ctrl.signal.aborted) setDetails((d) => ({ ...d, [id]: 'failed' }))
          }
        }
      }
      try {
        await Promise.all(Array.from({ length: Math.min(DETAIL_WORKERS, queue.length) }, worker))
      } finally {
        running = false
      }
    }
    void run()
    const timer = window.setInterval(() => void run(), DETAIL_EVERY_MS)
    return () => {
      ctrl.abort()
      window.clearInterval(timer)
    }
  }, [repo, token, key])
  return details
}

function List({ repo, token }: { repo: string; token: string }) {
  const pager = useHashCursor()
  const cursor = pager.cursor ?? ''
  const load = usePoll((signal) => listPublications(repo, token, signal, cursor || undefined), cursor ? 0 : 15_000, `${repo}|${cursor}`)
  const recent = loadRecent()
  const ready = load.kind === 'ready' && load.value.supported ? load.value : null
  const landed = ready?.publications.filter((p) => p.pr.state === 'merged') ?? []
  const details = useDetails(repo, token, landed.map((p) => p.id))
  const tasks = useTaskIndex(token)
  const shown = (p: Publication) => {
    const d = details[p.id]
    return d && d !== 'failed' ? d : p
  }
  // Only changes whose details loaded can count as failing; "Checking…" never counts either way.
  const failing = landed.filter((p) => details[p.id] && details[p.id] !== 'failed' && headline(shown(p)).tone === 'bad')
  const span = ready ? dateSpan(ready.publications) : null
  // "Not tracked" only once every merged change's details have loaded and none carries a CI read; until then the page makes no claim.
  const untracked = landed.length > 0 && landed.every((p) => { const d = details[p.id]; return d !== undefined && d !== 'failed' && d.external === null })
  return (
    <div className="page">
      <PageHead
        question="Were our agents’ changes merged, and what happened after?"
        answer={
          load.kind === 'loading'
            ? 'Looking…'
            : !ready
              ? 'This hub doesn’t let the status page read published changes yet — it needs a newer hub.'
              : ready.publications.length === 0
                ? 'No published changes yet.'
                : <>{landed.length} of {ready.publications.length} recent change{ready.publications.length === 1 ? '' : 's'} merged{span ? ` (${span})` : ''}{failing.length ? <>, and <span className="text-danger-emphasis">{failing.length} had a failure after merging</span></> : null}.{untracked ? ' Test and deploy results after merge are not tracked.' : null}</>
        }
        fact={
          untracked ? (
            <>Tracking is off. A hub admin can turn it on for this repository (GitHub only, experimental). <Explain text="The hub operator sets AGENTSCM_GITHUB_EXTERNAL_RUNS to this repository’s owner/name. The token needs Actions and Deployments read access. See External CI and deployment outcomes in the hub docs." /></>
          ) : (
            'Merged on GitHub or GitLab, as of the hub’s last sync. Nothing here decides whether a change can merge.'
          )
        }
      />
      <SectionTabs />
      <Box tour="landed-list" icon="rocket-takeoff" tone={failing.length ? 'bad' : 'brand'} title="Recent changes" explain="Changes agents published to this repository, newest first. Search by branch, title or PR number; each row shows how far it got. Nothing here decides whether a change can merge.">
        <Lookup repo={repo} token={token} recent={recent} onOpen={(id) => { window.location.hash = landedHref(id).slice(1) }} />
        {load.kind === 'loading' ? (
          <Skeleton rows={4} />
        ) : load.kind === 'error' ? (
          <Notice tone="bad" title="Could not load recent changes" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Try again</button>}>{load.message}</Notice>
        ) : !ready ? (
          <Empty icon="lock" title="Not readable with this page’s token yet">This hub doesn’t let the status page read published changes yet — it needs a newer hub. Once it is, every change shows here with its branch, pull request, merge, CI and deploys.</Empty>
        ) : ready.publications.length === 0 ? (
          <Empty icon="rocket" title="Nothing published yet">Changes appear here once an agent publishes one through the hub.</Empty>
        ) : (
          <>
            <ul className="run-list">
              {ready.publications.map((p) => {
                const checking = p.pr.state === 'merged' && (!details[p.id] || details[p.id] === 'failed')
                const h = checking ? { tone: 'muted' as const, text: details[p.id] === 'failed' ? 'Merged, couldn’t check after' : 'Merged, checking…' } : headline(shown(p))
                const branch = <code className="branch-name">{p.branch || p.id}</code>
                const task = p.branch ? tasks?.byBranch.get(p.branch) : undefined
                return (
                  <li key={p.id}>
                    <a href={landedHref(p.id)} className="run-row landed-row">
                      <span className="run-main">
                        <strong>{p.title ?? branch}</strong>
                        <span className="small muted">{p.title && <>{branch} </>}into <code>{p.base}</code>{p.pr.number !== null ? <> · PR #{p.pr.number}</> : null}</span>
                      </span>
                      <Track steps={listTrack(p, details[p.id])} compact />
                      <Pill tone={h.tone}>{h.text}</Pill>
                      <Icon name="chevron-right" className="muted" />
                    </a>
                    {task && <TaskLine item={task} className="px-3 pb-1" />}
                  </li>
                )
              })}
            </ul>
            <Pager newerLabel="Newer changes" olderLabel="Older changes" onNewer={pager.canNewer ? pager.newer : undefined} onOlder={ready.nextCursor ? () => pager.older(ready.nextCursor!) : undefined} complete={ready.complete} />
            <SyncNote />
          </>
        )}
      </Box>
    </div>
  )
}

export function LandedPage({ repo, token }: { repo: string | null; token: string }) {
  const [id, setId] = useState(() => landedIdFromHash())
  useEffect(() => {
    const on = () => setId(landedIdFromHash())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  if (!repo) return <Skeleton rows={4} />
  return id ? <Detail key={id} repo={repo} id={id} token={token} /> : <List repo={repo} token={token} />
}
