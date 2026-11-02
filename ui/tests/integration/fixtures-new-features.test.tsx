// Landed changes (#62), Templates (#64) and Timeline (#67) rendered against stubbed replies.
// Publications are the dev hub's real replies for its 7 seeded states; local reads use the CLI-shaped fixtures.
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { FIXTURE_ENTRY, FIXTURE_TIMELINE } from '../../src/localApi.ts'
import { LandedPage } from '../../src/pages/Landed.tsx'
import { TemplatesPage } from '../../src/pages/Templates.tsx'
import { TimelinePage } from '../../src/pages/Timeline.tsx'
import seeded from '../fixtures/publications.json'
import { fromMock, json, type Override } from '../fixtures/mockFetch.ts'

const LOAD = { timeout: 5_000 }
const REPO = 'counterbranch/agentscm'
const pubs = seeded as { id: string; branch: string }[]
const idOf = (branch: string) => pubs.find((p) => p.branch.endsWith(branch))!.id
const at = (hash: string) =>
  act(() => {
    window.location.hash = hash
    window.dispatchEvent(new HashChangeEvent('hashchange'))
  })

const overviewRoute = (url: URL) => url.pathname === '/v1/hub/standup/overview'
/** No task index unless a test supplies one: Landed rows then show no task line. */
const noTasks: Override = (url) => (overviewRoute(url) ? json(404, { error: 'not_found' }) : undefined)

/** Stub fetch with handlers; every request is recorded, and anything unhandled is a test failure. */
function stub(...handlers: Override[]) {
  const sent: { method: string; url: string }[] = []
  vi.stubGlobal('fetch', async (input: string, init: RequestInit = {}) => {
    const url = new URL(input, 'http://127.0.0.1:5184')
    sent.push({ method: (init.method ?? 'GET').toUpperCase(), url: url.pathname + url.search })
    for (const h of [...handlers, noTasks]) {
      const r = await h(url, init)
      if (r) return r
    }
    throw new Error(`unexpected request ${url.pathname}`)
  })
  return sent
}
const listRoute = (url: URL) => url.pathname === `/v1/hub/repos/${encodeURIComponent(REPO)}/publications`
const showRoute = (url: URL) => url.pathname.startsWith(`/v1/hub/repos/${encodeURIComponent(REPO)}/publications/`)
const showFixture: Override = (url) => (showRoute(url) ? json(200, pubs.find((p) => url.pathname.endsWith(p.id))) : undefined)
/** A list reply as the hub sends it: no `external`/`forge_check`, plus created_at/updated_at. */
const listItem = (p: (typeof pubs)[number], i: number) => {
  const rest: Record<string, unknown> = { ...p }
  delete rest['external']
  delete rest['forge_check']
  return { ...rest, created_at: 1792690000 - i * 60, updated_at: 1792690000 - i * 30 }
}
const listFixture: Override = (url) => (listRoute(url) ? json(200, { publications: pubs.map(listItem), next_cursor: null, complete: true }) : undefined)

afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
  sessionStorage.clear()
})

describe('Landed changes', () => {
  it('lists every change, then fills in CI and deploys for merged ones from their own detail, read-only', async () => {
    const sent = stub(listFixture, showFixture)
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText(/3 of 7 recent changes merged/, {}, LOAD)
    expect(screen.getAllByRole('list', { name: 'How far this change got' })).toHaveLength(7)
    expect(screen.getByText('Branch rejected')).toBeTruthy()
    // Once the three merged details load, the failing CI on schema-migration-lock is counted.
    await screen.findByText(/1 had a failure after merging/, {}, LOAD)
    expect(screen.getByText('That’s everything.')).toBeTruthy()
    const shows = sent.filter((r) => showRoute(new URL(r.url, 'http://x')))
    expect(shows.map((r) => r.url.split('/').pop()).sort()).toEqual([idOf('handoff-expiry-sweep'), idOf('schema-migration-lock'), idOf('token-ttl-docs')].sort())
    expect(sent.every((r) => r.method === 'GET')).toBe(true)
  })

  it('leads a row with its title, then the branch, and names the task and person when a task has that branch', async () => {
    const branch = pubs[0]!.branch
    const titled: Override = (url) => (listRoute(url) ? json(200, { publications: pubs.map((p, i) => ({ ...listItem(p, i), ...(i === 0 ? { title: 'Retry with jitter' } : {}) })), next_cursor: null, complete: true }) : undefined)
    // The mock hub's overview, with its first task moved onto this change's branch.
    const tasks: Override = (url, init) => (overviewRoute(url) ? fromMock(url, { ...init, headers: { Authorization: 'Bearer read-token' } }, (o) => ({ ...o, current: (o['current'] as Record<string, unknown>[]).map((t, i) => (i === 0 ? { ...t, branch } : t)) })) : undefined)
    stub(titled, tasks, showFixture)
    render(<LandedPage repo={REPO} token="read-only" />)
    const row = (await screen.findByText('Retry with jitter', {}, LOAD)).closest('a')!
    expect(row.querySelector('strong')!.textContent).toBe('Retry with jitter')
    expect(row.textContent!.indexOf('Retry with jitter')).toBeLessThan(row.textContent!.indexOf(branch))
    // Untitled rows still lead with the branch.
    expect(screen.getAllByText(pubs[1]!.branch)[0]!.closest('strong')).toBeTruthy()
    const task = await screen.findByRole('link', { name: 'Refactor the billing module' }, LOAD)
    expect(task.getAttribute('href')).toMatch(/^#work\/wrk_/)
    expect(task.closest('a.landed-row')).toBeNull()
    expect(task.closest('li')!.textContent).toContain('Task: Refactor the billing module · maria’s Claude Code')
  })

  it('shows merged rows as Checking… until their detail loads, never as passed', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((r) => (release = r))
    stub(listFixture, (url) => (showRoute(url) ? gate.then(() => showFixture(url, {}) as Response) : undefined))
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText(/3 of 7 recent changes merged/, {}, LOAD)
    expect(screen.getAllByText('Merged, checking…')).toHaveLength(3)
    expect(screen.queryByText('Merged and passed')).toBeNull()
    release()
    await waitFor(() => expect(screen.queryByText('Merged, checking…')).toBeNull(), LOAD)
  })

  it('says test and deploy results are not tracked, with a date range and how to turn it on, only once every merged change is read', async () => {
    at('#landed')
    const untracked: Override = (url) => (showRoute(url) ? json(200, { ...pubs.find((p) => url.pathname.endsWith(p.id)), external: null }) : undefined)
    stub(untracked, listFixture)
    render(<LandedPage repo={REPO} token="read-only" />)
    const head = () => document.querySelector('.answer-text')!.textContent ?? ''
    await waitFor(() => expect(head()).toMatch(/^3 of 7 recent changes merged \(.+\)\. Test and deploy results after merge are not tracked\./), LOAD)
    expect(document.querySelector('.answer-fact')!.textContent).toMatch(/^Tracking is off\. A hub admin can turn it on for this repository \(GitHub only, experimental\)\./)
    expect(document.body.textContent).toContain('AGENTSCM_GITHUB_EXTERNAL_RUNS')
    expect(document.body.textContent).not.toMatch(/\blanded\b/i)
  })

  it('does not say tracking is off while details are loading or when CI is read', async () => {
    at('#landed')
    stub(showFixture, listFixture)
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText(/1 had a failure after merging/, {}, LOAD)
    expect(document.querySelector('.answer-text')!.textContent).not.toMatch(/not tracked/)
    expect(document.querySelector('.answer-fact')!.textContent).toMatch(/^Merged on GitHub or GitLab, as of the hub’s last sync/)
  })

  it('pages with "Older changes" and only says "That’s everything" when the list is complete', async () => {
    at('#landed')
    const sent = stub(showFixture, (url) => {
      if (!listRoute(url)) return undefined
      const before = url.searchParams.get('before')
      return before
        ? json(200, { publications: pubs.slice(4).map(listItem), next_cursor: null, complete: true })
        : json(200, { publications: pubs.slice(0, 4).map(listItem), next_cursor: pubs[3]!.id, complete: false })
    })
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText(/of 4 recent changes merged/, {}, LOAD)
    expect(screen.queryByText('That’s everything.')).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: /Older changes/ }))
    await screen.findByText('That’s everything.', {}, LOAD)
    expect(sent.some((r) => r.url.includes(`before=${pubs[3]!.id}`) && r.url.includes('limit=25'))).toBe(true)
  })

  it('says plainly when the hub refuses the status page token, on the list and on one change', async () => {
    stub((url) => (listRoute(url) || showRoute(url) ? json(403, { error: 'read_only' }) : undefined))
    render(<LandedPage repo={REPO} token="read-only" />)
    expect((await screen.findAllByText(/needs a newer hub/, {}, LOAD)).length).toBeGreaterThan(0)
    at(`#landed/${idOf('handoff-expiry-sweep')}`)
    await screen.findByText('Published changes aren’t readable here yet', {}, LOAD)
  })

  it('treats a hub without the list route (405) the same way', async () => {
    stub((url) => (listRoute(url) ? new Response('', { status: 405 }) : undefined))
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText('Not readable with this page’s token yet', {}, LOAD)
  })

  it('shows CI and deployments with an honest strip for each external state', async () => {
    stub(showFixture)
    at(`#landed/${idOf('handoff-expiry-sweep')}`)
    const { unmount } = render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText('CI after merge', {}, LOAD)
    expect(document.querySelector('.honesty')!.textContent).toMatch(/^Checked/)
    expect(screen.getByText(/not evidence for the landing decision/i)).toBeTruthy()
    unmount()

    at(`#landed/${idOf('schema-migration-lock')}`)
    const second = render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findAllByText(/Partly checked/, {}, LOAD)
    expect(screen.getByText(/this list may be missing runs/)).toBeTruthy()
    second.unmount()

    at(`#landed/${idOf('token-ttl-docs')}`)
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findAllByText(/Not checked yet/, {}, LOAD)
    expect(screen.getByText(/An empty list does not mean success/)).toBeTruthy()
    expect(screen.getAllByText('Nothing to show yet')).toHaveLength(2)
  })

  it('renders a hostile provider link as text and strips control characters', async () => {
    const id = idOf('handoff-expiry-sweep')
    const hostile = structuredClone(pubs.find((p) => p.id === id)) as unknown as { external: { ci: { html_url: string; name: string }[] } }
    hostile.external.ci[0]!.html_url = 'javascript:alert(1)'
    hostile.external.ci[0]!.name = 'build\u0007\u001b'
    stub((url) => (showRoute(url) ? json(200, hostile) : undefined))
    at(`#landed/${id}`)
    render(<LandedPage repo={REPO} token="read-only" />)
    const box = (await screen.findByText('CI after merge', {}, LOAD)).closest('section')!
    expect(within(box).getByText('build')).toBeTruthy()
    expect(within(box).queryAllByRole('link').every((a) => (a as HTMLAnchorElement).href.startsWith('https://'))).toBe(true)
  })

  it('opens a change by ID and remembers it', async () => {
    stub(listFixture, showFixture)
    render(<LandedPage repo={REPO} token="read-only" />)
    await screen.findByText(/recent changes merged/, {}, LOAD)
    const id = idOf('activity-pagination')
    const picker = screen.getByRole('combobox', { name: 'Find a change' })
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: id } })
    fireEvent.keyDown(picker, { key: 'Enter' })
    await waitFor(() => expect(window.location.hash).toBe(`#landed/${id}`))
    await screen.findByText('How far it got', {}, LOAD)
    expect(localStorage.getItem('agentscm.hub.recentPublications')).toContain(id)
  })
})

describe('Templates', () => {
  // The Library opens on its workflows; these tests exercise its Inspect view.
  beforeEach(() => { window.location.hash = '#templates?view=inspect' })
  const openRecipe = (change: string, library: string) => {
    const picker = screen.getByRole('combobox', { name: 'Change ID' })
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: change } })
    fireEvent.keyDown(picker, { key: 'Enter' })
    fireEvent.change(screen.getByLabelText('Template file'), { target: { value: library } })
    fireEvent.click(screen.getAllByRole('button', { name: /Open/ })[0]!)
  }

  it('shows preview data with a banner when no local API answers, and remembers what was opened', async () => {
    stub(() => {
      throw new TypeError('connection refused')
    })
    render(<TemplatesPage />)
    expect((screen.getAllByRole('button', { name: /Open/ })[0] as HTMLButtonElement).disabled).toBe(true)
    openRecipe('other-change', 'skills/checks/v2/workflow.json')
    await screen.findByText(/Preview data/, {}, LOAD)
    expect(screen.getByText('Check this path is in the change: src/lib.rs')).toBeTruthy()
    expect(screen.getByText('Required')).toBeTruthy()
    const runPicker = screen.getByRole('combobox', { name: 'Run ID' })
    fireEvent.focus(runPicker)
    fireEvent.change(runPicker, { target: { value: 'loop-b863' } })
    fireEvent.keyDown(runPicker, { key: 'Enter' })
    await screen.findByText('What to do next', {}, LOAD)
    expect(screen.getByRole('button', { name: /other-change · skills\/checks\/v2\/workflow\.json/ })).toBeTruthy()
    expect(screen.getByRole('button', { name: 'loop-b863' })).toBeTruthy()
  })

  it('editing the picked change clears it, so Open stays disabled until another is chosen', async () => {
    stub(() => {
      throw new TypeError('connection refused')
    })
    render(<TemplatesPage />)
    const picker = screen.getByRole('combobox', { name: 'Change ID' })
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: 'change-a' } })
    fireEvent.keyDown(picker, { key: 'Enter' })
    fireEvent.change(screen.getByLabelText('Template file'), { target: { value: 'skills/x/workflow.json' } })
    const open = screen.getAllByRole('button', { name: /Open/ })[0] as HTMLButtonElement
    expect(open.disabled).toBe(false)
    fireEvent.change(picker, { target: { value: 'change-b' } })
    expect(open.disabled).toBe(true)
  })

  it('a plain word does not hijack Enter for the run picker', async () => {
    stub(() => {
      throw new TypeError('connection refused')
    })
    render(<TemplatesPage />)
    const run = screen.getByRole('combobox', { name: 'Run ID' })
    fireEvent.focus(run)
    fireEvent.change(run, { target: { value: 'preview' } })
    await screen.findAllByRole('option', { name: /preview-run/ }, LOAD)
    fireEvent.keyDown(run, { key: 'Enter' })
    expect(screen.queryByText(/^Run preview$/)).toBeNull()
  })

  it('asks the live server with both values and shows its own error as advice instead of sample data', async () => {
    const sent = stub(() => json(404, { error: 'manifest not found' }))
    render(<TemplatesPage />)
    openRecipe('demo-change', 'skills/checks/v9/workflow.json')
    await screen.findByText(/check the change ID and library path/, {}, LOAD)
    expect(screen.queryByText(/Preview data/)).toBeNull()
    expect(sent.find((r) => r.url.includes('skill-workflow-library'))!.url).toBe('/v1/local/skill-workflow-library?change=demo-change&library=skills%2Fchecks%2Fv9%2Fworkflow.json')
  })
})

describe('Timeline', () => {
  const local: Override = (url) => {
    if (url.pathname === '/v1/local/workspace/timeline') return json(200, FIXTURE_TIMELINE(url.searchParams.has('before') ? Number(url.searchParams.get('before')) : undefined))
    if (url.pathname.startsWith('/v1/local/workspace/timeline/')) return json(200, FIXTURE_ENTRY(Number(url.pathname.split('/').pop())))
    return undefined
  }
  /** Two diff pages: the first continues, the second is missing source objects. */
  const diffPages: Override = (url) => {
    if (url.pathname !== '/v1/local/workspace/diff') return undefined
    const after = url.searchParams.get('after')
    return after
      ? json(200, { version: 1, left: 2, right: 3, diff: { entries: [{ path: 'old.sh', before: 'b1', after: null }], needed_objects: ['obj-1'], next: null } })
      : json(200, { version: 1, left: 2, right: 3, diff: { entries: [{ path: 'src/more.rs', before: null, after: 'a1' }, { path: 'src/lib.rs', before: 'a2', after: 'a3' }], needed_objects: [], next: 'cursor-1' } })
  }

  it('lists saved moments live, pages to older ones, and compares two across diff pages', async () => {
    stub(local, diffPages)
    at('#timeline')
    render(<TimelinePage />)
    await screen.findByText('Saved moments', {}, LOAD)
    expect(screen.queryByText(/Preview data/)).toBeNull()
    expect(screen.getByText(/Work from before then isn’t here/)).toBeTruthy()
    fireEvent.click(screen.getByLabelText('Pick #3 to compare'))
    fireEvent.click(screen.getByLabelText('Pick #2 to compare'))
    await waitFor(() => expect(screen.getByRole('link', { name: /Compare these two/ }).getAttribute('href')).toBe('#timeline/compare/2/3'))
    fireEvent.click(screen.getByRole('button', { name: /Older/ }))
    await screen.findByLabelText('Pick #1 to compare', {}, LOAD)
    at('#timeline/compare/2/3')
    await screen.findByText('What changed from #2 to #3', {}, LOAD)
    expect(screen.getByText(/More differences exist/)).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Show more' }))
    await screen.findByText('old.sh', {}, LOAD)
    expect(screen.getAllByText('src/more.rs')).toHaveLength(1)
    expect(screen.getByText(/couldn’t be compared/)).toBeTruthy()
    expect(screen.queryByText(/More differences exist/)).toBeNull()
  })

  it('shows a check-link entry with its evidence and the fork command', async () => {
    stub(local)
    at('#timeline/entry/3')
    render(<TimelinePage />)
    await screen.findByText('Check found', {}, LOAD)
    expect(screen.getByText(/workspace fork ROOT 3/)).toBeTruthy()
  })

  it('falls back to the real example replies, labelled as preview, when no local API answers', async () => {
    stub(() => {
      throw new TypeError('connection refused')
    })
    render(<TimelinePage />)
    await screen.findByText(/Preview data/, {}, LOAD)
    expect(screen.getByLabelText('Pick #3 to compare')).toBeTruthy()
  })
})
