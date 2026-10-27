import { hostOf, selectorOf, useLocalComputer } from './ComputerBar.tsx'
import { disconnectComputer, isLocalCredential, localSelection, switchRepository, type ConnectedRepository } from './frontDoor.ts'
import { skillRunHref, skillRunsHref, skillsRoute } from './skills.ts'
import { createContext, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import type { Derived } from './derive.ts'
import { workHref, workIdFromHash, type WorkOverview } from './standup.ts'
import { loadSingleKeys, type Density, type ThemePref } from './storage.ts'
import { useNow, type HubData } from './useHub.ts'
import type { Section } from './App.tsx'
import { workTabFromHash, workTabHref } from './pages/Work.tsx'
import { changeRecordHref, changeRecordIdFromHash } from './pages/ChangeRecord.tsx'
import { routeFromHash as workflowRouteFromHash, runHref as workflowRunHref } from './pages/Workflows.tsx'
import { workspaceRoute, workspaceRunHref } from './pages/Workspace.tsx'
import { landedIdFromHash, landedHref } from './pages/Landed.tsx'
import { compareHref, entryHref, timelineRoute } from './pages/Timeline.tsx'
import { Icon, Tip, agentLabel, relative } from './ui.tsx'
import { TourTrigger, useTour } from './tour/Tour.tsx'
import { tourFor } from './tour/content.ts'

export type NavKey = 'home' | 'inbox' | 'history' | 'timeline' | 'skills' | 'setup'

export interface SectionDef {
  key: Section
  nav: NavKey
  icon: string
  /** The page's name: its tab label inside its nav item. */
  label: string
  question: string
}

export interface NavDef {
  key: NavKey
  icon: string
  label: string
  question: string
}

export type Badges = Partial<Record<NavKey, { n: number; tone: string; tip: string }>>

/** A tab's live status under its label: a coloured dot and a few words. */
export interface TabStatus {
  text: string
  tone: 'brand' | 'ok' | 'warn' | 'bad' | 'muted'
}
export type TabStatuses = Partial<Record<string, TabStatus>>

export interface TabDef {
  key: string
  label: string
  href: string
  on: boolean
}

/** The tabs of the nav item `section` belongs to. History splits Work into what finished and the change log. */
export function tabsFor(sections: SectionDef[], section: Section, hash: string): TabDef[] {
  const nav = sections.find((s) => s.key === section)?.nav
  const out: TabDef[] = []
  for (const s of sections.filter((x) => x.nav === nav)) {
    if (s.key === 'work') {
      const tab = workTabFromHash(hash)
      out.push({ key: 'work', label: 'Finished', href: workTabHref('finished'), on: section === 'work' && tab !== 'history' })
      out.push({ key: 'work-history', label: 'Changes', href: workTabHref('history'), on: section === 'work' && tab === 'history' })
    } else out.push({ key: s.key, label: s.label, href: `#${s.key}`, on: s.key === section })
  }
  return out.length > 1 ? out : []
}

interface ChromeState {
  sections: SectionDef[]
  section: Section
  statuses: TabStatuses
}
const ChromeContext = createContext<ChromeState | null>(null)

function useHash(): string {
  const [hash, setHash] = useState(window.location.hash)
  useEffect(() => {
    const on = () => setHash(window.location.hash)
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return hash
}

/**
 * The tab strip of the current nav item, each tab a label over its live status. Pages place it right
 * under their answer sentence, so nothing visible ever sits above the answer.
 */
export function SectionTabs({ label }: { label?: string }) {
  const chrome = useContext(ChromeContext)
  const hash = useHash()
  if (!chrome) return null
  const tabs = tabsFor(chrome.sections, chrome.section, hash)
  if (tabs.length === 0) return null
  const nav = chrome.sections.find((s) => s.key === chrome.section)?.nav
  return (
    <nav className="htabs" data-tour="tabs" aria-label={label ?? `${nav ?? ''} views`} style={{ ['--n' as string]: String(tabs.length) }}>
      {tabs.map((t) => {
        const st = chrome.statuses[t.key]
        return (
          <a key={t.key} href={t.href} className={`htab tone-${st?.tone ?? 'muted'}${t.on ? ' on' : ''}`} aria-current={t.on ? 'page' : undefined}>
            <b>{t.label}</b>
            <span>
              <i aria-hidden="true" />
              {st?.text ?? '\u00a0'}
            </span>
          </a>
        )
      })}
    </nav>
  )
}

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
      <button type="button" data-tour="repo" className={`repo-btn${open ? ' is-open' : ''}`} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
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

/**
 * The repository menu on this computer's own Hub page: the connected computer, every repository
 * connected on it (the same repository on two Hubs is told apart by Hub), and Disconnect. It shares
 * `useDismiss` with the Hub's other menus.
 */
export function LocalRepoSwitcher({ token, repo, navigationBlocked = false }: { token: string; repo: string; navigationBlocked?: boolean }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  const local = useLocalComputer()
  const identity = local?.identity.kind === 'ready' ? local.identity.value : null
  const repositories = local?.repositories ?? []
  const computer = identity?.computer ?? 'This computer'
  const repository = identity ? identity.repository : repo
  const selection = localSelection(token) ?? repository ?? ''
  const current = repositories.find((r) => selectorOf(r) === selection) ?? repositories.find((r) => r.repository.toLowerCase() === repository?.toLowerCase() && r.hub === identity?.hub?.url)
  const repeated = (r: ConnectedRepository) => repositories.filter((o) => o.repository.toLowerCase() === r.repository.toLowerCase()).length > 1
  return (
    <div className="pop" ref={ref}>
      <button type="button" data-tour="repo" className={`repo-btn${open ? ' is-open' : ''}`} aria-haspopup="menu" aria-expanded={open} aria-label={`${repository ?? 'No repository'} on ${computer}`} onClick={() => setOpen((o) => !o)}>
        <span className="repo-mark is-computer" aria-hidden="true"><Icon name="laptop" /></span>
        <span className="repo-name">{repository ?? 'Choose a repository'}</span>
        <Icon name="chevron-expand" className="repo-chevron" />
      </button>
      {open && (
        <div className="pop-menu pop-left" role="menu">
          <div className="pop-head">
            <Icon name="laptop" /> On {computer}
          </div>
          {repositories.map((r) => {
            const isCurrent = current !== undefined && selectorOf(r) === selectorOf(current)
            return (
              <button
                key={selectorOf(r)}
                type="button"
                role="menuitemradio"
                aria-checked={isCurrent}
                className={`pop-item${isCurrent ? ' is-current' : ''}`}
                onClick={() => {
                  setOpen(false)
                  if (!isCurrent && navigationBlocked && !window.confirm('Discard unsaved workflow edits?')) return
                  if (!isCurrent && switchRepository(token, selectorOf(r))) window.location.reload()
                }}
              >
                <span className="repo-mark" aria-hidden="true"><Icon name="git" /></span>
                <span className="pop-main">
                  <code>{r.repository}</code>
                  <span className="pop-sub">{repeated(r) ? hostOf(r.hub) : r.folder && r.folder !== r.repository.split('/')[1] ? r.folder : 'Read-only team view'}</span>
                </span>
                {isCurrent && <Icon name="check2" className="pop-check" />}
              </button>
            )
          })}
          <p className="pop-note">
            <Icon name="info-circle" /> Files and saved conversations come from this computer. Team work comes from each repository’s Hub, read-only.
          </p>
          <div className="pop-sep" />
          <button
            type="button"
            role="menuitem"
            className="pop-item"
            onClick={() => {
              setOpen(false)
              if (navigationBlocked && !window.confirm('Discard unsaved workflow edits?')) return
              void disconnectComputer().then(() => window.location.reload())
            }}
          >
            <Icon name="box-arrow-right" />
            <span className="pop-main">
              Disconnect this browser<span className="pop-sub">Connect again with Open Hub or <code>agentscm open</code></span>
            </span>
          </button>
        </div>
      )}
    </div>
  )
}

function AccountMenu({ data, onForget }: { data: HubData; onForget: () => void }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  const who = data.whoami
  const login = who?.owner.login ?? '…'
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
          <div className="pop-sep" />
          <a role="menuitem" href="#computers" className="pop-item" onClick={() => setOpen(false)}>
            <Icon name="laptop" />
            <span className="pop-main">Your computers<span className="pop-sub">Where you’re signed in, with Revoke</span></span>
          </a>
          <a role="menuitem" href="#settings" className="pop-item" onClick={() => setOpen(false)}>
            <Icon name="sliders" />
            <span className="pop-main">Preferences<span className="pop-sub">Theme, density, sidebar, token and shortcuts</span></span>
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
  /** Runs instead of following href (the page-tour entry). */
  run?: () => void
}

export function paletteHits(sections: SectionDef[], d: Derived | null, overview: WorkOverview | null): Hit[] {
  const hits: Hit[] = sections.map((s) => ({ group: 'Pages', icon: s.icon, label: s.label, hint: s.question, href: `#${s.key}` }))
  hits.push({ group: 'Pages', icon: 'list-ul', label: 'Changes', hint: 'Every change to Work, page by page', href: workTabHref('history') })
  for (const a of d?.agents ?? []) hits.push({ group: 'Agents', icon: 'robot', label: a.name, hint: `${a.owner}${a.task ? ` · ${a.task}` : ''}`, href: '#team' })
  const files = new Set((d?.agents ?? []).flatMap((a) => a.holdings.map((h) => h.spec)))
  for (const f of files) hits.push({ group: 'Held files', icon: 'file-earmark-code', label: f, hint: 'Claimed by a live agent', href: '#now' })
  for (const w of overview?.current ?? []) hits.push({ group: 'Work', icon: 'kanban', label: w.title, hint: `${agentLabel(w.lead_agent)} · ${w.state}`, href: workHref(w.id) })
  for (const o of overview?.recent ?? []) hits.push({ group: 'Recently finished', icon: 'check2-circle', label: o.item.title, hint: `${o.item.lead_agent} · reported ${o.kind}`, href: workHref(o.item.id) })
  return hits
}

/** Empty query shows only pages and help; otherwise a case-insensitive match on label and hint, capped at 40. */
export function filterHits(all: Hit[], q: string): Hit[] {
  const needle = q.trim().toLowerCase()
  const list = needle ? all.filter((h) => `${h.label} ${h.hint}`.toLowerCase().includes(needle)) : all.filter((h) => h.group === 'Pages' || h.group === 'Help')
  return list.slice(0, 40)
}

function CommandPalette({ onClose, sections, section, d, overview }: { onClose: () => void; sections: SectionDef[]; section: Section; d: Derived | null; overview: WorkOverview | null }) {
  const tour = useTour()
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

  const all = useMemo(() => {
    const hits = paletteHits(sections, d, overview)
    const here = sections.find((s) => s.key === section)
    if (tour && here && tourFor(section)) hits.push({ group: 'Help', icon: 'signpost-split', label: 'Walk me through this page', hint: `A short tour of ${here.label}`, href: '', run: () => tour.start(section) })
    if (tour) hits.push({ group: 'Help', icon: 'compass', label: 'Replay the welcome tour', hint: 'The top bar, the sections and what each one is for', href: '', run: () => tour.start('welcome') })
    return hits
  }, [sections, section, d, overview, tour])
  const shown = useMemo(() => filterHits(all, q), [all, q])
  useEffect(() => {
    if (shown.length > 0) document.getElementById(`hit-${sel}`)?.scrollIntoView?.({ block: 'nearest' })
  }, [sel, shown.length])

  const go = (h: Hit | undefined) => {
    if (!h) return
    onClose()
    if (h.run) h.run()
    else window.location.hash = h.href.slice(1)
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

/** Light, dark or the device's choice, then Spacious or Condensed: both kept in this browser only. */
function ViewMenu({ pref, onPref, density, onDensity }: { pref: ThemePref; onPref: (p: ThemePref) => void; density: Density; onDensity: (d: Density) => void }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  const themes: { v: ThemePref; icon: string; label: string; hint: string }[] = [
    { v: 'light', icon: 'sun', label: 'Light', hint: 'A white sheet in the violet frame' },
    { v: 'dark', icon: 'moon-stars', label: 'Dark', hint: 'A dusk sheet, the frame a step deeper' },
    { v: 'system', icon: 'circle-half', label: 'Match this device', hint: 'Follows your system setting' },
  ]
  const densities: { v: Density; label: string }[] = [
    { v: 'spacious', label: 'Spacious' },
    { v: 'condensed', label: 'Condensed' },
  ]
  return (
    <div className="pop" ref={ref}>
      <button type="button" data-tour="view" className={`top-icon${open ? ' is-open' : ''}`} aria-haspopup="menu" aria-expanded={open} aria-label="Theme and density" title="Theme and density" onClick={() => setOpen((o) => !o)}>
        <Icon name="circle-half" />
      </button>
      {open && (
        <div className="pop-menu view-menu" role="menu">
          <div className="pop-head">Theme</div>
          {themes.map((t) => (
            <button key={t.v} type="button" role="menuitemradio" aria-checked={pref === t.v} className={`pop-item${pref === t.v ? ' is-current' : ''}`} onClick={() => onPref(t.v)}>
              <Icon name={t.icon} />
              <span className="pop-main">
                {t.label}
                <span className="pop-sub">{t.hint}</span>
              </span>
              {pref === t.v && <Icon name="check2" className="pop-check" />}
            </button>
          ))}
          <div className="pop-sep" />
          <div className="pop-head">Density</div>
          <div className="density" role="group" aria-label="Density">
            {densities.map((x) => (
              <button key={x.v} type="button" role="menuitemradio" aria-checked={density === x.v} className={density === x.v ? 'on' : ''} onClick={() => onDensity(x.v)}>
                {x.label}
              </button>
            ))}
          </div>
          <p className="view-note">Condensed changes measurements only. Every page keeps the same parts in the same places.</p>
        </div>
      )}
    </div>
  )
}

/** Wraps a nav link in a tooltip only while the rail is compact, when its label is hidden. */
function RailTip({ on, text, children }: { on: boolean; text: string; children: ReactNode }) {
  return on ? <Tip text={text} placement="right" className="rail-tip">{children}</Tip> : <>{children}</>
}

function SideNav({ nav, sections, section, badges, collapsed, onCollapse, drawer, onClose }: { nav: NavDef[]; sections: SectionDef[]; section: Section; badges: Badges; collapsed: boolean; onCollapse: () => void; drawer: boolean; onClose: () => void }) {
  const hash = useHash()
  // A task's own page is reached from the dashboard's track, so the dashboard stays lit.
  const here = section === 'work' && workIdFromHash(hash) ? 'home' : sections.find((s) => s.key === section)?.nav
  return (
    <>
      {drawer && <div className="drawer-scrim" onClick={onClose} aria-hidden="true" />}
      <aside data-tour="nav" className={`side${collapsed ? ' is-compact' : ''}${drawer ? ' is-open' : ''}`} aria-label="Hub sections">
        <nav className="side-nav">
          <div className="side-group">
            {nav.map((n) => {
              const b = badges[n.key]
              const active = here === n.key
              const first = sections.find((s) => s.nav === n.key)!
              return (
                <RailTip key={n.key} on={collapsed && !drawer} text={`${n.label}: ${n.question}`}>
                  <a href={first.key === 'work' ? workTabHref('finished') : `#${first.key}`} data-tour={`nav-${n.key}`} className={`side-link${active ? ' active' : ''}`} aria-current={active ? 'page' : undefined} aria-label={collapsed && !drawer ? n.label : undefined} onClick={onClose}>
                    <span className="side-icon"><Icon name={n.icon} /></span>
                    <span className="side-text">
                      <span className="side-label">{n.label}</span>
                      <span className="side-q">{n.question}</span>
                    </span>
                    {b && <span className={`side-badge tone-${b.tone}`} title={b.tip}>{b.n}<span className="visually-hidden"> {b.tip}</span></span>}
                  </a>
                </RailTip>
              )
            })}
          </div>
        </nav>
        <div className="side-foot">
          <p className="side-foot-note"><Icon name="eye" /> Read-only. Opening a page never changes anything for your agents.</p>
          <Tip text={collapsed ? 'Expand the sidebar' : 'Show icons only'} placement="right">
            <button type="button" className="side-collapse" onClick={onCollapse} aria-pressed={collapsed} aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}>
              <Icon name={collapsed ? 'layout-sidebar-inset' : 'layout-sidebar'} />
              <span className="side-label">Collapse</span>
              <kbd>[</kbd>
            </button>
          </Tip>
        </div>
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

/**
 * The trail of a deep page: its tab, then the record it shows. Tabs already say where a top-level
 * page is, so it gets one crumb and no bar is drawn.
 */
export function crumbsFor(hash: string, section: SectionDef, overview: WorkOverview | null): Crumb[] {
  const out: Crumb[] = [{ label: section.label, href: `#${section.key}`, icon: section.icon }]
  if (section.key === 'work') {
    const id = workIdFromHash(hash)
    if (id) {
      const item = overview?.current.find((w) => w.id === id) ?? overview?.recent.find((o) => o.item.id === id)?.item
      out[0] = { label: 'Dashboard', href: '#home', icon: 'activity' }
      out.push({ label: item?.title ?? `Work item ${id.slice(0, 12)}…` })
    } else if (workTabFromHash(hash) === 'history') out[0] = { label: 'Changes', href: workTabHref('history'), icon: 'list-ul' }
    else out[0] = { label: 'Finished', href: workTabHref('finished'), icon: 'flag' }
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
    }
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

function Breadcrumbs({ section, overview }: { section: SectionDef; overview: WorkOverview | null }) {
  const hash = useHash()
  const crumbs = crumbsFor(hash, section, overview)
  if (crumbs.length < 2) return null
  return (
    <div className="crumbbar" data-tour="crumbs">
      <nav aria-label="Breadcrumb">
        <ol className="crumbs">
          {crumbs.map((c, i) => {
            const last = i === crumbs.length - 1
            const body = (
              <>
                {c.icon && <Icon name={i === 0 ? 'arrow-left' : c.icon} />}
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
    </div>
  )
}

export function AppChrome({ logo, token, repo, nav, sections, section, badges, statuses, data, d, overview, pref, onPref, density, onDensity, onForget, navigationBlocked = false, collapsed, onCollapsed, children }: {
  logo: ReactNode
  /** The Hub sign-in; on this computer's own page, its connection credential. */
  token: string
  repo: string
  nav: NavDef[]
  sections: SectionDef[]
  section: Section
  badges: Badges
  statuses: TabStatuses
  data: HubData
  d: Derived | null
  overview: WorkOverview | null
  pref: ThemePref
  onPref: (p: ThemePref) => void
  density: Density
  onDensity: (d: Density) => void
  onForget: () => void
  navigationBlocked?: boolean
  collapsed: boolean
  onCollapsed: (c: boolean) => void
  children: ReactNode
}) {
  const [palette, setPalette] = useState(false)
  const [drawerAt, setDrawerAt] = useState<Section | null>(null)
  const drawer = drawerAt === section
  const setDrawer = (open: boolean) => setDrawerAt(open ? section : null)
  const burger = useRef<HTMLButtonElement>(null)
  const now = useNow()
  const latest = useRef({ collapsed, onCollapsed, drawer })
  useEffect(() => {
    latest.current = { collapsed, onCollapsed, drawer }
  })
  useEffect(() => {
    if (drawer) window.setTimeout(() => document.querySelector<HTMLElement>('.side.is-open .side-link')?.focus(), 0)
  }, [drawer])
  // A new section starts at the top, with focus on the page so keyboard and screen-reader users land in it.
  const firstSection = useRef(true)
  // Scroll before paint, so the new page never shows for a frame at the old position; focus after, so a tour opened by the same change keeps its own focus order.
  useLayoutEffect(() => {
    if (!firstSection.current) window.scrollTo(0, 0)
  }, [section])
  useEffect(() => {
    if (firstSection.current) {
      firstSection.current = false
      return
    }
    document.getElementById('main')?.focus({ preventScroll: true })
  }, [section])
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing = e.target instanceof HTMLElement && (/^(INPUT|TEXTAREA|SELECT)$/.test(e.target.tagName) || e.target.isContentEditable) || !loadSingleKeys()
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
  const fresh = data.lastOk ? ` Updated ${relative(data.lastOk / 1000, now)}.` : ''
  const healthTip = data.failing
    ? 'The hub did not answer the last refresh. Showing the last data that loaded; retrying with backoff.'
    : data.health?.status === 'unavailable'
      ? 'The hub reports its storage is unavailable. Open Setup › Hub for details.'
      : data.health
        ? `The hub is answering; live data refreshes every 5 seconds while this tab is visible.${fresh}`
        : 'Checking the hub…'
  const isMac = /Mac|iPhone|iPad/.test(navigator.platform)
  const current = sections.find((s) => s.key === section) ?? sections[0]!
  const inbox = badges.inbox
  const chrome = useMemo(() => ({ sections, section, statuses }), [sections, section, statuses])

  return (
    <ChromeContext.Provider value={chrome}>
      <div className={`app-shell${collapsed ? ' rail-compact' : ''}`}>
        <a href="#main" className="skip-link" onClick={(e) => {
          e.preventDefault()
          document.getElementById('main')?.focus()
        }}>
          Skip to content
        </a>
        <header className="topbar" data-tour="topbar">
          <button ref={burger} type="button" data-tour="menu" className="burger" aria-label="Open navigation" aria-expanded={drawer} onClick={() => setDrawer(!drawer)}>
            <Icon name={drawer ? 'x-lg' : 'list'} />
          </button>
          <a href="#home" className="topbar-logo" aria-label="AgentSCM Hub home">
            {logo}
          </a>
          {isLocalCredential(token) ? <LocalRepoSwitcher token={token} repo={repo} navigationBlocked={navigationBlocked} /> : <RepoSwitcher repo={repo} onForget={onForget} />}

          <button type="button" data-tour="search" className="search-pill" onClick={() => setPalette(true)} aria-label="Search the hub">
            <Icon name="search" />
            <span className="search-text">Search agents, files, work…</span>
            <kbd>{isMac ? '⌘' : 'Ctrl'} K</kbd>
          </button>

          <div className="topbar-right">
            <Tip text={healthTip} placement="bottom">
              <a href="#health" data-tour="live" className={`health-chip${healthOk ? '' : ' is-bad'}`} aria-label={healthTip}>
                <span className="live-dot" aria-hidden="true" />
                <span className="health-text">{data.failing ? 'Reconnecting' : healthOk ? 'Hub live' : data.health ? 'Degraded' : '…'}</span>
              </a>
            </Tip>
            <TourTrigger section={section} />
            <Tip text={inbox ? inbox.tip : 'Inbox: overlaps and requests that need a conversation'} placement="bottom">
              <a href="#inbox" data-tour="bell" className="top-icon bell" aria-label={inbox ? `Inbox, ${inbox.tip}` : 'Inbox'}>
                <Icon name="bell" />
                {inbox && <span className={`bell-count tone-${inbox.tone}`} aria-hidden="true">{inbox.n}</span>}
              </a>
            </Tip>
            <ViewMenu pref={pref} onPref={onPref} density={density} onDensity={onDensity} />
            <AccountMenu data={data} onForget={onForget} />
          </div>
        </header>
        <SideNav nav={nav} sections={sections} section={section} badges={badges} collapsed={collapsed} onCollapse={() => onCollapsed(!collapsed)} drawer={drawer} onClose={() => setDrawer(false)} />
        <div className="content">
          <Breadcrumbs section={current} overview={overview} />
          <main className="main" id="main" tabIndex={-1} data-tour="main" data-section={current.key}>
            {children}
          </main>
        </div>
        {palette && <CommandPalette onClose={() => setPalette(false)} sections={sections} section={section} d={d} overview={overview} />}
      </div>
    </ChromeContext.Provider>
  )
}
