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
