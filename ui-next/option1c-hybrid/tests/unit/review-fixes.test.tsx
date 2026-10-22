// Regression tests for the 1c review findings. Each fails on the code before its fix.
import { act, fireEvent, render, screen, within } from '@testing-library/react'
import { useState } from 'react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { derive } from '../../../../ui/src/derive.ts'
import type { WorkOverview } from '../../../../ui/src/standup.ts'
import type { Activity, Work } from '../../../../ui/src/types.ts'
import { useNow, type HubData } from '../../../../ui/src/useHub.ts'
import { SECTIONS } from '../../src/App.tsx'
import { ErrorBoundary } from '../../src/ErrorBoundary.tsx'
import { AppChrome, crumbsFor, useDismiss } from '../../src/TopNav.tsx'
import { NowPage, httpsUrl } from '../../src/pages/Now.tsx'

const flush = () => act(() => new Promise((r) => setTimeout(r, 5)))
const key = (k: string) => fireEvent.keyDown(document.activeElement ?? document.body, { key: k })

const hubData = (over: Partial<HubData> = {}): HubData => ({
  whoami: { token: { id: 't', name: 'status-page', repository: 'acme/widgets', read_only: true, expires_at: null }, owner: { provider: 'github', host: 'github.com', login: 'maya-chen' } },
  work: null,
  activity: null,
  health: { status: 'ok', storage: 'sqlite' },
  fresh: new Set(),
  lastOk: Date.now(),
  failing: false,
  ...over,
})

function Chrome({ overview = null, data = hubData() }: { overview?: WorkOverview | null; data?: HubData }) {
  return (
    <AppChrome
      logo={<span>logo</span>}
      repo="acme/widgets"
      sections={SECTIONS}
      section="now"
      badges={{}}
      data={data}
      d={null}
      overview={overview}
      pref="system"
      onPref={() => {}}
      onForget={() => {}}
      collapsed={false}
      onCollapsed={() => {}}
    >
      <p>page</p>
    </AppChrome>
  )
}

afterEach(() => vi.restoreAllMocks())

describe('1. menu focus survives re-renders while open', () => {
  function Menu({ tick }: { tick: number }) {
    const [open, setOpen] = useState(false)
    // A fresh close arrow every render, exactly like the real menus.
    const ref = useDismiss(open, () => setOpen(false))
    return (
      <div className="pop" ref={ref} data-tick={tick}>
        <button type="button" aria-haspopup="menu" onClick={() => setOpen(true)}>
          trigger
        </button>
        {open && (
          <div className="pop-menu" role="menu">
            <button type="button">first</button>
            <button type="button">last</button>
          </div>
        )}
      </div>
    )
  }

  it('a data refresh (parent re-render) does not snap focus back to the first item', async () => {
    const { rerender } = render(<Menu tick={0} />)
    fireEvent.click(screen.getByText('trigger'))
    await flush()
    key('End')
    expect(document.activeElement).toBe(screen.getByText('last'))
    rerender(<Menu tick={1} />)
    await flush()
    rerender(<Menu tick={2} />)
    await flush()
    expect(document.activeElement).toBe(screen.getByText('last'))
    expect(screen.getByRole('menu')).toBeTruthy()
  })

  it('a key pressed immediately after opening is not overridden by the initial focus', async () => {
    render(<Menu tick={0} />)
    fireEvent.click(screen.getByText('trigger'))
    expect(document.activeElement).toBe(screen.getByText('first'))
    key('End') // before any timers run
    await flush()
    expect(document.activeElement).toBe(screen.getByText('last'))
  })
})

describe('2. pull request links accept only https', () => {
  it('httpsUrl', () => {
    expect(httpsUrl('https://github.com/acme/widgets/pull/7')).toBe('https://github.com/acme/widgets/pull/7')
    for (const bad of ['javascript:alert(1)', 'JAVASCRIPT:alert(1)', 'http://example.com', 'data:text/html,x', '//evil.example', '', null, undefined]) expect(httpsUrl(bad)).toBeNull()
  })

  it('NowPage renders a hostile PR URL as plain text, never as a link', () => {
    const now = Math.floor(Date.now() / 1000)
    const pr = (number: number, html_url: string) => ({ number, title: `PR ${number}`, state: 'open', draft: false, head_ref: 'h', base_ref: 'main', author: { login: 'bob' }, html_url, files: ['src/a.rs'] })
    const work: Work = {
      intents: [{ id: 'i1', task: 't', branch: null, machine: null, paths: ['src/a.rs'], via: ['hook'], created_at: now, updated_at: now, agent: { token: 'tok', name: 'claude-ivy' }, owner: { login: 'maya' } }],
      intents_truncated: false,
      connected: true,
      pull_requests: [pr(1, 'javascript:alert(document.cookie)'), pr(2, 'https://github.com/acme/widgets/pull/2')],
    }
    const activity: Activity = { sessions: [], events: [] }
    render(<NowPage repo="acme/widgets" data={hubData({ work, activity })} d={derive(work, activity)} overview={{ kind: 'loading' }} />)
    expect(screen.getByRole('link', { name: /#2 PR 2/ }).getAttribute('href')).toBe('https://github.com/acme/widgets/pull/2')
    expect(screen.queryByRole('link', { name: /#1 PR 1/ })).toBeNull()
    const hrefs = [...document.querySelectorAll('.pr-list a')].map((a) => a.getAttribute('href'))
    expect(hrefs).toEqual(['https://github.com/acme/widgets/pull/2'])
    expect(document.querySelector('.pr-list')!.textContent).toContain('#1 PR 1')
  })
})

describe('3. malformed workflow hashes never crash the page', () => {
  const workflows = SECTIONS.find((s) => s.key === 'workflows')!
  it('crumbsFor tolerates a broken percent-escape and invalid ids', () => {
    expect(() => crumbsFor('#workflows/runs/%E0%A4%A', workflows, null)).not.toThrow()
    expect(crumbsFor('#workflows/runs/%E0%A4%A', workflows, null).map((c) => c.label)).toEqual(['Workflows'])
    expect(crumbsFor('#workflows/runs/..%2F..%2Fetc', workflows, null).map((c) => c.label)).toEqual(['Workflows'])
    expect(crumbsFor('#workflows/runs/run_1/steps/%E0%A4%A', workflows, null).map((c) => c.label)).toEqual(['Workflows', 'Run run_1'])
    expect(crumbsFor('#workflows/runs/run_1/steps/lint%2Fdocs', workflows, null).map((c) => c.label)).toEqual(['Workflows', 'Run run_1', 'Step lint/docs'])
  })

  it('the error boundary shows a recovery message and "Go to Right now" resets the hash', async () => {
    window.location.hash = '#broken'
    function Boom() {
      if (window.location.hash !== '#now') throw new Error('kaboom')
      return <p>recovered</p>
    }
    vi.spyOn(console, 'error').mockImplementation(() => {})
    // jsdom reports the (expected) render error as an uncaught window error; it is the point of this test.
    const quiet = (e: ErrorEvent) => e.preventDefault()
    window.addEventListener('error', quiet)
    render(
      <ErrorBoundary>
        <Boom />
      </ErrorBoundary>,
    )
    const alert = screen.getByRole('alert')
    expect(within(alert).getByText('This page hit a problem')).toBeTruthy()
    expect(within(alert).getByText('kaboom')).toBeTruthy()
    fireEvent.click(within(alert).getByRole('link', { name: 'Go to Right now' }))
    expect(window.location.hash).toBe('#now')
    expect(screen.getByText('recovered')).toBeTruthy()
    window.removeEventListener('error', quiet)
  })
})

describe('5 + 8. search palette is an accessible combobox with a valid selection', () => {
  const W = `wrk_${'c'.repeat(32)}`
  const overviewWith = (title: string) => ({ current: [{ id: W, title, lead_agent: 'claude-ivy', state: 'active' }], recent: [] }) as unknown as WorkOverview

  it('wires combobox, listbox, option ids and the active descendant', async () => {
    render(<Chrome />)
    fireEvent.click(screen.getByRole('button', { name: 'Search the hub' }))
    const input = screen.getByRole('combobox', { name: 'Search' })
    expect(document.activeElement).toBe(input)
    expect(input.getAttribute('aria-controls')).toBe('palette-list')
    expect(document.getElementById('palette-list')!.getAttribute('role')).toBe('listbox')
    expect(input.getAttribute('aria-expanded')).toBe('true')
    expect(input.getAttribute('aria-activedescendant')).toBe('hit-0')
    fireEvent.keyDown(input, { key: 'ArrowDown' })
    expect(input.getAttribute('aria-activedescendant')).toBe('hit-1')
    expect(document.getElementById('hit-1')!.getAttribute('aria-selected')).toBe('true')
    expect(document.getElementById('hit-1')!.getAttribute('role')).toBe('option')
  })

  it('ArrowDown with no results keeps a valid selection when results arrive with the next refresh', async () => {
    const { rerender } = render(<Chrome />)
    fireEvent.click(screen.getByRole('button', { name: 'Search the hub' }))
    const input = screen.getByRole('combobox', { name: 'Search' })
    fireEvent.change(input, { target: { value: 'zq-marker' } })
    expect(screen.queryAllByRole('option')).toHaveLength(0)
    expect(input.getAttribute('aria-expanded')).toBe('false')
    fireEvent.keyDown(input, { key: 'ArrowDown' })
    // The overview poll brings in a matching work item; the query has not changed.
    rerender(<Chrome overview={overviewWith('zq-marker task')} />)
    const hit = screen.getByRole('option', { name: /zq-marker task/ })
    expect(hit.getAttribute('aria-selected')).toBe('true')
    expect(input.getAttribute('aria-activedescendant')).toBe(hit.id)
    fireEvent.keyDown(input, { key: 'Enter' })
    expect(window.location.hash).toBe(`#work/${W}`)
  })
})

describe('6. menu roles match what the keyboard reaches', () => {
  it('account menu theme choices are menuitemradio with aria-checked, and arrows reach them', async () => {
    render(<Chrome />)
    fireEvent.click(screen.getByRole('button', { name: /Account and settings/ }))
    await flush()
    const menu = screen.getByRole('menu')
    const radios = within(menu).getAllByRole('menuitemradio')
    expect(radios.map((r) => [r.textContent!.trim(), r.getAttribute('aria-checked')])).toEqual([
      ['Light', 'false'],
      ['System', 'true'],
      ['Dark', 'false'],
    ])
    expect(document.activeElement).toBe(radios[0])
    key('ArrowDown')
    expect(document.activeElement).toBe(radios[1])
    // Every element the arrow keys can reach is announced as a menu item.
    const reachable = [...menu.querySelectorAll('a[href], button:not([disabled])')]
    expect(reachable.every((el) => /^menuitem(radio)?$/.test(el.getAttribute('role') ?? ''))).toBe(true)
  })

  it('repository switcher: the current repository is plain text, not a menu item', async () => {
    render(<Chrome />)
    fireEvent.click(screen.getByRole('button', { name: 'acme/widgets' }))
    await flush()
    const menu = screen.getByRole('menu')
    expect(within(menu).queryAllByRole('menuitemradio')).toHaveLength(0)
    expect(within(menu).getAllByRole('menuitem').map((m) => m.textContent)).toEqual([expect.stringMatching(/^Open the workspace/), expect.stringMatching(/^Use a different token/)])
    expect(within(menu).getByText('acme/widgets').closest('[role]')).toBe(menu)
  })
})

describe('7. breadcrumbs share the page clock', () => {
  function OtherClockUser() {
    useNow()
    return null
  }
  it('rendering the chrome adds no 1-second timer of its own', () => {
    const spy = vi.spyOn(window, 'setInterval')
    // Another subscriber starts the shared clock, as relative times on the page do.
    render(
      <>
        <OtherClockUser />
        <Chrome />
      </>,
    )
    // useNow's shared clock is the only once-a-second timer.
    expect(spy.mock.calls.filter(([, ms]) => ms === 1000)).toHaveLength(1)
  })
})
