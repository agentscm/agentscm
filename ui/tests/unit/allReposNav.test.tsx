// The way back to All repositories: the rail leads with it whenever the web sign-in can read several repositories.
import { render, screen, within } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { NAV, SECTIONS, type Section } from '../../src/App.tsx'
import { AppChrome, type WebChoice } from '../../src/TopNav.tsx'
import type { HubData } from '../../src/useHub.ts'

const data: HubData = {
  whoami: { token: { id: 't', name: 'status-page', repository: 'acme/widgets', read_only: true, expires_at: null }, owner: { provider: 'github', host: 'github.com', login: 'maya-chen' } },
  work: null,
  activity: null,
  health: { status: 'ok', storage: 'sqlite' },
  fresh: new Set(),
  lastOk: Date.now(),
  failing: false,
}
const web = (repos: string[]): WebChoice => ({ repos, pick: () => {}, refusal: null })

function chrome(section: Section, w?: WebChoice) {
  render(
    <AppChrome token="read-only-hub-token" logo={<span>logo</span>} repo="acme/widgets" web={w} nav={NAV} sections={SECTIONS} section={section} badges={{}} statuses={{}} data={data} d={null} overview={null} pref="system" onPref={() => {}} density="spacious" onDensity={() => {}} onForget={() => {}} collapsed={false} onCollapsed={() => {}}>
      <p>page</p>
    </AppChrome>,
  )
  return within(screen.getByRole('complementary', { name: 'Hub sections' }))
}

describe('All repositories in the rail', () => {
  it('leads the rail from any page when several repositories are readable', () => {
    const rail = chrome('home', web(['acme/widgets', 'acme/infra']))
    const link = rail.getByRole('link', { name: /All repositories/ })
    expect(link.getAttribute('href')).toBe('#repositories')
    expect(link.getAttribute('aria-current')).toBeNull()
    expect(rail.getAllByRole('link')[0]).toBe(link)
  })

  it('is marked current on its own page', () => {
    const rail = chrome('repositories', web(['acme/widgets', 'acme/infra']))
    expect(rail.getByRole('link', { name: /All repositories/ }).getAttribute('aria-current')).toBe('page')
  })

  it('is absent with one readable repository or a pasted token', () => {
    expect(chrome('home', web(['acme/widgets'])).queryByRole('link', { name: /All repositories/ })).toBeNull()
  })

  it('is absent without a web sign-in', () => {
    expect(chrome('home').queryByRole('link', { name: /All repositories/ })).toBeNull()
  })
})
