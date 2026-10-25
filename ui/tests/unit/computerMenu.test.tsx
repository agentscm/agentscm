import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ComputerNotices, LocalRepoSwitcher } from '../../src/ComputerBar.tsx'
import { loadToken, saveToken } from '../../src/storage.ts'

const GRANT = 'a'.repeat(64)
const TOKEN = `local:${GRANT}:acme/widgets@https://hub.example`
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

function stub(identity: Record<string, unknown>) {
  const calls: { url: string; method: string }[] = []
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    calls.push({ url: String(url), method: init?.method ?? 'GET' })
    if (String(url) === '/v1/local/identity') return json({ version: 1, instance: '1'.repeat(32), computer: 'synthetic-mac', expires_at: 2_000_000_000, ...identity })
    if (String(url) === '/v1/local/repositories')
      return json({ version: 1, repositories: [
        { repository: 'acme/widgets', hub: 'https://hub.example', folder: 'widgets' },
        { repository: 'acme/gadgets', hub: 'https://hub.example', folder: 'gadgets-checkout' },
      ] })
    return new Response(null, { status: 204 })
  }))
  return calls
}

const reload = vi.fn()
beforeEach(() => {
  localStorage.clear()
  saveToken(TOKEN, true)
  reload.mockReset()
  vi.stubGlobal('location', { ...window.location, reload })
})
afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
})

describe('the repository menu on this computer’s Hub page', () => {
  it('names the computer, lists its repositories and switches with the same connection', async () => {
    stub({ repository: 'acme/widgets', hub: { url: 'https://hub.example', signed_in: true } })
    const user = userEvent.setup()
    render(<LocalRepoSwitcher token={TOKEN} repo="acme/widgets" />)
    const button = await screen.findByRole('button', { name: 'acme/widgets on synthetic-mac' })
    await user.click(button)
    expect(screen.getByText(/On synthetic-mac/)).toBeTruthy()
    expect((await screen.findByRole('menuitemradio', { name: /acme\/widgets/ })).getAttribute('aria-checked')).toBe('true')
    const gadgets = screen.getByRole('menuitemradio', { name: /acme\/gadgets/ })
    expect(gadgets.textContent).toContain('gadgets-checkout')
    await user.click(gadgets)
    expect(loadToken()).toBe(`local:${GRANT}:acme/gadgets@https://hub.example`)
    expect(reload).toHaveBeenCalled()
  })

  it('disconnects this browser from the same menu', async () => {
    const calls = stub({ repository: 'acme/widgets', hub: { url: 'https://hub.example', signed_in: true } })
    const user = userEvent.setup()
    render(<LocalRepoSwitcher token={TOKEN} repo="acme/widgets" />)
    await user.click(await screen.findByRole('button', { name: 'acme/widgets on synthetic-mac' }))
    await user.click(screen.getByRole('menuitem', { name: /Disconnect this browser/ }))
    await waitFor(() => expect(loadToken()).toBeNull())
    expect(calls.some((c) => c.url === '/v1/local/session' && c.method === 'DELETE')).toBe(true)
  })
})

describe('notices on this computer’s Hub page', () => {
  it('say nothing when all is well', async () => {
    const calls = stub({ repository: 'acme/widgets', hub: { url: 'https://hub.example', signed_in: true } })
    const { container } = render(<ComputerNotices token={TOKEN} />)
    await waitFor(() => expect(calls.some((c) => c.url === '/v1/local/identity')).toBe(true))
    expect(container.textContent).toBe('')
  })

  it('explain a missing Hub sign-in and a repository that is gone', async () => {
    stub({ repository: null, hub: null })
    render(<ComputerNotices token={TOKEN} />)
    expect(await screen.findByText('That repository is no longer connected here')).toBeTruthy()

    stub({ repository: 'acme/widgets', hub: { url: 'https://hub.example', signed_in: false } })
    render(<ComputerNotices token={`${TOKEN}x`} />)
    expect(await screen.findByText(/agentscm hub login --hub https:\/\/hub.example/)).toBeTruthy()
  })
})
