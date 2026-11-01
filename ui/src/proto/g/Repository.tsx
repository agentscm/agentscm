// G · Repository: the code as a map of components joined by what they use. A lens recolours every tile at once;
// the inspector shows one component's facts, its live work, and (with History on) its evidence trail.
import { useState, type CSSProperties } from 'react'
import { Avatar, Icon, PageHead, Pill } from '../../ui.tsx'
import { Go, morph } from '../kit.tsx'
import { COMPONENTS, G_LENSES, type GLens } from './gdata.ts'
import { useStore } from './store.tsx'

const TILE_W = 188, TILE_H = 128, GX = 34, GY = 44

export function RepositoryPage() {
  const st = useStore()
  const [lens, setLens] = useState<GLens>('work')
  const [sel, setSel] = useState('billing')
  const [history, setHistory] = useState(false)
  const c = COMPONENTS.find((x) => x.id === sel)
  const meta = G_LENSES.find((l) => l.id === lens)
  const pos = (id: string) => {
    const x = COMPONENTS.find((k) => k.id === id)
    return x ? { x: x.col * (TILE_W + GX), y: x.row * (TILE_H + GY) } : { x: 0, y: 0 }
  }
  const width = 3 * TILE_W + 2 * GX
  const height = 2 * TILE_H + GY
  return (
    <>
      <PageHead question="What do we know about this code?" answer={<>The code, with its context.<br /><span className="g-quiet">Explore one part at a time.</span></>} fact="acme/widgets · main · six components in this sample. Source, work and evidence are separate layers." />
      <div className="g-repo-bar">
        <div className="g-filters" role="group" aria-label="Lens">
          {G_LENSES.map((l) => <button key={l.id} type="button" aria-pressed={lens === l.id} className={lens === l.id ? 'on' : ''} onClick={() => morph(() => setLens(l.id))}><Icon name={l.icon} /> {l.label}</button>)}
        </div>
        <label className="g-switch"><input type="checkbox" checked={history} onChange={(e) => { const v = e.target.checked; morph(() => setHistory(v)) }} /><span aria-hidden="true" />History</label>
      </div>
      {meta && <p className="g-lens-note"><Icon name="info-circle" /> {meta.note}</p>}
      <div className="g-repo">
        <div className="g-map-wrap">
          <div className="g-map" style={{ width, height }}>
            <svg width={width} height={height} aria-hidden="true">
              {COMPONENTS.flatMap((a) => a.uses.map((u) => {
                const p = pos(a.id), q = pos(u)
                const x1 = p.x + TILE_W / 2, y1 = p.y + TILE_H / 2, x2 = q.x + TILE_W / 2, y2 = q.y + TILE_H / 2
                const on = sel === a.id || sel === u
                return <path key={`${a.id}-${u}`} className={`g-map-edge${on ? ' on' : ''}`} d={`M${x1} ${y1} C${(x1 + x2) / 2} ${y1} ${(x1 + x2) / 2} ${y2} ${x2} ${y2}`} />
              }))}
            </svg>
            {COMPONENTS.map((k, i) => {
              const p = pos(k.id)
              const f = k.facts[lens]
              const faces = k.work.map(st.work).flatMap((w) => (w ? w.agents.slice(0, 1) : []))
              return (
                <button key={k.id} type="button" className={`g-comp tone-${f.tone}${sel === k.id ? ' sel' : ''}`} style={{ left: p.x, top: p.y, width: TILE_W, height: TILE_H, '--i': i } as CSSProperties} aria-pressed={sel === k.id} onClick={() => morph(() => setSel(k.id))}>
                  <span className="g-comp-head"><span className="g-comp-icon"><Icon name={k.icon} /></span><span><b>{k.name}</b><code>{k.path}</code></span></span>
                  <span className="g-comp-foot">
                    <span className="px-status"><span className={`px-dot tone-${f.tone}`} aria-hidden="true" />{f.text}</span>
                    <span className="g-comp-faces">{faces.map((a, j) => <Avatar key={`${a.who}${a.tool}${j}`} name={`${a.who}-${a.tool}`} size={22} />)}</span>
                  </span>
                  {history && <span className="g-comp-hist" aria-hidden="true">{k.history.slice(0, 4).map((h) => <span key={h.text} className={`g-hist-dot tone-${h.tone}`} />)}</span>}
                </button>
              )
            })}
          </div>
        </div>
        {c && <aside className="g-inspector repo" aria-label={c.name}>
          <div key={c.id} className="g-inspector-in">
            <span className="g-ring big tone-brand"><Icon name={c.icon} /></span>
            <h2>{c.name}</h2>
            <p className="small muted">{c.desc} · <code>{c.path}</code></p>
            <dl>
              {G_LENSES.map((l) => <div key={l.id} className={lens === l.id ? 'on' : ''}><dt><Icon name={l.icon} /> {l.label}</dt><dd><Pill tone={c.facts[l.id].tone}>{c.facts[l.id].text}</Pill></dd></div>)}
              <div><dt><Icon name="person" /> Owner</dt><dd>{c.owner}</dd></div>
              <div><dt><Icon name="files" /> Files</dt><dd>{c.files} in this sample</dd></div>
            </dl>
            {c.work.length > 0 && <>
              <span className="g-k">Changes here now</span>
              <ul className="g-links">{c.work.map(st.work).map((w) => w && <li key={w.id}><span className={`px-dot tone-${w.tone}`} /><span><Go to={`work/${w.id}`} className="px-link-quiet"><b>{w.title}</b></Go><small>{w.state}</small></span></li>)}</ul>
            </>}
            {history && <>
              <span className="g-k">Evidence trail</span>
              <ol className="g-timeline">{c.history.map((h) => <li key={h.text}><span className={`g-tl-dot tone-${h.tone}`} aria-hidden="true" /><span className="g-tl-text"><Icon name={h.icon} /> {h.text}</span><span className="g-tl-at">{h.at}</span></li>)}</ol>
            </>}
            {!history && <button type="button" className="px-link" onClick={() => morph(() => setHistory(true))}><Icon name="clock-history" /> Show its history</button>}
          </div>
        </aside>}
      </div>
      <p className="small muted g-native-note"><Icon name="box" /> Native revisions and snapshots join this map as a Revisions layer when native source control is turned on in Setup.</p>
    </>
  )
}
