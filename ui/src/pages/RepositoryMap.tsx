import { useEffect, useMemo, useState } from 'react'
import { fetchExplorerFile, type ExplorerFile } from '../repoExplorer.ts'
import { component, fetchRepositoryMap, mapChanges, relationChanges, type MapSnapshot, type RepositoryMap as MapData } from '../repositoryMap.ts'
import { Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'
import '../repository-map.css'

type Read<T> = { kind: 'loading' } | { kind: 'error'; message: string } | { kind: 'ready'; value: T }
function Source({ repository, commit, path }: { repository: string; commit: string; path: string }) {
  const [read, setRead] = useState<Read<ExplorerFile>>({ kind: 'loading' })
  useEffect(() => {
    const controller = new AbortController()
    void fetchExplorerFile(commit, path, controller.signal, repository).then(value => { if (!controller.signal.aborted) setRead({ kind: 'ready', value }) }, (e: unknown) => { if (!controller.signal.aborted) setRead({ kind: 'error', message: e instanceof Error ? e.message : 'Source unavailable.' }) })
    return () => controller.abort()
  }, [repository, commit, path])
  return <section className="repository-map-source" aria-label="Map source"><h4>{path}</h4><p className="small muted">Committed source · <code>{commit}</code></p>{read.kind === 'loading' ? <Skeleton rows={3} /> : read.kind === 'error' ? <Notice tone="warn" title="Source unavailable">{read.message}</Notice> : read.value.content !== null ? <><pre className="explorer-code"><code>{read.value.content.slice(0, 65536)}</code></pre>{read.value.content.length > 65536 && <p>Source preview shortened to 65,536 characters.</p>}</> : <p>Source preview: {read.value.state.replaceAll('_', ' ')}.</p>}</section>
}

function Connections({ snapshot, scope, selected, onSelect }: { snapshot: MapSnapshot; scope: string; selected: string; onSelect: (name: string) => void }) {
  const groups = useMemo(() => {
    const groups = new Map<string, number>()
    snapshot.files.forEach(f => { const name = component(f.path, scope); groups.set(name, (groups.get(name) ?? 0) + 1) })
    return [...groups].sort(([a], [b]) => a.localeCompare(b))
  }, [snapshot, scope])
  const edges = useMemo(() => {
    const edges = new Map<string, { from: string; to: string; count: number }>()
    snapshot.relations.forEach(e => {
      const from = component(snapshot.files[e.from]!.path, scope), to = component(snapshot.files[e.to]!.path, scope)
      if (from === to) return
      const key = `${from}\0${to}`, current = edges.get(key)
      edges.set(key, { from, to, count: (current?.count ?? 0) + 1 })
    })
    return [...edges.values()]
  }, [snapshot, scope])
  const visible = groups.slice(0, 18)
  const positions = new Map(visible.map(([name], i) => [name, { x: 24 + (i % 3) * 240, y: 28 + Math.floor(i / 3) * 112 }]))
  return <><div className="repository-map-canvas" aria-label="Component connections">
    <svg viewBox={`0 0 744 ${Math.max(150, Math.ceil(visible.length / 3) * 112 + 28)}`} role="group" aria-label="Static reference candidates between directories">
      <defs><marker id="repository-map-arrow" markerWidth="7" markerHeight="7" refX="6" refY="3.5" orient="auto"><path d="M0,0 L7,3.5 L0,7" /></marker></defs>
      {edges.map(e => {
        const a = positions.get(e.from), b = positions.get(e.to)
        if (!a || !b) return null
        const dx = b.x - a.x, dy = b.y - a.y
        // Clip the line to just outside the 208 × 66 node rectangles.
        const scale = 1 / Math.max(Math.abs(dx) / 110, Math.abs(dy) / 39)
        return <path key={`${e.from}|${e.to}`} className={`repository-map-wire${selected === e.from || selected === e.to ? ' is-selected' : ''}`} d={`M${a.x + 104 + dx * scale},${a.y + 33 + dy * scale} L${b.x + 104 - dx * scale},${b.y + 33 - dy * scale}`} markerEnd="url(#repository-map-arrow)" />
      })}
      {visible.map(([name, count]) => { const p = positions.get(name)!; return <g key={name} role="button" tabIndex={0} aria-label={`Select component ${name}`} aria-pressed={selected === name} onClick={() => onSelect(name)} onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelect(name) } }} className={`repository-map-node${selected === name ? ' is-selected' : ''}`} transform={`translate(${p.x},${p.y})`}><rect width="208" height="66" rx="14" /><text x="14" y="28">{name.length > 23 ? `…${name.slice(-22)}` : name}</text><text x="14" y="48" className="repository-map-node-count">{count} shown {count === 1 ? 'file' : 'files'}</text></g> })}
    </svg>
  </div><div className="repository-map-components" aria-label="Choose a component">{groups.map(([name, count]) => <button key={name} type="button" className="btn btn-sm btn-outline-secondary" aria-pressed={selected === name} onClick={() => onSelect(name)}><Icon name="folder2" /> {name} <span className="muted">{count}</span></button>)}</div>{groups.length > visible.length && <p className="small muted">The diagram shows 18 components. Every returned component is available in the buttons above.</p>}</>
}

function Loaded({ data, repository, onScope, onExplain }: { data: MapData; repository: string; onScope: (scope: string) => void; onExplain: () => void }) {
  const [side, setSide] = useState<'head' | 'base'>('head')
  const [selected, setSelected] = useState('')
  const [source, setSource] = useState<string>()
  const [query, setQuery] = useState('')
  const snapshot = side === 'base' && data.base ? data.base : data.head
  const files = snapshot.files.filter(f => (!selected || component(f.path, data.scope) === selected) && f.path.toLocaleLowerCase().includes(query.toLocaleLowerCase()))
  const relations = snapshot.relations.filter(e => !selected || component(snapshot.files[e.from]!.path, data.scope) === selected || component(snapshot.files[e.to]!.path, data.scope) === selected)
  const gaps = snapshot.files.filter(f => f.inspection !== 'inspected').length
  const changes = data.base ? mapChanges(data.base, data.head) : []
  const changedRelations = data.base ? relationChanges(data.base, data.head) : []
  const c = snapshot.coverage
  return <div className="repository-map">
    <div className="repository-map-heading"><div><h3>{data.base ? 'How the connections changed' : 'How this source connects'}</h3><p>Explore directories, follow static references, and open the source behind each connection.</p></div><button type="button" className="btn btn-outline-secondary" onClick={onExplain}>Explanations &amp; evidence <Icon name="arrow-right" /></button></div>
    <div className="repository-map-controls"><nav aria-label="Map scope"><button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => onScope('')}>Repository</button>{data.scope && <><Icon name="chevron-right" /><code>{data.scope}</code><button type="button" className="btn btn-sm btn-link" onClick={() => onScope(data.scope.split('/').slice(0, -1).join('/'))}>Up one level</button></>}</nav>{data.base && <div role="group" aria-label="Map revision">{(['base', 'head'] as const).map(value => <button key={value} type="button" className="btn btn-sm btn-outline-secondary" aria-pressed={side === value} onClick={() => { setSide(value); setSource(undefined) }}>{value === 'base' ? 'Before' : 'After'} · {(value === 'base' ? data.base!.commit : data.head.commit).slice(0, 8)}</button>)}</div>}</div>
    <p className="small muted">Committed snapshot <code>{snapshot.commit.slice(0, 12)}</code> · {snapshot.files.length} of {c.discovered_files}{!c.tree_complete ? '+' : ''} files shown · {c.inspected_files} blob reads attempted. Working-tree edits are excluded.</p>
    {(c.omitted_files > 0 || !c.tree_complete || gaps > 0 || c.omitted_relations > 0 || c.omitted_unresolved > 0) && <details className="repository-map-coverage"><summary>Coverage: {gaps} shown files without inspected references{c.omitted_files > 0 ? `; ${c.omitted_files} files omitted` : ''}</summary><p>{!c.tree_complete ? 'Tree enumeration stopped at its limit. Totals are lower bounds. ' : ''}Rust module/use and JavaScript/TypeScript import candidates only. Runtime calls, conditional compilation, aliases, generated code and unsupported syntax are not resolved. {c.omitted_relations} reference candidates and {c.omitted_unresolved} unresolved entries omitted. Choose a smaller directory to inspect more of it.</p></details>}
    {snapshot.files.length === 0 ? <Empty icon="diagram-3" title="No committed files in this scope">Choose Repository to return to the full source view.</Empty> : <Connections snapshot={snapshot} scope={data.scope} selected={selected} onSelect={name => { setSelected(name === selected ? '' : name); setSource(undefined) }} />}
    <div className="repository-map-detail-head"><h4>{selected || 'Files in this view'}</h4>{selected && selected !== '(root files)' && selected !== data.scope && <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => onScope(selected)}>Explore this component</button>}<label><span className="visually-hidden">Filter map files</span><input className="form-control form-control-sm" value={query} onChange={e => setQuery(e.target.value)} placeholder="Find a file" /></label></div>
    <ul className="repository-map-files">{files.map(f => <li key={f.path}><button type="button" onClick={() => setSource(f.path)}><Icon name="file-earmark-code" /><span>{f.path}</span></button><span className="small muted">{f.inspection === 'inspected' ? 'References inspected' : f.inspection.replaceAll('_', ' ')}</span>{changes.some(c => c.path === f.path) && <Pill tone="muted">{changes.find(c => c.path === f.path)!.state === 'Outside the other snapshot’s coverage' ? 'Comparison incomplete' : 'Source changed'}</Pill>}</li>)}</ul>
    {snapshot.files.length > 0 && files.length === 0 && <div role="status"><p>{selected && !snapshot.files.some(f => component(f.path, data.scope) === selected) ? 'This component has no returned files in this revision.' : 'No files match this filter in the selected component.'}</p><button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => { setQuery(''); setSelected(''); setSource(undefined) }}>Show all returned files</button></div>}
    {source && <Source key={`${snapshot.commit}|${source}`} repository={repository} commit={snapshot.commit} path={source} />}
    <details open={!!selected}><summary>{relations.length} static reference candidates{selected ? ' touching this component' : ' in this view'}</summary><p className="small muted">These are source navigation hints, not a complete dependency or runtime call graph.</p><ul className="repository-map-relations">{relations.map((e, i) => <li key={i}><button type="button" onClick={() => setSource(snapshot.files[e.from]!.path)}>{snapshot.files[e.from]!.path}:{e.line}</button><Icon name="arrow-right" /><button type="button" onClick={() => setSource(snapshot.files[e.to]!.path)}>{snapshot.files[e.to]!.path}</button><span className="small muted">{e.kind.replace('_candidate', '').replaceAll('_', ' ')}</span></li>)}</ul>{relations.length === 0 && <p>No candidates in this returned slice. Missing connections do not establish independence.</p>}</details>
    {(snapshot.unresolved.length > 0 || c.omitted_unresolved > 0) && <details><summary>{snapshot.unresolved.length}{c.omitted_unresolved ? '+' : ''} references need more context</summary><ul>{snapshot.unresolved.map((e, i) => <li key={i}><code>{snapshot.files[e.file]!.path}:{e.line}</code> → <code>{e.reference}</code> · {e.reason.replaceAll('_', ' ')}</li>)}</ul></details>}
    {data.base && <section className="repository-map-change" aria-label="Map change history"><h4>Between these revisions</h4><p className="small muted">A source change remains visible even when its references stay the same. Renames appear as removal and addition; explanations are not automatically revalidated.</p>{changes.length === 0 ? <p>No changed blobs among the files present on both sides.</p> : <ul>{changes.map(c => <li key={c.path}><Pill tone="muted">{c.state}</Pill> <code>{c.path}</code></li>)}</ul>}{changedRelations.length > 0 && <><h5>Reference observations</h5><ul>{changedRelations.map((e, i) => <li key={i}>{e.state}: <code>{e.from}</code> → <code>{e.to}</code></li>)}</ul><p className="small muted">Incomplete comparisons do not assert that a dependency was added or removed.</p></>}</section>}
    <details><summary>Source and extraction details</summary><dl><dt>Tree</dt><dd><code>{snapshot.tree}</code></dd><dt>Extractor</dt><dd>{data.extractor}</dd><dt>Basis</dt><dd>Directory groups are path-derived. Connections are lexical candidates from committed blobs. No source code or check was executed to produce this map.</dd></dl></details>
  </div>
}

export function RepositoryMap({ repository, commit, base, scope, onScope, onExplain, onHeadOnly }: { repository: string; commit: string; base?: string; scope: string; onScope: (scope: string) => void; onExplain: () => void; onHeadOnly?: () => void }) {
  const key = `${repository}|${commit}|${base ?? ''}|${scope}`
  const [state, setState] = useState<{ key: string; read: Read<MapData> }>()
  useEffect(() => {
    const controller = new AbortController()
    void fetchRepositoryMap(repository, commit, base, scope, controller.signal).then(value => { if (!controller.signal.aborted) setState({ key, read: { kind: 'ready', value } }) }, (e: unknown) => { if (!controller.signal.aborted) setState({ key, read: { kind: 'error', message: e instanceof Error ? e.message : 'The map read failed.' } }) })
    return () => controller.abort()
  }, [key, repository, commit, base, scope])
  const read = state?.key === key ? state.read : { kind: 'loading' as const }
  return read.kind === 'loading' ? <Skeleton rows={8} /> : read.kind === 'error' ? <Notice tone="warn" title="Could not read this map">{read.message} Choose a smaller source directory or refresh the repository observation.{base && onHeadOnly && <><p>If the comparison parent is unavailable, inspect this commit without a comparison.</p><button type="button" className="btn btn-sm btn-outline-secondary" onClick={onHeadOnly}>Map this commit only</button></>}</Notice> : <Loaded key={key} data={read.value} repository={repository} onScope={onScope} onExplain={onExplain} />
}
