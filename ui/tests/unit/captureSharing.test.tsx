import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { CaptureSharingBox, CaptureSharingElsewhere } from '../../src/CaptureSharing.tsx'
import { decodeCheckouts, setSharing, sharingState } from '../../src/captureSharing.ts'

const TOKEN = `local:${'a'.repeat(64)}:acme/widgets`
const HUB = 'https://hub.example.test'
const ID = 'a1b2c3d4e5f60718293a4b5c'
const row = (over: Record<string, unknown> = {}) => ({ id: ID, repository: 'acme/widgets', hub: HUB, folder: 'widgets', configured: false, enabled: false, mode: null, readable: true, lineage_mismatch: false, saved_repository: null, saved_hub: null, ...over })
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
    expect(toggle.hasAttribute('aria-expanded')).toBe(false)
    const region = document.getElementById(toggle.getAttribute('aria-controls') ?? '')
    expect(region?.hasAttribute('hidden')).toBe(true)
    await userEvent.click(toggle)
    const confirm = screen.getByRole('button', { name: 'Share conversations' })
    expect(document.activeElement).toBe(confirm)
    expect(region?.hasAttribute('hidden')).toBe(false)
    expect(region?.contains(confirm)).toBe(true)

    await userEvent.click(screen.getByRole('button', { name: 'Cancel' }))
    expect(document.activeElement).toBe(toggle)
    expect(region?.hasAttribute('hidden')).toBe(true)

    await userEvent.click(toggle)
    await userEvent.click(screen.getByRole('button', { name: 'Share conversations' }))
    await waitFor(() => expect(screen.getByRole('status').textContent).toMatch(/Sharing turned on for acme\/widgets in widgets/))
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole('switch')))
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
    expect(document.getElementById(switches[1]!.getAttribute('aria-controls') ?? '')?.hasAttribute('hidden')).toBe(false)
    expect(document.getElementById(switches[0]!.getAttribute('aria-controls') ?? '')?.hasAttribute('hidden')).toBe(true)
    await userEvent.click(screen.getByRole('button', { name: 'Share conversations' }))
    await waitFor(() => expect(posts).toHaveLength(1))
    expect(posts[0]).toMatchObject({ id: other, repository: 'acme/widgets', hub: HUB, confirm_sharing: true })
  })

  it('names the repository, folder and Hub as each switch description, with no expanded state on the switch', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => reply(200, { version: 1, checkouts: [row(), row({ id: 'ffffffffffffffffffffffff', folder: 'widgets-two' })] })))
    render(<CaptureSharingBox token={TOKEN} />)
    const switches = await screen.findAllByRole('switch')
    for (const node of switches) {
      expect(node.hasAttribute('aria-expanded')).toBe(false)
      const about = document.getElementById(node.getAttribute('aria-describedby') ?? '')
      expect(about?.textContent).toContain('acme/widgets')
      expect(about?.textContent).toContain(HUB)
    }
    expect(switches[1]!.getAttribute('aria-describedby')).not.toBe(switches[0]!.getAttribute('aria-describedby'))
    expect(document.getElementById(switches[1]!.getAttribute('aria-describedby') ?? '')?.textContent).toContain('widgets-two')
  })

  it('shows a distinct state for every kind of checkout', async () => {
    const rows = [
      row({ id: 'a00000000001' }),
      row({ id: 'a00000000002', configured: true }),
      row({ id: 'a00000000003', configured: true, enabled: true, mode: 'metadata_only' }),
      row({ id: 'a00000000004', configured: true, enabled: true, mode: 'local_conversation' }),
      row({ id: 'a00000000005', configured: true, enabled: true, mode: 'shared_conversation' }),
    ]
    expect(decodeCheckouts({ version: 1, checkouts: rows }).map(sharingState)).toEqual(['Not set up', 'Off', 'Metadata only', 'Local only', 'Shared with Hub'])
    vi.stubGlobal('fetch', vi.fn(async () => reply(200, { version: 1, checkouts: rows })))
    render(<CaptureSharingBox token={TOKEN} />)
    await screen.findAllByRole('switch')
    for (const label of ['Not set up', 'Off', 'Metadata only', 'Local only', 'Shared with Hub']) expect(screen.getAllByText(label)).toHaveLength(1)
  })

  it('says which Hub sharing was set for after a reconnect and only offers to turn it off', async () => {
    const moved = { configured: true, enabled: true, mode: 'shared_conversation', lineage_mismatch: true, saved_repository: 'acme/widgets', saved_hub: 'https://old.example.test' }
    const posts: Array<Record<string, unknown>> = []
    let off = false
    vi.stubGlobal('fetch', vi.fn(async (_url: string, init?: RequestInit) => {
      if (init?.method === 'POST') {
        posts.push(JSON.parse(String(init.body)))
        off = true
        return reply(200, { version: 1, checkout: row({ ...moved, mode: 'metadata_only' }) })
      }
      return reply(200, { version: 1, checkouts: [row({ ...moved, mode: off ? 'metadata_only' : 'shared_conversation' })] })
    }))
    render(<CaptureSharingBox token={TOKEN} />)
    const toggle = (await screen.findByRole('switch')) as HTMLInputElement
    expect(screen.getByRole('note').textContent).toContain(`Sharing was set for https://old.example.test; this checkout is now connected to ${HUB}.`)
    expect(toggle.disabled).toBe(false)
    await userEvent.click(toggle)
    expect(screen.queryByRole('button', { name: 'Share conversations' })).toBeNull()
    await waitFor(() => expect(posts).toEqual([{ id: ID, repository: 'acme/widgets', hub: HUB, mode: 'metadata_only' }]))
    await waitFor(() => expect((screen.getByRole('switch') as HTMLInputElement).disabled).toBe(true))
    expect(screen.getByText('Metadata only')).toBeTruthy()
  })

  it('re-reads the list and asks for a fresh look when the checkout is no longer listed', async () => {
    let reads = 0
    vi.stubGlobal('fetch', vi.fn(async (_url: string, init?: RequestInit) => {
      if (init?.method === 'POST') return reply(404, { error: 'unknown_checkout', message: 'That repository is no longer connected on this computer.' })
      reads += 1
      return reply(200, { version: 1, checkouts: [row()] })
    }))
    render(<CaptureSharingBox token={TOKEN} />)
    await userEvent.click(await screen.findByRole('switch'))
    await userEvent.click(screen.getByRole('button', { name: 'Share conversations' }))
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('The checkout list changed; review and try again'))
    await waitFor(() => expect(reads).toBe(2))
  })

  it('only points at the computer when this page is not served by it', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => reply(200, { version: 1, checkouts: [row()] })))
    render(<CaptureSharingElsewhere />)
    expect(screen.getByText(/menu bar app/)).toBeTruthy()
    expect(screen.queryByRole('switch')).toBeNull()
  })
})
