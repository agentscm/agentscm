import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { CaptureSharingBox, CaptureSharingElsewhere } from '../../src/CaptureSharing.tsx'
import { decodeCheckouts, setSharing } from '../../src/captureSharing.ts'

const TOKEN = `local:${'a'.repeat(64)}:acme/widgets`
const HUB = 'https://hub.example.test'
const ID = 'a1b2c3d4e5f60718293a4b5c'
const row = (over: Record<string, unknown> = {}) => ({ id: ID, repository: 'acme/widgets', hub: HUB, folder: 'widgets', configured: false, enabled: false, mode: null, readable: true, ...over })
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

  it('requires an opaque id, so a path or a missing id is never accepted as a name', () => {
    expect(() => decodeCheckouts({ version: 1, checkouts: [row({ id: '/Users/me/widgets' })] })).toThrow(/opaque id/)
    expect(() => decodeCheckouts({ version: 1, checkouts: [row({ id: 'ABC' })] })).toThrow(/opaque id/)
    expect(() => decodeCheckouts({ version: 1, checkouts: [row({ id: undefined })] })).toThrow(/id/)
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
      { id: ID, repository: 'acme/widgets', hub: HUB, mode: 'shared_conversation', confirm_sharing: true },
      { id: ID, repository: 'acme/widgets', hub: HUB, mode: 'metadata_only' },
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

  it('moves focus into the confirmation, announces the result and gives focus back to the switch', async () => {
    let shared = false
    vi.stubGlobal('fetch', vi.fn(async (_url: string, init?: RequestInit) => {
      if (init?.method === 'POST') {
        shared = true
        return reply(200, { version: 1, checkout: row({ configured: true, enabled: true, mode: 'shared_conversation' }) })
      }
      return reply(200, { version: 1, checkouts: [shared ? row({ configured: true, enabled: true, mode: 'shared_conversation' }) : row()] })
    }))
    render(<CaptureSharingBox token={TOKEN} />)
    const toggle = (await screen.findByRole('switch')) as HTMLInputElement
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    const region = document.getElementById(toggle.getAttribute('aria-controls') ?? '')
    expect(region?.hasAttribute('hidden')).toBe(true)
    await userEvent.click(toggle)
    const confirm = screen.getByRole('button', { name: 'Share conversations' })
    expect(document.activeElement).toBe(confirm)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    expect(region?.hasAttribute('hidden')).toBe(false)
    expect(region?.contains(confirm)).toBe(true)

    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }))
    expect(document.activeElement).toBe(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('false')

    await userEvent.click(toggle)
    await userEvent.click(screen.getByRole('button', { name: 'Share conversations' }))
    await waitFor(() => expect(screen.getByRole('status').textContent).toMatch(/Sharing turned on for acme\/widgets in widgets/))
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('switch')))
    expect(screen.getByRole('switch').getAttribute('aria-expanded')).toBe('false')
  })

  it('keeps two checkouts of one repository apart, with unique label targets', async () => {
    const other = 'ffffffffffffffffffffffff'
    const posts: Array<Record<string, unknown>> = []
    vi.stubGlobal('fetch', vi.fn(async (_url: string, init?: RequestInit) => {
      if (init?.method === 'POST') {
        posts.push(JSON.parse(String(init.body)))
        return reply(200, { version: 1, checkout: row({ id: other, folder: 'widgets-two', configured: true, enabled: true, mode: 'shared_conversation' }) })
      }
      return reply(200, { version: 1, checkouts: [row(), row({ id: other, folder: 'widgets-two' }), row({ id: 'abcdef012345', repository: 'acme/a.b' }), row({ id: 'abcdef012346', repository: 'acme/a_b' })] })
    }))
    render(<CaptureSharingBox token={TOKEN} />)
    const switches = await screen.findAllByRole('switch')
    expect(switches).toHaveLength(4)
    const ids = switches.map((node) => node.id)
    expect(new Set(ids).size).toBe(4)
    for (const node of switches) {
      expect(document.querySelectorAll(`label[for="${node.id}"]`)).toHaveLength(1)
      expect(document.getElementById(node.getAttribute('aria-controls') ?? '')).not.toBeNull()
    }
    await userEvent.click(switches[1]!)
    expect(switches[1]!.getAttribute('aria-expanded')).toBe('true')
    expect(switches[0]!.getAttribute('aria-expanded')).toBe('false')
    await userEvent.click(screen.getByRole('button', { name: 'Share conversations' }))
    await waitFor(() => expect(posts).toHaveLength(1))
    expect(posts[0]).toMatchObject({ id: other, repository: 'acme/widgets', hub: HUB, confirm_sharing: true })
  })

  it('only points at the computer when this page is not served by it', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => reply(200, { version: 1, checkouts: [row()] })))
    render(<CaptureSharingElsewhere />)
    expect(screen.getByText(/menu bar app/)).toBeTruthy()
    expect(screen.queryByRole('switch')).toBeNull()
  })
})
