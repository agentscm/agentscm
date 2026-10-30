import { useEffect, useId, useRef, useState, type ReactNode } from 'react'
import { AskAgent } from '../AskAgent.tsx'
import { Combobox } from '../Combobox.tsx'
import { agentSetupPrompt } from '../connectAgent.ts'
import { fetchRepositoryList, type DeviceAuth, type RepositoryRow } from '../devices.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { SectionTabs, type WebChoice } from '../TopNav.tsx'
import { Ago, Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import type { Load } from './Computers.tsx'

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`
// A refresh can be one forge request per repository on the Hub, so this page polls slowly.
const EVERY_MS = 120_000
const RECENT_SECONDS = 7 * 86_400

/** Worth a row of its own: an agent connected, unfinished work, something that needs the person, or activity this past week. */
export const isActive = (r: RepositoryRow, now = Date.now() / 1000): boolean =>
  r.needsYou > 0 || r.agents > 0 || r.unfinished.total > 0 || (r.lastActivityAt !== null && now - r.lastActivityAt < RECENT_SECONDS)

/**
 * Every connected repository this web sign-in can read now, counts only. The active ones lead as rows, worst first;
 * the quiet rest fold into one line that opens to a filterable list. When no repository has an agent yet, the page
 * leads with connecting one instead. Opening a repository asks for its read token as the repository menu does, and
 * goes to its Dashboard once the token is in; a refusal stays on this page.
 */
export function RepositoriesPage({ auth, web, current }: { auth: Load<DeviceAuth | null>; web?: WebChoice; current: string | null }) {
  const session = auth.kind === 'ready' && auth.value?.kind === 'browser' ? auth.value.session : null
  // The list the web sign-in just fetched shows at once; the poll takes over once it is EVERY_MS old.
  const seed = session && web?.list ? { key: session.login, ...web.list } : null
  const list = usePoll((signal) => (session ? fetchRepositoryList(session, signal) : Promise.reject(new Error('Sign in on the web to list repositories'))), EVERY_MS, session?.login ?? '', 0, seed)
  const [opening, setOpening] = useState<string | null>(null)
  const refused = opening && web && !web.repos.includes(opening) ? web.refusal : null
  useEffect(() => {
    if (opening && current === opening) window.location.hash = '#home'
  }, [opening, current])
  const open = (repo: string) => {
    if (repo === current) window.location.hash = '#home'
    else if (web) {
      setOpening(repo)
      web.pick(repo)
    }
  }
  const v = list.kind === 'ready' ? list.value : null
  const retry = list.kind === 'loading' ? undefined : list.retry
  const active = v?.rows.filter((r) => isActive(r)) ?? []
  const quiet = v?.rows.filter((r) => !isActive(r)) ?? []
  const newHere = !!v && v.rows.length > 0 && !v.rows.some((r) => r.agents > 0)
  const needing = active.filter((r) => r.needsYou > 0).length
  const rowProps = { current, onOpen: web ? open : undefined, opening: refused ? null : opening }
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Which of my repositories need me?</h1>
        <p className="answer-text" aria-live="polite">
          {!v ? 'Every repository you can read on this Hub, with what needs you first.' : v.rows.length === 0 ? 'No connected repository you can read yet.' : newHere ? 'No agent is working in any of these yet.' : (
            <>
              <b>{plural(active.length, 'repository', 'repositories')}</b> {active.length === 1 ? 'has' : 'have'} activity. {needing > 0 ? <><b className="answer-trouble">{needing}</b> {needing === 1 ? 'needs' : 'need'} you.</> : 'Nothing needs you.'}
            </>
          )}
        </p>
        {v && v.rows.length > 0 && (
          <p className="answer-fact">{newHere ? 'Connect one and it shows up here, with what needs you first.' : quiet.length > 0 ? `The other ${quiet.length} ${quiet.length === 1 ? 'is' : 'are'} quiet.` : 'Counts only, checked against your role on the forge.'}</p>
        )}
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
        <div className="stack">
          {v.unchecked > 0 && (
            <Notice tone="warn" title={`${plural(v.unchecked, 'repository', 'repositories')} could not be checked`} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={retry}>Check again</button>}>
              The forge didn’t answer whether you can read {v.unchecked === 1 ? 'it' : 'them'}, so {v.unchecked === 1 ? 'it isn’t' : 'they aren’t'} listed. This is not a change to your access.
            </Notice>
          )}
          {v.rows.length === 0 && <Empty icon="plug" title="No connected repository you can read">Ask a repository owner for access, or connect an agent to a repository first.</Empty>}
          {newHere && <StartHere rows={v.rows} current={current} />}
          {active.length > 0 && <ActiveRows rows={active} {...rowProps} />}
          {quiet.length > 0 && <QuietDrawer rows={quiet} hint={newHere ? 'No agents or unfinished work yet' : 'Nothing needs you'} {...rowProps} />}
          {!v.complete && <p className="muted small">Only the first 200 connected repositories are checked.</p>}
        </div>
      ) : null}
    </div>
  )
}

type RowProps = { current: string | null; onOpen?: (repo: string) => void; opening: string | null }

const RepoName = ({ repo, provider }: { repo: string; provider?: string }) => {
  const [owner, ...rest] = repo.split('/')
  return (
    <>
      {provider && <Icon name={provider === 'gitlab' ? 'gitlab' : 'github'} className="repo-row-forge" />}
      <span className="repo-row-title"><span className="repo-row-owner">{owner}/</span><b>{rest.join('/')}</b></span>
    </>
  )
}

/** One ruled row per active repository: the name, coral only when something needs the person, then the counts. */
function ActiveRows({ rows, current, onOpen, opening }: { rows: RepositoryRow[] } & RowProps) {
  return (
    <ul className="repo-list" aria-label="Repositories with activity, what needs you first">
      {rows.map((r) => {
        const body = (
          <>
            <span className="repo-row-name">
              <RepoName repo={r.repository} provider={r.provider} />
              {r.repository === current && <Pill tone="brand">Open now</Pill>}
            </span>
            <span className="repo-row-status"><Status row={r} /></span>
            <span className="repo-row-meta">{r.repository === opening ? 'Opening its Dashboard…' : <Facts row={r} />}</span>
            {onOpen && <Icon name="chevron-right" className="repo-row-go" />}
          </>
        )
        return (
          <li key={r.repository}>
            {onOpen ? (
              <button type="button" className="repo-row" aria-busy={r.repository === opening || undefined} aria-label={`Open ${r.repository}`} aria-describedby={`repo-${r.repository}`} onClick={() => onOpen(r.repository)}>
                {body}
              </button>
            ) : (
              <div className="repo-row">{body}</div>
            )}
            <span id={`repo-${r.repository}`} className="visually-hidden">{summary(r)}</span>
          </li>
        )
      })}
    </ul>
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
        <b>{plural(rows.length, 'quiet repository', 'quiet repositories')}</b>
        <Icon name="chevron-right" className="repo-quiet-chev" />
        <span className="repo-quiet-hint">{hint}</span>
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

const atLeast = (r: RepositoryRow, n: number) => (r.complete || n === 0 ? String(n) : `at least ${n}`)

/** Coral only when something needs the person; amber for stuck work; otherwise nothing to flag. */
function Status({ row: r }: { row: RepositoryRow }) {
  if (r.needsYou > 0) return <Pill tone="bad">{atLeast(r, r.needsYou)} need{r.needsYou === 1 ? 's' : ''} you</Pill>
  if (r.unfinished.stuck > 0) return <Pill tone="warn">{atLeast(r, r.unfinished.stuck)} stuck</Pill>
  return null
}

/** Only the facts that are there: agents, unfinished work, the last activity. */
function Facts({ row: r }: { row: RepositoryRow }) {
  const parts: ReactNode[] = []
  if (r.agents > 0) parts.push(`${atLeast(r, r.agents)} agent${r.agents === 1 ? '' : 's'} connected`)
  else if (r.unfinished.total > 0) parts.push('no agents connected')
  if (r.unfinished.total > 0) parts.push(`${atLeast(r, r.unfinished.total)} unfinished · ${r.unfinished.moving} moving${r.unfinished.stuck ? ` · ${r.unfinished.stuck} stuck` : ''}`)
  if (r.lastActivityAt) parts.push(<>active <Ago at={r.lastActivityAt} /></>)
  return <>{parts.map((p, i) => <span key={i}>{i > 0 && ' · '}{p}</span>)}</>
}

const summary = (r: RepositoryRow) =>
  `${r.needsYou ? `${atLeast(r, r.needsYou)} need you` : 'Nothing needs you'}; ${atLeast(r, r.unfinished.total)} unfinished, ${r.unfinished.stuck} stuck; ${plural(r.agents, 'agent')} connected.`
