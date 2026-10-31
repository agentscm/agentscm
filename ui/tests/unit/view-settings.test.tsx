import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as api from '../../src/api.ts'
import * as ws from '../../src/workspace.ts'
import { ViewSettingsBox } from '../../src/ViewSettings.tsx'

vi.mock('../../src/api.ts', () => ({ fetchViewSettings: vi.fn() }))
vi.mock('../../src/workspace.ts', async (orig) => ({ ...(await orig<typeof ws>()), workspaceRequest: vi.fn(), workspaceMutation: vi.fn() }))

const on = { enabled: true, work_overview: true, coordination_inbox: false }
const report = (over: object | null = null, effective: object = on, hub_default: object = on) => ({ effective, hub_default, override: over })
const session = (manage: boolean) => ({ capabilities: { manage_workspace: manage }, csrf_token: 'c', workspace: { id: 'w' }, actor: { user: { id: 'u' } } })
const setup = (data: object, manage: boolean | null) => {
  vi.mocked(api.fetchViewSettings).mockResolvedValue(data as never)
  vi.mocked(ws.workspaceRequest).mockImplementation(() => (manage === null ? Promise.reject(new ws.WorkspaceError(401, 'no')) : Promise.resolve(session(manage))) as never)
  render(<ViewSettingsBox token="t" />)
}
const sw = (name: string) => screen.findByRole<HTMLInputElement>('switch', { name })
const described = (el: HTMLElement) => (el.getAttribute('aria-describedby') ?? '').split(' ').map((id) => document.getElementById(id)?.textContent ?? '').join(' ')
const ready = async (name: string) => { const el = await sw(name); await waitFor(() => expect(el.disabled).toBe(false)); return el }

beforeEach(() => vi.clearAllMocks())
afterEach(() => vi.restoreAllMocks())

describe('inline agent views settings', () => {
  it('says in plain words when the hub is too old, never the HTTP status', async () => {
    vi.mocked(api.fetchViewSettings).mockRejectedValue(Object.assign(new Error('HTTP 404'), { status: 404 }))
    vi.mocked(ws.workspaceRequest).mockResolvedValue(session(false) as never)
    render(<ViewSettingsBox token="t" />)
    expect(await screen.findByText('This hub does not offer inline agent views')).toBeTruthy()
    expect(screen.getByText(/Ask your hub admin to update it/)).toBeTruthy()
    expect(document.body.textContent).not.toContain('HTTP')
  })
  it('renders effective values with their source', async () => {
    setup(report({ coordination_inbox: false, work_overview: null }), true)
    expect((await sw('Show views in agent apps')).checked).toBe(true)
    expect((await sw('Work overview')).checked).toBe(true)
    expect(described(await sw('Work overview'))).toMatch(/from the hub default/)
    expect(described(await sw('Coordination inbox'))).toMatch(/from your setting/)
    expect(described(await sw('Work overview'))).toMatch(/only to your own agents/)
  })
  it('PUTs only the toggled field to /me', async () => {
    setup(report(), false)
    vi.mocked(ws.workspaceMutation).mockResolvedValue(report({ coordination_inbox: true }, { ...on, coordination_inbox: true }) as never)
    const box = await ready('Coordination inbox')
    fireEvent.click(box)
    await waitFor(() => expect(ws.workspaceMutation).toHaveBeenCalledWith(expect.anything(), '/view-settings/me', { coordination_inbox: true }, { method: 'PUT' }))
    await waitFor(() => expect(box.checked).toBe(true))
    expect(described(box)).toMatch(/from your setting/)
  })
  it('resets to the hub default only when an override exists', async () => {
    setup(report(), true)
    await ready('Work overview')
    expect(screen.queryByRole('button', { name: 'Use the hub default' })).toBeNull()
  })
  it('sends reset', async () => {
    setup(report({ work_overview: false }, { ...on, work_overview: false }), true)
    vi.mocked(ws.workspaceMutation).mockResolvedValue(report() as never)
    await ready('Work overview')
    fireEvent.click(await screen.findByRole('button', { name: 'Use the hub default' }))
    await waitFor(() => expect(ws.workspaceMutation).toHaveBeenCalledWith(expect.anything(), '/view-settings/me', { reset: true }, { method: 'PUT' }))
    await waitFor(() => expect(screen.queryByRole('button', { name: 'Use the hub default' })).toBeNull())
  })
  it('lets an administrator edit the hub default', async () => {
    setup(report(), true)
    vi.mocked(ws.workspaceMutation).mockResolvedValue({ ...on, coordination_inbox: true } as never)
    const box = await ready('Hub default: Coordination inbox')
    fireEvent.click(box)
    await waitFor(() => expect(ws.workspaceMutation).toHaveBeenCalledWith(expect.anything(), '/view-settings', { ...on, coordination_inbox: true }, { method: 'PUT' }))
    await waitFor(() => expect(box.checked).toBe(true))
  })
  it('lets a non-admin member edit only their own views', async () => {
    setup(report(), false)
    expect((await ready('Work overview')).disabled).toBe(false)
    expect((await sw('Hub default: Work overview')).disabled).toBe(true)
    expect(await screen.findByText('Only a workspace administrator can change the hub default.')).toBeTruthy()
  })
  it('is read-only without a session', async () => {
    setup(report(), null)
    expect(await screen.findByText('Sign in to the workspace to change your views.')).toBeTruthy()
    expect((await sw('Show views in agent apps')).disabled).toBe(true)
    expect((await sw('Hub default: Work overview')).disabled).toBe(true)
  })
  it('labels child switches when the master is off', async () => {
    setup(report({ enabled: false }, { enabled: false, work_overview: true, coordination_inbox: true }), true)
    const child = await sw('Work overview')
    expect(child.disabled).toBe(true)
    expect(child.checked).toBe(false)
    expect(described(child)).toMatch(/Off because the master switch is off/)
  })
})
