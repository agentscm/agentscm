import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import { ComputerNotices, LocalComputerProvider } from './ComputerBar.tsx'
import { derive } from './derive.ts'
import { isLocalCredential, onFrontDoor } from './frontDoor.ts'
import { fetchOverview } from './standup.ts'
import { clearToken, loadDensity, loadTheme, loadToken, saveDensity, saveTheme, saveToken, signInAfterRefusal, type Density, type ThemePref } from './storage.ts'
import { useHub } from './useHub.ts'
import { useHidden } from './dismiss.tsx'
import logoDark from './logo-dark.png'
import logoLight from './logo-light.png'
import { DashboardPage, dashboardStatus } from './pages/Dashboard.tsx'
import { HealthPage } from './pages/Health.tsx'
import { AdminPage, ComputersPage } from './pages/Computers.tsx'
import { SetupPage } from './pages/Setup.tsx'
import { SignIn } from './pages/SignIn.tsx'
import { isSignedInHash, SignedInPage } from './pages/SignedIn.tsx'
import { InboxPage, WorkPage } from './pages/Work.tsx'
import { WorkflowsPage } from './pages/Workflows.tsx'
import { ChangeRecordPage } from './pages/ChangeRecord.tsx'
import { WorkspacePage } from './pages/Workspace.tsx'
import { LandedPage } from './pages/Landed.tsx'
import { SkillsPage } from './pages/Skills.tsx'
import { TemplatesPage } from './pages/Templates.tsx'
import { TimelinePage } from './pages/Timeline.tsx'
import { ExplorerPage } from './pages/Explorer.tsx'
import { SettingsPage } from './pages/Settings.tsx'
import { AppChrome, type Badges, type NavDef, type SectionDef, type TabStatuses } from './TopNav.tsx'
import { Icon, Notice } from './ui.tsx'
import { usePoll } from './usePoll.ts'
import { TourProvider } from './tour/Tour.tsx'
import { hashPath } from './Pager.tsx'

const darkQuery = '(prefers-color-scheme: dark)'
const subscribeDark = (cb: () => void) => {
  const mq = window.matchMedia(darkQuery)
  mq.addEventListener('change', cb)
  return () => mq.removeEventListener('change', cb)
}
const systemDark = () => window.matchMedia(darkQuery).matches

export type Section = 'home' | 'now' | 'activity' | 'team' | 'inbox' | 'work' | 'workflows' | 'change-record' | 'landed' | 'skills' | 'templates' | 'timeline' | 'explorer' | 'setup' | 'workspace' | 'computers' | 'admin' | 'health' | 'settings'

/** Every routed page. `label` names its tab inside its nav item; `question` is the page's hidden h1. */
export const SECTIONS: SectionDef[] = [
  { key: 'home', nav: 'home', icon: 'activity', label: 'Now', question: 'What needs me, and where is every task?' },
  { key: 'now', nav: 'home', icon: 'grid-1x2', label: 'Where', question: 'Who is editing which files?' },
  { key: 'activity', nav: 'home', icon: 'clock-history', label: 'Catch up', question: 'What happened since I last looked?' },
  { key: 'team', nav: 'home', icon: 'people', label: 'Team', question: 'Who is working right now?' },
  { key: 'inbox', nav: 'inbox', icon: 'inbox', label: 'Inbox', question: 'Which overlaps need a conversation?' },
  { key: 'work', nav: 'history', icon: 'flag', label: 'Finished', question: 'What did agents report finished?' },
  { key: 'change-record', nav: 'history', icon: 'clipboard2-check', label: 'Change records', question: 'What is ready to hand off?' },
  { key: 'explorer', nav: 'history', icon: 'folder2-open', label: 'Files', question: 'How did this code and its evidence come together?' },
  { key: 'timeline', nav: 'timeline', icon: 'camera', label: 'Timeline', question: 'What did this workspace look like earlier?' },
  { key: 'skills', nav: 'skills', icon: 'journal-check', label: 'Skills', question: 'Which workflows do agents in this repository have to follow?' },
  { key: 'workflows', nav: 'skills', icon: 'diagram-3', label: 'Checks', question: 'Did the checks pass?' },
  { key: 'landed', nav: 'skills', icon: 'rocket-takeoff', label: 'After merge', question: 'Did our changes land, and did checks pass after?' },
  { key: 'templates', nav: 'skills', icon: 'journal-code', label: 'Library', question: 'Which reusable workflows can agents configure here?' },
  { key: 'setup', nav: 'setup', icon: 'plug', label: 'Agents', question: 'Is my agent set up?' },
  { key: 'health', nav: 'setup', icon: 'heart-pulse', label: 'Hub', question: 'Is the hub OK?' },
  { key: 'workspace', nav: 'setup', icon: 'people', label: 'Access', question: 'Who can run what?' },
  { key: 'computers', nav: 'setup', icon: 'laptop', label: 'Computers', question: 'Which computers can act for me?' },
  { key: 'admin', nav: 'setup', icon: 'shield-lock', label: 'Admin', question: 'Which repositories are connected, and who was turned away?' },
  { key: 'settings', nav: 'setup', icon: 'sliders', label: 'Preferences', question: 'How is this page set up?' },
]

/** The rail: one item per question; items with more than one page show them as tabs. */
export const NAV: NavDef[] = [
  { key: 'home', icon: 'activity', label: 'Dashboard', question: 'What’s happening?' },
  { key: 'inbox', icon: 'inbox', label: 'Inbox', question: 'What needs a conversation?' },
  { key: 'history', icon: 'clock-history', label: 'History', question: 'What happened before?' },
  { key: 'timeline', icon: 'camera', label: 'Timeline', question: 'What did it look like earlier?' },
  { key: 'skills', icon: 'lightning-charge', label: 'Skills', question: 'Did the agreed steps run?' },
  { key: 'setup', icon: 'plug', label: 'Setup', question: 'Is everything connected?' },
]

const RAIL_KEY = 'agentscm.hub.railCompact'
// Storage can throw (private mode, blocked site data); the rail then just starts expanded.
export const loadRail = (): boolean => {
  try {
    return window.localStorage.getItem(RAIL_KEY) === '1'
  } catch {
    return false
  }
}
export const saveRail = (v: boolean) => {
  try {
    window.localStorage.setItem(RAIL_KEY, v ? '1' : '0')
  } catch {
    /* not persisted */
  }
}

/** History's other tabs by their visible names, so `#work/files` or `#history/files` opens the tab it names. */
const HISTORY_TABS = new Map<string, Section>([['files', 'explorer'], ['change-records', 'change-record']])

export function sectionFromHash(raw = window.location.hash): Section {
  const h = hashPath(raw)
  if (h.startsWith('#coordination/work/')) return 'work'
  const [first, second] = h.slice(1).split('/')
  // The Work board became the dashboard's track and its coordination tab became the Inbox; their links keep working.
  if (first === 'work') return second === 'coordination' ? 'inbox' : !second || second === 'working' ? 'home' : (HISTORY_TABS.get(second) ?? 'work')
  if (first === 'history') return HISTORY_TABS.get(second ?? '') ?? 'work'
  if (first === 'coordination' || first === '') return 'home'
  return SECTIONS.some((s) => s.key === first) ? (first as Section) : 'home'
}

export function useSection(blocked = false): Section {
  const [section, setSection] = useState(() => sectionFromHash())
  const protectedHash = useRef(window.location.hash)
  useEffect(() => {
    const on = () => {
      const next = sectionFromHash()
      if (next === section) {
        if (!blocked) protectedHash.current = window.location.hash
        return
      }
      if (blocked && !window.confirm('Discard unsaved workflow edits?')) {
        history.replaceState(null, '', `${window.location.pathname}${window.location.search}${protectedHash.current}`)
        return
      }
      protectedHash.current = window.location.hash
      setSection(next)
    }
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [blocked, section])
  return section
}

export function Logo({ dark, height = 32 }: { dark: boolean; height?: number }) {
  return <img src={dark ? logoDark : logoLight} alt="AgentSCM" height={height} className="logo" />
}

function Shell({ token, pref, onPref, density, onDensity, onUnauthorized, onForget }: { token: string; pref: ThemePref; onPref: (p: ThemePref) => void; density: Density; onDensity: (d: Density) => void; onUnauthorized: (m?: string) => void; onForget: () => void }) {
  const data = useHub(token, onUnauthorized)
  const [workflowDirty, setWorkflowDirty] = useState(false)
  const section = useSection(workflowDirty)
  const d = useMemo(() => (data.work && data.activity ? derive(data.work, data.activity) : null), [data.work, data.activity])
  const overview = usePoll((signal) => fetchOverview(token, signal), 15_000, token)
  const repo = data.whoami?.token.repository ?? 'this repository'
  const ov = overview.kind === 'ready' ? overview.value : null
  // Measured against the last successful poll, not the 1-second clock, so the whole shell re-renders only when data arrives.
  const dash = dashboardStatus(ov, d, data.activity?.events ?? null, data.lastOk, useHidden())
  const badges: Badges = {}
  if (dash.needs.length > 0) badges.home = { n: dash.needs.length, tone: dash.needs.some((n) => n.tone === 'bad') ? 'bad' : 'warn', tip: `${dash.needs.length} thing${dash.needs.length === 1 ? ' needs' : 's need'} you` }
  if (ov && ov.overlaps.length > 0) badges.inbox = { n: ov.overlaps.length, tone: 'warn', tip: `${ov.overlaps.length} declared-scope overlap${ov.overlaps.length === 1 ? '' : 's'} to coordinate` }
  const finished = ov?.recent.filter((o) => o.kind === 'completion').length
  const statuses: TabStatuses = {
    home: dash.home,
    now: dash.now,
    activity: dash.activity,
    team: dash.team,
    work: finished === undefined ? undefined : { text: finished ? `${finished} recently` : 'None recently', tone: finished ? 'ok' : 'muted' },
    'work-history': { text: 'Every change, paged', tone: 'muted' },
    'change-record': { text: 'Per task', tone: 'muted' },
    explorer: { text: 'From this computer', tone: 'muted' },
    skills: { text: 'Required steps', tone: 'muted' },
    workflows: { text: 'Recorded runs', tone: 'muted' },
    landed: { text: 'CI and deploys', tone: 'muted' },
    templates: { text: 'From this computer', tone: 'muted' },
    setup: { text: 'Hooks and tokens', tone: 'muted' },
    health: data.failing ? { text: 'Not answering', tone: 'bad' } : data.health ? { text: data.health.status === 'ok' ? 'Answering' : 'Degraded', tone: data.health.status === 'ok' ? 'ok' : 'warn' } : undefined,
    workspace: { text: 'Members and runs', tone: 'muted' },
    settings: { text: 'This browser', tone: 'muted' },
  }

  const [rail, setRail] = useState(loadRail)
  const changeRail = useCallback((v: boolean) => {
    setRail(v)
    saveRail(v)
  }, [])

  const shell = (
    <TourProvider section={section} data={data} d={d} overview={overview.kind === 'ready' ? overview.value : null}>
      <AppChrome
        // The frame is deep violet in both themes, so the top bar always carries the light-on-dark logo.
        logo={<Logo dark height={26} />}
        token={token}
        repo={repo}
        nav={NAV}
        sections={SECTIONS}
        section={section}
        badges={badges}
        statuses={statuses}
        data={data}
        d={d}
        overview={overview.kind === 'ready' ? overview.value : null}
        pref={pref}
        onPref={onPref}
        density={density}
        onDensity={onDensity}
        onForget={onForget}
        navigationBlocked={workflowDirty}
        collapsed={rail}
        onCollapsed={changeRail}
      >
        <ComputerNotices />
        {data.health?.demo && (
          <Notice tone="brand" icon="play-circle" title="Demo hub">
            Simulated agents working on your repository’s file names. Nothing is written to your repository; this page stops when <code>agentscm demo</code> does. To set it up for real, stop the demo and run <code>agentscm quickstart</code> (or <code>npx agentscm</code>).
          </Notice>
        )}
        {(section === 'home' || section === 'now' || section === 'activity' || section === 'team') && <DashboardPage section={section} question={SECTIONS.find((s) => s.key === section)!.question} token={token} repo={repo} data={data} d={d} overview={overview} />}
        {section === 'inbox' && <InboxPage token={token} overview={overview} />}
        {section === 'work' && <WorkPage token={token} overview={overview} />}
        {section === 'workflows' && <WorkflowsPage token={token} repository={data.whoami?.token.repository ?? null} />}
        {section === 'change-record' && <ChangeRecordPage repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'landed' && <LandedPage repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'skills' && <SkillsPage repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'templates' && <TemplatesPage repository={data.whoami?.token.repository ?? null} onDirtyChange={setWorkflowDirty} />}
        {section === 'timeline' && <TimelinePage />}
        {section === 'explorer' && <ExplorerPage hubRepository={data.whoami?.token.repository ?? null} />}
        {section === 'setup' && <SetupPage token={token} />}
        {section === 'workspace' && <WorkspacePage />}
        {section === 'health' && <HealthPage health={data.health} />}
        {section === 'computers' && <ComputersPage token={token} />}
        {section === 'admin' && <AdminPage token={token} />}
        {section === 'settings' && <SettingsPage token={token} whoami={data.whoami} pref={pref} onPref={onPref} density={density} onDensity={onDensity} railCollapsed={rail} onRail={changeRail} onForget={onForget} />}
      </AppChrome>
    </TourProvider>
  )
  // On this computer's own Hub page, one read of the connection serves the menu, notices and Settings.
  return isLocalCredential(token) ? <LocalComputerProvider token={token}>{shell}</LocalComputerProvider> : shell
}

export default function App() {
  const [token, setToken] = useState<string | null>(loadToken)
  const [notice, setNotice] = useState<string | null>(null)
  const [pref, setPref] = useState<ThemePref>(loadTheme)
  const [density, setDensity] = useState<Density>(loadDensity)
  // The browser's landing page after signing this computer in; shown with or without a token.
  const [signedInLanding] = useState(isSignedInHash)
  const prefersDark = useSyncExternalStore(subscribeDark, systemDark)
  const dark = pref === 'dark' || (pref === 'system' && prefersDark)
  const section = useSection()

  useEffect(() => {
    document.documentElement.dataset['bsTheme'] = dark ? 'dark' : 'light'
  }, [dark])

  useLayoutEffect(() => {
    document.documentElement.dataset['density'] = density
  }, [density])

  const changeDensity = useCallback((d: Density) => {
    setDensity(d)
    saveDensity(d)
  }, [])
  const changePref = useCallback((p: ThemePref) => {
    setPref(p)
    saveTheme(p)
  }, [])
  const signedOut = useCallback((message: string | null) => {
    clearToken()
    setToken(null)
    setNotice(message)
  }, [])
  const onUnauthorized = useCallback(
    (m?: string) => {
      // Another tab may already have signed in or reconnected: use that rather than erasing it.
      const other = signInAfterRefusal(token, loadToken())
      if (other) return setToken(other)
      signedOut(m ?? (onFrontDoor() ? 'This connection ended, for example because AgentSCM restarted on this computer.' : 'Your token was rejected or has expired. Paste a current one to continue.'))
    },
    [signedOut, token],
  )
  const onForget = useCallback(() => signedOut('Token forgotten on this browser. The hub still accepts it until it expires or is revoked.'), [signedOut])

  if (signedInLanding) return <SignedInPage />
  if (token) return <Shell key={token} token={token} pref={pref} onPref={changePref} density={density} onDensity={changeDensity} onUnauthorized={onUnauthorized} onForget={onForget} />
  if (section === 'workspace')
    return (
      <div className="signed-out-workspace">
        <a href="#home" className="btn btn-link">
          <Icon name="arrow-left" /> Back to sign-in
        </a>
        <WorkspacePage />
      </div>
    )
  return (
    <SignIn
      notice={notice}
      onSignedIn={(t, keep) => {
        saveToken(t, keep)
        setNotice(null)
        setToken(t)
      }}
    />
  )
}
