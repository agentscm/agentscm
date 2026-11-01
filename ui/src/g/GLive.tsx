// The G layout over live Hub data. Its rail groups every Hub page under Now · Work · Workflows · Repository and the
// utilities; G-only pages (Now, Changes, a change, Process, Code map, Contributions, Feature map) render here, and
// every other route renders the Hub's own page, unchanged, inside this frame.
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Icon } from '../ui.tsx'
import logoDark from '../logo-dark.png'
import type { Derived } from '../derive.ts'
import type { HubData } from '../useHub.ts'
import type { Poll } from '../usePoll.ts'
import type { WorkOverview } from '../standup.ts'
import type { Need } from '../needs.ts'
import type { Handoff } from '../launchLink.ts'
import type { Section } from '../App.tsx'
import { gRoute, useHash, type GPage } from './layout.ts'
import { LiveNow } from './LiveNow.tsx'
import { LiveChanges, LiveChange } from './LiveChanges.tsx'
import { LiveProcess } from './LiveProcess.tsx'
import { LiveCode } from './LiveCode.tsx'
import { LiveContributions, LiveFeatures } from './LiveMore.tsx'
import '../proto/proto.css'
import '../proto/g/g.css'
import './live.css'

export interface LiveProps {
  token: string
  repo: string
  data: HubData
  d: Derived | null
  overview: Poll<WorkOverview>
  needs: Need[]
  handoff: Handoff
}

interface Item { label: string; href: string; icon: string; on: (s: Section, g: GPage | null) => boolean; q?: string; badge?: number; badgeTone?: 'warn' | 'bad'; dot?: boolean }
interface Group extends Item { subs: Item[] }

export function GLive({ live, section, page, showAdmin, hubSignedOut, onClassic, banner }: { live: LiveProps; section: Section; page: (s: Section) => ReactNode; showAdmin: boolean; hubSignedOut: boolean; onClassic: () => void; banner?: ReactNode }) {
  const hash = useHash()
  const g = hubSignedOut ? null : gRoute(hash)
  const cur = g?.page ?? null
  const [drawer, setDrawer] = useState(false)
  const [palette, setPalette] = useState(false)
  const { data, overview, needs } = live
  const ov = overview.kind === 'ready' ? overview.value : null
  const overlaps = ov?.overlaps.length ?? 0
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        setPalette(true)
      }
      if (e.key === 'Escape') {
        setDrawer(false)
        setPalette(false)
      }
    }
    window.addEventListener('keydown', key)
    return () => window.removeEventListener('keydown', key)
  }, [])
  useEffect(() => {
    window.scrollTo({ top: 0 })
  }, [cur, section])

  const s = (...xs: Section[]) => (sec: Section, gp: GPage | null) => gp === null && xs.includes(sec)
  const gp = (...xs: GPage[]) => (_: Section, p: GPage | null) => p !== null && xs.includes(p)
  const groups: Group[] = [
    { label: 'Now', href: '#home', icon: 'sun', q: 'What needs me?', badge: needs.length, badgeTone: needs.some((n) => n.tone === 'bad') ? 'bad' : 'warn', on: (a, b) => gp('now')(a, b) || s('activity', 'team', 'home')(a, b), subs: [
      { label: 'Needs you', href: '#home', icon: 'bell', on: gp('now') },
      { label: 'Catch up', href: '#activity', icon: 'clock-history', on: s('activity') },
      { label: 'Team', href: '#team', icon: 'people', on: s('team') },
    ] },
    { label: 'Work', href: '#changes', icon: 'layers', q: 'Where is each change?', on: (a, b) => gp('changes', 'change')(a, b) || s('work', 'change-record', 'landed')(a, b), subs: [
      { label: 'Changes', href: '#changes', icon: 'kanban', on: gp('changes', 'change') },
      { label: 'Finished and history', href: '#work/finished', icon: 'flag', on: s('work') },
      { label: 'Change records', href: '#change-record', icon: 'clipboard2-check', on: s('change-record') },
      { label: 'Landed', href: '#landed', icon: 'rocket-takeoff', on: s('landed') },
    ] },
    { label: 'Workflows', href: '#process', icon: 'diagram-3', q: 'What process applies?', on: (a, b) => gp('process')(a, b) || s('skills', 'workflows', 'templates')(a, b), subs: [
      { label: 'Process', href: '#process', icon: 'diagram-3', on: gp('process') },
      { label: 'Skills', href: '#skills', icon: 'journal-check', on: s('skills') },
      { label: 'Runs', href: '#workflows', icon: 'play-circle', on: s('workflows') },
      { label: 'Library', href: '#templates', icon: 'journal-code', on: s('templates') },
    ] },
    { label: 'Repository', href: '#code', icon: 'box', q: 'What do we know about this code?', on: (a, b) => gp('code')(a, b) || s('now', 'explorer', 'timeline', 'promises')(a, b), subs: [
      { label: 'Code map', href: '#code', icon: 'grid-1x2', on: gp('code') },
      { label: 'Held files', href: '#now', icon: 'lock', on: s('now') },
      { label: 'Files', href: '#explorer', icon: 'folder2-open', on: s('explorer') },
      { label: 'Timeline', href: '#timeline', icon: 'camera', on: s('timeline') },
      { label: 'Promises', href: '#promises', icon: 'journal-check', on: s('promises') },
    ] },
  ]
  const setupSubs: Item[] = [
    { label: 'Agents', href: '#setup', icon: 'robot', on: s('setup') },
    { label: 'Computers', href: '#computers', icon: 'laptop', on: s('computers') },
    { label: 'Access', href: '#workspace', icon: 'people', on: s('workspace') },
    { label: 'Hub', href: '#health', icon: 'heart-pulse', on: s('health') },
    ...(showAdmin ? [{ label: 'Admin', href: '#admin', icon: 'shield-lock', on: s('admin') }] : []),
    { label: 'Preferences', href: '#settings', icon: 'sliders', on: s('settings') },
  ]
  const utils: Group[] = [
    { label: 'Inbox', href: '#inbox', icon: 'inbox', badge: overlaps, badgeTone: 'warn', on: s('inbox'), subs: [] },
    { label: 'Setup', href: '#setup', icon: 'plug', dot: data.failing || data.health?.status === 'unavailable', on: s('setup', 'computers', 'workspace', 'health', 'admin', 'settings'), subs: setupSubs },
    { label: 'Contributions', href: '#contributions', icon: 'stars', on: gp('contributions'), subs: [] },
    { label: 'Feature map', href: '#features', icon: 'compass', on: gp('features'), subs: [] },
    { label: 'All repositories', href: '#repositories', icon: 'collection', on: s('repositories'), subs: [] },
  ]

  return (
    <div className={`app-shell g-shell gl-shell${hubSignedOut ? ' hub-signed-out' : ''}`}>
      <header className="topbar px-topbar">
        {!hubSignedOut && <button type="button" className="burger" aria-label={drawer ? 'Close navigation' : 'Open navigation'} aria-expanded={drawer} onClick={() => setDrawer(!drawer)}><Icon name={drawer ? 'x-lg' : 'list'} /></button>}
        <a href="#home" className="topbar-logo" aria-label="AgentSCM Hub, Now"><img src={logoDark} alt="AgentSCM" height={30} className="logo" /></a>
        <a href="#repositories" className="px-repo g-repo-link"><Icon name="journal-code" /> <span className="g-repo-name">{live.repo}</span> <Icon name="chevron-down" /></a>
        {!hubSignedOut && <button type="button" className="search-pill" onClick={() => setPalette(true)} aria-label="Search the Hub">
          <Icon name="search" /><span className="search-text">Search pages, changes and paths…</span><kbd>⌘ K</kbd>
        </button>}
        <div className="topbar-right">
          <a href="#health" className={`health-chip${data.failing || data.health?.status === 'unavailable' ? ' is-bad' : ''}`}><span className="live-dot" aria-hidden="true" /><span className="health-text">{data.failing ? 'Reconnecting' : data.health ? (data.health.status === 'ok' ? 'Hub live' : 'Degraded') : '…'}</span></a>
          <a href="#inbox" className="top-icon bell" aria-label={`Inbox, ${overlaps} overlap${overlaps === 1 ? '' : 's'}`}><Icon name="bell" />{overlaps > 0 && <span className="bell-count tone-warn" aria-hidden="true">{overlaps}</span>}</a>
          <button type="button" className="g-classic" onClick={onClassic} title="Switch back to the classic Hub layout"><Icon name="layout-sidebar" /> <span>Classic layout</span></button>
        </div>
      </header>
      {drawer && <div className="drawer-scrim" onClick={() => setDrawer(false)} aria-hidden="true" />}
      <aside className={`side${drawer ? ' is-open' : ''}`} aria-label="Hub sections" hidden={hubSignedOut}>
        <nav className="side-nav" onClick={(e) => { if ((e.target as HTMLElement).closest('a')) setDrawer(false) }}>
          <div className="side-group">{groups.map((n) => <RailGroup key={n.label} n={n} section={section} g={cur} />)}</div>
          <div className="side-group g-util">{utils.map((n) => <RailGroup key={n.label} n={n} section={section} g={cur} />)}</div>
          {/* phones have no room in the top bar for the switch, so it moves into the drawer */}
          <button type="button" className="side-link g-classic-rail" onClick={onClassic}><span className="side-icon"><Icon name="layout-sidebar" /></span><span className="side-text"><span className="side-label">Classic layout</span></span></button>
        </nav>
        <div className="side-foot"><p className="side-foot-note"><Icon name="eye" /> Reading changes nothing. Actions say what they change.</p></div>
      </aside>
      <div className="content">
        <main className="main g-main" id="main" tabIndex={-1} key={cur ?? section}>
          {banner}
          {cur === 'now' && <LiveNow live={live} />}
          {cur === 'changes' && <LiveChanges live={live} />}
          {cur === 'change' && g?.id && <LiveChange id={g.id} live={live} />}
          {cur === 'process' && <LiveProcess live={live} />}
          {cur === 'code' && <LiveCode live={live} />}
          {cur === 'contributions' && <LiveContributions live={live} />}
          {cur === 'features' && <LiveFeatures />}
          {cur === null && <div className="g-classic-page">{page(section)}</div>}
        </main>
      </div>
      {palette && <LivePalette live={live} onClose={() => setPalette(false)} />}
    </div>
  )
}

function RailGroup({ n, section, g }: { n: Group; section: Section; g: GPage | null }) {
  const on = n.on(section, g)
  return (
    <div className={`g-railgroup${on ? ' on' : ''}`}>
      <a href={n.href} className={`side-link${on ? ' active' : ''}`} aria-current={on && !n.subs.some((x) => x.on(section, g)) ? 'page' : undefined}>
        {on && <span className="g-pill" aria-hidden="true" />}
        <span className="side-icon"><Icon name={n.icon} /></span>
        <span className="side-text"><span className="side-label">{n.label}</span>{n.q && <span className="side-q">{n.q}</span>}</span>
        {!!n.badge && <span key={n.badge} className={`side-badge tone-${n.badgeTone ?? 'warn'} g-badge`}>{n.badge}</span>}
        {n.dot && <span className="g-util-dot" aria-label="Needs attention" />}
      </a>
      {on && n.subs.length > 1 && <ul className="g-subs">{n.subs.map((x) => {
        const active = x.on(section, g)
        return <li key={x.href + x.label}><a href={x.href} className={active ? 'on' : ''} aria-current={active ? 'page' : undefined}><Icon name={x.icon} /> {x.label}</a></li>
      })}</ul>}
    </div>
  )
}

/** ⌘K: every page and every unfinished change, as one keyboard-driven list. */
function LivePalette({ live, onClose }: { live: LiveProps; onClose: () => void }) {
  const [q, setQ] = useState('')
  const [at, setAt] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => input.current?.focus(), [])
  const ov = live.overview.kind === 'ready' ? live.overview.value : null
  const items = [
    ...[['Now', '#home', 'sun'], ['Changes', '#changes', 'kanban'], ['Process', '#process', 'diagram-3'], ['Code map', '#code', 'grid-1x2'], ['Inbox', '#inbox', 'inbox'], ['Catch up', '#activity', 'clock-history'], ['Team', '#team', 'people'], ['Finished and history', '#work/finished', 'flag'], ['Change records', '#change-record', 'clipboard2-check'], ['Landed', '#landed', 'rocket-takeoff'], ['Skills', '#skills', 'journal-check'], ['Runs', '#workflows', 'play-circle'], ['Library', '#templates', 'journal-code'], ['Held files', '#now', 'lock'], ['Files', '#explorer', 'folder2-open'], ['Timeline', '#timeline', 'camera'], ['Promises', '#promises', 'journal-check'], ['Setup: agents', '#setup', 'robot'], ['Computers', '#computers', 'laptop'], ['Access', '#workspace', 'people'], ['Hub health', '#health', 'heart-pulse'], ['Preferences', '#settings', 'sliders'], ['Contributions', '#contributions', 'stars'], ['Feature map', '#features', 'compass'], ['All repositories', '#repositories', 'collection']].map(([label, href, icon]) => ({ label: label!, href: href!, icon: icon!, sub: 'Page', paths: '' })),
    ...(ov?.current ?? []).map((w) => ({ label: w.title, href: `#work/${w.id}`, icon: 'record-circle', sub: `${w.owner_login} · ${w.state}`, paths: w.scope.join(' ') })),
  ]
  const hits = items.filter((i) => `${i.label} ${i.sub} ${i.paths}`.toLowerCase().includes(q.toLowerCase())).slice(0, 10)
  const go = (href: string) => { onClose(); window.location.hash = href }
  return (
    <div className="px-palette-scrim" onClick={onClose}>
      <div className="px-palette" role="dialog" aria-modal="true" aria-label="Search the Hub" onClick={(e) => e.stopPropagation()}>
        <input ref={input} className="form-control" role="combobox" aria-expanded="true" aria-controls="g-palette-list" aria-activedescendant={hits[at] ? `g-hit-${at}` : undefined} aria-autocomplete="list" placeholder="Go to a page, or find a change by title or path…" value={q}
          onChange={(e) => { setQ(e.target.value); setAt(0) }}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') { e.preventDefault(); setAt((a) => Math.min(hits.length - 1, a + 1)) }
            if (e.key === 'ArrowUp') { e.preventDefault(); setAt((a) => Math.max(0, a - 1)) }
            const hit = hits[at]
            if (e.key === 'Enter' && hit) go(hit.href)
          }} />
        <ul id="g-palette-list" role="listbox" aria-label="Pages and changes">{hits.map((h, i) => <li key={h.href + h.label} id={`g-hit-${i}`} role="option" aria-selected={i === at} className={i === at ? 'on' : ''} onClick={() => go(h.href)} onMouseEnter={() => setAt(i)}><Icon name={h.icon} /><span>{h.label}</span><span className="small muted">{h.sub}</span></li>)}</ul>
        {hits.length === 0 && <p className="small muted p-3 mb-0">Nothing matches “{q}”.</p>}
      </div>
    </div>
  )
}
