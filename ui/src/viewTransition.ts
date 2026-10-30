import { flushSync } from 'react-dom'

/**
 * Applies `apply` (which swaps the opening screen for the app) inside a view transition so the sign-in sheet
 * widens into the app's sheet. No API, reduced motion, or no opening screen on the page: it just applies.
 * The CSS names the sheet and logo in both layouts (`html.vt-app` names the app's side, only for the transition).
 */
export function morphToApp(apply: () => void): void {
  if (typeof document.startViewTransition !== 'function' || !document.querySelector('.signin-opening') || window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) return apply()
  const root = document.documentElement
  const run = document.startViewTransition(() => {
    root.classList.add('vt-app')
    flushSync(apply)
  })
  void run.finished.catch(() => undefined).finally(() => root.classList.remove('vt-app'))
}

/**
 * Swaps the sign-in sheet's content for the roll call (`apply`) inside a view transition, then runs `go` (the
 * navigation to the provider). The sheet carries `hub-sheet` only while `html.vt-leave` is set, so it stays put and
 * its content crossfades. No API or reduced motion: apply, then go after a beat so the step can be read.
 */
export function leaveToProvider(apply: () => void, go: () => void): void {
  if (typeof document.startViewTransition !== 'function' || window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) {
    apply()
    window.setTimeout(go, 300)
    return
  }
  const root = document.documentElement
  root.classList.add('vt-leave')
  const run = document.startViewTransition(() => flushSync(apply))
  void run.finished.catch(() => undefined).finally(() => {
    root.classList.remove('vt-leave')
    go()
  })
}
