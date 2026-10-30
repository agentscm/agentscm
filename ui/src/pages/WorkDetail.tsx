// The page of one task: `#work/<work-id>`.
//
// API for other packages:
//   WorkDetail({ id, token, overview })   the task page; WorkPage renders it for a work-ID route
import type { ReactNode } from 'react'
import { fetchWork } from '../api.ts'
import { fetchChangeRecord } from '../changeRecord.ts'
import { Gloss } from '../Gloss.tsx'
import { lifecycle } from '../lifecycle.ts'
import { LifecycleStrip } from '../LifecycleStrip.tsx'
import { NextStepView } from '../NextStep.tsx'
import type { Handoff } from '../launchLink.ts'
import { needsYou } from '../needs.ts'
import { blockedStep, handoverStep, overlapSteps, quietStep, who, type NextStep } from '../nextStep.ts'
import { httpsOnly, listPublications } from '../publications.ts'
import { fetchRuns } from '../skills.ts'
import { fetchCoordination, fetchStandupWork, safeCommitHref, type WorkCheckpoint, type WorkItem, type WorkOverview, type WorkSourceRef } from '../standup.ts'
import { Ago, Avatar, Box, Empty, Icon, Notice, PageHead, Path, Paths, Pill, Skeleton, localDate } from '../ui.tsx'
import { CHECKPOINT_NOT_PROOF, STILL_ASSIGNED } from '../copy.ts'
import { usePoll, type Poll } from '../usePoll.ts'
import { localCredential } from '../localApi.ts'
import { onFrontDoor } from '../frontDoor.ts'
import { IntentReviewPanel } from '../IntentReviewPanel.tsx'
import { ClaimPill, lead, Source, StatePill, WorkLink } from './workParts.tsx'

function Ref({ r, item }: { r: WorkSourceRef; item: WorkItem }) {
  if (r.kind === 'work') return <WorkLink id={r.work_id} />
  if (r.kind === 'repository_path') return <Path>{`${r.path}${r.line === null ? '' : `:${r.line}`}`}</Path>
  if (r.kind === 'captured_event') {
    return <span title="Caller-declared metadata reference; any local event body is unavailable here and the Work view does not verify conversation content.">Declared captured event <code>{r.session_id} / {r.event_id}</code></span>
  }
  if (r.kind === 'prior_work_version') {
    return <span title="The exact Work version identity was checked when this checkpoint was recorded; its older snapshot may no longer be retained.">Supersedes version <code>{r.event_seq}</code> of <WorkLink id={r.work_id} /></span>
  }
  const href = safeCommitHref(item.source, r.oid)
  const text = <code>{r.oid.slice(0, 12)}{r.path ? ` · ${r.path}` : ''}</code>
  return href ? <a href={href} target="_blank" rel="noreferrer noopener">{text} <Icon name="box-arrow-up-right" /></a> : text
}

function Report({ c, item, title }: { c: WorkCheckpoint; item: WorkItem; title: ReactNode }) {
  const login = item.participants.find((p) => p.agent === c.agent)?.login
  return (
    <div className="report">
      <div className="report-head">
        <Pill tone="muted" icon="chat-square-text" tip={CHECKPOINT_NOT_PROOF}>
          Agent report
        </Pill>
        <strong>{title}</strong>
      </div>
      <p>{c.summary}</p>
      {c.source_refs.length > 0 && (
        <div className="refs">
          <Icon name="link-45deg" /> {c.source_refs.map((r, i) => <Ref key={i} r={r} item={item} />)}
        </div>
      )}
      <div className="small muted">
        Reported by {login ? who(login, c.agent) : c.agent} · <Ago at={c.at} />
      </div>
    </div>
  )
}

/** A poll as a lifecycle input: null while loading, 'failed' when the read failed. */
const input = <T, U>(p: Poll<T>, pick: (v: T) => U): U | null | 'failed' => (p.kind === 'ready' ? pick(p.value) : p.kind === 'error' ? 'failed' : null)

/** What the Dashboard's Needs-you list would show for this task, as next steps. Contested files and pull requests need live-agent data this page does not load. */
function needsFor(id: string, overview: WorkOverview | null): NextStep[] {
  const steps: NextStep[] = []
  for (const n of needsYou(overview, null)) {
    if (n.kind === 'overlap') {
      const o = overview?.overlaps.find((x) => `overlap:${x.left.id}:${x.right.id}` === n.key)
      if (o && (o.left.id === id || o.right.id === id)) steps.push(overlapSteps(o))
    } else if (n.item?.id === id) steps.push(n.kind === 'blocked' ? blockedStep(n.item) : n.kind === 'handover' ? handoverStep(n.item) : quietStep(n.item, null))
  }
  return steps
}

/** One task: where it is in its lifecycle, state, claim, people, declared scope, agent reports and technical details. */
export function WorkDetail({ id, token, overview, handoff }: { id: string; token: string; overview: Poll<WorkOverview>; handoff?: Handoff }) {
  const load = usePoll((signal) => fetchStandupWork(id, token, signal), 0, id)
  return (
    <div className="page">
      {load.kind === 'loading' && <Skeleton rows={6} />}
      {load.kind === 'error' && (
        <Notice tone="bad" title={load.status === 404 ? 'This work item was not found' : 'Could not load this work item'} action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>
          {load.message}
        </Notice>
      )}
      {load.kind === 'ready' && <Task item={load.value} token={token} overview={overview.kind === 'ready' ? overview.value : null} handoff={handoff} />}
    </div>
  )
}

function Task({ item, token, overview, handoff }: { item: WorkItem; token: string; overview: WorkOverview | null; handoff?: Handoff }) {
  const l = lead(item)
  const branch = item.branch
  const record = usePoll((s) => fetchChangeRecord(item.repository, item.id, token, s), 0, item.id)
  const requests = usePoll((s) => fetchCoordination(token, s, { limit: 50, activeOnly: true }), 0, item.id)
  const runs = usePoll((s) => fetchRuns(item.repository, token, s), 0, item.id)
  const pubs = usePoll((s) => (branch ? listPublications(item.repository, token, s, undefined, { q: branch, limit: 10 }) : Promise.resolve(null)), 0, item.id)
  const work = usePoll((s) => fetchWork(item.repository, token, s), 0, item.id)
  const steps = lifecycle({
    item,
    overview,
    requests: input(requests, (v) => v.requests),
    skillRuns: input(runs, (v) => v.runs),
    // A 404 means no record exists; any other failure is unknown, never "none".
    changeRecord: record.kind === 'error' && record.status === 404 ? 'none' : input(record, (v) => v.summary),
    publications: input(pubs, (v) => (v === null ? [] : v.supported ? v.publications : 'failed')),
    pulls: input(work, (v) => v.pull_requests),
  })
  // Branch, pull request and head are shown only where something recorded them; each comes from the read that names this branch.
  const pub = pubs.kind === 'ready' && pubs.value?.supported ? pubs.value.publications.find((x) => x.branch === branch) : undefined
  const open = work.kind === 'ready' ? work.value.pull_requests.find((x) => x.state === 'open' && x.head_ref === branch) : undefined
  const run = runs.kind === 'ready' ? runs.value.runs.find((x) => x.branch === branch) : undefined
  const prNumber = pub?.pr.number ?? open?.number ?? run?.pull_request ?? null
  const prUrl = pub?.pr.url ?? httpsOnly(open?.html_url)
  const base = pub?.base || open?.base_ref
  const head = pub?.commit || run?.head_sha
  const quiet = item.state !== 'finished' && item.claim_status !== 'active'
  const leads = item.participants.filter((p) => p.role === 'lead').length
  const needSteps = needsFor(item.id, overview)
  return (
    <div className="stack">
      <PageHead className="detail-head" question={item.title} answer={<>Led by {who(l.login, l.agent)}.</>} fact={
          <>
            {branch && <>On <code className="branch-name">{branch}</code>{base ? <> into <code>{base}</code></> : null}{prNumber !== null ? <>, {prUrl ? <a href={prUrl} target="_blank" rel="noreferrer noopener">PR #{prNumber}</a> : `PR #${prNumber}`}</> : null}{head ? <>, head <code>{head.slice(0, 7)}</code></> : null}. </>}
            Last check-in <Ago at={item.last_checkin_at} />.
          </>
        } />
      <div className="pills">
        <StatePill state={item.state} />
        <ClaimPill item={item} />
        {item.source.kind !== 'native' && <Source item={item} />}
        {record.kind === 'ready' && (
          <a className="btn btn-sm btn-outline-secondary" href={`#change-record/${encodeURIComponent(item.id)}`}>
            <Icon name="clipboard2-check" /> Change record
          </a>
        )}
      </div>
      <Box icon="signpost-split" title="Where this task is" explain="Each step shows what the hub has and how sure it is. Open a step for its evidence.">
        <LifecycleStrip steps={steps} />
      </Box>
      {needSteps.length > 0 && (
        <Box icon="exclamation-circle" title="Needs you" tone="warn">
          <div className="stack">
            {needSteps.map((n, i) => <NextStepView key={i} step={n} viewer={handoff?.me} repo={handoff?.repo} originUrl={handoff?.originUrl} />)}
          </div>
        </Box>
      )}
      {quiet && needSteps.length === 0 && <Notice title={item.claim_status === 'stale' ? 'No recent check-in' : 'Check-in unknown'}>{STILL_ASSIGNED}</Notice>}
      <div className="action-grid is-two">
        <div>
          <span className="label">
            <Icon name="sign-stop" /> Reported blocker
          </span>
          {item.blocker?.summary ?? <span className="muted">None reported</span>}
        </div>
        <div>
          <span className="label">
            <Icon name="arrow-right-circle" /> Next action
          </span>
          {item.next_action ?? <span className="muted">None reported</span>}
        </div>
      </div>
      <div className="grid-2">
        <Box icon="people" title="People" count={item.participants.length} explain="The lead owns the task. Helpers and reviewers joined it.">
          <ul className="plain-list">
            {item.participants.map((p) => (
              <li key={`${p.token}-${p.session}`} className="d-flex align-items-center gap-2">
                <Avatar name={p.agent} size={28} />
                <span>
                  <strong title={p.agent}>{who(p.login, p.agent)}</strong> <span className="muted">·</span>
                </span>
                <Pill tone={p.role === 'lead' ? 'brand' : 'muted'} icon={p.role === 'lead' ? 'star' : p.role === 'reviewer' ? 'eye' : 'person'}>
                  {p.role === 'lead' && leads > 1 && p.agent === l.agent && p.login === l.login ? 'lead (current)' : p.role}
                </Pill>
              </li>
            ))}
          </ul>
        </Box>
        <Box icon="bounding-box" title="Files this task plans to change" count={item.scope.length} explain="The paths this task says it will touch. The hub compares these lists across tasks to find overlaps.">
          {item.scope.length ? <Paths items={item.scope} max={40} /> : <Empty icon="question-circle" title="No scope declared">Overlap checks cannot cover this task.</Empty>}
        </Box>
      </div>
      <Box id="agent-reports" icon="chat-square-text" title="Agent reports" explain="The latest checkpoint and any open blocker, as written by the agent.">
        {!item.current && !item.blocker && <Empty icon="chat-square" title="No checkpoint yet" />}
        {item.current && (
          <Report
            c={item.current}
            item={item}
            title={item.current.kind === 'blocker' ? 'Latest report · blocker' : 'Latest report'}
          />
        )}
        {item.blocker && item.blocker !== item.current && <Report c={item.blocker} item={item} title="Open blocker" />}
      </Box>
      {onFrontDoor() && localCredential() && <IntentReviewPanel workId={item.id} />}
      <details className="disclosure">
        <summary>
          <Icon name="code-square" /> Technical details
        </summary>
        <dl className="tech">
          <dt>Work ID</dt>
          <dd><code>{item.id}</code></dd>
          <dt>Source</dt>
          <dd>{item.source.kind === 'native' ? <><Gloss term="plainGit">No issue linked</Gloss>. Plain Git work request <code>{item.source.request_key}</code></> : <Source item={item} />}</dd>
          <dt>Branch</dt>
          <dd>{item.branch ? <code>{item.branch}</code> : '—'}</dd>
          <dt>Worktree</dt>
          <dd>{item.worktree ? <code>{item.worktree}</code> : '—'}</dd>
          <dt>Generation</dt>
          <dd>{item.generation}</dd>
          <dt>Lead session</dt>
          <dd><code>{item.lead_session}</code></dd>
          <dt>Depends on</dt>
          <dd>{item.depends_on.length ? item.depends_on.map((d) => <WorkLink key={d} id={d} />) : '—'}</dd>
          <dt>Related</dt>
          <dd>{item.related_work.length ? item.related_work.map((d) => <WorkLink key={d} id={d} />) : '—'}</dd>
          <dt>Created</dt>
          <dd>{localDate(item.created_at)}</dd>
          <dt>Updated</dt>
          <dd>{localDate(item.updated_at)}</dd>
        </dl>
      </details>
    </div>
  )
}
