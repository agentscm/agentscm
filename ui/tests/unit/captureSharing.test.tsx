import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { CaptureSharingBox, CaptureSharingElsewhere } from '../../src/CaptureSharing.tsx'
import { decodeCheckouts, setSharing } from '../../src/captureSharing.ts'

const TOKEN = `local:${'a'.repeat(64)}:acme/widgets`
const HUB = 'https://hub.example.test'
const row = (over: Record<string, unknown> = {}) => ({ repository: 'acme/widgets', hub: HUB, folder: 'widgets', configured: false, enabled: false, mode: null, readable: true, ...over })
const reply = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

afterEach(() => vi.unstubAllGlobals())

describe('conversation sharing decoding', () => {
  it('accepts the service list and rejects unknown shapes and modes', () => {
    expect(decodeCheckouts({ version: 1, checkouts: [row({ configured: true, enabled: true, mode: 'shared_conversation' })] })[0]).toMatchObject({ mode: 'shared_conversation' })
    expect(() => decodeCheckouts({ version: 2, checkouts: [] })).toThrow()
    expect(() => decodeCheckouts({ version: 1, checkouts: [row({ mode: 'everything' })] })).toThrow(/mode/)
    expect(() => decodeCheckouts({ version: 1, checkouts: [row({ enabled: 'yes' })] })).toThrow(/enabled/)
    expect(() => decodeCheckouts({ version: 1, checkouts: [{ ...row(), path: '/x' }, 7] })).toThrow()
  })

  it('sends the confirmation only when turning sharing on', async () => {
    const bodies: unknown[] = []
    vi.stubGlobal('fetch', vi.fn(async (_url: string, init: RequestInit) => {
      bodies.push(JSON.parse(String(init.body)))
      return reply(200, { version: 1, checkout: row({ configured: true, enabled: true, mode: 'metadata_only' }) })
    }))
    const checkout = decodeCheckouts({ version: 1, checkouts: [row()] })[0]!
    await setSharing(TOKEN, checkout, true)
    await setSharing(TOKEN, checkout, false)
    expect(bodies).toEqual([
      { repository: 'acme/widgets', hub: HUB, mode: 'shared_conversation', confirm_sharing: true },
      { repository: 'acme/widgets', hub: HUB, mode: 'metadata_only' },
    ])
  })
})

describe('Conversation sharing in Settings', () => {
  it('shows what is uploaded, uploads nothing until confirmed, then re-reads the state', async () => {
    let shared = false
    const posts: unknown[] = []
    vi.stubGlobal('fetch', vi.fn(async (_url: string, init?: RequestInit) => {
      if (init?.method === 'POST') {
        posts.push(JSON.parse(String(init.body)))
        shared = true
        return reply(200, { version: 1, checkout: row({ configured: true, enabled: true, mode: 'shared_conversation' }) })
      }
      return reply(200, { version: 1, checkouts: [shared ? row({ configured: true, enabled: true, mode: 'shared_conversation' }) : row()] })
    }))
    render(<CaptureSharingBox token={TOKEN} />)
    const toggle = await screen.findByRole('switch', { name: /share agent conversations/i })
    expect((toggle as HTMLInputElement).checked).toBe(false)
    await userEvent.click(toggle)
    expect(posts).toEqual([])
    expect(screen.getByText(/kept 30 days by default/)).toBeTruthy()
    expect(screen.getByText(/redacted for secrets on this computer/)).toBeTruthy()
    expect(screen.getByText(HUB)).toBeTruthy()
    await userEvent.click(screen.getByRole('button', { name: 'Share conversations' }))
    await waitFor(() => expect((screen.getByRole('switch') as HTMLInputElement).checked).toBe(true))
    expect(posts).toHaveLength(1)
    expect(screen.getByText('Shared with Hub')).toBeTruthy()
  })

  it('only points at the computer when this page is not served by it', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => reply(200, { version: 1, checkouts: [row()] })))
    render(<CaptureSharingElsewhere />)
    expect(screen.getByText(/menu bar app/)).toBeTruthy()
    expect(screen.queryByRole('switch')).toBeNull()
  })
})
