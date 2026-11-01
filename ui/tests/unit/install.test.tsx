import { act, cleanup, fireEvent, render, renderHook, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { InstallOffer } from '../../src/InstallOffer.tsx'
import { TourProvider } from '../../src/tour/Tour.tsx'
import type { HubData } from '../../src/useHub.ts'
import { dockTip, installHint, installKind, listenForLaunches, routeLaunch, setBadge, useAppBadge, useInstall, watchTitleBar, type InstallEnv } from '../../src/install.ts'

const SAFARI_MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15'
const SAFARI_IOS = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1'
const CHROME_MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36'
const CHROME_IOS = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/126.0.0.0 Mobile/15E148 Safari/604.1'
const FIREFOX_WIN = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0'
const FIREFOX_MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:144.0) Gecko/20100101 Firefox/144.0'

const env = (userAgent: string, more: Partial<InstallEnv> = {}): InstallEnv => ({ userAgent, maxTouchPoints: 0, standalone: false, canPrompt: false, ...more })

describe('installKind', () => {
  it.each([
    ['Safari on a Mac', env(SAFARI_MAC), 'safari-mac'],
    ['Safari 16 on a Mac', env(SAFARI_MAC.replace('Version/17.4', 'Version/16.6')), 'none'],
    ['Safari on an iPhone', env(SAFARI_IOS), 'safari-ios'],
    ['iPadOS Safari, which looks like a Mac', env(SAFARI_MAC, { maxTouchPoints: 5 }), 'safari-ios'],
    ['Chrome on iOS', env(CHROME_IOS), 'none'],
    ['Chrome without an install event (already installed)', env(CHROME_MAC), 'none'],
    ['anything that fired beforeinstallprompt', env(CHROME_MAC, { canPrompt: true }), 'chromium'],
    ['Firefox on Windows', env(FIREFOX_WIN), 'firefox-windows'],
    ['an old Firefox on Windows', env(FIREFOX_WIN.replace('Firefox/144', 'Firefox/120')), 'none'],
    ['Firefox on a Mac', env(FIREFOX_MAC), 'none'],
    ['an unknown browser', env('curl/8'), 'none'],
    ['an installed app, even with an event', env(CHROME_MAC, { canPrompt: true, standalone: true }), 'none'],
    ['an installed iOS app', env(SAFARI_IOS, { standalone: true }), 'none'],
  ] as const)('%s', (_name, e, kind) => {
    expect(installKind(e)).toBe(kind)
  })

  it('has a one-line instruction only for the browsers that install from their own menu', () => {
    expect(installHint('safari-mac')).toBe('In Safari, choose File › Add to Dock.')
    expect(installHint('safari-ios')).toBe('Tap Share, then Add to Home Screen.')
    expect(installHint('firefox-windows')).toMatch(/Add to taskbar/)
    expect(installHint('chromium')).toBeNull()
    expect(installHint('none')).toBeNull()
  })
})

const setNavigator = (props: Record<string, unknown>) => {
  for (const [k, v] of Object.entries(props)) Object.defineProperty(window.navigator, k, { value: v, configurable: true })
}
const standalone = (on: boolean) => {
  window.matchMedia = vi.fn((q: string) => ({ matches: on && q.includes('standalone') })) as unknown as typeof window.matchMedia
}
const promptEvent = () => {
  const e = new Event('beforeinstallprompt', { cancelable: true })
  const prompt = vi.fn(() => Promise.resolve())
  Object.assign(e, { prompt })
  act(() => void window.dispatchEvent(e))
  return { e, prompt }
}

beforeEach(() => {
  window.localStorage.clear()
  standalone(false)
  setNavigator({ userAgent: SAFARI_MAC, maxTouchPoints: 0, standalone: undefined })
})
afterEach(() => {
  act(() => void window.dispatchEvent(new Event('appinstalled')))
})

describe('useInstall', () => {
  it('keeps the browser event, prompts once on install, and forgets it after appinstalled', async () => {
    setNavigator({ userAgent: CHROME_MAC })
    const { result } = renderHook(() => useInstall())
    expect(result.current.kind).toBe('none')
    const { e, prompt } = promptEvent()
    expect(e.defaultPrevented).toBe(true)
    expect(result.current.kind).toBe('chromium')
    await act(async () => result.current.install())
    expect(prompt).toHaveBeenCalledTimes(1)
    expect(result.current.kind).toBe('none')
    promptEvent()
    expect(result.current.kind).toBe('chromium')
    act(() => void window.dispatchEvent(new Event('appinstalled')))
    expect(result.current.kind).toBe('none')
  })
})

describe('InstallOffer', () => {
  it('shows the instruction for Safari on a Mac, without a button', () => {
    render(<InstallOffer />)
    expect(screen.getByText(/in your Dock/)).toBeTruthy()
    expect(screen.getByText('In Safari, choose File › Add to Dock.')).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'Install' })).toBeNull()
  })

  it('shows an Install button for a browser with the install event', async () => {
    setNavigator({ userAgent: CHROME_MAC })
    render(<InstallOffer />)
    expect(screen.queryByText(/in your Dock/)).toBeNull()
    const { prompt } = promptEvent()
    await act(async () => void fireEvent.click(screen.getByRole('button', { name: 'Install' })))
    expect(prompt).toHaveBeenCalled()
  })

  it('shows the iPhone instruction', () => {
    setNavigator({ userAgent: SAFARI_IOS })
    render(<InstallOffer />)
    expect(screen.getByText('Tap Share, then Add to Home Screen.')).toBeTruthy()
  })

  it('waits for the Dashboard’s tips only while some are on the page, never for tips that are not showing', () => {
    const data: HubData = { whoami: null, work: null, activity: null, health: null, fresh: new Set(), lastOk: null, failing: false }
    const inTour = (tipsHere: boolean) => <TourProvider section="home" data={data} d={null} overview={null}><InstallOffer tipsHere={tipsHere} /></TourProvider>
    const tour = (on: boolean) => localStorage.setItem('agentscm.tour.v1', JSON.stringify({ welcomeSeen: !on, welcomeOff: !on, autoplay: [], sectionsSeen: [], helpSeen: false }))
    tour(true)
    const waiting = render(inTour(true))
    expect(screen.queryByText(/in your Dock/)).toBeNull() // the tips come first
    waiting.unmount()
    render(inTour(false)) // another page: no tips here, so no waiting
    expect(screen.getByText(/in your Dock/)).toBeTruthy()
    cleanup()
    tour(false)
    render(inTour(true)) // the default: the introduction is off
    expect(screen.getByText(/in your Dock/)).toBeTruthy()
  })

  it('stays gone once dismissed, across a new mount', () => {
    const first = render(<InstallOffer />)
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }))
    expect(screen.queryByText(/in your Dock/)).toBeNull()
    first.unmount()
    render(<InstallOffer />)
    expect(screen.queryByText(/in your Dock/)).toBeNull()
  })

  it('shows nothing in an installed Safari app, already in the Dock', () => {
    standalone(true)
    const { container } = render(<InstallOffer />)
    expect(container.firstChild).toBeNull()
  })

  it('shows nothing in an installed iOS app', () => {
    setNavigator({ userAgent: SAFARI_IOS, standalone: true })
    const { container } = render(<InstallOffer />)
    expect(container.firstChild).toBeNull()
  })
})

describe('app badge', () => {
  const badge = () => {
    const api = { setAppBadge: vi.fn(() => Promise.resolve()), clearAppBadge: vi.fn(() => Promise.resolve()) }
    setNavigator(api)
    return api
  }
  afterEach(() => setNavigator({ setAppBadge: undefined, clearAppBadge: undefined }))

  it('sets and clears the badge only when installed', () => {
    const api = badge()
    setBadge(3)
    expect(api.setAppBadge).not.toHaveBeenCalled()
    standalone(true)
    setBadge(3)
    expect(api.setAppBadge).toHaveBeenCalledWith(3)
    setBadge(0)
    expect(api.clearAppBadge).toHaveBeenCalledTimes(1)
  })

  it('does nothing where the Badging API is missing', () => {
    standalone(true)
    setNavigator({ setAppBadge: undefined, clearAppBadge: undefined })
    expect(() => setBadge(2)).not.toThrow()
  })

  it('follows the count while mounted and clears on unmount', () => {
    standalone(true)
    const api = badge()
    const { rerender, unmount } = renderHook(({ n }) => useAppBadge(n), { initialProps: { n: 2 } })
    expect(api.setAppBadge).toHaveBeenLastCalledWith(2)
    rerender({ n: 0 })
    expect(api.clearAppBadge).toHaveBeenCalledTimes(1)
    rerender({ n: 4 })
    expect(api.setAppBadge).toHaveBeenLastCalledWith(4)
    unmount()
    expect(api.clearAppBadge).toHaveBeenCalledTimes(2)
  })
})

describe('launch queue', () => {
  const origin = window.location.origin
  beforeEach(() => history.replaceState(null, '', '/ui/#home'))
  afterEach(() => {
    delete (window as { launchQueue?: unknown }).launchQueue
    history.replaceState(null, '', '/')
  })

  it('routes a launched Hub link in the open window', () => {
    let consume: ((p: { targetURL?: string }) => void) | undefined
    Object.assign(window, { launchQueue: { setConsumer: (c: typeof consume) => (consume = c) } })
    listenForLaunches()
    consume!({ targetURL: `${origin}/ui/#inbox` })
    expect(window.location.hash).toBe('#inbox')
  })

  it('leaves the window where it was for the bare app URL, another origin or a malformed URL', () => {
    for (const target of [`${origin}/ui/`, 'https://example.test/ui/#inbox', 'not a url']) routeLaunch(target)
    expect(window.location.hash).toBe('#home')
  })

  it('does nothing without a launch queue', () => {
    expect(() => listenForLaunches()).not.toThrow()
  })
})

describe('window-controls overlay', () => {
  afterEach(() => {
    setNavigator({ windowControlsOverlay: undefined })
    document.documentElement.classList.remove('wco')
  })

  it('marks the page only while the overlay is visible', () => {
    const overlay = Object.assign(new EventTarget(), { visible: true })
    setNavigator({ windowControlsOverlay: overlay })
    watchTitleBar()
    expect(document.documentElement.classList.contains('wco')).toBe(true)
    overlay.visible = false
    overlay.dispatchEvent(new Event('geometrychange'))
    expect(document.documentElement.classList.contains('wco')).toBe(false)
  })

  it('leaves a normal tab alone', () => {
    watchTitleBar()
    expect(document.documentElement.classList.contains('wco')).toBe(false)
  })
})

describe('Dock tip', () => {
  const WINDOWS_CHROME = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36'

  it('says Dock on a Mac, taskbar on Windows, and nothing elsewhere', () => {
    expect(dockTip(CHROME_MAC, 0)).toMatch(/Keep in Dock/)
    expect(dockTip(WINDOWS_CHROME, 0)).toMatch(/pin it to the taskbar/)
    expect(dockTip(SAFARI_MAC, 0)).toBeNull()
    expect(dockTip(SAFARI_MAC, 5)).toBeNull()
    expect(dockTip(SAFARI_IOS, 5)).toBeNull()
  })

  it('shows only in the installed app, and stays gone once dismissed', () => {
    setNavigator({ userAgent: CHROME_MAC })
    const tab = render(<InstallOffer />)
    expect(screen.queryByText(/Keep in Dock/)).toBeNull()
    tab.unmount()
    standalone(true)
    const first = render(<InstallOffer />)
    expect(screen.getByText(/Keep AgentSCM in your Dock/)).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }))
    expect(screen.queryByText(/Keep in Dock/)).toBeNull()
    first.unmount()
    render(<InstallOffer />)
    expect(screen.queryByText(/Keep in Dock/)).toBeNull()
  })
})
