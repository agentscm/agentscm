import { render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { SetupStatus } from '../../src/onboarding.ts'
import { checksLine, SetupPage, setupAnswer } from '../../src/pages/Setup.tsx'

const NOW = 1_000_000_000
const row = (over: Partial<SetupStatus> = {}): SetupStatus => ({ agent: 'claude', key: 'a1b2c3d4e5f6', name: 'Claude Code', owner: 'maria', blocker: null, mcp_verified: true, native_verified: true, expires_at: NOW / 1000 + 600, next_action: 'Continue work.', ...over })
const codex = row({ agent: 'codex', key: '9f8e7d6c5b4a', name: 'Codex', native_verified: false, blocker: 'trust', next_action: 'Approve the hooks yourself.' })

const setup = vi.hoisted(() => ({ rows: [] as unknown[] }))
vi.mock('../../src/api.ts', async (orig) => ({ ...(await orig<typeof import('../../src/api.ts')>()), fetchSetup: () => Promise.resolve(setup.rows) }))

describe('setupAnswer', () => {
  it('counts ready agents and names the one that is not', () => {
    expect(setupAnswer([row(), codex], NOW)).toBe('1 of 2 agents is ready; maria’s Codex needs its AgentSCM hooks approved.')
    expect(setupAnswer([codex], NOW)).toBe('maria’s Codex needs its AgentSCM hooks approved.')
    expect(setupAnswer([row({ native_verified: false })], NOW)).toBe('maria’s Claude Code hasn’t checked a real edit yet.')
    expect(setupAnswer([row({ expires_at: 1 })], NOW)).toBe('maria’s Claude Code needs to check in again.')
  })

  it('says all are ready, or how many still need a step', () => {
    expect(setupAnswer([row()], NOW)).toBe('maria’s Claude Code is ready.')
    expect(setupAnswer([row(), row({ key: 'b' })], NOW)).toBe('All 2 agents are ready.')
    expect(setupAnswer([row(), codex, { ...codex, key: 'c' }], NOW)).toBe('1 of 3 agents is ready; 2 need a step from you.')
    expect(setupAnswer([codex, { ...codex, key: 'c' }], NOW)).toBe('None of the 2 agents is ready yet.')
  })

  it('keeps an instruction only when no agent has connected', () => {
    expect(setupAnswer([], NOW)).toBe('No agent has connected yet. Copy the setup instructions into your agent to start.')
  })
})

describe('checksLine', () => {
  it('shows both checks in one line, and none while evidence is not live', () => {
    expect(checksLine(codex, true)).toBe('MCP ✓ · Edit hook not yet')
    expect(checksLine(row({ agent: 'cursor' }), true)).toBe('MCP ✓ · Edit hook (after edit) ✓')
    expect(checksLine(row(), false)).toBe('MCP not yet · Edit hook not yet')
  })
})

describe('SetupPage', () => {
  beforeEach(() => {
    setup.rows = []
  })

  it('names each agent the way the Dashboard does, with one state pill and no repeated caveat', async () => {
    const at = { ...row(), expires_at: Date.now() / 1000 + 600 }
    setup.rows = [at, { ...codex, expires_at: at.expires_at }]
    const { container } = render(<SetupPage token="t" />)
    await waitFor(() => expect(screen.getByText('maria’s Codex')).toBeTruthy())
    expect(screen.getByText('maria’s Claude Code')).toBeTruthy()
    expect(container.querySelector('.answer-text')?.textContent).toBe('1 of 2 agents is ready; maria’s Codex needs its AgentSCM hooks approved.')
    expect(container.querySelectorAll('.setup-row')[1]?.querySelectorAll('.pill')).toHaveLength(1)
    expect(container.textContent).not.toMatch(/For Ready agents|coordination evidence/)
  })

  it('answers the zero-agents case with what to do', async () => {
    const { container } = render(<SetupPage token="t" />)
    await waitFor(() => expect(screen.getByText('No agent has checked in yet')).toBeTruthy())
    expect(container.querySelector('.answer-text')?.textContent).toBe('No agent has connected yet. Copy the setup instructions into your agent to start.')
  })
})
