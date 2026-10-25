import { useCallback, useEffect, useLayoutEffect, useMemo, useState, useSyncExternalStore } from 'react'
import { derive } from './derive.ts'
import { fetchOverview } from './standup.ts'
import { clearToken, loadDensity, loadTheme, loadToken, saveDensity, saveTheme, saveToken, type Density, type ThemePref } from './storage.ts'
import { useHub } from './useHub.ts'
import logoDark from './logo-dark.png'
import logoLight from './logo-light.png'
import { ActivityPage } from './pages/Activity.tsx'
import { HealthPage } from './pages/Health.tsx'
import { NowPage } from './pages/Now.tsx'
import { SetupPage } from './pages/Setup.tsx'
import { SignIn } from './pages/SignIn.tsx'
import { WorkPage } from './pages/Work.tsx'
import { WorkflowsPage } from './pages/Workflows.tsx'
import { ChangeRecordPage } from './pages/ChangeRecord.tsx'
import { WorkspacePage } from './pages/Workspace.tsx'
import { LandedPage } from './pages/Landed.tsx'
import { SkillsPage } from './pages/Skills.tsx'
import { TemplatesPage } from './pages/Templates.tsx'
import { TimelinePage } from './pages/Timeline.tsx'
import { ExplorerPage } from './pages/Explorer.tsx'
import { SettingsPage } from './pages/Settings.tsx'
import { AppChrome, type Badges, type SectionDef } from './TopNav.tsx'
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

export type Section = 'now' | 'work' | 'activity' | 'workflows' | 'change-record' | 'landed' | 'skills' | 'templates' | 'timeline' | 'explorer' | 'setup' | 'workspace' | 'health' | 'settings'

export const SECTIONS: SectionDef[] = [
  { key: 'now', icon: 'broadcast', label: 'Right now', question: 'Who is editing which files?', group: 'Coordinate' },
  { key: 'work', icon: 'kanban', label: 'Work', question: 'Who owns which task?', group: 'Coordinate' },
  { key: 'activity', icon: 'clock-history', label: 'Activity', question: 'What just happened?', group: 'Coordinate' },
  { key: 'workflows', icon: 'diagram-3', label: 'Workflows', question: 'Did the checks pass?', group: 'Verify' },
  { key: 'change-record', icon: 'clipboard2-check', label: 'Change records', question: 'What is ready to hand off?', group: 'Verify' },
  { key: 'landed', icon: 'rocket-takeoff', label: 'Landed changes', question: 'Did our changes land, and did checks pass after?', group: 'Verify' },
  { key: 'skills', icon: 'journal-check', label: 'Skills', question: 'Which workflows do agents in this repository have to follow?', group: 'Verify' },
  { key: 'templates', icon: 'journal-code', label: 'Templates', question: 'Which workflow recipes can agents reuse?', group: 'This computer' },
  { key: 'timeline', icon: 'camera', label: 'Timeline', question: 'What did this workspace look like earlier?', group: 'This computer' },
  { key: 'explorer', icon: 'folder2-open', label: 'Repository explorer', question: 'How did this code and its evidence come together?', group: 'This computer' },
  { key: 'workspace', icon: 'people', label: 'Workspace', question: 'Who can run what?', group: 'Manage' },
  { key: 'setup', icon: 'plug', label: 'Connect agents', question: 'Is my agent set up?', group: 'Manage' },
  { key: 'health', icon: 'heart-pulse', label: 'Hub health', question: 'Is the hub OK?', group: 'Manage' },
  { key: 'settings', icon: 'sliders', label: 'Settings', question: 'How is this page set up?', group: 'Manage' },
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

export function sectionFromHash(raw = window.location.hash): Section {
  const h = hashPath(raw)
  if (h.startsWith('#coordination/work/')) return 'work'
  const first = h.slice(1).split('/')[0]
  if (first === 'coordination') return 'now'
  return SECTIONS.some((s) => s.key === first) ? (first as Section) : 'now'
}

export function useSection(): Section {
  const [section, setSection] = useState(() => sectionFromHash())
  useEffect(() => {
    const on = () => setSection(sectionFromHash())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return section
}

export function Logo({ dark, height = 32 }: { dark: boolean; height?: number }) {
  return <img src={dark ? logoDark : logoLight} alt="AgentSCM" height={height} className="logo" />
}

function Shell({ token, dark, pref, onPref, density, onDensity, onUnauthorized, onForget }: { token: string; dark: boolean; pref: ThemePref; onPref: (p: ThemePref) => void; density: Density; onDensity: (d: Density) => void; onUnauthorized: (m?: string) => void; onForget: () => void }) {
  const data = useHub(token, onUnauthorized)
  const section = useSection()
  const d = useMemo(() => (data.work && data.activity ? derive(data.work, data.activity) : null), [data.work, data.activity])
  const overview = usePoll((signal) => fetchOverview(token, signal), 15_000, token)
  const repo = data.whoami?.token.repository ?? 'this repository'
  const badges: Badges = {}
  if (d && d.contested.length > 0) badges.now = { n: d.contested.length, tone: 'bad', tip: `${d.contested.length} contested file${d.contested.length === 1 ? '' : 's'}` }
  if (overview.kind === 'ready' && overview.value.overlaps.length > 0) badges.work = { n: overview.value.overlaps.length, tone: 'warn', tip: `${overview.value.overlaps.length} declared-scope overlap${overview.value.overlaps.length === 1 ? '' : 's'} to coordinate` }

  const [rail, setRail] = useState(loadRail)
  const changeRail = useCallback((v: boolean) => {
    setRail(v)
    saveRail(v)
  }, [])

  return (
    <TourProvider section={section} data={data} d={d} overview={overview.kind === 'ready' ? overview.value : null}>
      <AppChrome
        logo={<Logo dark={dark} height={26} />}
        repo={repo}
        sections={SECTIONS}
        section={section}
        badges={badges}
        data={data}
        d={d}
        overview={overview.kind === 'ready' ? overview.value : null}
        pref={pref}
        onPref={onPref}
        onForget={onForget}
        collapsed={rail}
        onCollapsed={changeRail}
      >
        {data.health?.demo && (
          <Notice tone="brand" icon="play-circle" title="Demo hub">
            Simulated agents working on your repository’s file names. Nothing is written to your repository; this page stops when <code>agentscm demo</code> does. To set it up for real, stop the demo and run <code>agentscm quickstart</code> (or <code>npx agentscm</code>).
          </Notice>
        )}
        {section === 'now' && <NowPage repo={repo} data={data} d={d} overview={overview} />}
        {section === 'work' && <WorkPage token={token} overview={overview} />}
        {section === 'activity' && <ActivityPage d={d} events={data.activity?.events ?? null} fresh={data.fresh} repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'workflows' && <WorkflowsPage token={token} repository={data.whoami?.token.repository ?? null} />}
        {section === 'change-record' && <ChangeRecordPage repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'landed' && <LandedPage repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'skills' && <SkillsPage repo={data.whoami?.token.repository ?? null} token={token} />}
        {section === 'templates' && <TemplatesPage />}
        {section === 'timeline' && <TimelinePage />}
        {section === 'explorer' && <ExplorerPage hubRepository={data.whoami?.token.repository ?? null} />}
        {section === 'setup' && <SetupPage token={token} />}
        {section === 'workspace' && <WorkspacePage />}
        {section === 'health' && <HealthPage health={data.health} />}
        {section === 'settings' && <SettingsPage token={token} whoami={data.whoami} pref={pref} onPref={onPref} density={density} onDensity={onDensity} railCollapsed={rail} onRail={changeRail} onForget={onForget} />}
      </AppChrome>
    </TourProvider>
  )
}

export default function App() {
  const [token, setToken] = useState<string | null>(loadToken)
  const [notice, setNotice] = useState<string | null>(null)
  const [pref, setPref] = useState<ThemePref>(loadTheme)
  const [density, setDensity] = useState<Density>(loadDensity)
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
  const onUnauthorized = useCallback((m?: string) => signedOut(m ?? 'Your token was rejected or has expired. Paste a current one to continue.'), [signedOut])
  const onForget = useCallback(() => signedOut('Token forgotten on this browser. The hub still accepts it until it expires or is revoked.'), [signedOut])

  if (token) return <Shell key={token} token={token} dark={dark} pref={pref} onPref={changePref} density={density} onDensity={changeDensity} onUnauthorized={onUnauthorized} onForget={onForget} />
  if (section === 'workspace')
    return (
      <div className="signed-out-workspace">
        <a href="#now" className="btn btn-link">
          <Icon name="arrow-left" /> Back to sign-in
        </a>
        <WorkspacePage />
      </div>
    )
  return (
    <SignIn
      dark={dark}
      notice={notice}
      onSignedIn={(t, keep) => {
        saveToken(t, keep)
        setNotice(null)
        setToken(t)
      }}
    />
  )
}
