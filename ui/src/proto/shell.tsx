// The prototype frame: route state, the variation/role switcher, the five comparison tasks and ⌘K search.
import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react'
import { loadTheme, saveTheme } from '../storage.ts'
import { Icon } from '../ui.tsx'
import logoDark from '../logo-dark.png'
import { FLOWS, NEEDS, ROLES, TASKS, WORK, needsFor, type Role } from './data.ts'
import { Go, ProtoCtx, morph, parseRoute, useProto, type Route, type Variant } from './kit.tsx'
import { VariantA } from './VariantA.tsx'
import { VariantB } from './VariantB.tsx'
import { VariantC } from './VariantC.tsx'
import { VariantD, VariantE, VariantF } from './ab.tsx'
import { VariantG } from './g/GApp.tsx'
import './proto.css'

const VARIANTS: { id: Variant; name: string; idea: string; early?: boolean }[] = [
  { id: 'g', name: 'G · Flagship', idea: 'Every option combined, with live decisions and motion' },
  { id: 'd', name: '1', early: true, idea: 'A+B: every change as a lane; the process is the column header' },
  { id: 'e', name: '2', early: true, idea: 'A+B: the stages as map tiles with the faces working there' },
  { id: 'f', name: '3', early: true, idea: 'A+B: pick a change; its whole journey stands beside the list' },
  { id: 'a', name: 'A', idea: 'Earlier: control tower', early: true },
  { id: 'b', name: 'B', idea: 'Earlier: process canvas', early: true },
  { id: 'c', name: 'C', idea: 'Earlier: quiet', early: true },
]

const read = <T,>(key: string, fallback: T): T => {
  try {
    const v = localStorage.getItem(key)
    return v ? (JSON.parse(v) as T) : fallback
  } catch {
    return fallback
  }
}
const write = (key: string, v: unknown) => {
  try {
    localStorage.setItem(key, JSON.stringify(v))
  } catch {
    // Private windows can refuse storage; the prototype still works for this visit.
  }
}

export function ProtoApp() {
  const [route, setRoute] = useState<Route>(() => parseRoute(window.location.hash.replace(/^#proto\/?/, '')))
  const [role, setRole] = useState<Role>(() => read<Role>('px-role', 'author'))
  const [resolved, setResolved] = useState<Set<string>>(() => new Set())
  const [dark, setDark] = useState(() => {
    const p = loadTheme()
    return p === 'dark' || (p === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches)
  })
  useEffect(() => {
    document.documentElement.dataset['bsTheme'] = dark ? 'dark' : 'light'
  }, [dark])
  useEffect(() => {
    const on = () => setRoute(parseRoute(window.location.hash.replace(/^#proto\/?/, '')))
    window.addEventListener('popstate', on)
    window.addEventListener('hashchange', on)
    return () => {
      window.removeEventListener('popstate', on)
      window.removeEventListener('hashchange', on)
    }
  }, [])
  const page = route.parts[0]
  useEffect(() => {
    window.scrollTo({ top: 0 })
  }, [page, route.v])

  const go = useCallback((path: string) => {
    morph(() => {
      const next = parseRoute(`${route.v}/${path}`)
      window.history.pushState(null, '', `#proto/${next.v}/${next.parts.join('/')}`)
      setRoute(next)
    })
  }, [route.v])
  const ctx = useMemo(() => ({
    route, go, role,
    resolved,
    resolve: (id: string) => morph(() => setResolved((s) => new Set(s).add(id))),
  }), [route, go, role, resolved])

  const View = { a: VariantA, b: VariantB, c: VariantC, d: VariantD, e: VariantE, f: VariantF, g: VariantG }[route.v]
  return (
    <ProtoCtx.Provider value={ctx}>
      <div className={`px-root px-v-${route.v}`}>
        <View />
        <Palette />
        <ProtoBar route={route} role={role} onRole={(r) => { setRole(r); write('px-role', r) }} dark={dark} onDark={(d) => { setDark(d); saveTheme(d ? 'dark' : 'light') }} />
      </div>
    </ProtoCtx.Provider>
  )
}

function ProtoBar({ route, role, onRole, dark, onDark }: { route: Route; role: Role; onRole: (r: Role) => void; dark: boolean; onDark: (d: boolean) => void }) {
  return (
    <div className="px-bar" role="region" aria-label="Prototype controls">
      <span className="px-bar-tag"><Icon name="easel" /> Prototype · sample data</span>
      <span className="px-bar-group" role="group" aria-label="Variation">
        {VARIANTS.map((v) => <a key={v.id} href={`#proto/${v.id}/`} className={`${route.v === v.id ? 'on' : ''}${v.early ? ' early' : ''}`} title={v.idea} onClick={(e) => { e.preventDefault(); morph(() => { window.history.pushState(null, '', `#proto/${v.id}/`); window.dispatchEvent(new PopStateEvent('popstate')) }) }}>{v.name}</a>)}
      </span>
      <span className="px-bar-group" role="group" aria-label="Viewing as">
        {ROLES.map((r) => <button key={r.id} type="button" className={role === r.id ? 'on' : ''} title={r.who} onClick={() => onRole(r.id)}>{r.label}</button>)}
      </span>
      <Tasks key={route.v} v={route.v} />
      <button type="button" className="px-bar-icon" onClick={() => onDark(!dark)} aria-label={dark ? 'Use light theme' : 'Use dark theme'}><Icon name={dark ? 'sun' : 'moon-stars'} /></button>
      <a className="px-bar-icon" href="#home" title="The current Hub"><Icon name="box-arrow-up-right" /></a>
    </div>
  )
}

/** The five comparison tasks, ticked separately per variation (remounted by key when the variation changes). */
function Tasks({ v }: { v: Variant }) {
  const key = `px-tasks-${v}`
  const [open, setOpen] = useState(false)
  const [checked, setChecked] = useState<number[]>(() => read<number[]>(key, []))
  const toggle = (i: number) => {
    const next = checked.includes(i) ? checked.filter((x) => x !== i) : [...checked, i]
    setChecked(next)
    write(key, next)
  }
  return (
    <span className="px-bar-tasks">
      <button type="button" onClick={() => setOpen(!open)} aria-expanded={open}><Icon name="list-check" /> Tasks {checked.length}/{TASKS.length}</button>
      {open && <div className="px-bar-pop">
        <p className="small muted mb-2">Try the same five on each variation.</p>
        {TASKS.map((t, i) => <label key={t}><input type="checkbox" checked={checked.includes(i)} onChange={() => toggle(i)} /> {t}</label>)}
      </div>}
    </span>
  )
}

export const openPalette = () => window.dispatchEvent(new Event('px-palette'))

/** ⌘K: every destination and every change, one list. */
function Palette() {
  const { go } = useProto()
  const [open, setOpen] = useState(false)
  const [q, setQ] = useState('')
  const [at, setAt] = useState(0)
  useEffect(() => {
    const show = () => { setQ(''); setAt(0); setOpen(true) }
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        show()
      }
      if (e.key === 'Escape') setOpen(false)
    }
    window.addEventListener('px-palette', show)
    window.addEventListener('keydown', key)
    return () => {
      window.removeEventListener('px-palette', show)
      window.removeEventListener('keydown', key)
    }
  }, [])
  if (!open) return null
  const items: { to: string; icon: string; label: string; sub: string }[] = [
    { to: '', icon: 'bell', label: 'Now', sub: 'What needs me' },
    { to: 'work', icon: 'kanban', label: 'Work', sub: 'Every change by stage' },
    { to: 'workflows', icon: 'diagram-3', label: 'Workflows', sub: 'What process applies here' },
    { to: 'repository', icon: 'folder2-open', label: 'Repository', sub: 'Code, owners and evidence' },
    { to: 'value', icon: 'stars', label: 'This week’s contributions', sub: 'With their records' },
    { to: 'setup', icon: 'plug', label: 'Setup', sub: 'Connections and access' },
    ...WORK.map((w) => ({ to: `work/${w.id}`, icon: 'record-circle', label: w.title, sub: w.state })),
    ...FLOWS.map((f) => ({ to: `workflows/${f.id}`, icon: 'diagram-3', label: f.name, sub: f.state })),
  ]
  const hits = items.filter((i) => `${i.label} ${i.sub}`.toLowerCase().includes(q.toLowerCase())).slice(0, 9)
  const pick = (to: string) => { setOpen(false); go(to) }
  return (
    <div className="px-palette-scrim" onClick={() => setOpen(false)}>
      <div className="px-palette" role="dialog" aria-modal="true" aria-label="Search" onClick={(e) => e.stopPropagation()}>
        <input autoFocus className="form-control" role="combobox" aria-expanded="true" aria-controls="px-palette-list" aria-activedescendant={hits[at] ? `px-hit-${at}` : undefined} aria-autocomplete="list" placeholder="Go to a change, workflow or place…" value={q}
          onChange={(e) => { setQ(e.target.value); setAt(0) }}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') { e.preventDefault(); setAt((a) => Math.min(hits.length - 1, a + 1)) }
            if (e.key === 'ArrowUp') { e.preventDefault(); setAt((a) => Math.max(0, a - 1)) }
            const hit = hits[at]
            if (e.key === 'Enter' && hit) pick(hit.to)
          }} />
        <ul id="px-palette-list" role="listbox" aria-label="Places and changes">{hits.map((h, i) => <li key={h.to + h.label} id={`px-hit-${i}`} role="option" aria-selected={i === at} className={i === at ? 'on' : ''} onClick={() => pick(h.to)} onMouseEnter={() => setAt(i)}><Icon name={h.icon} /><span>{h.label}</span><span className="small muted">{h.sub}</span></li>)}</ul>
        {hits.length === 0 && <p className="small muted p-3 mb-0">Nothing matches “{q}”.</p>}
      </div>
    </div>
  )
}

/** The Twilight top bar every variation shares; the rail is the variation's choice. */
export function TopBar({ minimal = false, count, onMenu, menuOpen = false }: { minimal?: boolean; /** Open decisions, when the variation tracks resolutions. */ count?: number; /** Shows the menu button that opens the rail as a drawer below 960px. */ onMenu?: () => void; menuOpen?: boolean }) {
  const { role } = useProto()
  const mine = count ?? needsFor(role).length
  const all = NEEDS.length
  return (
    <header className="topbar px-topbar">
      {onMenu && <button type="button" className="burger" aria-label={menuOpen ? 'Close navigation' : 'Open navigation'} aria-expanded={menuOpen} onClick={onMenu}><Icon name={menuOpen ? 'x-lg' : 'list'} /></button>}
      <Go to="" className="topbar-logo" label="Now"><img src={logoDark} alt="AgentSCM" height={30} className="logo" /></Go>
      {!minimal && <span className="px-repo"><Icon name="journal-code" /> acme/widgets <Icon name="chevron-down" /></span>}
      <button type="button" className="search-pill" onClick={openPalette} aria-label="Search">
        <Icon name="search" /><span className="search-text">{minimal ? 'Go anywhere' : 'Search changes, workflows, files…'}</span><kbd>⌘ K</kbd>
      </button>
      <div className="topbar-right">
        {!minimal && <Go to="setup" className="health-chip"><span className="live-dot" aria-hidden="true" /><span className="health-text">Connected · 1 to fix</span></Go>}
        <Go to="inbox" className="top-icon bell" label={`Inbox, ${all} open`}><Icon name="bell" />{mine > 0 && <span className="bell-count tone-warn" aria-hidden="true">{mine}</span>}</Go>
        <span className="px-me" title="Signed in as maria">M</span>
      </div>
    </header>
  )
}

export function Sheet({ children, wide = false }: { children: ReactNode; wide?: boolean }) {
  return <div className={`px-sheet${wide ? ' wide' : ''}`}>{children}</div>
}
