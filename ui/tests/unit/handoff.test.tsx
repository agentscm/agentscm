import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { NextStepView } from '../../src/NextStep.tsx'
import { PLACEHOLDER, type NextStep } from '../../src/nextStep.ts'
import { StartTask, startTaskPrompt } from '../../src/StartTask.tsx'

const row = (agent: string, owner: string) => ({ agent, key: `${agent}${owner}`, name: agent, owner, blocker: null, mcp_verified: true, native_verified: true, expires_at: 9e9, next_action: '' })
const setup = vi.hoisted(() => ({ rows: [] as unknown[] }))
vi.mock('../../src/api.ts', async (orig) => ({ ...(await orig<typeof import('../../src/api.ts')>()), fetchSetup: () => Promise.resolve(setup.rows) }))

const step = (over: Partial<NextStep['prompts'][number]> = {}): NextStep => ({ human: 'Do it', prompts: [{ for: 'maria’s claude-code', text: 'Resume work', login: 'maria', agent: 'claude-code', ...over }] })
const view = (s: NextStep, viewer: string | null) => render(<NextStepView step={s} viewer={viewer} repo="acme/widgets" originUrl="https://github.com/acme/widgets" />)

describe('NextStepView', () => {
  it('offers Open in beside Copy for the viewer’s own agent only', () => {
    view(step(), 'maria')
    expect(screen.getByRole('button', { name: 'Copy prompt for maria’s claude-code' })).toBeTruthy()
    const open = screen.getByRole('link', { name: /Open in Claude Code/ })
    expect(open.getAttribute('href')).toBe('claude-cli://open?repo=acme/widgets&q=Resume%20work')
    expect(open.getAttribute('title')).toMatch(/Nothing is sent until you press Enter/)
  })

  it('keeps Copy only for someone else’s agent, an agent without a link, or no known viewer', () => {
    const { unmount } = view(step(), 'dev')
    expect(screen.queryByRole('link')).toBeNull()
    unmount()
    const again = view(step({ agent: 'gemini' }), 'maria')
    expect(screen.queryByRole('link')).toBeNull()
    expect(screen.getByRole('button', { name: /Copy prompt for/ })).toBeTruthy()
    again.unmount()
    view(step(), null)
    expect(screen.queryByRole('link')).toBeNull()
  })

  it('shows who decides', () => {
    view({ ...step(), decides: ['maria', 'dev'] }, 'dev')
    expect(screen.getByText('Decision for maria and dev')).toBeTruthy()
  })

  it('will not open an app with the blank still in the prompt, and fills it from the person’s words', () => {
    view(step({ text: `Here is the decision: ${PLACEHOLDER}. Go.` }), 'maria')
    const disabled = screen.getByRole('button', { name: /Open in Claude Code/ }) as HTMLButtonElement
    expect(disabled.disabled).toBe(true)
    expect(screen.queryByRole('link')).toBeNull()
    expect(screen.getByRole('button', { name: /Copy prompt for/ })).toBeTruthy()
    fireEvent.change(screen.getByLabelText('Your decision'), { target: { value: 'use “Postgres”' } })
    const href = screen.getByRole('link', { name: /Open in Claude Code/ }).getAttribute('href')!
    expect(decodeURIComponent(href)).toContain("Here is the decision: use 'Postgres'. Go.")
    expect(href).not.toContain('write')
  })
})

describe('StartTask', () => {
  beforeEach(() => { setup.rows = [] })
  const card = () => render(<StartTask token="t" me="maria" repo="acme/widgets" originUrl="https://github.com/acme/widgets" />)

  it('builds the prompt: overview, preflight, then start', () => {
    expect(startTaskPrompt('Fix “login”\nbug')).toBe("Use AgentSCM. Read agentscm_work_overview, run agentscm_work_preflight for the files you plan to change, then agentscm_start with a fixed merge window. Task: “Fix 'login' bug”")
  })

  it('opens only the viewer’s own supported agents, and disables everything until a task is described', async () => {
    setup.rows = [row('claude', 'maria'), row('cursor', 'maria'), row('copilot', 'maria'), row('codex', 'dev')]
    card()
    await screen.findByRole('button', { name: /Open in Claude Code/ })
    expect(screen.getByRole('button', { name: /Open in Cursor/ })).toBeTruthy()
    expect(screen.queryByText(/Open in Codex/)).toBeNull()
    expect(screen.queryByText(/Open in Copilot/)).toBeNull()
    for (const b of screen.getAllByRole('button')) expect((b as HTMLButtonElement).disabled).toBe(true)
    fireEvent.change(screen.getByLabelText('What should the agent do?'), { target: { value: 'fix login' } })
    const link = screen.getByRole('link', { name: /Open in Claude Code/ })
    expect(link.getAttribute('href')).toMatch(/^claude-cli:\/\/open\?repo=acme\/widgets&q=.*agentscm_work_preflight.*fix%20login/)
    expect(screen.getByRole('button', { name: /Copy prompt/ })).toBeTruthy()
  })

  it('points to Setup › Agents when the viewer has no connected agent', async () => {
    setup.rows = [row('claude', 'dev')]
    card()
    await waitFor(() => expect(screen.getByRole('link', { name: /Connect one in Setup/ }).getAttribute('href')).toBe('#setup'))
    expect(screen.queryByText(/Open in/)).toBeNull()
  })
})
