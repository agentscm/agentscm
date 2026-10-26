import { cleanup } from '@testing-library/react'
import { afterEach, beforeEach } from 'vitest'
import { clearPollCache } from '../src/usePoll.ts'

// The welcome tour opens on first sign-in; app tests start as a viewer who has already dismissed it.
export const TOUR_OFF = '{"welcomeSeen":true,"welcomeOff":true,"autoplay":[],"sectionsSeen":[]}'
beforeEach(() => {
  try {
    window.localStorage.setItem('agentscm.tour.v1', TOUR_OFF)
  } catch {
    // storage unavailable: the tour then stays manual anyway
  }
})

afterEach(() => {
  cleanup()
  // Each test stubs its own hub; a value cached by the last test must not show in the next.
  clearPollCache()
  window.location.hash = ''
})

// jsdom lacks matchMedia; the app reads it for the system theme.
if (!window.matchMedia) {
  window.matchMedia = (query: string) =>
    ({ matches: false, media: query, onchange: null, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, dispatchEvent: () => false }) as MediaQueryList
}
