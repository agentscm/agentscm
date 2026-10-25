import { act, render, screen, waitFor, within } from '@testing-library/react'
import { afterAll, beforeEach, describe, expect, it } from 'vitest'
import { saveToken } from '../../src/storage.ts'
import App from '../../src/App.tsx'
import { AGENT_TOKEN, READ_TOKEN, checkpoint, freshHandoff, hubReachable, marker, mcp, readWork, startWork } from '../hub.ts'
import { requests } from './live.ts'

const live = await hubReachable()
if (!live) console.warn('Skipping live-hub app tests: start the dev hub with ui/dev-seed/start.sh (127.0.0.1:7332).')

const go = (hash: string) => act(() => {
  window.location.hash = hash
  window.dispatchEvent(new HashChangeEvent('hashchange'))
})
const main = () => within(document.getElementById('main')!)
const LOAD = { timeout: 15_000 }

describe.skipIf(!live)('the 1c app rendered against the live hub', () => {
  beforeEach(() => {
    localStorage.clear()
    sessionStorage.clear()
    saveToken(READ_TOKEN, false)
  })

  it('shell: identity, repository, grouped rail and breadcrumbs', async () => {
    render(<App />)
    await screen.findByRole('button', { name: /Account and settings for/ }, LOAD)
    expect((await screen.findAllByText('counterbranch/agentscm', {}, LOAD)).length).toBeGreaterThan(0)
    const rail = within(screen.getByRole('complementary', { name: 'Hub sections' }))
    expect(rail.getAllByText(/^(Coordinate|Verify|This computer|Manage)$/).map((e) => e.textContent)).toEqual(['Coordinate', 'Verify', 'This computer', 'Manage'])
    expect(rail.getAllByRole('link').map((a) => a.querySelector('.side-label')!.textContent)).toEqual(['Right now', 'Work', 'Activity', 'Workflows', 'Change records', 'Landed changes', 'Skills', 'Templates', 'Timeline', 'Repository explorer', 'Workspace', 'Connect agents', 'Hub health', 'Settings'])
    expect(screen.getByRole('navigation', { name: 'Breadcrumb' })).toBeTruthy()
  })

  it('Right now answers who holds which files', async () => {
    render(<App />)
    await main().findByText(/are holding/, {}, LOAD)
    expect(main().getByText('Live agents')).toBeTruthy()
    expect(main().getByText('Contested files')).toBeTruthy()
  })

  it('Work shows the four progress lanes with seeded tasks', async () => {
    go('#work')
    render(<App />)
    await main().findByText('Rebuild the hub status page shell', {}, LOAD)
    for (const lane of ['In progress', 'Blocked', 'Handover', 'Paused']) expect(main().getAllByText(lane).length).toBeGreaterThan(0)
  })

  it('Work › Needs coordination lists directed requests', async () => {
    go('#work/coordination')
    render(<App />)
    await main().findByText('Directed requests', {}, LOAD)
    await main().findAllByText(/Awaiting receipt|Replied|Awaiting replies/, {}, LOAD)
  })

  it('Activity shows a refusal and a handoff as they happen', async () => {
    const handoff = await freshHandoff(marker())
    go('#activity')
    render(<App />)
    await main().findAllByText(handoff.file, {}, LOAD)
    await handoff.release()
    // Event sentences are split across name, file-chip and text elements.
    await waitFor(() => expect(document.getElementById('main')!.textContent).toMatch(/asked \S+’s \S+ for/), LOAD)
    expect(document.getElementById('main')!.textContent).toMatch(/handed .+ to \S+’s \S+/)
  })

  it('Hub health reports SQLite and counters', async () => {
    go('#health')
    render(<App />)
    await main().findByText(/The hub is answering and storing data in SQLite/, {}, LOAD)
    expect(main().getByText('Refused first edits')).toBeTruthy()
  })

  it('Workflows shows the unavailable state honestly', async () => {
    go('#workflows')
    render(<App />)
    await main().findAllByText(/not configured|unavailable/i, {}, LOAD)
  })

  it('Settings shows token facts without changing the hub', async () => {
    go('#settings')
    render(<App />)
    await main().findByText('How is this page set up for me?', {}, LOAD)
    await main().findAllByText(/status-page/, {}, LOAD)
  })

  it('never sends anything but GET to the hub', () => {
    const hub = requests.filter((r) => r.url.includes('/v1/'))
    expect(hub.length).toBeGreaterThan(10)
    expect(hub.filter((r) => r.method !== 'GET')).toEqual([])
  })
})

describe.skipIf(!live || !AGENT_TOKEN)('MCP writes reach the UI', () => {
  const made: { item: Awaited<ReturnType<typeof startWork>>['item']; session: string }[] = []
  afterAll(async () => {
    // Finish what the test started so the dev hub's current work stays as seeded.
    for (const { item, session } of made) {
      const latest = await readWork(item.id)
      await checkpoint(latest, session, 'finish', 'Test probe finished.')
    }
  })

  it('a read-only token is refused for MCP writes', async () => {
    const r = await mcp(READ_TOKEN, 'agentscm_start', { task: 'should be refused', paths: ['docs/x.md'], session: 'ro' })
    expect(r.status).toBe(403)
    expect(r.body).toEqual({ error: 'read_only' })
  })

  it('work started and checkpointed over MCP is read back over HTTP and rendered', async () => {
    const m = marker()
    const started = await startWork(m)
    made.push(started)
    expect((await readWork(started.item.id)).title).toBe(`E2E probe ${m}`)

    const cp = await checkpoint(started.item, started.session, 'progress', `Checkpoint ${m} via MCP`)
    expect(cp.status).toBe(200)
    expect(cp.body.result?.isError).toBeFalsy()
    const back = await readWork(started.item.id)
    expect(back.current?.summary).toBe(`Checkpoint ${m} via MCP`)
    expect(back.updated_seq).toBeGreaterThan(started.item.updated_seq)

    saveToken(READ_TOKEN, false)
    go(`#work/${started.item.id}`)
    render(<App />)
    await main().findAllByText(`E2E probe ${m}`, {}, LOAD)
    await waitFor(() => expect(main().getAllByText(new RegExp(`Checkpoint ${m} via MCP`)).length).toBeGreaterThan(0), LOAD)
  })
})
