import type { ReactNode } from 'react'
import { notable, type Contested, type Derived, type LiveAgent } from '../derive.ts'
import type { WorkOverview } from '../standup.ts'
import type { PullRequest } from '../types.ts'
import type { HubData } from '../useHub.ts'
import { Ago, Avatar, Box, Empty, Explain, Icon, Notice, Path, Pill, Skeleton, Tip, Via } from '../ui.tsx'
import type { Poll } from '../usePoll.ts'

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

/** The signature move: counts in the answer are links that bring their evidence box into view and ring it. */
function Jump({ to, children, tone = 'brand' }: { to: string; children: ReactNode; tone?: string }) {
  return (
    <a
      href={`#${to}`}
      className={`jump tone-${tone}`}
      onClick={(e) => {
        const el = document.getElementById(to)
        if (!el) return
        e.preventDefault()
        el.scrollIntoView({ behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth', block: 'start' })
        el.classList.remove('ring')
        void el.offsetWidth
        el.classList.add('ring')
      }}
    >
      {children}
    </a>
  )
}

function KeyFact({ d }: { d: Derived }) {
  const n = notable(d.contested)
  if (n?.kind === 'handoff' && n.c.handoff) {
    const h = n.c.handoff
    const state = h.answer ? (h.answer.kind === 'granted' ? 'handed it over' : 'kept it') : h.lapsed ? 'the request lapsed' : 'waiting for an answer'
    return (
      <>
        <Icon name="arrow-left-right" /> {h.from.owner}’s {h.from.name} asked {h.to.owner}’s {h.to.name} for <Path>{n.c.path}</Path> · {state}.
      </>
    )
  }
  if (n?.c.refused) {
    const r = n.c.refused
    return (
      <>
        <Icon name="slash-circle" /> {r.owner}’s {r.name} was refused an edit to <Path>{n.c.path}</Path>
        {n.c.repeated ? ', then repeated it and was let through.' : '.'}
      </>
    )
  }
  const c = d.contested[0]
  if (c)
    return (
      <>
        <Icon name="exclamation-diamond" /> {c.holders.map((h) => `${h.owner}’s ${h.name}`).join(' and ')} both hold <Path>{c.path}</Path>.
      </>
    )
  const p = d.prOverlaps[0]
  const w = p?.with[0]
  if (p && w)
    return (
      <>
        <Icon name="git" /> PR #{p.pr.number} touches files {w.agent.owner}’s {w.agent.name} is working on.
      </>
    )
  if (d.agents.length > 0) return <><Icon name="check2-circle" /> Agents are working on separate files.</>
  return <>Live activity appears here within seconds of an agent starting.</>
}

function Answer({ repo, d, overview, failing }: { repo: string; d: Derived | null; overview: Poll<WorkOverview>; failing: boolean }) {
  if (!d) {
    return (
      <div className="answer">
        <h1 className="page-question">Who is editing which files right now?</h1>
        {failing ? <p className="answer-text">Waiting for the hub. This page keeps trying and fills in as soon as it answers.</p> : <Skeleton rows={2} />}
      </div>
    )
  }
  const overlaps = overview.kind === 'ready' ? overview.value.overlaps.length : null
  const c = d.contested.length
  return (
    <div className="answer" data-tour="answer">
      <h1 className="page-question">Who is editing which files right now?</h1>
      <p className="answer-text">
        {d.agents.length === 0 ? (
          <>No agent is holding files in {repo} right now.</>
        ) : (
          <>
            <Jump to="box-agents">{plural(d.agents.length, 'agent')}</Jump> {d.agents.length === 1 ? 'is' : 'are'} holding <Jump to="box-agents">{plural(d.filesHeld, 'file')}</Jump> in <span className="repo-name">{repo}</span>.{' '}
            {c === 0 ? (
              <>No file is contested.</>
            ) : (
              <>
                <Jump to="box-contested" tone="bad">
                  {plural(c, 'file')} {c === 1 ? 'is' : 'are'} contested
                </Jump>
                .
              </>
            )}{' '}
            {d.prOverlaps.length > 0 && (
              <>
                <Jump to="box-prs" tone="warn">
                  {plural(d.prOverlaps.length, 'pull request')}
                </Jump>{' '}
                {d.prOverlaps.length === 1 ? 'touches' : 'touch'} held files.
              </>
            )}
          </>
        )}
      </p>
      <p className="answer-fact">
        <KeyFact d={d} />
      </p>
      {overlaps !== null && overlaps > 0 && (
        <a href="#work" className="answer-next">
          <Icon name="sign-intersection-y" /> {plural(overlaps, 'task')} declare overlapping scope — review on Work
        </a>
      )}
    </div>
  )
}

function AgentRow({ a }: { a: LiveAgent }) {
  return (
    <li className="agent">
      <Avatar name={a.name} size={36} />
      <div className="agent-main">
        <div className="agent-line">
          <strong>{a.name}</strong>
          <span className="muted">{a.owner}</span>
          {a.via.map((v) => (
            <Via key={v} via={v} />
          ))}
          <span className="ms-auto muted small">
            <Icon name="activity" /> <Ago at={a.lastActive} />
          </span>
        </div>
        {a.task ? <p className="agent-task">{a.task}</p> : <p className="agent-task muted">No task description</p>}
        <div className="agent-meta">
          {a.branch && (
            <Tip text="Branch the agent reported" focusable>
              <Icon name="git" /> <code>{a.branch}</code>
            </Tip>
          )}
          {a.machine && (
            <Tip text="Machine the agent runs on" focusable>
              <Icon name="laptop" /> {a.machine}
            </Tip>
          )}
        </div>
        <div className="holdings" aria-label="Files held">
          {a.holdings.length === 0 && <span className="muted small">Not holding any files</span>}
          {a.holdings.map((h) =>
            h.contested ? (
              <Tip key={h.spec} text="Another agent also holds this path. See Contested files." focusable className="holding is-contested">
                <Icon name="exclamation-diamond-fill" /> {h.spec} <span className="visually-hidden">(contested)</span>
              </Tip>
            ) : (
              <span key={h.spec} className="holding">
                <Icon name={h.spec.endsWith('**') ? 'folder2' : 'file-earmark-code'} /> {h.spec}
              </span>
            ),
          )}
        </div>
      </div>
    </li>
  )
}

function ContestedRow({ c }: { c: Contested }) {
  const h = c.handoff
  return (
    <li className="contested">
      <div className="contested-path">
        <Icon name="file-earmark-break" /> <Path>{c.path}</Path>
      </div>
      <div className="contested-holders">
        {c.holders.map((x, i) => (
          <span key={x.id}>
            {i > 0 && <Icon name="arrow-left-right" className="muted" />} <strong>{x.name}</strong> <span className="muted">{x.owner}</span>
          </span>
        ))}
      </div>
      <div className="contested-state">
        {c.refused && (
          <Pill tone="bad" icon="slash-circle" tip="The hub refused this agent’s first edit because another agent holds the file.">
            {c.refused.owner}’s {c.refused.name} refused <Ago at={c.refused.at} />
          </Pill>
        )}
        {c.repeated && (
          <Pill tone="warn" icon="arrow-repeat" tip="After one refusal, repeating the same edit is let through. The hub warns; it does not block twice.">
            Repeated and let through
          </Pill>
        )}
        {h && (
          <Pill
            tone={h.answer ? (h.answer.kind === 'granted' ? 'ok' : 'warn') : h.lapsed ? 'muted' : 'brand'}
            icon={h.answer ? (h.answer.kind === 'granted' ? 'check2-circle' : 'lock') : h.lapsed ? 'hourglass-bottom' : 'hourglass-split'}
            tip="Experimental handoff: one agent asked the holder to hand the file over. Unanswered requests lapse after 30 minutes."
          >
            {h.from.name} asked {h.to.name} · {h.answer ? (h.answer.kind === 'granted' ? 'handed over' : 'kept') : h.lapsed ? 'lapsed' : 'waiting'}
          </Pill>
        )}
      </div>
      {h?.note && <p className="quote">“{h.note}”</p>}
      {h?.answer?.reason && <p className="quote">“{h.answer.reason}”</p>}
    </li>
  )
}

/** PR links come from the forge mirror (teammate-controlled data): only https:// is ever a link. */
export const httpsUrl = (u: string | null | undefined): string | null => (u?.startsWith('https://') ? u : null)

/** Forge URLs are untrusted hub data: only https:// becomes a link. */
function PrTitle({ pr }: { pr: PullRequest }) {
  const url = httpsUrl(pr.html_url)
  return url ? (
    <a href={url} target="_blank" rel="noreferrer noopener">
      #{pr.number} {pr.title} <Icon name="box-arrow-up-right" />
    </a>
  ) : (
    <strong>
      #{pr.number} {pr.title}
    </strong>
  )
}

export function NowPage({ repo, data, d, overview }: { repo: string; data: HubData; d: Derived | null; overview: Poll<WorkOverview> }) {
  const work = data.work
  return (
    <div className="page">
      <Answer repo={repo} d={d} overview={overview} failing={data.failing} />
      {data.failing && d && (
        <Notice title="The hub is not answering">Showing the last data that loaded. This page keeps retrying.</Notice>
      )}
      <div className="grid-now">
        <Box
          id="box-agents"
          tour="now-agents"
          icon="people"
          title="Live agents"
          count={d ? d.agents.length : undefined}
          explain="Agents that announced work or edited files recently, grouped by the person who owns them. Each chip is a file or folder the agent holds."
          className="area-agents"
        >
          {!d ? (
            <Skeleton rows={4} />
          ) : d.groups.length === 0 ? (
            <Empty icon="moon-stars" title="No agents active">
              When an agent connected through a hook or MCP starts editing, it appears here with the files it holds.
            </Empty>
          ) : (
            d.groups.map((g) => (
              <div key={g.owner} className="owner-group">
                <h3 className="owner-name">
                  <Icon name="person" /> {g.owner} <span className="muted small">· {plural(g.agents.length, 'agent')}</span>
                </h3>
                <ul className="agent-list">
                  {g.agents.map((a) => (
                    <AgentRow key={a.id} a={a} />
                  ))}
                </ul>
              </div>
            ))
          )}
          {work?.intents_truncated && <Notice title="Some agents are not shown">The hub returned its maximum number of intents. More agents may be active.</Notice>}
        </Box>

        <Box
          id="box-contested"
          tour="now-contested"
          icon="exclamation-diamond"
          tone={d && d.contested.length > 0 ? 'bad' : 'ok'}
          title="Contested files"
          count={d ? d.contested.length : undefined}
          explain="A file is contested when two agents hold overlapping paths. The hub refuses the second agent’s first edit and lets a repeated edit through."
          className="area-contested"
        >
          {!d ? (
            <Skeleton rows={3} />
          ) : d.contested.length === 0 ? (
            <Empty icon="check2-circle" title="No contested files">
              Every held path belongs to exactly one agent.
            </Empty>
          ) : (
            <ul className="contested-list">
              {d.contested.map((c) => (
                <ContestedRow key={c.path} c={c} />
              ))}
            </ul>
          )}
        </Box>

        <Box
          id="box-prs"
          tour="now-prs"
          icon="git"
          tone={d && d.prOverlaps.length > 0 ? 'warn' : 'brand'}
          title="Pull requests touching held files"
          count={d ? d.prOverlaps.length : undefined}
          explain="Open pull or merge requests mirrored from your forge whose changed files overlap paths a live agent holds."
          className="area-prs"
        >
          {!d || !work ? (
            <Skeleton rows={2} />
          ) : !work.connected ? (
            <Empty icon="plug" title="No forge connection">
              {data.health?.demo ? 'The demo hub has no GitHub or GitLab connection.' : 'Connect GitHub or GitLab to the hub to see open pull requests here.'}
            </Empty>
          ) : d.prOverlaps.length === 0 ? (
            <Empty icon="check2-circle" title="No overlaps with open pull requests">
              {d.otherOpenPrs > 0 ? `${plural(d.otherOpenPrs, 'other open pull request')} touch only files no agent holds.` : 'There are no open pull requests.'}
            </Empty>
          ) : (
            <ul className="pr-list">
              {d.prOverlaps.map((p) => (
                <li key={p.pr.number} className="pr">
                  <div className="pr-head">
                    <Icon name={p.pr.draft ? 'file-earmark-diff' : 'git'} />
                    <PrTitle pr={p.pr} />
                    {p.pr.draft && <Pill tone="muted">Draft</Pill>}
                  </div>
                  <div className="muted small">
                    {p.pr.author.login} · <code>{p.pr.head_ref}</code> → <code>{p.pr.base_ref}</code>
                    {p.pr.files_truncated && (
                      <>
                        {' '}
                        · file list truncated <Explain text="The forge returned only part of this PR’s file list, so more overlaps may exist." />
                      </>
                    )}
                  </div>
                  {p.with.map((w) => (
                    <div key={w.agent.id} className="pr-with">
                      <Icon name="arrow-return-right" /> overlaps <strong>{w.agent.name}</strong> <span className="muted">({w.agent.owner})</span> on {w.files.map((f) => <Path key={f}>{f}</Path>)}
                    </div>
                  ))}
                </li>
              ))}
              {d.otherOpenPrs > 0 && <li className="muted small">+ {plural(d.otherOpenPrs, 'other open pull request')} with no overlap</li>}
            </ul>
          )}
        </Box>
      </div>
    </div>
  )
}
