import { useState, type ReactNode } from 'react'
import { overlaps, pathsOf, type Derived } from '../derive.ts'
import { fetchActivityPage } from '../api.ts'
import type { HubEvent } from '../types.ts'
import { Ago, Avatar, Box, Empty, Icon, Path, Pill, Skeleton, viaLabel, type Tone } from '../ui.tsx'

const strs = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [])
const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {})

function Files({ paths }: { paths: string[] }) {
  if (paths.length === 0) return <>files</>
  return (
    <>
      {paths.slice(0, 2).map((p) => (
        <Path key={p}>{p}</Path>
      ))}
      {paths.length > 2 && <span className="muted"> +{paths.length - 2} more</span>}
    </>
  )
}

interface Row {
  icon: string
  tone: Tone
  text: ReactNode
  sub?: ReactNode
  experimental?: boolean
}

function describe(e: HubEvent, d: Derived): Row | null {
  const data = e.data
  switch (e.kind) {
    case 'session_started':
    case 'session_connected': {
      const v = strs(data['via'])
      const label = v.length ? ` via ${v.map(viaLabel).join(', ')}` : typeof data['via'] === 'string' ? ` via ${viaLabel(data['via'])}` : ''
      return e.kind === 'session_started' ? { icon: 'box-arrow-in-right', tone: 'ok', text: <>started a session{label}</> } : { icon: 'plug', tone: 'ok', text: <>connected{label}</> }
    }
    case 'session_ended':
      return { icon: 'power', tone: 'muted', text: <>ended a session</> }
    case 'edit_denied': {
      const holders = [...new Set(strs(data['holders']).map((id) => {
        const i = d.intentById.get(id)
        return i ? `${i.owner.login}’s ${i.agent.name}` : 'another agent'
      }))]
      return { icon: 'slash-circle', tone: 'bad', text: <>was refused an edit to <Files paths={pathsOf(e)} /> (held by {holders.length ? holders.join(', ') : 'another agent'})</> }
    }
    case 'edit_repeated':
      return { icon: 'arrow-repeat', tone: 'warn', text: <>repeated an edit to <Files paths={pathsOf(e)} /> and was let through</> }
    case 'handoff_request': {
      const to = rec(data['to'])
      const note = typeof data['note'] === 'string' && data['note'] ? data['note'] : null
      return { icon: 'arrow-left-right', tone: 'brand', experimental: true, text: <>asked {String(to['login'])}’s {String(to['agent'])} for <Files paths={pathsOf(e)} /></>, sub: note && <>“{note}”</> }
    }
    case 'handoff_grant':
    case 'handoff_decline': {
      const from = rec(data['from'])
      const granted = e.kind === 'handoff_grant'
      const reason = typeof data['reason'] === 'string' && data['reason'] ? data['reason'] : null
      return granted
        ? { icon: 'check2-circle', tone: 'ok', experimental: true, text: <>handed <Files paths={pathsOf(e)} /> to {String(from['login'])}’s {String(from['agent'])}</> }
        : { icon: 'lock', tone: 'warn', experimental: true, text: <>kept <Files paths={pathsOf(e)} /></>, sub: reason && <>“{reason}”</> }
    }
    case 'hold_expired':
      return { icon: 'hourglass-bottom', tone: 'muted', text: <>let a hold expire</> }
    case 'intent_announced': {
      const task = typeof data['task'] === 'string' && data['task'] ? data['task'] : null
      const paths = pathsOf(e)
      return { icon: 'flag', tone: 'brand', text: <>announced work{paths.length > 0 && <> on <Files paths={paths} /></>}</>, sub: task }
    }
    case 'intent_released':
      return { icon: 'unlock', tone: 'muted', text: <>released its files</> }
    case 'intent_notice': {
      const other = rec(data['owner'])['login']
      const agent = typeof data['agent'] === 'string' ? data['agent'] : rec(data['agent'])['name']
      const theirs = strs(data['their_paths'])
      const yours = strs(data['your_paths'])
      const shared = theirs.filter((t) => yours.some((y) => overlaps(t, y)))
      const who = typeof other === 'string' && typeof agent === 'string' ? `${other}’s ${agent}` : 'another agent'
      return { icon: 'bell', tone: 'warn', text: <>was told {who} started on <Files paths={shared.length ? shared : theirs} /></> }
    }
    case 'pr_notice': {
      const n = typeof data['number'] === 'number' ? data['number'] : null
      const title = typeof data['title'] === 'string' ? data['title'] : null
      const pr = n !== null ? `PR #${n}` : 'a pull request'
      if (data['merged'] === true) {
        const base = typeof data['base_ref'] === 'string' ? data['base_ref'] : 'the base'
        return { icon: 'git', tone: 'warn', text: <>was told {pr} merged into {base} under its files, so it should sync first</>, sub: title }
      }
      return { icon: 'git', tone: 'warn', text: <>was told {pr} touches its files</>, sub: title }
    }
    case 'task_overlap': {
      const task = typeof data['task'] === 'string' ? data['task'] : null
      const matches = strs(data['work']).length + (Array.isArray(data['pulls']) ? data['pulls'].length : 0)
      return { icon: 'people', tone: 'warn', text: <>started a task that reads like {matches === 1 ? 'work' : `${matches} pieces of work`} already under way</>, sub: task }
    }
    default:
      return null
  }
}

const KINDS: { key: string; label: string; match: (k: string) => boolean }[] = [
  { key: 'all', label: 'Everything', match: () => true },
  { key: 'conflict', label: 'Refusals & handoffs', match: (k) => k.startsWith('edit_') || k.startsWith('handoff_') },
  { key: 'work', label: 'Announcements', match: (k) => k.startsWith('intent_') || k === 'pr_notice' || k === 'task_overlap' || k === 'hold_expired' },
  { key: 'session', label: 'Sessions', match: (k) => k.startsWith('session_') },
]

export function ActivityPage({ d, events: live, fresh, repo, token }: { d: Derived | null; events: HubEvent[] | null; fresh: ReadonlySet<string>; repo: string | null; token: string }) {
  const [kind, setKind] = useState('all')
  const [expanded, setExpanded] = useState(false)
  // Older pages are appended below the live window; the live events keep updating at the top.
  const [older, setOlder] = useState<HubEvent[]>([])
  const [exhausted, setExhausted] = useState(false)
  const [loadingOlder, setLoadingOlder] = useState(false)
  const [olderError, setOlderError] = useState<string | null>(null)
  const events = live ? [...new Map([...live, ...older].map((e) => [e.id, e])).values()].sort((a, b) => b.seq - a.seq) : null
  const loadOlder = async () => {
    if (!repo || !events || events.length === 0) return
    setLoadingOlder(true)
    setOlderError(null)
    try {
      const page = await fetchActivityPage(repo, token, new AbortController().signal, Math.min(...events.map((e) => e.seq)))
      setOlder([...events, ...page.events])
      setExhausted(page.next_before === null)
    } catch {
      setOlderError('Could not load older events.')
    } finally {
      setLoadingOlder(false)
    }
  }
  const match = KINDS.find((k) => k.key === kind)!.match
  const rows = d && events ? events.filter((e) => match(e.kind)).map((e) => ({ e, row: describe(e, d) })).filter((x): x is { e: HubEvent; row: Row } => x.row !== null).slice(0, older.length ? undefined : 30) : null
  const shown = rows ? (expanded || older.length ? rows : rows.slice(0, 12)) : null
  return (
    <>
      <Box
        id="box-log"
        icon="clock-history"
        title="The full log"
        count={rows?.length}
        explain="What agents did on this repository, newest first: sessions, announced work, refused and repeated edits, and handoffs. New events slide in as they arrive."
        actions={
          <div className="btn-group btn-group-sm" data-tour="activity-filter" role="group" aria-label="Filter events">
            {KINDS.map((k) => (
              <button key={k.key} type="button" className={`btn ${kind === k.key ? 'btn-secondary' : 'btn-outline-secondary'}`} aria-pressed={kind === k.key} onClick={() => setKind(k.key)}>
                {k.label}
              </button>
            ))}
          </div>
        }
      >
        {!shown ? (
          <Skeleton rows={6} />
        ) : shown.length === 0 ? (
          <Empty icon="clock" title="No activity yet">
            Events appear here as agents start sessions, announce work and edit files.
          </Empty>
        ) : (
          <ol className="timeline" data-tour="activity-feed">
            {shown.map(({ e, row }) => (
              <li key={e.id} className={`event tone-${row.tone}${fresh.has(e.id) ? ' is-fresh' : ''}`}>
                <span className={`event-icon tone-${row.tone}`}>
                  <Icon name={row.icon} />
                </span>
                <div className="event-body">
                  <div>
                    <Avatar name={e.agent.name} size={20} /> <strong>{e.agent.name}</strong> <span className="muted">({e.owner.login})</span> {row.text}{' '}
                    {row.experimental && (
                      <Pill tone="muted" icon="flask" tip="Handoff is experimental: the API and behaviour may change.">
                        Experimental
                      </Pill>
                    )}
                  </div>
                  {row.sub && <div className="event-sub">{row.sub}</div>}
                </div>
                <span className="event-time small muted">
                  <Ago at={e.at} />
                </span>
              </li>
            ))}
          </ol>
        )}
        {repo && rows && (
          <div className="more-row" data-tour="activity-load-older">
            {exhausted ? <span className="small muted"><Icon name="check2" /> That’s everything.</span> : <button type="button" className="btn btn-sm btn-outline-secondary" disabled={loadingOlder} onClick={() => void loadOlder()}><Icon name="clock-history" /> {loadingOlder ? 'Loading…' : 'Load older'}</button>}
            {olderError && <span className="small text-danger-emphasis" role="alert">{olderError}</span>}
          </div>
        )}
        {rows && rows.length > 12 && (
          <button type="button" className="btn btn-link btn-sm" onClick={() => setExpanded((x) => !x)}>
            <Icon name={expanded ? 'chevron-up' : 'chevron-down'} /> {expanded ? 'Show fewer' : `Show all ${rows.length}`}
          </button>
        )}
      </Box>
    </>
  )
}
