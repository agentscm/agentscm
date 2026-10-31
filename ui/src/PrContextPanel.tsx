import { useState } from 'react'
import { fetchPrContext, safeContextUrl, validHead, type PrContext, type ProviderCategory } from './prContext.ts'
import { skillRunHref } from './skills.ts'
import { workHref } from './standup.ts'
import type { PullRequest } from './types.ts'
import { Ago, Box, Empty, Notice, Paths, Pill, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'
import { useNow } from './useHub.ts'

const short = (head: string | null | undefined) => head?.slice(0, 8) ?? 'unknown'


export function PrContextPanel({ repo, token, pulls }: { repo: string; token: string; pulls: PullRequest[] }) {
  const [selected, setSelected] = useState<number | null>(null)
  const open = pulls.filter((p) => p.state === 'open')
  const pr = open.find((p) => p.number === selected) ?? open[0]
  return <Box id="pr-context" title="What do we know about this PR?" icon="git">
    {open.length === 0 ? <Empty icon="git" title="No retained open pull requests">Connect a forge in Setup, then let the Hub observe an open PR. Missing observations do not mean there is no work.</Empty> : <>
      <label className="form-label" htmlFor="context-pr">Pull request</label>
      <select id="context-pr" className="form-select mb-3" value={pr?.number} onChange={(e) => setSelected(Number(e.target.value))}>
        {open.map((p) => <option key={p.number} value={p.number}>#{p.number} {p.title}</option>)}
      </select>
      {pr && <Context key={`${repo}|${token}|${pr.number}`} repo={repo} token={token} pr={pr} />}
    </>}
  </Box>
}

function Context({ repo, token, pr }: { repo: string; token: string; pr: PullRequest }) {
  // Preserve the selected row's head as a caller-reported baseline while this panel is open.
  const [expected] = useState(validHead(pr.head_sha) ? pr.head_sha : undefined)
  const poll = usePoll((signal) => fetchPrContext(repo, pr.number, expected, token, signal), 0, `pr-context|${repo}|${token}|${pr.number}|${expected ?? ''}`)
  if (poll.kind === 'loading') return <Skeleton rows={3} />
  if (poll.kind === 'error' || poll.refreshError) return <Notice tone="warn" title="PR context is unavailable" action={<button className="btn btn-sm btn-outline-secondary" onClick={poll.retry}>Try again</button>}>{poll.kind === 'error' ? poll.message : poll.refreshError} Previous context is hidden until access and data can be checked again.</Notice>
  const data = poll.value
  if (data.availability !== 'available') return <Empty icon="git" title={data.availability === 'not_connected' ? 'No forge connection' : 'This PR is not in the retained mirror'}>Open GitHub to check its current state, or <button className="btn btn-link p-0" onClick={poll.retry}>refresh the retained context</button>.{(data.coverage?.last_sync_partial || data.coverage?.retained_pr_scan_saturated) && <span className="d-block mt-2">The retained scan or last sync is incomplete; this PR may be missing because of that gap.</span>}</Empty>
  return <ContextFacts data={data} refresh={poll.retry} />
}

export function ContextFacts({ data, refresh, headBasis = 'pr_list' }: { data: PrContext; refresh: () => void; headBasis?: 'pr_list' | 'work_link' }) {
  const now = useNow()
  const p = data.pull_request!
  const url = safeContextUrl(p.url)
  const old = p.observed_at_ms <= 0 || now - p.observed_at_ms > 15 * 60 * 1000 || p.observed_at_ms > now + 60_000
  const stale = data.workflows?.some((r) => r.applicability !== 'matches_observed_head')
  return <div className="pr-context-facts">
    <div className="d-flex flex-wrap align-items-center gap-2 mb-2">
      <Pill tone={old ? 'warn' : 'muted'}>{old ? 'Older or uncertain observation' : 'Retained observation'}</Pill>
      {p.draft && <Pill tone="warn">Draft</Pill>}
      <button className="btn btn-sm btn-outline-secondary ms-auto" onClick={refresh}>Refresh context</button>
    </div>
    <p className="mb-2"><strong>#{p.number} {p.title}</strong> <span className="small muted">by {p.author || 'unknown author'}</span><br /><code title={p.head_sha ?? undefined}>{short(p.head_sha)}</code> on <code>{p.head_ref}</code> → <code>{p.base_ref}</code><br /><span className="small muted">Provider observation · <Ago at={p.observed_at_ms / 1000} />. GitHub may have changed since this read.</span></p>
    {data.head_relation === 'changed' && <Notice tone="warn" title="The observed head changed">{headBasis === 'work_link' ? 'The recorded Work relationship names head ' : 'When this panel opened, the PR list showed '}<code>{short(data.expected_head)}</code>; context now reports <code>{short(p.head_sha)}</code>. Recheck evidence for this head.</Notice>}
    {stale && <Notice tone="warn" title="Some workflow evidence belongs to another head">Earlier evidence does not establish the status of this head.</Notice>}
    {(data.coverage?.last_sync_partial || data.coverage?.retained_pr_scan_saturated || data.coverage?.files_partial || !data.coverage?.work_complete) && <Notice tone="warn" title="Some context coverage is incomplete">The last sync, retained scan, PR paths, or Work list has a known gap. Missing overlaps or workflow records do not establish that there is nothing to check.</Notice>}
    <ProviderFacts data={data} />
    <h3 className="h6 mt-4">Reported workflow steps</h3>
    <p className="small muted">Self-reported AgentSCM records, linked by PR number. These are not GitHub approval or a merge decision.</p>
    {data.workflows?.length ? <ul className="list-unstyled">{data.workflows.map((r) => <li key={r.run_id} className="mb-3">
      <Pill tone={r.applicability !== 'matches_observed_head' ? 'warn' : r.blocks_finish ? 'bad' : 'muted'}>{r.applicability !== 'matches_observed_head' ? 'Different or unknown head' : r.blocks_finish ? 'Reported blocker' : 'Reported on this head'}</Pill>
      <p className="mb-1 mt-1">{r.next_action}</p>
      <a href={skillRunHref(r.run_id)}>Inspect workflow record</a> <span className="small muted">· <code>{short(r.head_sha)}</code> · reported <Ago at={r.updated_at} /></span>
    </li>)}</ul> : <p>No explicitly linked workflow in the bounded result. <a href="#skills/runs">Inspect workflow runs</a>.</p>}
    {!!data.workflows_omitted && <p>{data.workflows_omitted} more linked workflows omitted.</p>}
    <h3 className="h6 mt-4">Who shares these paths?</h3>
    <p className="small muted">Advisory overlap only. Shared paths do not prove a conflict or link a task to this PR.</p>
    {data.pull_request_overlaps?.map((other) => <div key={other.number} className="mb-3">
      <strong>PR #{other.number}: {other.title}</strong> <span className="small muted">by {other.author || 'unknown author'}</span> {other.draft && <Pill tone="muted">Draft</Pill>}
      <div><Paths items={other.paths} /> <span className="small muted">· observed head <code>{short(other.head_sha)}</code> · <Ago at={other.observed_at_ms / 1000} /></span></div>
      {(other.paths_omitted > 0 || other.files_partial) && <span className="small muted">Only some paths are shown.</span>}
    </div>)}
    {data.work_overlaps?.map((work) => <div key={work.work_id} className="mb-3">
      <a href={workHref(work.work_id)}>{work.title}</a>
      <div className="small muted">{work.agent} · {work.owner} · reported <Ago at={work.updated_at} /> · {work.claim_status === 'active' ? 'assigned' : 'quiet or unknown, still assigned'}</div>
      <Paths items={work.paths} />
      {work.paths_partial && <span className="small muted"> · path witnesses may be incomplete</span>}
      {work.blocker && <p className="mb-1">Task’s reported blocker: {work.blocker}</p>}
      {work.next_action && <p className="mb-1">Task’s next step: {work.next_action}</p>}
    </div>)}
    {!data.pull_request_overlaps?.length && !data.work_overlaps?.length && <p>No overlap in the retained, bounded records. This is not an all-clear.</p>}
    {(!!data.work_overlaps_omitted || !!data.pull_request_overlaps_omitted) && <p>More overlapping records were omitted.</p>}
    <p><a href="#inbox">Open coordination inbox</a> · <a href="#activity">Catch up since your last review</a></p>
    <details className="mt-3">
      <summary>Coverage and source identity</summary>
      <p className="small mt-2">Provider evidence coverage is shown above. Local dirty source is not observed. Retained PRs and the newest 25 workflow runs only; neither list establishes complete coverage. Sources are read separately.</p>
      <p className="small">Last completed sync: {data.coverage?.last_sync_partial === null ? 'coverage unknown' : data.coverage?.last_sync_partial ? 'PR pages or rereads were incomplete' : 'no PR gaps recorded; current coverage is still unknown'}. {data.coverage?.retained_pr_scan_saturated && 'The retained PR scan reached its limit.'}</p>
      <p className="small">PR paths: {data.coverage?.files_partial ? 'partial' : 'complete at the retained head'}. Work list: {data.coverage?.work_complete ? 'complete for its read' : 'partial'} · read <Ago at={data.coverage?.work_observed_at ?? 0} />.</p>
      <p className="small">PR head: <code>{p.head_sha ?? 'unknown'}</code><br />Base: <code>{p.base_ref}</code> · <code>{p.base_sha ?? 'revision not retained'}</code>.</p>
    </details>
    <Notice title="Provider observations are not a merge decision">Open the PR for current checks, review requests and merge requirements. A recorded result here cannot establish that the PR is ready to merge.</Notice>
    {url && <a className="btn btn-sm btn-outline-secondary" href={url} target="_blank" rel="noreferrer noopener">Open PR on forge</a>}
  </div>
}

function ProviderCategoryFacts({ title, category }: { title: string; category: ProviderCategory }) {
  return <section aria-label={title} className="mb-3">
    <h4 className="h6">{title}</h4>
    <p className="small muted">{category.complete ? 'Complete for this bounded read' : `Incomplete or unavailable: ${category.availability.replaceAll('_', ' ')}`} · {category.items.length} shown · {category.omitted} omitted · {category.malformed} malformed · {category.pages_read} pages read · unread pages {category.unread_pages ?? 'unknown'}.</p>
    {category.items.length > 0 && <ul className="list-unstyled">{category.items.map((item) => <li key={item.provider_id} className="mb-2">
      <strong>{item.name ?? `Review ${item.provider_id}`}</strong>{title === 'Reviews' && <> · by {item.reviewer ?? 'unknown reviewer'}</>} · {item.state.replaceAll('_', ' ')}{item.conclusion && <> · {item.conclusion.replaceAll('_', ' ')}</>}
      <div className="small muted">Provider ID {item.provider_id} · commit <code title={item.commit_sha}>{short(item.commit_sha)}</code> · {item.head_relation === 'same' ? 'matches observed head' : 'different from observed head'} · provider {item.provider_time_kind ?? 'time'} {item.provider_time ?? 'unknown'}.</div>
      <div className="small muted">Attempt unknown · tested merge unknown. {item.source_url && <a href={safeContextUrl(item.source_url) ?? undefined} target="_blank" rel="noreferrer noopener">Inspect provider record</a>}</div>
    </li>)}</ul>}
  </section>
}
function ProviderFacts({ data }: { data: PrContext }) {
  const evidence = data.provider_evidence
  if (!evidence || evidence.availability !== 'observed') return <section aria-label="Provider reviews and checks" className="mt-4">
    <h3 className="h6">Provider reviews and checks</h3><p>Provider evidence {evidence?.availability.replaceAll('_', ' ') ?? 'not collected'}. Missing records do not establish a successful check or an approval.</p>
  </section>
  const snapshot = evidence.snapshot
  return <section aria-label="Provider reviews and checks" className="mt-4">
    <h3 className="h6">Provider reviews and checks</h3>
    <p className="small muted">Retained provider observation · <Ago at={snapshot.observed_at / 1000} /> · repository ID {snapshot.repository_id}. Sources were read separately. Access uses your Hub token; live requester access on GitHub was not revalidated.</p>
    <p className="small muted">Granted App permissions: {snapshot.permissions.availability.replaceAll('_', ' ')} · reviews {snapshot.permissions.pull_requests?.replaceAll('_', ' ') ?? 'unknown'} · checks {snapshot.permissions.checks?.replaceAll('_', ' ') ?? 'unknown'} · statuses {snapshot.permissions.statuses?.replaceAll('_', ' ') ?? 'unknown'}.</p>
    {snapshot.retention_truncated && <p>Some provider items were omitted to fit the retained snapshot.</p>}
    <ProviderCategoryFacts title="Reviews" category={snapshot.reviews} />
    <ProviderCategoryFacts title="Latest check runs by name" category={snapshot.check_runs} />
    <ProviderCategoryFacts title="Commit statuses" category={snapshot.commit_statuses} />
    <p className="small muted">Attempts and tested merge revisions remain unknown. These records do not establish current merge requirements or readiness.</p>
  </section>
}
