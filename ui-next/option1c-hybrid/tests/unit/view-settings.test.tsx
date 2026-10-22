import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import * as api from '../../../../ui/src/api.ts'
import * as ws from '../../../../ui/src/workspace.ts'
import { ViewSettingsBox } from '../../src/ViewSettings.tsx'

vi.mock('../../../../ui/src/api.ts', () => ({ fetchViewSettings: vi.fn() }))
vi.mock('../../../../ui/src/workspace.ts', async (orig) => ({ ...(await orig<typeof ws>()), workspaceRequest: vi.fn(), workspaceMutation: vi.fn() }))

const on = { enabled: true, work_overview: true, coordination_inbox: false }
const session = (manage: boolean) => ({ capabilities: { manage_workspace: manage }, csrf_token: 'c', workspace: { id: 'w' }, actor: { user: { id: 'u' } } })
const setup = (settings: object, manage: boolean | null) => {
  vi.mocked(api.fetchViewSettings).mockResolvedValue(settings as never)
  vi.mocked(ws.workspaceRequest).mockImplementation(() => (manage === null ? Promise.reject(new ws.WorkspaceError(401, 'no')) : Promise.resolve(session(manage))) as never)
  render(<ViewSettingsBox token="t" />)
}
const sw = (name: string) => screen.findByRole<HTMLInputElement>('switch', { name })
const described = (el: HTMLElement) => (el.getAttribute('aria-describedby') ?? '').split(' ').map((id) => document.getElementById(id)?.textContent ?? '').join(' ')

beforeEach(() => vi.clearAllMocks())
afterEach(() => vi.restoreAllMocks())

describe('inline agent views settings', () => {
  it('renders the hub values', async () => {
    setup(on, true)
    expect((await sw('Show views in agent apps')).checked).toBe(true)
    expect((await sw('Work overview')).checked).toBe(true)
    expect((await sw('Coordination inbox')).checked).toBe(false)
    expect(described(await sw('Work overview'))).toMatch(/plain text answers/)
  })
  it('PUTs the full body when toggled', async () => {
    setup(on, true)
    vi.mocked(ws.workspaceMutation).mockResolvedValue({ ...on, coordination_inbox: true } as never)
    const box = await sw('Coordination inbox')
    await waitFor(() => expect(box.disabled).toBe(false))
    fireEvent.click(box)
    await waitFor(() => expect(ws.workspaceMutation).toHaveBeenCalledWith(expect.anything(), '/view-settings', { ...on, coordination_inbox: true }, { method: 'PUT' }))
    await waitFor(() => expect(box.checked).toBe(true))
  })
  it('is read-only without a session', async () => {
    setup(on, null)
    expect(await screen.findByText('Sign in to the workspace as an administrator to change this.')).toBeTruthy()
    expect((await sw('Show views in agent apps')).disabled).toBe(true)
  })
  it('is read-only for a non-admin session', async () => {
    setup(on, false)
    expect(await screen.findByText('Sign in to the workspace as an administrator to change this.')).toBeTruthy()
  })
  it('labels child switches when the master is off', async () => {
    setup({ enabled: false, work_overview: true, coordination_inbox: true }, true)
    const child = await sw('Work overview')
    expect(child.disabled).toBe(true)
    expect(child.checked).toBe(false)
    expect(described(child)).toMatch(/Off because the master switch is off/)
  })
})
