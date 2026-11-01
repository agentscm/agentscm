// G · Repository over live data: one tile per top-level folder someone holds or plans to touch (the Hub's own
// `areas`), coloured by what needs attention there, with an inspector for who is in it and what they plan.
import { useState, type CSSProperties } from 'react'
import { Avatar, Box, Empty, Icon, PageHead, Pill, Path } from '../ui.tsx'
import { who } from '../nextStep.ts'
import { plural } from '../pages/workParts.tsx'
import { workHref } from '../standup.ts'
import { areas } from '../needs.ts'
import type { LiveProps } from './GLive.tsx'

export function LiveCode({ live }: { live: LiveProps }) {
  const ov = live.overview.kind === 'ready' ? live.overview.value : null
  const list = areas(ov, live.d)
  const [pick, setPick] = useState<string | null>(null)
  const a = list.find((x) => x.name === pick) ?? list[0] ?? null
  const busiest = Math.max(1, ...list.map((x) => x.holders.length + x.planned.length))
  return (
    <>
      <PageHead question="What do we know about this code?" answer={<>The code, with its context.<br /><span className="g-quiet">{list.length ? `${plural(list.length, 'area has', 'areas have')} agents in or planning to be.` : 'Nobody is in the code right now.'}</span></>} fact="From live file holds and the scope each unfinished change declared. Areas nobody holds or plans to touch are not shown; that does not mean unused." />
      <div className="g-repo">
        <div className="g-map-wrap">
          {list.length === 0 && <Empty icon="grid-1x2" title="No agent holds or plans files right now">Tiles appear as agents announce edits or declare scope.</Empty>}
          <div className="g-areas">
            {list.map((x, i) => {
              const people = x.holders.length + x.planned.length
              return (
                <button key={x.name || '(root)'} type="button" aria-pressed={a?.name === x.name} className={`tile g-area${a?.name === x.name ? ' is-selected' : ''}${x.contested ? ' is-crossing' : ''}${!x.contested && x.overlaps ? ' px-is-wait' : ''}`} style={{ '--heat': Math.min(1, people / busiest), '--i': i } as CSSProperties} onClick={() => setPick(x.name)}>
                  <span className="tile-head"><b>{x.name || 'Repository root'}</b><code>{x.name ? `${x.name}/` : '/'}</code></span>
                  <span className="tile-people">{x.holders.slice(0, 5).map((h) => <Avatar key={h.id} name={h.name} size={26} />)}{x.holders.length > 5 && <span className="tile-more">+{x.holders.length - 5}</span>}</span>
                  <span className="tile-foot g-area-foot">
                    <span>{x.holders.length} editing · {x.planned.length} planned</span>
                    {(x.contested > 0 || x.overlaps > 0) && <span className={`px-crossing${x.contested ? '' : ' wait'}`}>{x.contested ? `${x.contested} contested` : plural(x.overlaps, 'overlap')}</span>}
                  </span>
                </button>
              )
            })}
          </div>
        </div>
        {a && <aside className="g-inspector repo" aria-label={a.name || 'Repository root'}>
          <div key={a.name} className="g-inspector-in">
            <span className="g-ring big tone-brand"><Icon name="folder2-open" /></span>
            <h2>{a.name || 'Repository root'}</h2>
            <span className="g-k">Editing now</span>
            {a.holders.length === 0 && <p className="small muted">Nobody holds a file here.</p>}
            <ul className="g-links">{a.holders.map((h) => <li key={h.id}><Avatar name={h.name} size={20} /><span><b>{who(h.owner, h.name)}</b><small>{h.files.slice(0, 3).join(', ')}{h.files.length > 3 ? ` +${h.files.length - 3}` : ''}</small></span></li>)}</ul>
            <span className="g-k">Planned by unfinished changes</span>
            {a.planned.length === 0 && <p className="small muted">No change declared scope here.</p>}
            <ul className="g-links">{a.planned.map((p) => <li key={p.item.id}><Icon name="record-circle" /><span><a href={workHref(p.item.id)} className="px-link-quiet"><b>{p.item.title}</b></a><small>{p.paths.slice(0, 2).join(', ')}</small></span></li>)}</ul>
            {(a.contested > 0 || a.overlaps > 0) && <p className="small"><Pill tone={a.contested ? 'bad' : 'warn'} icon="exclamation-diamond">{a.contested ? plural(a.contested, 'contested file') : plural(a.overlaps, 'overlap')}</Pill> <a href={a.contested ? '#now' : '#inbox'} className="px-link">{a.contested ? 'Held files' : 'Inbox'}</a></p>}
            <div className="g-acts"><a href="#explorer" className="btn btn-sm btn-outline-secondary"><Icon name="folder2-open" /> Files and history</a><a href="#now" className="btn btn-sm btn-outline-secondary"><Icon name="lock" /> Held files</a></div>
          </div>
        </aside>}
      </div>
      <Box icon="info-circle" title="Not on this map yet" tone="muted">
        <p className="small mb-0 g-measure">Review coverage, scan results and ownership per folder are not recorded by the Hub today. History, diffs and evidence for a file live in <a href="#explorer">Files</a> (this computer), committed promises in <a href="#promises">Promises</a>, and native snapshots in <a href="#timeline">Timeline</a>. A path like <Path>src/billing/totals.rs</Path> opens there.</p>
      </Box>
    </>
  )
}
