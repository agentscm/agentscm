import { Fragment, useEffect, useId, useRef, useState, type ReactNode } from 'react'
import { AskAgent } from '../AskAgent.tsx'
import { Combobox } from '../Combobox.tsx'
import { agentSetupPrompt } from '../connectAgent.ts'
import { fetchRepositoryList, type DeviceAuth, type RepositoryRow } from '../devices.ts'
import { FINISHED_MEANS_AGENT_SAID } from '../copy.ts'
import { fetchNarrative } from '../narrative.ts'
import { agentName, who } from '../nextStep.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { workHref, type WorkItem, type WorkOverview } from '../standup.ts'
import { SectionTabs, type WebChoice } from '../TopNav.tsx'
import { Ago, Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import type { Load } from './Computers.tsx'
import { workTabHref } from './Work.tsx'

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`
// A refresh can be one forge request per repository on the Hub, so this page polls slowly.
const EVERY_MS = 120_000
const RECENT_SECONDS = 7 * 86_400

/** Worth a row of its own: an agent connected, unfinished work, something that needs the person, or activity this past week. */
export const isActive = (r: RepositoryRow, now = Date.now() / 1000): boolean =>
  r.needsYou > 0 || r.agents > 0 || r.unfinished.total > 0 || (r.lastActivityAt !== null && now - r.lastActivityAt < RECENT_SECONDS)

const goHome = () => { window.location.hash = '#home' }
const nameOf = (repo: string) => repo.slice(repo.indexOf('/') + 1)
/** Short names for a headline, full `owner/name` only where two repositories would read the same. */
const labelsOf = (repos: string[]) => repos.map((r) => (repos.filter((o) => nameOf(o) === nameOf(r)).length > 1 ? r : nameOf(r)))
const joinNodes = (items: ReactNode[]): ReactNode[] =>
  items.flatMap((x, i) => [i === 0 ? '' : i === items.length - 1 ? ' and ' : ', ', <Fragment key={i}>{x}</Fragment>])

/**
 * Every connected repository this web sign-in can read now, counts only. The one with the most waiting on the person
 * leads as a card, the active ones follow as grouped rows, and the quiet rest fold into one line that opens to a
 * filterable list. When no repository has an agent yet, the page leads with connecting one instead. The right column
 * names who is working in the open repository, from the work overview already polled for it. Opening a repository asks
 * for its read token as the repository menu does, and goes to its Dashboard once the token is in; a refusal stays here.
 */
export function RepositoriesPage({ auth, web, current, overview = null, token = '' }: { auth: Load<DeviceAuth | null>; web?: WebChoice; current: string | null; overview?: WorkOverview | null; token?: string }) {
  const session = auth.kind === 'ready' && auth.value?.kind === 'browser' ? auth.value.session : null
  // The list the web sign-in just fetched shows at once; the poll takes over once it is EVERY_MS old.
  const seed = session && web?.list ? { key: session.login, ...web.list } : null
  // The overview behind Working now polls every few seconds; when its working set changes, refresh the slow list at
  // once so the headline never contradicts that card for up to EVERY_MS.
  const liveSet = `${current}:${workingNow(overview).map((i) => i.id).sort().join(',')}`
  const [seen, setSeen] = useState(liveSet)
  const [bump, setBump] = useState(0)
  if (seen !== liveSet) {
    setSeen(liveSet)
    setBump(bump + 1)
  }
  const list = usePoll((signal) => (session ? fetchRepositoryList(session, signal) : Promise.reject(new Error('Sign in on the web to list repositories'))), EVERY_MS, session?.login ?? '', bump, seed)
  const [opening, setOpening] = useState<string | null>(null)
  const refused = opening && web && !web.repos.includes(opening) ? web.refusal : null
  useEffect(() => {
    if (opening && current === opening) goHome()
  }, [opening, current])
  const open = (repo: string) => {
    if (repo === current) goHome()
    else if (web) {
      setOpening(repo)
      web.pick(repo)
    }
  }
  const v = list.kind === 'ready' ? list.value : null
  const retry = list.kind === 'loading' ? undefined : list.retry
  // Most waiting on the person first, then the most recent activity.
  const byNeed = (a: RepositoryRow, b: RepositoryRow) => b.needsYou - a.needsYou || (b.lastActivityAt ?? 0) - (a.lastActivityAt ?? 0)
  const active = v?.rows.filter((r) => isActive(r)).sort(byNeed) ?? []
  const quiet = v?.rows.filter((r) => !isActive(r)) ?? []
  const newHere = !!v && v.rows.length > 0 && !v.rows.some((r) => r.agents > 0)
  const needing = active.filter((r) => r.needsYou > 0)
  const focus = !newHere ? needing[0] : undefined
  const stuck = active.filter((r) => r.needsYou === 0 && r.unfinished.stuck > 0)
  const rest = active.filter((r) => r.needsYou === 0 && r.unfinished.stuck === 0)
  const working = active.filter((r) => r.agents > 0)
  const rowProps = { current, onOpen: web ? open : undefined, opening: refused ? null : opening }
  const link = (repo: string, label: string) => (rowProps.onOpen ? <button type="button" className="rp-link" onClick={() => open(repo)}>{label}</button> : <span className="rp-link">{label}</span>)
  const needLabels = labelsOf(needing.map((r) => r.repository))
  const headline = !v ? 'Every repository you can read on this Hub, with what needs you first.'
    : v.rows.length === 0 ? 'No connected repository you can read yet.'
    : newHere ? 'No agent is working in any of these yet.'
    : needing.length === 0 ? 'Nothing needs you.'
    : needing.length > 3 ? <><b className="answer-trouble">{needing.length}</b> repositories need you.</>
    : <>{joinNodes(needing.map((r, i) => link(r.repository, needLabels[i]!)))} {needing.length === 1 ? 'needs' : 'need'} you.</>
  const workingLabels = labelsOf(working.slice(0, 3).map((r) => r.repository))
  const sub = !v || v.rows.length === 0 ? null
    : newHere ? 'Connect one and it shows up here, with what needs you first.'
    : needing.length === 0 ? <>Agents are working in {joinNodes(workingLabels.map((l, i) => link(working[i]!.repository, l)))}{working.length > 3 ? ` and ${working.length - 3} more` : ''}.</>
    : `Start with ${nameOf(focus!.repository)}: it has the most waiting on you.`
  const showRail = !!v && v.rows.length > 0
  return (
    <div className="page">
      <div className={`rp${showRail ? '' : ' rp-solo'}`}>
        <div className="rp-main">
          <div className="answer" data-tour="answer">
            <h1 className="page-question">Which of my repositories need me?</h1>
            <p className="answer-text" aria-live="polite">{headline}</p>
            {sub && <p className="answer-fact">{sub}</p>}
            {v && v.rows.length > 0 && !newHere && <p className="rp-checked"><i className="rp-dot ok" aria-hidden="true" />Counts only, checked against your role on the forge.</p>}
          </div>
          <SectionTabs />
          {refused && <RefusalNotice refusal={refused} />}
          {auth.kind === 'loading' || (session && list.kind === 'loading') ? (
            <Skeleton rows={4} />
          ) : !session ? (
            <Notice tone="muted" icon="person-lock" title="Sign in on the web to see every repository">This page lists the repositories your Hub web sign-in can read. A pasted read-only token, or this computer’s own page, opens one repository.</Notice>
          ) : list.kind === 'error' ? (
            <Notice tone="bad" title="The repository list didn’t load" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={retry}>Try again</button>}>{list.status === 401 ? 'Your Hub sign-in has ended. Sign in again to see your repositories.' : "The Hub didn’t answer. Your access hasn’t changed; try again in a moment."}</Notice>
          ) : v ? (
            <div className="rp-body">
              {v.unchecked > 0 && (
                <Notice tone="warn" title={`${plural(v.unchecked, 'repository', 'repositories')} could not be checked`} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={retry}>Check again</button>}>
                  The forge didn’t answer whether you can read {v.unchecked === 1 ? 'it' : 'them'}, so {v.unchecked === 1 ? 'it isn’t' : 'they aren’t'} listed. This is not a change to your access.
                </Notice>
              )}
              {v.rows.length === 0 && <Empty icon="plug" title="No connected repository you can read">Ask a repository owner for access, or connect an agent to a repository first.</Empty>}
              {newHere && <StartHere rows={v.rows} current={current} />}
              {focus && <Focus row={focus} stacked={needing.length > 1} {...rowProps} />}
              {newHere ? (
                <Group title="Not connected yet" icon="journal-code" tone="" rows={active} {...rowProps} />
              ) : (
                <>
                  <Group title="Also needs you" icon="exclamation-circle" tone="bad" rows={needing.slice(1)} {...rowProps} />
                  <Group title="Stuck" icon="pause-circle" tone="warn" rows={stuck} {...rowProps} />
                  <Group title={needing.length === 0 ? 'Moving on their own' : 'Moving'} icon="arrow-right-circle" tone="brand" rows={rest} {...rowProps} />
                </>
              )}
              {quiet.length > 0 && <QuietDrawer rows={quiet} hint={newHere ? 'No agents or unfinished work yet' : 'Nothing in progress'} {...rowProps} />}
              {!v.complete && <p className="muted small">Only the first 200 connected repositories are checked.</p>}
              {current && overview && v.rows.length > 0 && !newHere && <Done repo={current} overview={overview} token={token} />}
            </div>
          ) : null}
        </div>
        {showRail && <Rail current={current} overview={overview} rows={v!.rows} />}
      </div>
    </div>
  )
}

type RowProps = { current: string | null; onOpen?: (repo: string) => void; opening: string | null }

/** A rounded icon tile; `tone` colours it (bad, warn, brand, ok), `size` makes it small or large. */
const Chip = ({ icon, tone = '', size = '' }: { icon: string; tone?: string; size?: 'sm' | 'lg' | '' }) => (
  <span className={`rp-chip${tone ? ` is-${tone}` : ''}${size ? ` is-${size}` : ''}`}><Icon name={icon} /></span>
)

const RepoName = ({ repo, provider }: { repo: string; provider?: string }) => {
  const [owner, ...rest] = repo.split('/')
  return (
    <>
      {provider && <Icon name={provider === 'gitlab' ? 'gitlab' : 'github'} className="repo-row-forge" />}
      <span className="repo-row-title"><span className="repo-row-owner">{owner}/</span><b>{rest.join('/')}</b></span>
    </>
  )
}

const atLeast = (r: RepositoryRow, n: number) => (r.complete || n === 0 ? String(n) : `at least ${n}`)

/** One cursor per connected agent, four at most. Decorative: the row's summary says the count. */
const Agents = ({ n, label }: { n: number; label?: string }) => (
  <span className="rp-agents" title={n > 0 ? label : undefined} aria-hidden="true">
    {Array.from({ length: Math.min(n, 4) }, (_, i) => <Icon key={i} name="cursor-fill" />)}
    {n > 4 && <span className="rp-more">+{n - 4}</span>}
  </span>
)

/** The status line under a name: only what is there. Coral for waiting on the person, amber for stuck. */
function Say({ row: r }: { row: RepositoryRow }) {
  const bits: ReactNode[] = []
  if (r.needsYou > 0) bits.push(<span className="rp-bad">{r.needsYou} waiting on you</span>)
  if (r.unfinished.stuck > 0) bits.push(<span className="rp-warn">{r.unfinished.stuck} stuck</span>)
  if (r.unfinished.moving > 0) bits.push(`${r.unfinished.moving} moving`)
  if (bits.length === 0 && r.unfinished.total > 0) bits.push(`${r.unfinished.total} unfinished`)
  if (bits.length === 0) return <>{r.agents > 0 ? 'Connected, nothing in progress' : 'No agent connected'}</>
  return <>{!r.complete && 'at least '}{bits.map((b, i) => <Fragment key={i}>{i > 0 && ', '}{b}</Fragment>)}</>
}

const lastActive = (r: RepositoryRow) => (r.lastActivityAt ? <Ago at={r.lastActivityAt} /> : 'no activity yet')

/** The repository with the most waiting on the person, as the one thing to open next. */
function Focus({ row: r, stacked, current, onOpen, opening }: { row: RepositoryRow; stacked: boolean } & RowProps) {
  const [owner, ...rest] = r.repository.split('/')
  const busy = r.repository === opening
  return (
    <div className={`rp-stack${stacked ? '' : ' is-one'}`}>
      <section className="rp-focus" aria-label={`Needs you most: ${r.repository}`}>
        <span className="rp-ring" aria-hidden="true" />
        <div className="rp-focus-l">
          <span className="rp-focus-name"><span>{owner}/</span><b>{rest.join('/')}</b>{r.repository === current && ' · open now'}</span>
          <h2><em>{atLeast(r, r.needsYou)} {r.needsYou === 1 ? 'thing is' : 'things are'}</em> waiting on you.</h2>
          <div className="rp-facts">
            <span><i className="rp-dot warn" aria-hidden="true" />{atLeast(r, r.unfinished.stuck)} stuck</span>
            <span><i className="rp-dot brand" aria-hidden="true" />{atLeast(r, r.unfinished.moving)} moving</span>
            <span>{atLeast(r, r.agents)} {r.agents === 1 ? 'agent' : 'agents'} connected</span>
            {r.lastActivityAt && <span>active {lastActive(r)}</span>}
          </div>
        </div>
        {onOpen && (
          <button type="button" className="rp-open" aria-busy={busy || undefined} aria-label={`Open what needs you in ${r.repository}`} onClick={() => onOpen(r.repository)}>
            {busy ? 'Opening its Dashboard…' : 'Open what needs you'}
          </button>
        )}
      </section>
    </div>
  )
}

/** A titled group of rows on one shared grid; nothing when it is empty. */
function Group({ title, icon, tone, rows, ...rp }: { title: string; icon: string; tone: string; rows: RepositoryRow[] } & RowProps) {
  const id = useId()
  if (rows.length === 0) return null
  return (
    <section className="rp-group" aria-labelledby={id}>
      <div className="rp-ghead"><Chip icon={icon} tone={tone} size="sm" /><h2 id={id}>{title}</h2><span className="rp-n">{rows.length}</span></div>
      <ul className="rp-rows" aria-labelledby={id}>
        {rows.map((r) => <RepoRow key={r.repository} row={r} {...rp} />)}
      </ul>
    </section>
  )
}

/** One repository: icon, name with its status line, agents, last activity. A button when the page can open it. */
function RepoRow({ row: r, current, onOpen, opening }: { row: RepositoryRow } & RowProps) {
  const body = (
    <>
      <Chip icon="journal-code" />
      <span className="rp-rmain">
        <span className="rp-rname">
          <RepoName repo={r.repository} />
          {r.repository === current && <Pill tone="brand">Open now</Pill>}
        </span>
        <span className="rp-say">{r.repository === opening ? 'Opening its Dashboard…' : <Say row={r} />}</span>
      </span>
      <Agents n={r.agents} label={`${atLeast(r, r.agents)} ${r.agents === 1 ? 'agent' : 'agents'} connected`} />
      <span className="rp-time">{lastActive(r)}</span>
      {onOpen && <Icon name="chevron-right" className="rp-go" />}
    </>
  )
  return (
    <li>
      {onOpen ? (
        <button type="button" className="rp-row" aria-busy={r.repository === opening || undefined} aria-label={`Open ${r.repository}`} aria-describedby={`repo-${r.repository}`} onClick={() => onOpen(r.repository)}>
          {body}
        </button>
      ) : (
        <div className="rp-row" aria-describedby={`repo-${r.repository}`}>{body}</div>
      )}
      <span id={`repo-${r.repository}`} className="visually-hidden">{summary(r)}</span>
    </li>
  )
}

/** The quiet repositories as one line; opened, a compact name list with a filter. */
function QuietDrawer({ rows, hint, current, onOpen, opening }: { rows: RepositoryRow[]; hint: string } & RowProps) {
  const [open, setOpen] = useState(false)
  const [filter, setFilter] = useState('')
  const drawer = useRef<HTMLDivElement>(null)
  const id = useId()
  // React 18 has no `inert` prop: closed, the list is out of the tab order and the accessibility tree.
  useEffect(() => {
    drawer.current?.toggleAttribute('inert', !open)
  }, [open])
  const q = filter.trim().toLowerCase()
  const shown = rows.filter((r) => r.repository.toLowerCase().includes(q))
  return (
    <section className="repo-quiet">
      <button type="button" className="repo-quiet-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen((o) => !o)}>
        <Chip icon="moon" size="sm" />
        <b>{plural(rows.length, 'quiet repository', 'quiet repositories')}</b>
        <span className="repo-quiet-hint">{hint}</span>
        <Icon name="chevron-right" className="repo-quiet-chev" />
      </button>
      <div id={id} ref={drawer} className={`repo-drawer${open ? ' is-open' : ''}`}>
        <div className="repo-drawer-in">
          <input type="search" className="form-control repo-quiet-filter" placeholder={`Filter ${rows.length} repositories`} aria-label="Filter quiet repositories" value={filter} onChange={(e) => setFilter(e.target.value)} />
          <ul className="repo-quiet-list" aria-label="Quiet repositories">
            {shown.length === 0 ? (
              <li className="repo-quiet-none">No quiet repository matches “{filter.trim()}”.</li>
            ) : shown.map((r) => {
              const name = (
                <>
                  <RepoName repo={r.repository} provider={r.provider} />
                  {r.repository === current && <Pill tone="brand">Open now</Pill>}
                  {r.repository === opening && <span className="repo-quiet-opening">Opening…</span>}
                </>
              )
              return (
                <li key={r.repository}>
                  {onOpen ? (
                    <button type="button" className="repo-quiet-item" aria-busy={r.repository === opening || undefined} aria-label={`Open ${r.repository}`} onClick={() => onOpen(r.repository)}>{name}</button>
                  ) : (
                    <span className="repo-quiet-item">{name}</span>
                  )}
                </li>
              )
            })}
          </ul>
        </div>
      </div>
    </section>
  )
}

/** No agent anywhere yet: pick a repository and open your own agent with the setup request for it. */
function StartHere({ rows, current }: { rows: RepositoryRow[]; current: string | null }) {
  const names = rows.map((r) => r.repository)
  const [pick, setPick] = useState(() => (current && names.includes(current) ? current : names[0]!))
  const repo = names.includes(pick) ? pick : names[0]!
  const options = rows.map((r) => ({ value: r.repository, text: r.repository, label: <RepoName repo={r.repository} /> }))
  return (
    <section className="repo-start" aria-label="Connect an agent">
      <div className="repo-start-field">
        <p className="repo-start-label" aria-hidden="true">Repository</p>
        <Combobox id="repo-start-pick" label="Repository" placeholder={`Search ${plural(rows.length, 'repository', 'repositories')}`} options={options} onSelect={setPick} keepText />
      </div>
      <AskAgent title={<>Open it in your agent for <b>{repo}</b></>} prompt={agentSetupPrompt(window.location.origin, repo)} repo={repo} />
    </section>
  )
}

const DONE_SHOWN = 6
/** "day", "36 hours", "7 days": the overview's window, said plainly after "the last". */
function windowWords(seconds: number): string {
  const hours = Math.round(seconds / 3600)
  return hours === 24 ? 'day' : hours < 48 ? plural(hours, 'hour') : plural(Math.round(hours / 24), 'day')
}

/** The open repository's agent briefing headline, only when one is ready; nothing otherwise. */
function DoneBriefing({ token }: { token: string }) {
  const brief = usePoll((signal) => fetchNarrative(token, signal, { subscriber: 'human' }), EVERY_MS, token)
  const view = brief.kind === 'ready' ? brief.value : null
  if (!view || view.status !== 'ready' || !view.output) return null
  return (
    <div className="rp-brief">
      <p className="rp-brief-h">{view.output.headline}</p>
      <p className="rp-brief-by">
        Written by {view.generated_by ? agentName(view.generated_by) : 'a connected agent'}{view.generated_at !== null && <> · <Ago at={view.generated_at} /></>}
        {' · '}<a href="#activity">Read the full briefing</a>
      </p>
    </div>
  )
}

/** What the open repository's agents reported finished or decided in the overview's window, newest first. */
function Done({ repo, overview, token }: { repo: string; overview: WorkOverview; token: string }) {
  const head = useId()
  const name = nameOf(repo)
  const list = [...overview.recent].sort((a, b) => b.at - a.at || b.seq - a.seq)
  const span = windowWords(overview.until - overview.recent_since)
  return (
    <section className="rp-sec" aria-labelledby={head}>
      <div className="rp-sec-head">
        <Chip icon="flag" tone="ok" size="lg" />
        <div className="rp-sec-t">
          <h2 id={head}>Done in {name} lately</h2>
          <p>Reported in the last {span}, newest first. {FINISHED_MEANS_AGENT_SAID}</p>
        </div>
      </div>
      {token && <DoneBriefing token={token} />}
      {list.length === 0 ? (
        <p className="rp-empty">Nothing was reported finished in {name} in the last {span}.</p>
      ) : (
        <ul className="rp-done" aria-labelledby={head}>
          {list.slice(0, DONE_SHOWN).map((o) => (
            <li key={o.seq}>
              <a className="rp-done-t" href={workHref(o.item.id)}>{o.item.title}</a>
              <span className="rp-done-at"><Ago at={o.at} /></span>
              <span className="rp-done-who">{o.kind === 'completion' ? 'Finished' : 'Decision'} · {who(o.item.owner_login, o.item.lead_agent)}</span>
              {o.item.current && <span className="rp-done-say">{o.item.current.summary}</span>}
            </li>
          ))}
        </ul>
      )}
      {!overview.recent_complete && (
        <p className="rp-note">This list is partial. {overview.history_retention_gap ? 'Some outcomes in this window were compacted from retained history.' : 'The overview limit was reached; more completions exist.'}</p>
      )}
      <p className="rp-more-link"><a href={workTabHref('finished')}>See all finished work in {name} <Icon name="chevron-right" /></a></p>
    </section>
  )
}

const STATE_ORDER: Record<string, number> = { blocked: 0, handover: 1, paused: 2, active: 3 }
/** Work that is live: not finished, and its lead still holds the claim or the claim is unknown. */
export const workingNow = (ov: WorkOverview | null): WorkItem[] =>
  (ov?.current ?? [])
    .filter((i) => i.state !== 'finished' && i.claim_status !== 'stale')
    .sort((a, b) => (STATE_ORDER[a.state] ?? 9) - (STATE_ORDER[b.state] ?? 9) || b.last_checkin_at - a.last_checkin_at)

const WORD: Record<string, { text: string; tone: 'bad' | 'warn' }> = { blocked: { text: 'blocked', tone: 'bad' }, handover: { text: 'handover', tone: 'warn' }, paused: { text: 'paused', tone: 'warn' } }
const SHOWN = 6

/** Who is working in the open repository, then the other repositories with an agent connected, then a way to its Catch up page. */
function Rail({ current, overview, rows }: { current: string | null; overview: WorkOverview | null; rows: RepositoryRow[] }) {
  const head = useId()
  const items = current ? workingNow(overview) : []
  const elsewhere = rows.filter((r) => r.agents > 0 && r.repository !== current)
  return (
    <aside className="rp-rail" aria-label="Working now">
      <div className="rp-card">
        <div className="rp-chead">
          <Chip icon="activity" tone={items.length > 0 ? 'ok' : ''} />
          <h2 id={head}>Working now in {current ? nameOf(current) : 'this repository'}</h2>
          {items.length > 0 && <span className="rp-live" aria-hidden="true" />}
        </div>
        {items.length > 0 ? (
          <>
            <ul className="rp-people" aria-labelledby={head}>
              {items.slice(0, SHOWN).map((i) => {
                const w = WORD[i.state]
                return (
                  <li key={i.id} className={`rp-person${w ? ` is-${w.tone}` : ''}`}>
                    <span className="rp-av" aria-hidden="true" title={`${i.owner_login} with ${i.lead_agent}`}>{(i.owner_login[0] ?? '?').toUpperCase()}</span>
                    <b>{i.owner_login}</b>
                    <span className="rp-pt">{w ? w.text : <Ago at={i.last_checkin_at} />}</span>
                    <small>{i.title}</small>
                  </li>
                )
              })}
            </ul>
            {items.length > SHOWN && <p className="rp-note">and {items.length - SHOWN} more</p>}
          </>
        ) : (
          <p className="rp-empty">Nobody yet. People and their agents appear here the moment they connect.</p>
        )}
        {elsewhere.length > 0 && (
          <>
            <ul className="rp-else" aria-label="Other repositories with an agent connected">
              {elsewhere.map((r) => (
                <li key={r.repository}>
                  <Icon name="journal-code" />
                  <span className="rp-else-name">{r.repository}</span>
                  <Agents n={r.agents} label={`${atLeast(r, r.agents)} ${r.agents === 1 ? 'agent' : 'agents'} connected`} />
                  <span className="visually-hidden">{atLeast(r, r.agents)} {r.agents === 1 ? 'agent' : 'agents'} connected</span>
                </li>
              ))}
            </ul>
            <p className="rp-note">Names show for the repository you have open. Open another to see who is in it.</p>
          </>
        )}
      </div>
      {current && (
        <a className="rp-card rp-teaser" href="#activity">
          <span className="rp-k"><Icon name="clock-history" />Catch up</span>
          <span className="rp-h">Catch up on {nameOf(current)}</span>
          <span className="rp-tgo">What happened since you last looked <Icon name="chevron-right" /></span>
        </a>
      )}
    </aside>
  )
}

const summary = (r: RepositoryRow) =>
  `${r.needsYou ? `${atLeast(r, r.needsYou)} need you` : 'Nothing needs you'}; ${atLeast(r, r.unfinished.total)} unfinished, ${r.unfinished.stuck} stuck; ${plural(r.agents, 'agent')} connected.`
