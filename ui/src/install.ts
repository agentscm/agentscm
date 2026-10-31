// Installing the Hub as an app (a PWA): which browsers can, what to tell them, and the Dock badge.
import { useEffect, useSyncExternalStore } from 'react'

export type InstallKind = 'chromium' | 'safari-mac' | 'safari-ios' | 'firefox-windows' | 'none'

export interface InstallEnv {
  userAgent: string
  /** iPadOS Safari presents a Mac user agent; only touch points tell them apart. */
  maxTouchPoints: number
  /** Running in its own window already (display-mode: standalone, or iOS's `navigator.standalone`). */
  standalone: boolean
  /** `beforeinstallprompt` fired and has not been used. */
  canPrompt: boolean
}

/** The first Firefox with taskbar web apps on Windows. shortcut: unconfirmed against release notes, check when touching this. */
const FIREFOX_WINDOWS_MIN = 143

/** Pure: what, if anything, this browser can do to install the page. */
export function installKind({ userAgent: ua, maxTouchPoints, standalone, canPrompt }: InstallEnv): InstallKind {
  if (standalone) return 'none'
  if (canPrompt) return 'chromium'
  const otherBrowser = /Chrome|Chromium|CriOS|FxiOS|EdgiOS|Edg|OPR|Firefox/.test(ua)
  const safari = /Safari\//.test(ua) && !otherBrowser
  if (safari && (/iPhone|iPad/.test(ua) || (/Macintosh/.test(ua) && maxTouchPoints > 1))) return 'safari-ios'
  if (safari && /Macintosh/.test(ua) && Number(/Version\/(\d+)/.exec(ua)?.[1]) >= 17) return 'safari-mac'
  if (/Windows NT/.test(ua) && Number(/Firefox\/(\d+)/.exec(ua)?.[1]) >= FIREFOX_WINDOWS_MIN) return 'firefox-windows'
  return 'none'
}

/** One line for a browser that installs from its own menu; null where the Hub can prompt itself or nothing works. */
export function installHint(kind: InstallKind): string | null {
  switch (kind) {
    case 'safari-mac':
      return 'In Safari, choose File › Add to Dock.'
    case 'safari-ios':
      return 'Tap Share, then Add to Home Screen.'
    case 'firefox-windows':
      // shortcut: the exact Firefox menu path is unconfirmed, so this stays general.
      return 'Use Firefox’s “Add to taskbar” option in the address bar.'
    default:
      return null
  }
}

export function isStandalone(): boolean {
  return window.matchMedia?.('(display-mode: standalone)').matches === true || (navigator as { standalone?: boolean }).standalone === true
}

interface InstallPromptEvent extends Event {
  prompt: () => Promise<void>
}

// The event can fire before React mounts, so it is caught at import and read through a store.
let deferred: InstallPromptEvent | null = null
const listeners = new Set<() => void>()
const set = (e: InstallPromptEvent | null) => {
  deferred = e
  listeners.forEach((l) => l())
}
if (typeof window !== 'undefined') {
  window.addEventListener('beforeinstallprompt', (e) => {
    e.preventDefault()
    set(e as InstallPromptEvent)
  })
  window.addEventListener('appinstalled', () => set(null))
}
const subscribe = (l: () => void) => {
  listeners.add(l)
  return () => void listeners.delete(l)
}

export interface Install {
  kind: InstallKind
  hint: string | null
  /** Opens the browser's install dialog; only meaningful for `chromium`. */
  install: () => void
}

export function useInstall(): Install {
  const event = useSyncExternalStore(subscribe, () => deferred)
  const kind = installKind({ userAgent: navigator.userAgent, maxTouchPoints: navigator.maxTouchPoints, standalone: isStandalone(), canPrompt: event !== null })
  // A prompt can be used once: drop it either way, the browser fires a new event if the offer stands.
  return { kind, hint: installHint(kind), install: () => void event?.prompt().catch(() => undefined).finally(() => set(null)) }
}

type Badging = { setAppBadge?: (n: number) => Promise<void>; clearAppBadge?: () => Promise<void> }

/** The Dock badge, only for the installed app; 0 clears it. */
export function setBadge(n: number): void {
  const nav = navigator as Badging
  if (!isStandalone()) return
  void (n > 0 ? nav.setAppBadge?.(n) : nav.clearAppBadge?.())?.catch(() => undefined)
}

/** Keeps the Dock badge at `n` while mounted and clears it on unmount (sign-out). */
export function useAppBadge(n: number): void {
  useEffect(() => setBadge(n), [n])
  useEffect(() => () => setBadge(0), [])
}

type LaunchQueue = { setConsumer: (consume: (params: { targetURL?: string }) => void) => void }

/**
 * Routes a Hub link opened while the app window is open (`launch_handler: focus-existing`): the
 * window is focused, not duplicated, and this moves it to the link's page. A bare `/ui/` (the Dock
 * icon) leaves the window where it was.
 */
export function routeLaunch(target: string): void {
  let url: URL
  try {
    url = new URL(target)
  } catch {
    return
  }
  const here = window.location
  if (url.origin !== here.origin) return
  if (url.pathname !== here.pathname || url.search !== here.search) here.assign(url.href)
  // The hashchange listeners route it, including the reload a `#token=` link needs.
  else if (url.hash && url.hash !== here.hash) here.hash = url.hash
}

/** Feature-detected: only an installed Chromium app window has a launch queue. */
export function listenForLaunches(): void {
  ;(window as { launchQueue?: LaunchQueue }).launchQueue?.setConsumer((p) => {
    if (p.targetURL) routeLaunch(p.targetURL)
  })
}

type Overlay = EventTarget & { visible: boolean }

/** Marks `<html>` with `wco` while the window-controls overlay is on, so the top bar fills the title bar. */
export function watchTitleBar(): void {
  const overlay = (navigator as { windowControlsOverlay?: Overlay }).windowControlsOverlay
  if (!overlay) return
  const sync = () => document.documentElement.classList.toggle('wco', overlay.visible)
  sync()
  overlay.addEventListener('geometrychange', sync)
}

/**
 * The once-only tip for the installed app; null where there is nothing to do. A Chromium app on a
 * Mac leaves the Dock when closed unless kept; Safari's File › Add to Dock already keeps it.
 */
export function dockTip(userAgent: string, maxTouchPoints: number): string | null {
  if (/Windows NT/.test(userAgent)) return 'Keep AgentSCM on your taskbar: right-click its icon and pin it to the taskbar.'
  if (/Macintosh/.test(userAgent) && /Chrome\//.test(userAgent) && maxTouchPoints <= 1) return 'Keep AgentSCM in your Dock: right-click its icon › Options › Keep in Dock.'
  return null
}
