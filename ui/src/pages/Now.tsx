import { useMemo, useState } from 'react'
import { type Contested, type Derived, type LiveAgent } from '../derive.ts'
import { areas } from '../needs.ts'
import { workHref, type WorkOverview } from '../standup.ts'
import type { PullRequest } from '../types.ts'
import type { HubData } from '../useHub.ts'
import { Ago, Avatar, Box, Empty, Explain, Icon, Notice, Path, Pill, Skeleton, Tip, Via } from '../ui.tsx'
import type { Poll } from '../usePoll.ts'

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

export function AgentRow({ a, awake }: { a: LiveAgent; awake?: boolean }) {
  return (
    <li className={`agent${awake === undefined ? '' : awake ? ' is-awake' : ' is-quiet'}`}>
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

/** Where: the repository as a map of areas, then the two kinds of crossing in full. */
export function NowPage({ data, d, overview }: { data: HubData; d: Derived | null; overview: Poll<WorkOverview> }) {
  const work = data.work
  const ov = overview.kind === 'ready' ? overview.value : null
  return (
    <>
      {data.failing && d && (
        <Notice title="The hub is not answering">Showing the last data that loaded. This page keeps retrying.</Notice>
      )}
      <AreaMap d={d} ov={ov} />
      {work?.intents_truncated && <Notice title="Some agents are not shown">The hub returned its maximum number of intents. More agents may be active.</Notice>}
      <div className="grid-2">
        <Box
          id="box-contested"
          tour="now-contested"
          icon="exclamation-diamond"
          tone={d && d.contested.length > 0 ? 'bad' : 'ok'}
          title="Contested files"
          count={d ? d.contested.length : undefined}
          explain="Two agents hold overlapping paths. The hub refuses the second agent’s first edit and lets a repeated edit through."
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
    </>
  )
}

const MAX_TILES = 12

/**
 * The repository as tiles, one per top-level folder someone holds or plans to touch. Faces are the
 * agents there; a coral ring and pill mark a crossing. Picking a tile opens what is happening in it.
 */
function AreaMap({ d, ov }: { d: Derived | null; ov: WorkOverview | null }) {
  const all = useMemo(() => areas(ov, d), [ov, d])
  const [picked, setPicked] = useState<string | null>(null)
  const shown = all.slice(0, MAX_TILES)
  const area = shown.find((a) => a.name === picked) ?? null
  const most = Math.max(1, ...shown.map((a) => a.holders.length + a.planned.length))
  return (
    <Box
      id="box-map"
      tour="now-map"
      icon="grid-1x2"
      title="The repository"
      count={d ? `${plural(all.length, 'area')}` : undefined}
      tone={shown.some((a) => a.crossings > 0) ? 'bad' : 'brand'}
      explain="One tile per top-level folder an agent holds files in or a task declared it will touch. Deeper colour means more people there. Pick a tile to see who and why."
    >
      {!d ? (
        <Skeleton rows={4} />
      ) : all.length === 0 ? (
        <Empty icon="grid-1x2" title="Nobody is holding or planning files">
          When an agent starts editing or a task declares its scope, its folder appears here.
        </Empty>
      ) : (
        <>
          <div className="map" role="list">
            {shown.map((a) => {
              const people = [...new Map([...a.holders.map((h) => [h.name, h.name] as const), ...a.planned.map((p) => [p.item.lead_agent, p.item.lead_agent] as const)]).values()]
              const heat = (a.holders.length + a.planned.length) / most
              return (
                <div role="listitem" key={a.name || '(root)'} className="map-cell">
                  <button
                    type="button"
                    className={`tile${a.crossings > 0 ? ' is-crossing' : ''}${picked === a.name ? ' is-selected' : ''}`}
                    style={{ ['--heat' as string]: heat.toFixed(2) }}
                    aria-expanded={picked === a.name}
                    aria-controls="area-drawer"
                    onClick={() => setPicked((p) => (p === a.name ? null : a.name))}
                  >
                    <span className="tile-head">
                      <b>{a.name || 'Repository root'}</b>
                      <code>{a.name ? `${a.name}/` : 'files at the top level'}</code>
                    </span>
                    {a.crossings > 0 && <span className="crossing">{plural(a.crossings, 'crossing')}</span>}
                    <span className="tile-people" aria-label={`${people.length} ${people.length === 1 ? 'agent' : 'agents'}: ${people.join(', ')}`}>
                      {people.slice(0, 5).map((n) => (
                        <Avatar key={n} name={n} size={26} />
                      ))}
                      {people.length > 5 && <span className="tile-more">+{people.length - 5}</span>}
                    </span>
                    <span className="tile-lens">
                      {a.holders.length > 0 ? `${a.holders.length} editing` : 'Nobody editing'}
                      {a.planned.length > 0 && ` · ${plural(a.planned.length, 'task')} planned`}
                    </span>
                  </button>
                </div>
              )
            })}
          </div>
          {all.length > shown.length && <p className="map-hint">{plural(all.length - shown.length, 'quieter area')} not shown.</p>}
          {area ? (
            <div className="area-drawer" id="area-drawer">
              <div className="area-drawer-head">
                <h3>
                  {area.name || 'Repository root'} <code>{area.name ? `${area.name}/` : ''}</code>
                </h3>
                <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => setPicked(null)}>
                  Close
                </button>
              </div>
              <div className="area-cols">
                <section>
                  <h4>Editing now</h4>
                  {area.holders.length === 0 ? (
                    <p className="muted small">No live agent holds files here.</p>
                  ) : (
                    <ul className="ad-list">
                      {area.holders.map((h) => (
                        <li key={h.id} className="ad-item">
                          <Avatar name={h.name} size={26} />
                          <div>
                            <div className="ad-main">
                              {h.name} <span className="muted small">{h.owner}</span>
                            </div>
                            <div className="ad-meta">{h.files.slice(0, 4).map((f) => <Path key={f}>{f}</Path>)}{h.files.length > 4 && <span>+{h.files.length - 4} more</span>}</div>
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
                <section>
                  <h4>Tasks planned here</h4>
                  {area.planned.length === 0 ? (
                    <p className="muted small">No unfinished task declared scope here.</p>
                  ) : (
                    <ul className="ad-list">
                      {area.planned.map(({ item, paths }) => (
                        <li key={item.id} className="ad-item">
                          <Avatar name={item.lead_agent} size={26} />
                          <div>
                            <a className="ad-main work-link" href={workHref(item.id)}>{item.title}</a>
                            <div className="ad-meta">
                              {item.lead_agent} · {paths.slice(0, 3).map((f) => <Path key={f}>{f}</Path>)}
                              {paths.length > 3 && <span>+{paths.length - 3} more</span>}
                            </div>
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
                <section>
                  <h4>Crossings</h4>
                  {area.crossings === 0 ? (
                    <p className="muted small">None here.</p>
                  ) : (
                    <p className="small">
                      {plural(area.crossings, 'crossing')}. <a href="#box-contested">Contested files</a> are two agents holding one path; <a href="#inbox">overlaps in the Inbox</a> are two tasks planning the same one.
                    </p>
                  )}
                </section>
              </div>
            </div>
          ) : (
            <p className="map-hint">
              <Icon name="hand-index" /> Pick an area to see who is editing there, which tasks plan to, and any crossings.
            </p>
          )}
        </>
      )}
    </Box>
  )
}
