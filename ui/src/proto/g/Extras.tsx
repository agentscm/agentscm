// G · utilities: the feature map (what AgentSCM has built and where it lives), Setup, contributions and Inbox.
import type { CSSProperties } from 'react'
import { Avatar, Box, Icon, PageHead, Pill } from '../../ui.tsx'
import { CONNECTIONS, NEEDS, VALUE } from '../data.ts'
import { Go } from '../kit.tsx'
import { FEATURES, F_TONE } from './gdata.ts'
import { useState } from 'react'
import { copyText } from './copy.ts'
import { NeedCard } from './Now.tsx'
import { useStore, type Story } from './store.tsx'

export function FeatureMap() {
  const total = FEATURES.reduce((n, g) => n + g.items.length, 0)
  return (
    <>
      <PageHead question="What has AgentSCM built?" answer={<>{total} capabilities, six jobs.<br /><span className="g-quiet">Each one has a place.</span></>} fact="Status is the roadmap’s own label. Experimental means built and tested against fakes or one machine; Proposed means not built." />
      <div className="g-legend">{(['Implemented', 'Experimental', 'Partial', 'Draft', 'Proposed'] as const).map((s) => <Pill key={s} tone={F_TONE[s]}>{s}</Pill>)}</div>
      <div className="g-fmap">
        {FEATURES.map((g, gi) => (
          <section key={g.job} className="g-fgroup" style={{ '--i': gi } as CSSProperties}>
            <span className="g-ring big tone-brand"><Icon name={g.icon} /></span>
            <h2>{g.job}</h2>
            <p>{g.blurb}</p>
            <ul>
              {g.items.map((it) => (
                <li key={it.name}>
                  {it.status === 'Proposed'
                    ? <span className="g-fitem"><span className="g-fitem-name">{it.name}<small><Icon name="geo-alt" /> {it.where}</small></span><Pill tone={F_TONE[it.status]}>{it.status}</Pill></span>
                    : <Go to={it.to} className="g-fitem"><span className="g-fitem-name">{it.name}<small><Icon name="geo-alt" /> {it.where}</small></span><Pill tone={F_TONE[it.status]}>{it.status}</Pill></Go>}
                </li>
              ))}
            </ul>
          </section>
        ))}
      </div>
    </>
  )
}

export function SetupG() {
  const st = useStore()
  const rows = CONNECTIONS.map((c) => (c.name === 'copilot' && st.copilotFixed ? { ...c, state: 'ok' as const, note: 'Fix sent to sam; waiting for its first edit' } : c))
  const ready = rows.filter((c) => c.state === 'ok').length
  return (
    <>
      <PageHead question="Is everything connected?" answer={<>{ready} of {rows.length - 2} connections are ready.<br /><span className="g-quiet">Optional ones stay optional.</span></>} />
      <div className="g-setup">
        {rows.map((c, i) => (
          <article key={c.name} className={`g-conn tone-${c.state}`} style={{ '--i': i } as CSSProperties}>
            <span className={`g-ring big tone-${c.state === 'muted' ? 'brand' : c.state}`}><Icon name={c.kind === 'Agent' ? 'robot' : c.kind === 'Forge' ? 'github' : c.kind === 'Computer' ? 'laptop' : c.kind === 'Optional' ? 'slack' : 'box'} /></span>
            <b>{c.name}</b>
            <span className="small muted">{c.kind}</span>
            <p>{c.note}</p>
            <span className="px-status"><span className={`px-dot tone-${c.state}`} />{c.state === 'ok' ? 'Ready' : c.state === 'warn' ? 'Needs a fix' : c.kind === 'Advanced' ? 'Off' : 'Not connected'}</span>
            {c.state === 'warn' && <button type="button" className="btn btn-sm btn-primary" onClick={st.fixCopilot}><Icon name="clipboard" /> Copy the fix</button>}
            {c.kind === 'Optional' && <SlackSetup />}
          </article>
        ))}
      </div>
    </>
  )
}

/** Slack is set up from the CLI; the Hub shows the command rather than a button that cannot do it. */
function SlackSetup() {
  const [state, setState] = useState<'idle' | 'ok' | 'failed'>('idle')
  const cmd = 'agentscm hub slack setup'
  return (
    <div className="g-slack-setup">
      <div className="g-command"><Icon name="terminal" /><code>{cmd}</code></div>
      <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => void copyText(cmd).then((ok) => setState(ok ? 'ok' : 'failed'))}><Icon name={state === 'ok' ? 'clipboard-check' : 'clipboard'} /> {state === 'ok' ? 'Copied' : 'Copy'}</button>
      {state === 'failed' && <span className="px-err" role="alert">The clipboard refused; select the command instead.</span>}
    </div>
  )
}

export function ValueG() {
  const st = useStore()
  const stories: Story[] = [...st.stories, ...VALUE.stories]
  return (
    <>
      <PageHead question="What did AgentSCM contribute?" answer={<>{stories.length} contributions this week.<br /><span className="g-quiet">Each one links to its record.</span></>} fact="Counted once across agents, retries and surfaces. Unknown usage stays unknown. No savings are claimed without a comparable baseline." />
      <div className="g-value">
        <Box icon="stars" title="Contributions" count={stories.length}>
          <ul className="g-stories">{stories.map((s, i) => <li key={s.text} className={s.fresh ? 'fresh' : ''} style={{ '--i': i } as CSSProperties}><span className="g-ring tone-brand"><Icon name={s.icon} /></span><span>{s.text}</span>{s.work && <Go to={`work/${s.work}`} className="px-link">Record</Go>}</li>)}</ul>
        </Box>
        <div className="g-counts">
          {VALUE.counts.map((c, i) => <div key={c.label} className="g-count" style={{ '--i': i } as CSSProperties}><span className="g-count-n">{c.n}</span><b>{c.label}</b><span className="small muted">{c.sub}</span></div>)}
        </div>
      </div>
    </>
  )
}

export function InboxG() {
  const st = useStore()
  return (
    <>
      <PageHead question="What conversations are open?" answer={<>{NEEDS.filter((n) => !st.resolved.has(n.id)).length} open, {st.resolved.size} settled.<br /><span className="g-quiet">Answer once; it’s answered everywhere.</span></>} />
      <div className="g-needs">
        {NEEDS.map((n, i) => (
          <div key={n.id} className="g-thread">
            <span className="g-inbox-roles">For {n.roles.map((r) => <span key={r} className="g-role"><Avatar name={r} size={18} />{r}</span>)}</span>
            <NeedCard n={n} i={i} />
          </div>
        ))}
      </div>
    </>
  )
}
