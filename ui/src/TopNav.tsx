import { skillRunHref, skillRunsHref, skillsRoute } from './skills.ts'
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import type { Derived } from './derive.ts'
import { workHref, workIdFromHash, type WorkOverview } from './standup.ts'
import type { ThemePref } from './storage.ts'
import { useNow, type HubData } from './useHub.ts'
import type { Section } from './App.tsx'
import { workTabFromHash, workTabHref } from './pages/Work.tsx'
import { changeRecordHref, changeRecordIdFromHash } from './pages/ChangeRecord.tsx'
import { routeFromHash as workflowRouteFromHash, runHref as workflowRunHref } from './pages/Workflows.tsx'
import { workspaceRoute, workspaceRunHref } from './pages/Workspace.tsx'
import { landedIdFromHash, landedHref } from './pages/Landed.tsx'
import { compareHref, entryHref, timelineRoute } from './pages/Timeline.tsx'
import { Icon, Tip, relative } from './ui.tsx'

export interface SectionDef {
  key: Section
  icon: string
  label: string
  question: string
  group: 'Coordinate' | 'Verify' | 'This computer' | 'Manage'
}

export type Badges = Partial<Record<Section, { n: number; tone: string; tip: string }>>

const GROUPS: { name: SectionDef['group']; tip: string }[] = [
  { name: 'Coordinate', tip: 'Who holds which files, who owns which task, and what just happened.' },
  { name: 'Verify', tip: 'Recorded workflow checks, and what CI and deploys did after a change landed. Results here never decide landing.' },
  { name: 'This computer', tip: 'Read from agentscm serve on this machine, not from the hub: workflow templates and this workspace’s saved moments.' },
  { name: 'Manage', tip: 'Connect agents, workspace access, hub health and your own settings.' },
]

const initials = (login: string) => login.split(/[-_.\s]+/).filter(Boolean).slice(0, 2).map((p) => p[0]!.toUpperCase()).join('') || '?'

const MENU_ITEMS = '.pop-menu a[href], .pop-menu button:not([disabled])'

/**
 * Popover menu behaviour: closes on outside click, Escape or focus leaving it; on open, focus
 * moves to the first item; arrow keys move between items; Escape hands focus back to the trigger.
 */
export function useDismiss(open: boolean, close: () => void) {
  const ref = useRef<HTMLDivElement>(null)
  // Callers pass a fresh arrow each render; reading it through a ref keeps the effect (and the
  // initial focus) from re-running on every data refresh while the menu is open.
  const closeRef = useRef(close)
  useEffect(() => {
    closeRef.current = close
  })
  // Synchronous, so a key pressed right after opening (End, ArrowDown) is not overridden by a late focus.
  useLayoutEffect(() => {
    if (open) ref.current?.querySelector<HTMLElement>(MENU_ITEMS)?.focus()
  }, [open])
  useEffect(() => {
    if (!open) return
    const close = () => closeRef.current()
    const root = ref.current
    const trigger = root?.querySelector<HTMLElement>('[aria-haspopup]')
    const items = () => [...(root?.querySelectorAll<HTMLElement>(MENU_ITEMS) ?? [])]
    const onDown = (e: MouseEvent) => {
      if (root && !root.contains(e.target as Node)) close()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        close()
        trigger?.focus()
      } else if ((e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Home' || e.key === 'End') && root?.contains(document.activeElement)) {
        e.preventDefault()
        const list = items()
        const at = list.indexOf(document.activeElement as HTMLElement)
        const next = e.key === 'Home' ? 0 : e.key === 'End' ? list.length - 1 : (at + (e.key === 'ArrowDown' ? 1 : -1) + list.length) % list.length
        list[next]?.focus()
      }
    }
    const onFocusOut = (e: FocusEvent) => {
      if (root && e.relatedTarget instanceof Node && !root.contains(e.relatedTarget)) close()
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    root?.addEventListener('focusout', onFocusOut)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
      root?.removeEventListener('focusout', onFocusOut)
    }
  }, [open])
  return ref
}

function RepoSwitcher({ repo, onForget }: { repo: string; onForget: () => void }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  return (
    <div className="pop" ref={ref}>
      <button type="button" className={`repo-btn${open ? ' is-open' : ''}`} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        <span className="repo-mark" aria-hidden="true"><Icon name="git" /></span>
        <span className="repo-name">{repo}</span>
        <Icon name="chevron-expand" className="repo-chevron" />
      </button>
      {open && (
        <div className="pop-menu pop-left" role="menu">
          <div className="pop-head">Repositories you can see</div>
          <div className="pop-item is-current">
            <span className="repo-mark" aria-hidden="true"><Icon name="git" /></span>
            <span className="pop-main">
              <code>{repo}</code>
              <span className="pop-sub"><Icon name="eye" /> Read-only</span>
            </span>
            <Icon name="check2" className="pop-check" />
          </div>
          <p className="pop-note">
            <Icon name="info-circle" /> A read-only token is scoped to one repository. To look at another one, sign in with a token made for it.
          </p>
          <div className="pop-sep" />
          <a role="menuitem" href="#workspace" className="pop-item" onClick={() => setOpen(false)}>
            <Icon name="people" />
            <span className="pop-main">Open the workspace<span className="pop-sub">Members, repositories and runs</span></span>
          </a>
          <button type="button" role="menuitem" className="pop-item" onClick={onForget}>
            <Icon name="arrow-left-right" />
            <span className="pop-main">Use a different token<span className="pop-sub">Forgets this one in this browser</span></span>
          </button>
        </div>
      )}
    </div>
  )
}

function AccountMenu({ data, pref, onPref, onForget }: { data: HubData; pref: ThemePref; onPref: (p: ThemePref) => void; onForget: () => void }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  const who = data.whoami
  const login = who?.owner.login ?? '…'
  const themes: { v: ThemePref; icon: string; label: string }[] = [
    { v: 'light', icon: 'sun', label: 'Light' },
    { v: 'system', icon: 'circle-half', label: 'System' },
    { v: 'dark', icon: 'moon-stars', label: 'Dark' },
  ]
  return (
    <div className="pop" ref={ref}>
      <button type="button" className={`acct-btn${open ? ' is-open' : ''}`} aria-haspopup="menu" aria-expanded={open} aria-label={`Account and settings for ${login}`} onClick={() => setOpen((o) => !o)}>
        <span className="me-avatar" aria-hidden="true">{initials(login)}</span>
      </button>
      {open && (
        <div className="pop-menu" role="menu">
          <div className="acct-id">
            <span className="me-avatar me-avatar-lg" aria-hidden="true">{initials(login)}</span>
            <div>
              <strong>{login}</strong>
              <div className="muted small d-flex align-items-center gap-1">
                <Icon name={who?.owner.provider === 'gitlab' ? 'gitlab' : 'github'} /> {who?.owner.provider === 'gitlab' ? 'GitLab' : 'GitHub'}
                {who?.owner.host ? ` · ${who.owner.host}` : ''}
              </div>
            </div>
          </div>
          {who && (
            <div className="acct-token">
              <Icon name="key" />
              <span>
                Token <strong>{who.token.name}</strong>
                <span className="muted"> · read-only{who.token.expires_at ? ` · expires ${new Date(who.token.expires_at * 1000).toLocaleDateString()}` : ''}</span>
              </span>
            </div>
          )}
          <div className="acct-row">
            <span className="muted small">Theme</span>
            <div className="seg" role="group" aria-label="Theme">
              {themes.map((t) => (
                <button key={t.v} type="button" role="menuitemradio" aria-checked={pref === t.v} className={pref === t.v ? 'on' : ''} onClick={() => onPref(t.v)}>
                  <Icon name={t.icon} /> {t.label}
                </button>
              ))}
            </div>
          </div>
          <div className="pop-sep" />
          <a role="menuitem" href="#settings" className="pop-item" onClick={() => setOpen(false)}>
            <Icon name="sliders" />
            <span className="pop-main">Settings<span className="pop-sub">Theme, sidebar, token and refresh</span></span>
          </a>
          <button type="button" role="menuitem" className="pop-item pop-danger" onClick={onForget}>
            <Icon name="box-arrow-right" />
            <span className="pop-main">Forget token<span className="pop-sub">The hub still accepts it until it expires or is revoked.</span></span>
          </button>
        </div>
      )}
    </div>
  )
}

export interface Hit {
  group: string
  icon: string
  label: string
  hint: string
  href: string
}

export function paletteHits(sections: SectionDef[], d: Derived | null, overview: WorkOverview | null): Hit[] {
  const hits: Hit[] = sections.map((s) => ({ group: 'Pages', icon: s.icon, label: s.label, hint: s.question, href: `#${s.key}` }))
  for (const a of d?.agents ?? []) hits.push({ group: 'Agents', icon: 'robot', label: a.name, hint: `${a.owner}${a.task ? ` · ${a.task}` : ''}`, href: '#now' })
  const files = new Set((d?.agents ?? []).flatMap((a) => a.holdings.map((h) => h.spec)))
  for (const f of files) hits.push({ group: 'Held files', icon: 'file-earmark-code', label: f, hint: 'Claimed by a live agent', href: '#now' })
  for (const w of overview?.current ?? []) hits.push({ group: 'Work', icon: 'kanban', label: w.title, hint: `${w.lead_agent} · ${w.state}`, href: workHref(w.id) })
  for (const o of overview?.recent ?? []) hits.push({ group: 'Recently finished', icon: 'check2-circle', label: o.item.title, hint: `${o.item.lead_agent} · reported ${o.kind}`, href: workHref(o.item.id) })
  return hits
}

/** Empty query shows only pages; otherwise a case-insensitive match on label and hint, capped at 40. */
export function filterHits(all: Hit[], q: string): Hit[] {
  const needle = q.trim().toLowerCase()
  const list = needle ? all.filter((h) => `${h.label} ${h.hint}`.toLowerCase().includes(needle)) : all.filter((h) => h.group === 'Pages')
  return list.slice(0, 40)
}

function CommandPalette({ onClose, sections, d, overview }: { onClose: () => void; sections: SectionDef[]; d: Derived | null; overview: WorkOverview | null }) {
  const [q, setQ] = useState('')
  const [sel, setSel] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  // Mounted only while open, so search and selection start empty. Layout effect: focus lands
  // before the next key event, so typing straight after ⌘K is not lost.
  useLayoutEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    input.current?.focus()
    return () => opener?.focus({ preventScroll: true })
  }, [])

  const all = useMemo(() => paletteHits(sections, d, overview), [sections, d, overview])
  const shown = useMemo(() => filterHits(all, q), [all, q])

  const go = (h: Hit | undefined) => {
    if (!h) return
    window.location.hash = h.href.slice(1)
    onClose()
  }
  return (
    <div className="palette-scrim" onMouseDown={onClose}>
      <div
        className="palette"
        role="dialog"
        aria-modal="true"
        aria-label="Search the hub"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          // Results are reached with the arrow keys, so Tab stays on the input while the dialog is open.
          if (e.key === 'Tab') {
            e.preventDefault()
            input.current?.focus()
          }
        }}
      >
        <div className="palette-input">
          <Icon name="search" />
          <input
            ref={input}
            value={q}
            placeholder="Search pages, agents, files and work…"
            aria-label="Search"
            role="combobox"
            aria-expanded={shown.length > 0}
            aria-controls="palette-list"
            aria-autocomplete="list"
            aria-activedescendant={shown.length > 0 ? `hit-${sel}` : undefined}
            onChange={(e) => {
              setQ(e.target.value)
              setSel(0)
            }}
            onKeyDown={(e) => {
              if (e.key === 'ArrowDown') {
                e.preventDefault()
                setSel((s) => Math.max(0, Math.min(s + 1, shown.length - 1)))
              } else if (e.key === 'ArrowUp') {
                e.preventDefault()
                setSel((s) => Math.max(s - 1, 0))
              } else if (e.key === 'Enter') go(shown[sel])
              else if (e.key === 'Escape') onClose()
            }}
          />
          <kbd>esc</kbd>
        </div>
        <div className="palette-list" id="palette-list" role="listbox" aria-label="Results">
          {shown.length === 0 && <div className="palette-empty">Nothing on this page matches “{q}”. Search covers the data already loaded, not the whole hub.</div>}
          {shown.map((h, i) => {
            const head = i === 0 || shown[i - 1]!.group !== h.group ? h.group : null
            return (
              <div key={`${h.group}:${h.label}:${i}`}>
                {head && <div className="palette-group">{head}</div>}
                <button type="button" id={`hit-${i}`} tabIndex={-1} role="option" aria-selected={i === sel} className={`palette-hit${i === sel ? ' on' : ''}`} onMouseEnter={() => setSel(i)} onClick={() => go(h)}>
                  <Icon name={h.icon} />
                  <span className="palette-label">{h.label}</span>
                  <span className="palette-hint">{h.hint}</span>
                  <Icon name="arrow-return-left" className="palette-enter" />
                </button>
              </div>
            )
          })}
        </div>
        <div className="palette-foot">
          <span><kbd>↑</kbd><kbd>↓</kbd> move</span>
          <span><kbd>↵</kbd> open</span>
          <span className="ms-auto">Read-only: opening a result never changes anything.</span>
        </div>
      </div>
    </div>
  )
}

/** Wraps a nav link in a tooltip only while the rail is compact, when its label is hidden. */
function RailTip({ on, text, children }: { on: boolean; text: string; children: ReactNode }) {
  return on ? <Tip text={text} placement="right" className="rail-tip">{children}</Tip> : <>{children}</>
}

function SideNav({ sections, section, badges, collapsed, onCollapse, drawer, onClose }: { sections: SectionDef[]; section: Section; badges: Badges; collapsed: boolean; onCollapse: () => void; drawer: boolean; onClose: () => void }) {
  return (
    <>
      {drawer && <div className="drawer-scrim" onClick={onClose} aria-hidden="true" />}
      <aside data-tour="nav" className={`side${collapsed ? ' is-compact' : ''}${drawer ? ' is-open' : ''}`} aria-label="Hub sections">
        <nav className="side-nav">
          {GROUPS.map((g) => (
            <div key={g.name} className="side-group">
              <div className="side-head">
                <span>{g.name}</span>
                <Tip text={g.tip} focusable className="side-head-tip"><Icon name="info-circle" /><span className="visually-hidden">{g.tip}</span></Tip>
              </div>
              {sections.filter((s) => s.group === g.name).map((s) => {
                const b = badges[s.key]
                const active = section === s.key
                return (
                  <RailTip key={s.key} on={collapsed && !drawer} text={`${s.label}: ${s.question}`}>
                    <a href={`#${s.key}`} className={`side-link${active ? ' active' : ''}`} aria-current={active ? 'page' : undefined} aria-label={collapsed && !drawer ? s.label : undefined} onClick={onClose}>
                      <span className="side-icon"><Icon name={s.icon} /></span>
                      <span className="side-text">
                        <span className="side-label">{s.label}</span>
                        <span className="side-q">{s.question}</span>
                      </span>
                      {b && <span className={`side-badge tone-${b.tone}`} title={b.tip}>{b.n}<span className="visually-hidden"> {b.tip}</span></span>}
                    </a>
                  </RailTip>
                )
              })}
            </div>
          ))}
        </nav>
        <Tip text={collapsed ? 'Expand the sidebar  [' : 'Show icons only  ['} placement="right" className="side-foot">
          <button type="button" className="side-collapse" onClick={onCollapse} aria-pressed={collapsed} aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}>
            <Icon name={collapsed ? 'layout-sidebar-inset' : 'layout-sidebar'} />
            <span className="side-label">Collapse</span>
            <kbd>[</kbd>
          </button>
        </Tip>
      </aside>
    </>
  )
}

export interface Crumb {
  label: string
  href?: string
  icon?: string
  mono?: boolean
}

const WORK_TAB_LABEL = { working: 'Working now', finished: 'Recently finished', coordination: 'Needs coordination', history: 'Change history' } as const

export function crumbsFor(hash: string, section: SectionDef, overview: WorkOverview | null): Crumb[] {
  const out: Crumb[] = [{ label: section.label, href: `#${section.key}`, icon: section.icon }]
  if (section.key === 'work') {
    const id = workIdFromHash(hash)
    if (id) {
      const item = overview?.current.find((w) => w.id === id) ?? overview?.recent.find((o) => o.item.id === id)?.item
      out.push({ label: item?.title ?? `Work item ${id.slice(0, 12)}…` })
    } else {
      const tab = workTabFromHash(hash)
      out.push({ label: WORK_TAB_LABEL[tab], href: workTabHref(tab) })
    }
  } else if (section.key === 'workspace') {
    const route = workspaceRoute(hash)
    if (route.tab === 'members') out.push({ label: 'Members', href: '#workspace/members' })
    else if (route.tab === 'settings') out.push({ label: 'Settings', href: '#workspace/settings' })
    else if (route.tab === 'workflow') {
      out.push({ label: 'Repositories & runs', href: '#workspace' })
      if (!('invalid' in route)) {
        out.push({ label: `Run ${route.runId.length > 14 ? `${route.runId.slice(0, 14)}…` : route.runId}`, href: workspaceRunHref(route.repositoryId, route.runId), mono: true })
        if (route.stepId) out.push({ label: `Step ${route.stepId}`, mono: true })
      }
    } else out.push({ label: 'Repositories & runs', href: '#workspace' })
  } else if (section.key === 'workflows') {
    // Same guarded parsing as the Workflows page: malformed escapes or invalid ids mean the run list.
    const route = workflowRouteFromHash(hash)
    if (route.kind === 'run') {
      const run = route.runId
      out.push({ label: `Run ${run.length > 14 ? `${run.slice(0, 14)}…` : run}`, href: workflowRunHref(run), mono: true })
      if (route.stepId) out.push({ label: `Step ${route.stepId}`, mono: true })
    }
  } else if (section.key === 'change-record') {
    const id = changeRecordIdFromHash(hash)
    if (id) out.push({ label: `Work ${id.slice(0, 12)}…`, href: changeRecordHref(id), mono: true })
  } else if (section.key === 'landed') {
    const id = landedIdFromHash(hash)
    if (id) out.push({ label: `Change ${id.slice(0, 12)}…`, href: landedHref(id), mono: true })
  } else if (section.key === 'skills') {
    const route = skillsRoute(hash)
    if (route.kind !== 'library') out.push({ label: 'Runs', href: skillRunsHref })
    if (route.kind === 'run') out.push({ label: `Run ${route.runId.length > 14 ? `${route.runId.slice(0, 14)}…` : route.runId}`, href: skillRunHref(route.runId), mono: true })
  } else if (section.key === 'timeline') {
    const route = timelineRoute(hash)
    if (route.kind === 'entry') out.push({ label: `Saved moment #${route.id}`, href: entryHref(route.id) })
    else if (route.kind === 'compare') out.push({ label: `Compare #${route.left} and #${route.right}`, href: compareHref(route.left, route.right) })
  }
  return out
}

function Breadcrumbs({ repo, section, overview, data }: { repo: string; section: SectionDef; overview: WorkOverview | null; data: HubData }) {
  const [hash, setHash] = useState(window.location.hash)
  const now = useNow()
  useEffect(() => {
    const on = () => setHash(window.location.hash)
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  const crumbs = [{ label: repo, href: '#now', icon: 'git', mono: true } as Crumb, ...crumbsFor(hash, section, overview)]
  return (
    <div className="crumbbar" data-tour="crumbs">
      <nav aria-label="Breadcrumb">
        <ol className="crumbs">
          {crumbs.map((c, i) => {
            const last = i === crumbs.length - 1
            const body = (
              <>
                {c.icon && <Icon name={c.icon} />}
                <span className={c.mono ? 'crumb-mono' : undefined}>{c.label}</span>
              </>
            )
            return (
              <li key={`${i}:${c.label}`} className={last ? 'is-last' : undefined}>
                {last || !c.href ? <span aria-current={last ? 'page' : undefined}>{body}</span> : <a href={c.href}>{body}</a>}
                {!last && <Icon name="chevron-right" className="crumb-sep" />}
              </li>
            )
          })}
        </ol>
      </nav>
      <Tip text={data.failing ? 'The hub did not answer the last refresh. Showing the last data that loaded; retrying with backoff.' : 'Live data refreshes every 5 seconds while this tab is visible.'} focusable className={`crumb-fresh${data.failing ? ' is-failing' : ''}`}>
        <Icon name={data.failing ? 'exclamation-triangle' : 'arrow-repeat'} />
        {data.failing ? 'Reconnecting…' : data.lastOk ? `Updated ${relative(data.lastOk / 1000, now)}` : 'Connecting…'}
      </Tip>
    </div>
  )
}

export function AppChrome({ logo, repo, sections, section, badges, data, d, overview, pref, onPref, onForget, collapsed, onCollapsed, children }: {
  logo: ReactNode
  repo: string
  sections: SectionDef[]
  section: Section
  badges: Badges
  data: HubData
  d: Derived | null
  overview: WorkOverview | null
  pref: ThemePref
  onPref: (p: ThemePref) => void
  onForget: () => void
  collapsed: boolean
  onCollapsed: (c: boolean) => void
  children: ReactNode
}) {
  const [palette, setPalette] = useState(false)
  const [drawerAt, setDrawerAt] = useState<Section | null>(null)
  const drawer = drawerAt === section
  const setDrawer = (open: boolean) => setDrawerAt(open ? section : null)
  const burger = useRef<HTMLButtonElement>(null)
  const latest = useRef({ collapsed, onCollapsed, drawer })
  useEffect(() => {
    latest.current = { collapsed, onCollapsed, drawer }
  })
  useEffect(() => {
    if (drawer) window.setTimeout(() => document.querySelector<HTMLElement>('.side.is-open .side-link')?.focus(), 0)
  }, [drawer])
  // A new section starts at the top, with focus on the page so keyboard and screen-reader users land in it.
  const firstSection = useRef(true)
  useEffect(() => {
    if (firstSection.current) {
      firstSection.current = false
      return
    }
    window.scrollTo(0, 0)
    document.getElementById('main')?.focus({ preventScroll: true })
  }, [section])
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing = e.target instanceof HTMLElement && (/^(INPUT|TEXTAREA|SELECT)$/.test(e.target.tagName) || e.target.isContentEditable)
      if ((e.key === 'k' && (e.metaKey || e.ctrlKey)) || (e.key === '/' && !typing)) {
        e.preventDefault()
        setPalette(true)
      } else if (e.key === '[' && !typing && !e.metaKey && !e.ctrlKey) {
        latest.current.onCollapsed(!latest.current.collapsed)
      } else if (e.key === 'Escape' && latest.current.drawer) {
        setDrawerAt(null)
        burger.current?.focus()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const healthOk = data.health?.status === 'ok' && !data.failing
  const healthTip = data.failing
    ? 'The hub did not answer the last refresh. Showing the last data that loaded; retrying with backoff.'
    : data.health?.status === 'unavailable'
      ? 'The hub reports its storage is unavailable. Open Hub health for details.'
      : data.health
        ? 'The hub is answering. Open Hub health for coordination counters and hook latency.'
        : 'Checking the hub…'
  const isMac = /Mac|iPhone|iPad/.test(navigator.platform)
  const current = sections.find((s) => s.key === section) ?? sections[0]!

  return (
    <div className={`app-shell${collapsed ? ' rail-compact' : ''}`}>
      <a href="#main" className="skip-link" onClick={(e) => {
        e.preventDefault()
        document.getElementById('main')?.focus()
      }}>
        Skip to content
      </a>
      <header className="topbar" data-tour="topbar">
        <button ref={burger} type="button" className="burger" aria-label="Open navigation" aria-expanded={drawer} onClick={() => setDrawer(!drawer)}>
          <Icon name={drawer ? 'x-lg' : 'list'} />
        </button>
        <a href="#now" className="topbar-logo" aria-label="AgentSCM Hub home">
          {logo}
        </a>
        <span className="topbar-div" aria-hidden="true" />
        <RepoSwitcher repo={repo} onForget={onForget} />

        <button type="button" className="search-pill" onClick={() => setPalette(true)} aria-label="Search the hub">
          <Icon name="search" />
          <span className="search-text">Search agents, files, work…</span>
          <kbd>{isMac ? '⌘' : 'Ctrl'} K</kbd>
        </button>

        <div className="topbar-right">
          <Tip text={healthTip} placement="bottom">
            <a href="#health" className={`health-chip${healthOk ? '' : ' is-bad'}`} aria-label={healthTip}>
              <span className="live-dot" aria-hidden="true" />
              <span className="health-text">{data.failing ? 'Reconnecting' : healthOk ? 'Live' : data.health ? 'Degraded' : '…'}</span>
            </a>
          </Tip>
          <Tip text="Connect an agent: install the hook, mint a token, verify the setup." placement="bottom">
            <a href="#setup" className="btn btn-sm btn-primary topbar-cta">
              <Icon name="plug" /> <span className="cta-text">Connect agent</span>
            </a>
          </Tip>
          <AccountMenu data={data} pref={pref} onPref={onPref} onForget={onForget} />
        </div>
      </header>
      <SideNav sections={sections} section={section} badges={badges} collapsed={collapsed} onCollapse={() => onCollapsed(!collapsed)} drawer={drawer} onClose={() => setDrawer(false)} />
      <div className="content">
        <Breadcrumbs repo={repo} section={current} overview={overview} data={data} />
        <main className="main" id="main" tabIndex={-1} data-tour="main">
          {children}
        </main>
      </div>
      {palette && <CommandPalette onClose={() => setPalette(false)} sections={sections} d={d} overview={overview} />}
    </div>
  )
}
