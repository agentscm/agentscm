// Shared pieces of the Work pages: state labels, the lead, pills, the lane bar and small links.
//
// API for other packages:
//   STATE        label, tone, icon and tooltip for each WorkState
//   lead(item)   the current lead's { agent, login }
//   StatePill, ClaimPill, LaneBar   the task's state, its claim, and the four-segment capsule
//   WorkLink     a link to one task (`#work/<id>`); Source: the task's issue or Plain Git origin
//   Search       the search field used on Work and Inbox; PathPair: two overlapping paths
//   plural(n, one, many?)  "1 task" / "2 tasks"
import { type ReactNode } from 'react'
import { currentLeadParticipant, safeForgeIssueHref, workHref, type WorkItem, type WorkState } from '../standup.ts'
import { agentLabel, Avatar, Icon, Path, Pill, Tip, localDate, type Tone } from '../ui.tsx'

/** Label, tone, icon and tooltip for each reported work state. */
export const STATE: Record<WorkState, { label: string; tone: Tone; icon: string; tip: string }> = {
  active: { label: 'In progress', tone: 'brand', icon: 'play-circle', tip: 'The lead agent reports it is actively working on this.' },
  blocked: { label: 'Blocked', tone: 'bad', icon: 'sign-stop', tip: 'The lead agent reported a blocker and is waiting on it.' },
  handover: { label: 'Handover', tone: 'brand', icon: 'arrow-left-right', tip: 'The lead reported this is ready for another agent to take over.' },
  paused: { label: 'Paused', tone: 'warn', icon: 'pause-circle', tip: 'Still assigned, but the lead reported it has paused.' },
  finished: { label: 'Reported finished', tone: 'ok', icon: 'check2-circle', tip: 'The agent reported this finished. That does not confirm review, verification or merge.' },
}

/** "1 task" / "2 tasks". */
export const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

/** The current lead participant, falling back to the item's lead fields. */
export function lead(item: WorkItem): { agent: string; login: string } {
  try {
    const p = currentLeadParticipant(item)
    return { agent: p.agent, login: p.login }
  } catch {
    return { agent: item.lead_agent, login: item.owner_login }
  }
}

export function StatePill({ state }: { state: WorkState }) {
  const s = STATE[state]
  return (
    <Pill tone={s.tone} icon={s.icon} tip={s.tip}>
      {s.label}
    </Pill>
  )
}

export function ClaimPill({ item }: { item: WorkItem }) {
  if (item.state === 'finished') return null
  if (item.claim_status === 'active')
    return (
      <Pill tone="ok" icon="person-check" tip="The lead checked in recently and still holds this assignment.">
        Assigned
      </Pill>
    )
  return (
    <Pill tone="warn" icon="exclamation-triangle" tip="An old or unknown check-in does not release the assignment. Coordinate with the lead before taking it over.">
      {item.claim_status === 'stale' ? 'No recent check-in' : 'Check-in unknown'} · still assigned
    </Pill>
  )
}

/** The current segment's look follows the reported state; a stale claim is amber whatever the state says. */
function nowClass(i: WorkItem): string {
  if (i.state === 'blocked') return 'lb-fail'
  if (i.claim_status !== 'active' || i.state === 'paused') return 'lb-stale'
  if (i.state === 'handover') return 'lb-ready'
  return ''
}

const NOW_NOTE: Record<WorkState, (i: WorkItem) => string> = {
  active: (i) => i.current?.summary ?? 'Working, no report yet',
  blocked: (i) => i.blocker?.summary ?? 'Blocked',
  handover: (i) => i.next_action ?? 'Ready to hand over',
  paused: (i) => i.current?.summary ?? 'No reason reported',
  finished: (i) => i.current?.summary ?? 'Reported finished',
}

/**
 * One task as a segmented capsule, read left to right: claimed, where it is now (wide, with the
 * holder's face), its next step, then finished. Every segment opens the task.
 */
export function LaneBar({ item, fresh }: { item: WorkItem; fresh: boolean }) {
  const l = lead(item)
  const href = workHref(item.id)
  const s = STATE[item.state]
  // The latest report is quoted once, under the task; the capsule only adds what that line doesn't say.
  const note = NOW_NOTE[item.state](item)
  const claimTip = `Claimed by ${agentLabel(l.agent)} (${l.login}) ${localDate(item.created_at)}`
  return (
    <div className="lb-wrap">
      <ol className="lb lb-fit" aria-label={`${item.title}: ${s.label}`}>
        <li className="lb-seg lb-done">
          <Tip text={claimTip} className="lb-tip">
            <a className="lb-hit" href={href}>
              <Icon name="check2" />
              <span className="lb-label">Claimed</span>
            </a>
          </Tip>
        </li>
        <li className={`lb-seg is-here ${nowClass(item)}${fresh ? ' is-new' : ''}`}>
          <a className="lb-hit" href={href}>
            <span className="baton">
              <Avatar name={l.agent} size={24} />
            </span>
            <span className="lb-label">{item.claim_status === 'active' || item.state === 'paused' ? s.label : item.claim_status === 'stale' ? 'Quiet' : 'Unknown'}</span>
            {note !== item.current?.summary && <span className="lb-note">{note}</span>}
          </a>
        </li>
        <li className={`lb-seg ${item.next_action ? 'lb-none' : 'lb-unknown'}`}>
          <Tip text={item.next_action ? `Next: ${item.next_action}` : 'No next step reported'} className="lb-tip">
            <a className="lb-hit" href={href}>
              <Icon name="arrow-right" />
              <span className="lb-label">Next</span>
            </a>
          </Tip>
        </li>
        <li className="lb-seg">
          <Tip text="Not reported finished yet. Even a finished report doesn’t mean it was reviewed or merged." className="lb-tip">
            <a className="lb-hit" href={href}>
              <Icon name="flag" />
              <span className="lb-label">Finished</span>
            </a>
          </Tip>
        </li>
      </ol>
    </div>
  )
}

/** A link to one task; shows the ID when no children are given. */
export function WorkLink({ id, children }: { id: string; children?: ReactNode }) {
  return <a href={workHref(id)}>{children ?? <code>{id}</code>}</a>
}

/** Where the task came from: a forge issue link or "Plain Git". */
export function Source({ item }: { item: WorkItem }) {
  if (item.source.kind === 'native')
    return (
      <Tip text={`Plain Git work request ${item.source.request_key}`} focusable className="muted small">
        <Icon name="git" /> Plain Git
      </Tip>
    )
  const href = safeForgeIssueHref(item.source)
  const text = (
    <>
      <Icon name={item.source.provider === 'gitlab' ? 'gitlab' : 'github'} /> issue {item.source.issue}
    </>
  )
  return href ? (
    <a className="small" href={href} target="_blank" rel="noreferrer noopener">
      {text} <Icon name="box-arrow-up-right" />
    </a>
  ) : (
    <span className="muted small">{text}</span>
  )
}

/** The search field used on Work and Inbox. */
export function Search({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
  return (
    <div className="input-group search">
      <span className="input-group-text">
        <Icon name="search" />
      </span>
      <input className="form-control" aria-label={placeholder} placeholder={placeholder} value={value} onChange={(e) => onChange(e.target.value)} />
      {value && (
        <button type="button" className="btn btn-outline-secondary" onClick={() => onChange('')} aria-label="Clear search">
          <Icon name="x-lg" />
        </button>
      )}
    </div>
  )
}

/** Both sides of an overlap reason; a path both sides declared shows once. */
export function PathPair({ a, b, joiner = <Icon name="intersect" className="muted" /> }: { a: string; b: string; joiner?: ReactNode }) {
  return a === b ? <Path>{a}</Path> : <><Path>{a}</Path> {joiner} <Path>{b}</Path></>
}
