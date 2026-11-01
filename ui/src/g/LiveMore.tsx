// G · Contributions and the Feature map over live data. Contributions shows only what the Hub has recorded, with
// where each number comes from; the Feature map lists everything AgentSCM has built and where it lives here.
import type { CSSProperties } from 'react'
import { Ago, Box, Icon, PageHead, Pill, relative } from '../ui.tsx'
import type { Tone } from '../ui.tsx'
import { useNow } from '../useHub.ts'
import { workHref } from '../standup.ts'
import type { LiveProps } from './GLive.tsx'

export function LiveContributions({ live }: { live: LiveProps }) {
  const ov = live.overview.kind === 'ready' ? live.overview.value : null
  const c = live.data.health?.coordination
  const recent = ov?.recent ?? []
  const done = recent.filter((o) => o.kind === 'completion').length
  const decisions = recent.filter((o) => o.kind === 'decision').length
  const now = useNow()
  const since = ov ? relative(ov.recent_since, now) : null
  const counts: { n: number | string; label: string; sub: string }[] = [
    { n: c?.denied ?? '—', label: 'first contested edits refused', sub: 'Hub counter since it started; each one asked an agent to coordinate first' },
    { n: c?.repeated ?? '—', label: 'repeated edits let through', sub: 'The agent edited again after the refusal' },
    { n: ov?.overlaps.length ?? '—', label: 'scope overlaps open', sub: 'Two unfinished changes declared the same paths' },
    { n: live.d?.contested.length ?? '—', label: 'contested files now', sub: 'Two agents hold one file' },
    { n: done, label: 'reported finished', sub: since ? `Since ${since}` : 'In the recent window' },
    { n: decisions, label: 'decisions recorded', sub: since ? `Since ${since}` : 'In the recent window' },
  ]
  return (
    <>
      <PageHead question="What did AgentSCM contribute?" answer={<>What the Hub has recorded.<br /><span className="g-quiet">Each number says where it comes from.</span></>} fact="No savings are claimed. Token and cost reports exist in the Hub’s API but have no page yet; unknown usage stays unknown, never zero." />
      <div className="g-counts g-counts-wide">
        {counts.map((k, i) => <div key={k.label} className="g-count" style={{ '--i': i } as CSSProperties}><span className="g-count-n">{k.n}</span><b>{k.label}</b><span className="small muted">{k.sub}</span></div>)}
      </div>
      <Box icon="clock-history" title="Recently reported" count={recent.length} explain="Completions and decisions agents reported. Reported finished does not confirm review, verification or merge.">
        {recent.length === 0 && <p className="small muted mb-0">Nothing reported in this window.</p>}
        <ul className="g-stories">{recent.slice(0, 12).map((o, i) => <li key={o.seq} style={{ '--i': i } as CSSProperties}><span className={`g-ring tone-${o.kind === 'completion' ? 'ok' : 'brand'}`}><Icon name={o.kind === 'completion' ? 'flag' : 'signpost-split'} /></span><span><b>{o.item.title}</b><span className="small muted d-block">{o.item.owner_login} · {o.kind === 'completion' ? 'reported finished' : 'decision'} · <Ago at={o.at} /></span></span><a href={workHref(o.item.id)} className="px-link">Record</a></li>)}</ul>
      </Box>
      <p className="small muted g-measure"><Icon name="info-circle" /> More history: <a href="#activity">Catch up</a> for what happened since you last looked, <a href="#work/finished">Finished and history</a> for every change.</p>
    </>
  )
}

type FStatus = 'Implemented' | 'Experimental' | 'Partial' | 'Proposed'
const F_TONE: Record<FStatus, Tone> = { Implemented: 'ok', Experimental: 'brand', Partial: 'warn', Proposed: 'muted' }
const FEATURES: { job: string; icon: string; blurb: string; items: { name: string; status: FStatus; where: string; href?: string }[] }[] = [
  { job: 'Coordinate before work collides', icon: 'arrow-left-right', blurb: 'Agents announce what they edit; overlaps become conversations, not merge conflicts.', items: [
    { name: 'File holds, first contested edit refused', status: 'Implemented', where: 'Repository › Held files', href: '#now' },
    { name: 'Scope overlaps and directed requests', status: 'Experimental', where: 'Inbox', href: '#inbox' },
    { name: 'Handoffs between agents', status: 'Experimental', where: 'Inbox', href: '#inbox' },
    { name: 'Branch collision scan', status: 'Implemented', where: 'CLI: agentscm collisions' },
    { name: 'Slack coordination messages', status: 'Experimental', where: 'CLI: agentscm hub slack setup' },
    { name: 'Two-way forge coordination', status: 'Experimental', where: 'Repository › Held files (open pull requests)', href: '#now' },
  ] },
  { job: 'Carry context between people and agents', icon: 'journal-text', blurb: 'Pick up any change without reading a transcript.', items: [
    { name: 'Work, checkpoints and handovers', status: 'Implemented', where: 'Work › Changes', href: '#changes' },
    { name: 'Catch up and agent briefings', status: 'Implemented', where: 'Now › Catch up', href: '#activity' },
    { name: 'Change records', status: 'Experimental', where: 'Work › Change records', href: '#change-record' },
    { name: 'Shared conversations (opt-in)', status: 'Experimental', where: 'Work › Change records', href: '#change-record' },
    { name: 'Promises', status: 'Experimental', where: 'Repository › Promises', href: '#promises' },
    { name: 'Repository explorer and evidence', status: 'Experimental', where: 'Repository › Files', href: '#explorer' },
  ] },
  { job: 'Apply a repeatable process', icon: 'diagram-3', blurb: 'Set the process once; agents read it at session start.', items: [
    { name: 'Supported skills (pr-gate)', status: 'Experimental', where: 'Workflows › Process and Skills', href: '#process' },
    { name: 'Workflow and skill library', status: 'Experimental', where: 'Workflows › Library', href: '#templates' },
    { name: 'Skill workflow runs', status: 'Experimental', where: 'Workflows › Runs', href: '#workflows' },
    { name: 'Local validation, signed', status: 'Experimental', where: 'CLI: agentscm local-validation' },
    { name: 'After-merge pipeline rules', status: 'Experimental', where: 'CLI: agentscm pipeline' },
    { name: 'Stop hooks', status: 'Partial', where: 'Workflows › Process', href: '#process' },
  ] },
  { job: 'Explain whether a change is ready', icon: 'patch-check', blurb: 'Every step says how it is known: reported, recorded, observed or verified.', items: [
    { name: 'Lifecycle with proof per step', status: 'Experimental', where: 'Work › a change', href: '#changes' },
    { name: 'PR context and linked work', status: 'Experimental', where: 'A change › everything recorded', href: '#changes' },
    { name: 'Landed: merge, CI and deploys', status: 'Experimental', where: 'Work › Landed', href: '#landed' },
    { name: 'Intent review', status: 'Experimental', where: 'A change (local page only)' },
  ] },
  { job: 'Run the team’s Hub', icon: 'plug', blurb: 'Connections, people and this computer.', items: [
    { name: 'Agent setup and verification', status: 'Partial', where: 'Setup › Agents', href: '#setup' },
    { name: 'Sign in once per computer', status: 'Experimental', where: 'Setup › Computers', href: '#computers' },
    { name: 'Workspace members and runs', status: 'Experimental', where: 'Setup › Access', href: '#workspace' },
    { name: 'Hub health', status: 'Implemented', where: 'Setup › Hub', href: '#health' },
    { name: 'Menu-bar companion (macOS)', status: 'Experimental', where: 'Desktop app' },
    { name: 'All repositories', status: 'Experimental', where: 'All repositories', href: '#repositories' },
  ] },
  { job: 'Native source control (opt-in)', icon: 'box', blurb: 'Immutable revisions and gates for teams that turn them on.', items: [
    { name: 'Workspace snapshots and timeline', status: 'Experimental', where: 'Repository › Timeline', href: '#timeline' },
    { name: 'Ledger, approvals and landing', status: 'Experimental', where: 'CLI and Setup › Access', href: '#workspace' },
    { name: 'Git shadow workflow', status: 'Experimental', where: 'CLI only' },
    { name: 'Attached Git worktrees', status: 'Experimental', where: 'CLI only' },
    { name: 'Publish a branch and pull request', status: 'Experimental', where: 'CLI: agentscm hub publish' },
    { name: 'Native authority per target', status: 'Proposed', where: 'Not built' },
  ] },
]

export function LiveFeatures() {
  const total = FEATURES.reduce((n, g) => n + g.items.length, 0)
  return (
    <>
      <PageHead question="What has AgentSCM built?" answer={<>{total} capabilities, six jobs.<br /><span className="g-quiet">Each one has a place.</span></>} fact="Status is the roadmap’s own label. Experimental means built and tested against fakes or one machine; Proposed means not built. Items marked CLI have no page." />
      <div className="g-legend">{(['Implemented', 'Experimental', 'Partial', 'Proposed'] as const).map((s) => <Pill key={s} tone={F_TONE[s]}>{s}</Pill>)}</div>
      <div className="g-fmap">
        {FEATURES.map((g, gi) => (
          <section key={g.job} className="g-fgroup" style={{ '--i': gi } as CSSProperties}>
            <span className="g-ring big tone-brand"><Icon name={g.icon} /></span>
            <h2>{g.job}</h2>
            <p>{g.blurb}</p>
            <ul>
              {g.items.map((it) => {
                const inner = <><span className="g-fitem-name">{it.name}<small><Icon name={it.href ? 'geo-alt' : it.where.startsWith('CLI') ? 'terminal' : 'laptop'} /> {it.where}</small></span><Pill tone={F_TONE[it.status]}>{it.status}</Pill></>
                return <li key={it.name}>{it.href ? <a href={it.href} className="g-fitem">{inner}</a> : <span className="g-fitem">{inner}</span>}</li>
              })}
            </ul>
          </section>
        ))}
      </div>
    </>
  )
}
