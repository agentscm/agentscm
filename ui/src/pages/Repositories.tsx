import { useEffect, useState } from 'react'
import { fetchRepositoryList, type DeviceAuth, type RepositoryRow } from '../devices.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { SectionTabs, type WebChoice } from '../TopNav.tsx'
import { Ago, Box, Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import { ValueMetricsSection } from '../ValueMetrics.tsx'
import type { Load } from './Computers.tsx'

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`
// A refresh can be one forge request per repository on the Hub, so this page polls slowly.
const EVERY_MS = 120_000

/**
 * Every connected repository this web sign-in can read now, worst first, counts only. Opening a row
 * asks for that repository's read token as the repository menu does, and goes to its Dashboard
 * once the token is in; a refusal stays on this page.
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
  const needing = v?.rows.filter((r) => r.needsYou > 0).length ?? 0
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Which of my repositories need me?</h1>
        <p className="answer-text" aria-live="polite">
          {!v ? 'Every repository you can read on this Hub, with what needs you first.' : v.rows.length === 0 ? 'No connected repository you can read yet.' : (
            <>
              <b>{plural(v.rows.length, 'repository', 'repositories')}</b> you can read. {needing > 0 ? <><b className="answer-trouble">{needing}</b> {needing === 1 ? 'needs' : 'need'} you.</> : 'Nothing needs you.'}
            </>
          )}
        </p>
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
          <Box icon="collection" tone={needing > 0 ? 'bad' : 'brand'} title="All repositories" count={v.rows.length} explain="Counts only, checked against your role on the forge when this page refreshes (every two minutes; the Hub reuses an answer for up to 90 seconds). Needs you counts blockers, quiet leads, handovers, overlapping declared work and files two agents hold; pull-request overlaps show on each Dashboard.">
            {v.rows.length === 0 ? <Empty icon="plug" title="No connected repository you can read">Ask a repository owner for access, or connect an agent to a repository first.</Empty> : <Rows rows={v.rows} current={current} onOpen={web ? open : undefined} opening={refused ? null : opening} />}
          </Box>
          <ValueMetricsSection session={session} repositories={v.rows} onOpen={web ? open : undefined} />
          {!v.complete && <p className="muted small">Only the first 200 connected repositories are checked.</p>}
        </div>
      ) : null}
    </div>
  )
}

/** One ruled row per repository: the name, one status in words, then the counts behind it. */
function Rows({ rows, current, onOpen, opening }: { rows: RepositoryRow[]; current: string | null; onOpen?: (repo: string) => void; opening: string | null }) {
  return (
    <ul className="repo-list" aria-label="Repositories you can read, what needs you first">
      {rows.map((r) => {
        const [owner, ...rest] = r.repository.split('/')
        const body = (
          <>
            <span className="repo-row-name">
              <Icon name={r.provider === 'gitlab' ? 'gitlab' : 'github'} className="repo-row-forge" />
              <span className="repo-row-title"><span className="repo-row-owner">{owner}/</span><b>{rest.join('/')}</b></span>
              {r.repository === current && <Pill tone="brand">Open now</Pill>}
            </span>
            <span className="repo-row-status"><Status row={r} /></span>
            <span className="repo-row-meta">{r.repository === opening ? 'Opening its Dashboard…' : <Facts row={r} />}</span>
            {onOpen && <Icon name="chevron-right" className="repo-row-go" />}
          </>
        )
        const quiet = r.needsYou === 0 && r.unfinished.total === 0 && r.agents === 0
        return (
          <li key={r.repository} className={quiet ? 'is-quiet' : undefined}>
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

const atLeast = (r: RepositoryRow, n: number) => (r.complete || n === 0 ? String(n) : `at least ${n}`)

/** The one thing to know first: what needs you, else stuck work, else all clear. */
function Status({ row: r }: { row: RepositoryRow }) {
  if (r.needsYou > 0) return <Pill tone="bad">{atLeast(r, r.needsYou)} need{r.needsYou === 1 ? 's' : ''} you</Pill>
  if (r.unfinished.stuck > 0) return <Pill tone="warn">{atLeast(r, r.unfinished.stuck)} stuck</Pill>
  return <Pill tone="ok">Nothing needs you</Pill>
}

function Facts({ row: r }: { row: RepositoryRow }) {
  const work = r.unfinished.total === 0 ? 'No unfinished work' : `${atLeast(r, r.unfinished.total)} unfinished · ${r.unfinished.moving} moving${r.unfinished.stuck ? ` · ${r.unfinished.stuck} stuck` : ''}`
  const agents = r.agents === 0 ? 'no agents connected' : `${atLeast(r, r.agents)} agent${r.agents === 1 ? '' : 's'} connected`
  return (
    <>
      {work} · {agents} · {r.lastActivityAt ? <>active <Ago at={r.lastActivityAt} /></> : 'no activity yet'}
    </>
  )
}

const summary = (r: RepositoryRow) =>
  `${r.needsYou ? `${atLeast(r, r.needsYou)} need you` : 'Nothing needs you'}; ${atLeast(r, r.unfinished.total)} unfinished, ${r.unfinished.stuck} stuck; ${plural(r.agents, 'agent')} connected.`
