import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from 'react'
import { ComputerNotices, LocalComputerProvider } from './ComputerBar.tsx'
import { derive } from './derive.ts'
import { isLocalCredential, onFrontDoor } from './frontDoor.ts'
import { fetchOverview } from './standup.ts'
import { fragmentLinkRefused, LINK_REFUSED } from './fragmentToken.ts'
import { clearToken, loadDensity, loadTheme, loadToken, saveDensity, saveTheme, saveToken, signInAfterRefusal, type Density, type ThemePref } from './storage.ts'
import { useHub } from './useHub.ts'
import { useHidden } from './dismiss.tsx'
import { needHideKey } from './needs.ts'
import logoDark from './logo-dark.png'
import logoLight from './logo-light.png'
import { DashboardPage, dashboardStatus } from './pages/Dashboard.tsx'
import { HealthPage } from './pages/Health.tsx'
import { AdminPage, ComputersPage, useDeviceAuth } from './pages/Computers.tsx'
import type { BrowserSession } from './devices.ts'
import { SetupPage } from './pages/Setup.tsx'
import { SignIn } from './pages/SignIn.tsx'
import { Opening } from './pages/Opening.tsx'
import { morphToApp } from './viewTransition.ts'
import { isSignedInHash, SignedInPage } from './pages/SignedIn.tsx'
import { InboxPage, WorkPage } from './pages/Work.tsx'
import { handoffOf } from './launchLink.ts'
import { WorkflowsPage } from './pages/Workflows.tsx'
import { ChangeRecordPage } from './pages/ChangeRecord.tsx'
import { WorkspacePage } from './pages/Workspace.tsx'
import { LandedPage } from './pages/Landed.tsx'
import { SkillsPage } from './pages/Skills.tsx'
import { TemplatesPage } from './pages/Templates.tsx'
import { TimelinePage } from './pages/Timeline.tsx'
import { ExplorerPage } from './pages/Explorer.tsx'
import { SettingsPage } from './pages/Settings.tsx'
import { PromisesPage } from './pages/Promises.tsx'
import { RepositoriesPage } from './pages/Repositories.tsx'
import { AppChrome, type Badges, type NavDef, type SectionDef, type TabStatuses, type WebChoice } from './TopNav.tsx'
import { useWebRead, type WebRead } from './webRead.ts'
import { Notice } from './ui.tsx'
import { usePoll } from './usePoll.ts'
import { TourProvider } from './tour/Tour.tsx'
import { InstallOffer } from './InstallOffer.tsx'
import { useAppBadge } from './install.ts'
import { hashPath } from './Pager.tsx'
import { GLive } from './g/GLive.tsx'
import { gAnswer } from './g/answers.tsx'
import { useLayout } from './g/layout.ts'
import { needsYou } from './needs.ts'

const darkQuery = '(prefers-color-scheme: dark)'
const subscribeDark = (cb: () => void) => {
  const mq = window.matchMedia(darkQuery)
  mq.addEventListener('change', cb)
  return () => mq.removeEventListener('change', cb)
}
const systemDark = () => window.matchMedia(darkQuery).matches

export type Section = 'home' | 'now' | 'activity' | 'team' | 'inbox' | 'work' | 'promises' | 'workflows' | 'change-record' | 'landed' | 'skills' | 'templates' | 'timeline' | 'explorer' | 'setup' | 'workspace' | 'computers' | 'admin' | 'health' | 'settings' | 'repositories'

/** Every routed page. `label` names its tab inside its nav item; `question` is the page's hidden h1. */
export const SECTIONS: SectionDef[] = [
  { key: 'home', nav: 'home', icon: 'activity', label: 'Now', question: 'What needs me, and where is every task?' },
  { key: 'now', nav: 'home', icon: 'grid-1x2', label: 'Where', question: 'Who is editing which files?' },
  { key: 'activity', nav: 'home', icon: 'clock-history', label: 'Catch up', question: 'What happened since I last looked?' },
  { key: 'team', nav: 'home', icon: 'people', label: 'Team', question: 'Who is working right now?' },
  { key: 'inbox', nav: 'inbox', icon: 'inbox', label: 'Inbox', question: 'Which overlaps need a conversation?' },
  { key: 'work', nav: 'history', icon: 'flag', label: 'Finished', question: 'What did agents report finished?' },
  { key: 'promises', nav: 'history', icon: 'journal-check', label: 'Promises', question: 'What must remain true?' },
  { key: 'change-record', nav: 'history', icon: 'clipboard2-check', label: 'Change records', question: 'What is ready to hand off?' },
  { key: 'explorer', nav: 'history', icon: 'folder2-open', label: 'Files', question: 'What happened around this code?' },
  { key: 'timeline', nav: 'timeline', icon: 'camera', label: 'Timeline', question: 'What did this workspace look like earlier?' },
  { key: 'skills', nav: 'skills', icon: 'journal-check', label: 'Skills', question: 'Which workflows are agents in this repository expected to follow?' },
  { key: 'workflows', nav: 'skills', icon: 'diagram-3', label: 'Checks', question: 'Did the checks pass?' },
  { key: 'landed', nav: 'skills', icon: 'rocket-takeoff', label: 'After merge', question: 'Were our agents’ changes merged, and what happened after?' },
  { key: 'templates', nav: 'skills', icon: 'journal-code', label: 'Library', question: 'Which reusable workflows can agents configure here?' },
  { key: 'setup', nav: 'setup', icon: 'plug', label: 'Agents', question: 'Is my agent set up?' },
  { key: 'health', nav: 'setup', icon: 'heart-pulse', label: 'Hub', question: 'Is the hub OK?' },
  { key: 'workspace', nav: 'setup', icon: 'people', label: 'Access', question: 'Who can run what?' },
  { key: 'computers', nav: 'setup', icon: 'laptop', label: 'Computers', question: 'Which computers can act for me?' },
  { key: 'admin', nav: 'setup', icon: 'shield-lock', label: 'Admin', question: 'Which repositories are connected, and who was turned away?' },
  { key: 'settings', nav: 'setup', icon: 'sliders', label: 'Preferences', question: 'How is this page set up for me?' },
  // Outside the rail: reached from the repository menu and the logo.
  { key: 'repositories', nav: 'home', icon: 'collection', label: 'All repositories', question: 'Which of my repositories need me?' },
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
/** Box anchors (`#box-contested`) open the page that holds the box; the page then scrolls to it. */
const BOX_SECTIONS = new Map<string, Section>([
  ['box-needs', 'home'], ['box-track', 'home'], ['box-contested', 'now'], ['box-prs', 'now'], ['box-map', 'now'],
  ['box-agents', 'team'], ['box-log', 'activity'], ['box-overlaps', 'inbox'], ['box-requests', 'inbox'],
])

export function sectionFromHash(raw = window.location.hash): Section {
  const h = hashPath(raw)
  if (h.startsWith('#coordination/work/')) return 'work'
  const [first, second] = h.slice(1).split('/')
  const box = BOX_SECTIONS.get(first ?? '')
  if (box) return box
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

function Shell({ token, web, hubSignedOut = false, hubSignOutError, hubUnavailable, hubChecking = false, hubSessionKnown = false, section, workflowDirty, onWorkflowDirtyChange, onHubSignedOut, onHubSignOutFailed, onHubRetry, onHubSignOut, pref, onPref, density, onDensity, onUnauthorized, onForget }: { token: string; web?: WebChoice; hubSignedOut?: boolean; hubSignOutError?: string | null; hubUnavailable?: { title: string; message: string; retryLabel: string } | null; hubChecking?: boolean; hubSessionKnown?: boolean; section: Section; workflowDirty: boolean; onWorkflowDirtyChange: (dirty: boolean) => void; onHubSignedOut?: (session: BrowserSession) => void; onHubSignOutFailed?: (session: BrowserSession) => void; onHubRetry?: () => void; onHubSignOut?: () => void; pref: ThemePref; onPref: (p: ThemePref) => void; density: Density; onDensity: (d: Density) => void; onUnauthorized: (m?: string) => void; onForget: () => void }) {
  const data = useHub(token, onUnauthorized)
  const d = useMemo(() => (data.work && data.activity ? derive(data.work, data.activity) : null), [data.work, data.activity])
  const overview = usePoll((signal) => token ? fetchOverview(token, signal) : Promise.reject(new Error('Hub pages are signed out')), 15_000, token)
  const repo = data.whoami?.token.repository ?? 'this repository'
  const handoff = handoffOf(data.whoami)
  const device = useDeviceAuth(token)
  const provider = data.whoami?.owner.provider === 'gitlab' ? 'gitlab' : 'github'
  // The Admin tab is only for a browser signed in as a Hub admin; someone who opens #admin anyway keeps the tabs and is told why.
  const showAdmin = section === 'admin' || (device.auth.kind === 'ready' && device.auth.value?.kind === 'browser' && device.auth.value.session.admin)
  const sections = useMemo(() => (showAdmin ? SECTIONS : SECTIONS.filter((s) => s.key !== 'admin')), [showAdmin])
  const ov = overview.kind === 'ready' ? overview.value : null
  // Measured against the last successful poll, not the 1-second clock, so the whole shell re-renders only when data arrives.
  const dash = dashboardStatus(ov, d, data.activity?.events ?? null, data.lastOk, useHidden())
  useAppBadge(hubSignedOut ? 0 : dash.needs.length)
  const badges: Badges = {}
  if (dash.needs.length > 0) badges.home = { n: dash.needs.length, tone: dash.needs.some((n) => n.tone === 'bad') ? 'bad' : 'warn', tip: `${dash.needs.length} thing${dash.needs.length === 1 ? ' needs' : 's need'} you` }
  if (ov && ov.overlaps.length > 0) badges.inbox = { n: ov.overlaps.length, tone: 'warn', tip: `${ov.overlaps.length} overlap${ov.overlaps.length === 1 ? '' : 's'} to talk about` }
  const finished = ov?.recent.filter((o) => o.kind === 'completion').length
  const statuses: TabStatuses = {
    home: dash.home,
    now: dash.now,
    activity: dash.activity,
    team: dash.team,
    work: finished === undefined ? undefined : { text: finished ? `${finished} recently` : 'None recently', tone: finished ? 'ok' : 'muted' },
    'work-history': { text: 'Every change, paged', tone: 'muted' },
    'change-record': { text: 'Per task', tone: 'muted' },
    promises: { text: 'Owner commitments', tone: 'muted' },
    explorer: { text: 'From this computer', tone: 'muted' },
    skills: { text: 'Required steps', tone: 'muted' },
    workflows: { text: 'Recorded runs', tone: 'muted' },
    landed: { text: 'CI and deploys', tone: 'muted' },
    templates: { text: 'From this computer', tone: 'muted' },
    setup: { text: 'Hooks and tokens', tone: 'muted' },
    health: data.failing ? { text: 'Not answering', tone: 'bad' } : data.health ? { text: data.health.status === 'ok' ? 'Answering' : 'Degraded', tone: data.health.status === 'ok' ? 'ok' : 'warn' } : undefined,
    workspace: { text: 'Members and runs', tone: 'muted' },
    // Computers and Admin read the browser sign-in this shell already loaded, so their tabs say what that sign-in allows.
    computers: device.auth.kind === 'ready' ? { text: device.auth.value?.kind === 'browser' ? 'Signed in here' : 'Needs a web sign-in', tone: 'muted' } : undefined,
    admin: device.auth.kind === 'ready' ? { text: device.auth.value?.kind === 'browser' && device.auth.value.session.admin ? 'You are an admin' : 'Admins only', tone: 'muted' } : undefined,
    repositories: { text: 'Every one you can read', tone: 'muted' },
    settings: { text: 'This browser', tone: 'muted' },
  }

  const [layout, setLayout] = useLayout()
  const hidden = useHidden()
  const [rail, setRail] = useState(loadRail)
  const changeRail = useCallback((v: boolean) => {
    setRail(v)
    saveRail(v)
  }, [])

  /** Every page's element with its live props; the classic chrome and the G layout (`#g`) render the same pages. */
  const page = (s: Section): ReactNode => {
    switch (s) {
      case 'home':
      case 'now':
      case 'activity':
      case 'team':
        return <DashboardPage section={s} question={SECTIONS.find((x) => x.key === s)!.question} token={token} repo={repo} data={data} d={d} overview={overview} answer={layout === 'g' ? gAnswer(s, d, data.activity?.events ?? null, data.lastOk) : undefined} />
      case 'inbox':
        return <InboxPage token={token} overview={overview} handoff={handoff} session={device.auth.kind === 'ready' && device.auth.value?.kind === 'browser' ? device.auth.value.session : null} live={d ? { agents: d.agents, contested: d.contested } : null} />
      case 'work':
        return <WorkPage token={token} overview={overview} handoff={handoff} />
      case 'promises':
        return <PromisesPage />
      case 'workflows':
        return <WorkflowsPage token={token} repository={data.whoami?.token.repository ?? null} />
      case 'change-record':
        return <ChangeRecordPage repo={data.whoami?.token.repository ?? null} token={token} />
      case 'landed':
        return <LandedPage repo={data.whoami?.token.repository ?? null} token={token} />
      case 'skills':
        return <SkillsPage repo={data.whoami?.token.repository ?? null} token={token} />
      case 'templates':
        return <TemplatesPage repository={data.whoami?.token.repository ?? null} onDirtyChange={onWorkflowDirtyChange} />
      case 'timeline':
        return <TimelinePage />
      case 'explorer':
        return <ExplorerPage hubRepository={data.whoami?.token.repository ?? null} />
      case 'setup':
        return <SetupPage token={token} />
      case 'workspace':
        return <WorkspacePage />
      case 'repositories':
        return <RepositoriesPage auth={device.auth} web={web} token={token} current={data.whoami?.token.repository ?? null} overview={overview.kind === 'ready' ? overview.value : null} />
      case 'health':
        return <HealthPage health={data.health} />
      case 'computers':
        return <ComputersPage auth={device.auth} provider={provider} onSignedOut={(session) => { device.reload(); onHubSignedOut?.(session) }} onSignOutFailed={onHubSignOutFailed} />
      case 'admin':
        return <AdminPage auth={device.auth} provider={provider} />
      case 'settings':
        return <SettingsPage token={token} whoami={data.whoami} pref={pref} onPref={onPref} density={density} onDensity={onDensity} railCollapsed={rail} onRail={changeRail} onForget={onForget} />
    }
  }
  if (layout === 'g') {
    const needs = needsYou(ov, d).filter((n) => n.who === 'you' && !hidden.has(needHideKey(n)))
    const banner = (hubSignOutError || hubUnavailable || (hubChecking && hubSessionKnown) || data.health?.demo)
      ? <>
        {(hubSignOutError || hubUnavailable || (hubChecking && hubSessionKnown)) && <Notice tone={hubSignOutError || hubUnavailable ? 'bad' : 'muted'} title={hubSignOutError ? 'Hub sign-out failed' : hubUnavailable ? hubUnavailable.title : 'Checking Hub connection'}>
          {hubSignOutError ?? (hubUnavailable ? hubUnavailable.message : 'Checking which Hub session is active.')}
          {hubUnavailable && onHubRetry && <button type="button" className="btn btn-link" onClick={onHubRetry}>{hubUnavailable.retryLabel}</button>}
        </Notice>}
        {data.health?.demo && <Notice tone="brand" icon="play-circle" title="Demo hub">Simulated agents on your repository’s file names; nothing is written to it.</Notice>}
      </>
      : null
    return (
      <LocalComputerProvider token={isLocalCredential(token) ? token : null}>
        <ComputerNotices />
        <GLive live={{ token, repo, data, d, overview, needs, handoff }} section={section} sections={sections} page={page} showAdmin={!!showAdmin} hubSignedOut={hubSignedOut} onClassic={() => setLayout('classic')} banner={banner} />
      </LocalComputerProvider>
    )
  }
  const shell = (
    <TourProvider key={section === 'promises' || section === 'workspace' || section === 'repositories' ? 'independent' : web ? token : 'hub'} signedIn={!hubSignedOut} joinedAt={device.auth.kind === 'ready' && device.auth.value?.kind === 'browser' ? device.auth.value.session.joinedAt : undefined} section={section} data={data} d={d} overview={overview.kind === 'ready' ? overview.value : null}>
      <AppChrome
        // The frame is deep violet in both themes, so the top bar always carries the light-on-dark logo.
        logo={<Logo dark height={26} />}
        token={token}
        web={web}
        hubSignedOut={hubSignedOut}
        repo={repo}
        nav={NAV}
        sections={sections}
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
        {(hubSignOutError || hubUnavailable || (hubChecking && hubSessionKnown)) && <Notice tone={hubSignOutError || hubUnavailable ? 'bad' : 'muted'} title={hubSignOutError ? 'Hub sign-out failed' : hubUnavailable ? hubUnavailable.title : 'Checking Hub connection'}>
          {hubSignOutError ?? (hubUnavailable ? hubUnavailable.message : 'Checking which Hub session is active.')}
          {hubSignOutError && hubUnavailable && <p>{hubUnavailable.message}</p>}
          {hubSessionKnown && onHubSignOut && <button type="button" className="btn btn-link" onClick={onHubSignOut}>{hubSignOutError ? 'Retry Hub sign-out' : 'Sign out of Hub pages'}</button>}
          {hubUnavailable && onHubRetry && <button type="button" className="btn btn-link" onClick={onHubRetry}>{hubUnavailable.retryLabel}</button>}
        </Notice>}
        {!hubSignedOut && <InstallOffer tipsHere={section === 'home'} />}
        <ComputerNotices />
        {data.health?.demo && (
          <Notice tone="brand" icon="play-circle" title="Demo hub">
            Simulated agents on your repository’s file names; nothing is written to it. To set up for real, stop the demo and run <code>agentscm quickstart</code> (or <code>npx agentscm</code>).
          </Notice>
        )}
        {page(section)}
      </AppChrome>
    </TourProvider>
  )
  // Keep this provider's React position stable as independent pages change credential sources.
  return <LocalComputerProvider token={isLocalCredential(token) ? token : null}>{shell}</LocalComputerProvider>
}

export default function App() {
  const [token, setToken] = useState<string | null>(loadToken)
  const [notice, setNotice] = useState<string | null>(() => (fragmentLinkRefused() ? LINK_REFUSED : null))
  const linkRefused = useRef(fragmentLinkRefused())
  const [pref, setPref] = useState<ThemePref>(loadTheme)
  const [density, setDensity] = useState<Density>(loadDensity)
  // The browser's landing page after signing this computer in; shown with or without a token.
  const [signedInLanding] = useState(isSignedInHash)
  // No pasted token on the hosted Hub: open its pages on the web sign-in, if there is one.
  const web = useWebRead(!token && !onFrontDoor() && !signedInLanding, morphToApp)
  // Someone who can read several repositories lands on All repositories, once per page load.
  const landed = useRef(false)
  useEffect(() => {
    if (landed.current || web.state.kind !== 'ready') return
    landed.current = true
    if (web.state.readable > 1 && ['', '#', '#home'].includes(window.location.hash)) window.location.replace('#repositories')
  }, [web.state])
  const prefersDark = useSyncExternalStore(subscribeDark, systemDark)
  const dark = pref === 'dark' || (pref === 'system' && prefersDark)
  const [workflowDirty, setWorkflowDirty] = useState(false)
  const section = useSection(workflowDirty)

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
      const refused = linkRefused.current
      linkRefused.current = false
      const other = signInAfterRefusal(token, loadToken())
      if (other) return setToken(other)
      signedOut((refused ? LINK_REFUSED : null) ?? m ?? (onFrontDoor() ? 'This connection ended, for example because AgentSCM restarted on this computer.' : 'Your token was rejected or has expired. Paste a current one to continue.'))
    },
    [signedOut, token],
  )
  const onForget = useCallback(() => signedOut('Token forgotten on this browser. The hub still accepts it until it expires or is revoked.'), [signedOut])
  const independent = section === 'workspace' || section === 'promises' || section === 'repositories'
  const unavailable = web.state.kind === 'unavailable' ? unavailableCopy(web.state) : null
  const acceptToken = (t: string, keep: boolean) => {
    saveToken(t, keep)
    setNotice(null)
    setToken(t)
  }

  if (signedInLanding) return <SignedInPage />
  if (token) return <Shell key={independent ? 'independent' : token} token={token} section={section} workflowDirty={workflowDirty} onWorkflowDirtyChange={setWorkflowDirty} pref={pref} onPref={changePref} density={density} onDensity={changeDensity} onUnauthorized={onUnauthorized} onForget={onForget} />
  if (web.state.kind === 'ready' || independent) {
    const ready = web.state.kind === 'ready' ? web.state : null
    const hubSessionKnown = web.state.kind === 'ready' || ((web.state.kind === 'checking' || web.state.kind === 'unavailable') && !!web.state.session)
    // Every credential source keeps one independent-page tree; Hub pages still reset with their token.
    return <Shell key={independent ? 'independent' : 'web'} token={ready?.read.token ?? ''} web={ready ? { repos: ready.repos, pick: web.pick, refusal: ready.refusal, list: ready.list } : undefined} hubSignedOut={!ready} hubSignOutError={web.signOutError} hubUnavailable={unavailable} hubChecking={web.state.kind === 'checking'} hubSessionKnown={hubSessionKnown} section={section} workflowDirty={workflowDirty} onWorkflowDirtyChange={setWorkflowDirty} onHubSignedOut={web.confirmSignedOut} onHubSignOutFailed={web.reportSignOutFailure} onHubRetry={web.retry} onHubSignOut={() => void web.signOut()} pref={pref} onPref={changePref} density={density} onDensity={changeDensity} onUnauthorized={web.renew} onForget={() => void web.signOut()} />
  }
  if (web.state.kind === 'checking' || web.state.kind === 'unavailable') {
    const { state } = web
    const compact = state.kind === 'unavailable' && state.reason !== 'connection'
    const title = web.signOutError ? 'Hub sign-out failed' : unavailable?.title ?? 'Checking Hub connection'
    const warning = web.signOutError ? `${web.signOutError} ${unavailable?.message ?? ''}` : unavailable?.message ?? null
    if (!compact) {
      const trouble = warning === null ? null : { title, message: warning, onRetry: unavailable ? web.retry : undefined, retryLabel: unavailable?.retryLabel, onSignOut: state.session ? () => void web.signOut() : undefined, signOutLabel: warning.startsWith('Hub sign-out') ? 'Retry Hub sign-out' : 'Sign out of Hub pages' }
      return <Opening login={state.session?.login} name={state.session?.name} repos={state.repos} trouble={trouble} />
    }
    return <>
      <div className="opening opening-inline" role="status" aria-label={title}>
        <Logo dark height={32} />
        <Notice tone="bad" title={title}>
          {warning}
          {state.session && <button type="button" className="btn btn-link" onClick={() => void web.signOut()}>{warning?.startsWith('Hub sign-out') ? 'Retry Hub sign-out' : 'Sign out of Hub pages'}</button>}
          {unavailable && <button type="button" className="btn btn-link" onClick={web.retry}>{unavailable.retryLabel}</button>}
        </Notice>
      </div>
      <SignIn notice={null} refusal={state.kind === 'unavailable' ? state.refusal : null} onSignedIn={acceptToken} />
    </>
  }
  return (
    <SignIn
      notice={notice ?? (web.state.kind === 'none' && web.state.empty ? 'Your web sign-in has no repository to open yet. Connect an agent or this computer to one first, or paste a read-only token.' : null)}
      refusal={web.state.kind === 'none' ? web.state.refusal : null}
      onSignedIn={acceptToken}
    />
  )
}

function unavailableCopy(state: Extract<WebRead, { kind: 'unavailable' }>): { title: string; message: string; retryLabel: string } {
  if (state.reason === 'empty') return { title: 'No repository available', message: 'Your web sign-in has no repository to open yet. Connect an agent or this computer to one first, or paste a read-only token.', retryLabel: 'Check repositories again' }
  if (state.reason === 'refused') return { title: 'Hub access refused', message: state.refusal?.message ?? 'This web sign-in cannot open any connected repository. Ask a repository owner for access or paste a read-only token.', retryLabel: 'Check repositories again' }
  return { title: 'Hub connection unavailable', message: state.refusal?.message ?? 'The browser sign-in could not be checked. Try the connection again.', retryLabel: 'Retry Hub connection' }
}
