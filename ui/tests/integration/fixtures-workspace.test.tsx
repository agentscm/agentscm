// Workspace screens and every workspace mutation, against fixtures (the dev hub has no forge sign-in).
// The mock in ui/dev/mock.ts answers reads; overrides answer writes the way the hub API documents them.
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { WorkspacePage } from '../../src/pages/Workspace.tsx'
import { fromMock, json, stubHub, type Override, type Sent } from '../fixtures/mockFetch.ts'

const LOAD = { timeout: 5_000 }
const CSRF = 'synthetic-csrf-no-authority'
const ACTOR = 'synthetic-member'
const REPO = encodeURIComponent('acme/widgets')
const SUBJECT = { change_id: 'change-invoice-rounding', change_version: 3, context_digest: 'c'.repeat(64), revision: '2'.repeat(64), requirement_digest: 'd'.repeat(64) }
const path = (url: URL) => url.pathname.replace(/^\/v1\/hub\/workspace/, '')
const isWrite = (init: RequestInit) => (init.method ?? 'GET') !== 'GET'
const bodyOf = (init: RequestInit) => JSON.parse(String(init.body)) as Record<string, unknown>
const at = (hash: string) => act(() => {
  window.location.hash = hash
  window.dispatchEvent(new HashChangeEvent('hashchange'))
})
const writes = (sent: Sent[]) => sent.filter((r) => r.method !== 'GET')

/** The exact set of writes this page may make. Each carries the CSRF header; mutations carry a request ID. */
const ALLOWED = [
  ['DELETE', '/v1/hub/workspace/session'],
  ['POST', /^\/v1\/hub\/workspace\/repositories\/[^/]+\/runs$/],
  ['POST', /^\/v1\/hub\/workspace\/repositories\/[^/]+\/runs\/[^/]+\/cancel$/],
  ['POST', /^\/v1\/hub\/workspace\/repositories\/[^/]+\/profiles\/[^/]+\/(approve|integrate)$/],
  ['PUT', '/v1/hub/workspace/members'],
  ['PUT', '/v1/hub/workspace/settings'],
] as const
function expectAllowedWrites(sent: Sent[]) {
  for (const w of writes(sent)) {
    expect(ALLOWED.some(([m, p]) => m === w.method && (typeof p === 'string' ? p === w.url : p.test(w.url))), `${w.method} ${w.url}`).toBe(true)
    expect(w.headers['x-csrf-token']).toBe(CSRF)
    if (w.method !== 'DELETE') expect(w.body?.['request_id']).toMatch(/^[0-9a-f-]{36}$/)
  }
}

/** Session with changed capabilities (the mock grants manage, read and execute only). */
const withCaps = (caps: Record<string, boolean>): Override => (url, init) =>
  path(url) === '/session' && !isWrite(init) ? fromMock(url, init, (s) => ({ ...s, capabilities: { ...(s['capabilities'] as object), ...caps } })) : undefined

const startRun = (subject = SUBJECT, state = 'queued') => ({ run_id: 'ws-run-new', repository_id: 'acme/widgets', profile_id: 'quality-gate', actor_id: ACTOR, generation: 1, state, subject, native_run_id: null, receipt: null, detail: null })

beforeEach(() => sessionStorage.clear())
afterEach(() => {
  vi.unstubAllGlobals()
  sessionStorage.clear()
  window.location.hash = ''
})

async function openComposerWithSubject() {
  render(<WorkspacePage />)
  const profile = await screen.findByRole('combobox', { name: 'Workflow profile' }, LOAD)
  fireEvent.change(profile, { target: { value: 'quality-gate' } })
  const known = screen.getByRole('combobox', { name: 'Current native Changes' })
  await waitFor(() => expect(within(known).getAllByRole('option')).toHaveLength(2), LOAD)
  fireEvent.change(known, { target: { value: SUBJECT.change_id } })
  expect((screen.getByRole('textbox', { name: /Native Change ID/ }) as HTMLInputElement).value).toBe(SUBJECT.change_id)
  fireEvent.click(screen.getByRole('button', { name: 'Load revision' }))
  return screen.findByRole('region', { name: 'Revision to execute' }, LOAD)
}

describe('signed-out entrance', () => {
  it('lists configured forge providers as sign-in links, ignoring unknown ones', async () => {
    stubHub((url) => {
      if (path(url) === '/session') return json(401, { error: 'unauthorized' })
      if (path(url) === '/login') return json(200, { providers: ['github', 'gitlab', 'javascript'] })
    })
    render(<WorkspacePage />)
    const gh = await screen.findByRole('link', { name: /Continue with GitHub/ }, LOAD)
    expect(gh.getAttribute('href')).toBe('/v1/hub/login/github?workspace=1')
    expect(screen.getByRole('link', { name: /Continue with GitLab/ }).getAttribute('href')).toBe('/v1/hub/login/gitlab?workspace=1')
    expect(screen.getAllByRole('link', { name: /Continue with/ })).toHaveLength(2)
  })

  it('says so when no sign-in provider is configured', async () => {
    stubHub((url) => (path(url) === '/session' ? json(401, { error: 'unauthorized' }) : undefined))
    render(<WorkspacePage />)
    expect(await screen.findByText('Workspace sign-in is not configured on this hub.', {}, LOAD)).toBeTruthy()
  })
})

describe('signed-in overview', () => {
  it('identity, roles, capabilities, repositories and runs — reads only', async () => {
    const sent = stubHub()
    render(<WorkspacePage />)
    expect(await screen.findByText(/signed in to/, {}, LOAD)).toBeTruthy()
    expect(screen.getByText('Synthetic workspace fixture')).toBeTruthy()
    const you = screen.getByRole('heading', { name: 'You' }).closest('section')!
    for (const role of ['reader', 'contributor', 'administrator']) expect(within(you).getByText(role)).toBeTruthy()
    const rows = [...you.querySelectorAll('.caps li')]
    const cap = (label: string) => rows.find((li) => li.textContent!.includes(label))!.className
    expect([cap('Read repositories'), cap('Run workflows'), cap('Approve revisions'), cap('Integrate revisions'), cap('Manage workspace')]).toEqual(['is-on', 'is-on', 'is-off', 'is-off', 'is-on'])

    await screen.findByText('synthetic-workspace-run', {}, LOAD)
    const runs = [...document.querySelectorAll('.ws-run')].map((r) => [r.querySelector('code')!.textContent, r.querySelector('.pill')!.textContent])
    expect(runs).toEqual([
      ['synthetic-workspace-run', 'succeeded'],
      ['synthetic-unknown-run', 'unknown'],
    ])
    // Inspect links for runs with a native workflow; nothing here is active, so nothing to cancel.
    expect(screen.getAllByRole('link', { name: /Inspect/ }).map((a) => a.getAttribute('href'))).toEqual([`#workspace/workflow/${REPO}/synthetic-workspace-run`, `#workspace/workflow/${REPO}/synthetic-unknown-run`])
    expect(screen.queryByRole('button', { name: /^Cancel$/ })).toBeNull()
    expect(writes(sent)).toEqual([])
  })

  it('section tabs follow the address and mark the current one', async () => {
    stubHub()
    at('#workspace/members')
    render(<WorkspacePage />)
    await screen.findByRole('list', { name: 'Workspace members' }, LOAD)
    const tabs = screen.getByRole('navigation', { name: 'Workspace sections' })
    expect(within(tabs).getByRole('link', { name: /Members/ }).getAttribute('aria-current')).toBe('page')
    at('#workspace/settings')
    expect(await screen.findByRole('textbox', { name: 'Workspace name' }, LOAD)).toBeTruthy()
    expect(within(tabs).getByRole('link', { name: /Settings/ }).getAttribute('aria-current')).toBe('page')
  })

  it('runs page past the first 20 with the before cursor', async () => {
    const page2 = { ...startRun(), run_id: 'older-run', state: 'succeeded' }
    const sent = stubHub((url, init) => {
      if (path(url) !== `/repositories/${REPO}/runs`) return undefined
      return url.searchParams.get('before') === 'cursor-1' ? json(200, { runs: [page2], next_cursor: null }) : fromMock(url, init, (b) => ({ ...b, next_cursor: 'cursor-1' }))
    })
    render(<WorkspacePage />)
    await screen.findByText('synthetic-workspace-run', {}, LOAD)
    expect(sent.find((r) => r.url.includes('/runs?'))!.url).toBe(`/v1/hub/workspace/repositories/${REPO}/runs?limit=20`)
    fireEvent.click(screen.getByRole('button', { name: /Older runs/ }))
    await screen.findByText('older-run', {}, LOAD)
    expect(sent.some((r) => r.url === `/v1/hub/workspace/repositories/${REPO}/runs?limit=20&before=cursor-1`)).toBe(true)
    expect((screen.getByRole('button', { name: /Older runs/ }) as HTMLButtonElement).disabled).toBe(true)
    fireEvent.click(screen.getByRole('button', { name: /Newer runs/ }))
    await screen.findByText('synthetic-workspace-run', {}, LOAD)
  })
})

describe('capability gating', () => {
  it('task-only profiles inspect empty server history and keep native admission without start controls', async () => {
    const sent = stubHub(
      withCaps({ approve_revisions: true, integrate_revisions: true }),
      (url, init) => path(url) === '/repositories' && !isWrite(init) ? fromMock(url, init, (body) => ({
        ...body,
        repositories: [{
          repository_id: 'acme/widgets',
          profiles: [{
            profile_id: 'quality-gate',
            target: 'main',
            requirement_digest: SUBJECT.requirement_digest,
            server_execution: false,
            readiness: 'configured',
            limitations: 'Agent-owned tasks only.',
          }],
        }],
      })) : undefined,
      (url, init) => path(url) === `/repositories/${REPO}/runs` && !isWrite(init) ? json(200, { runs: [], next_cursor: null }) : undefined,
    )
    const subject = await openComposerWithSubject()
    expect(screen.getByText('No server-executed runs yet')).toBeTruthy()
    expect(screen.getByText(/Task qualification is recorded through agent-owned tasks/)).toBeTruthy()
    expect(within(subject).queryByRole('button', { name: /Start workflow/ })).toBeNull()
    expect(within(subject).getByText(/agent-owned task qualification only/)).toBeTruthy()
    expect(within(subject).getByRole('button', { name: /Approve revision/ })).toBeTruthy()
    expect(within(subject).getByRole('button', { name: /Land revision/ })).toBeTruthy()
    expect(sent.some((request) => request.url === `/v1/hub/workspace/repositories/${REPO}/runs?limit=20`)).toBe(true)
    expect(writes(sent)).toEqual([])
  })

  it('without execute_workflows: no Start or Cancel buttons, and a note why', async () => {
    stubHub(withCaps({ execute_workflows: false }), (url, init) => (path(url) === `/repositories/${REPO}/runs` && !isWrite(init) ? json(200, { runs: [startRun(SUBJECT, 'running')], next_cursor: null }) : undefined))
    const subject = await openComposerWithSubject()
    expect(within(subject).queryByRole('button', { name: /Start workflow/ })).toBeNull()
    expect(within(subject).getByText('Starting runs requires the contributor role.')).toBeTruthy()
    expect(screen.getByText(/Starting or cancelling runs requires the contributor role/)).toBeTruthy()
    expect(screen.queryByRole('button', { name: /^Cancel$/ })).toBeNull()
  })

  it('without approve or integrate: no Approve or Land buttons, and the role each needs', async () => {
    stubHub()
    const subject = await openComposerWithSubject()
    expect(within(subject).queryByRole('button', { name: /Approve revision/ })).toBeNull()
    expect(within(subject).queryByRole('button', { name: /Land revision/ })).toBeNull()
    expect(within(subject).getByText('Reviewer role required to approve.')).toBeTruthy()
    expect(within(subject).getByText('Integrator role required to land.')).toBeTruthy()
  })

  it('without manage_workspace: members and settings are not requested', async () => {
    const sent = stubHub(withCaps({ manage_workspace: false }))
    at('#workspace/members')
    render(<WorkspacePage />)
    expect(await screen.findByText('Administrator access is required to view or change workspace members.', {}, LOAD)).toBeTruthy()
    at('#workspace/settings')
    expect(await screen.findByText('Administrator access is required to view or change workspace settings.', {}, LOAD)).toBeTruthy()
    expect(sent.some((r) => r.url.includes('/members') || r.url.includes('/settings'))).toBe(false)
  })

  it('without read_repositories: repositories are not requested', async () => {
    const sent = stubHub(withCaps({ read_repositories: false }))
    render(<WorkspacePage />)
    expect(await screen.findByText('Your membership does not include repository read access.', {}, LOAD)).toBeTruthy()
    expect(sent.some((r) => r.url.includes('/repositories'))).toBe(false)
  })
})

describe('start a run', () => {
  it('POSTs the exact subject, profile and generation with CSRF and a request ID, then refreshes runs', async () => {
    let runsAfter = 0
    const sent = stubHub((url, init) => {
      if (path(url) === `/repositories/${REPO}/runs` && isWrite(init)) return json(201, startRun(bodyOf(init)['subject'] as typeof SUBJECT))
      if (path(url).startsWith(`/repositories/${REPO}/runs`) && sent.some((r) => r.method === 'POST')) runsAfter++
    })
    const subject = await openComposerWithSubject()
    expect(within(subject).getByText(SUBJECT.revision)).toBeTruthy()
    fireEvent.click(within(subject).getByRole('button', { name: /Start workflow/ }))
    expect(await screen.findByText('Run ws-run-new accepted as queued.', {}, LOAD)).toBeTruthy()
    const [w] = writes(sent)
    expect(w).toMatchObject({ method: 'POST', url: `/v1/hub/workspace/repositories/${REPO}/runs`, body: { subject: SUBJECT, profile_id: 'quality-gate', expected_generation: 1 } })
    expectAllowedWrites(sent)
    await waitFor(() => expect(runsAfter).toBeGreaterThan(0), LOAD)
    expect(screen.queryByRole('heading', { name: 'Pending operation recovery' })).toBeNull()
  })

  it('a 409 generation conflict is reported and nothing is retained for retry', async () => {
    stubHub((url, init) => (path(url) === `/repositories/${REPO}/runs` && isWrite(init) ? json(409, { error: 'generation_conflict' }) : undefined))
    const subject = await openComposerWithSubject()
    fireEvent.click(within(subject).getByRole('button', { name: /Start workflow/ }))
    expect(await screen.findByText(/The workspace changed while you were editing/, {}, LOAD)).toBeTruthy()
    expect(screen.queryByText('Start request outcome unknown')).toBeNull()
    expect(screen.queryByRole('heading', { name: 'Pending operation recovery' })).toBeNull()
  })

  it('an unknown outcome is journalled; Pending operation recovery retries the exact request', async () => {
    let fail = true
    const sent = stubHub((url, init) => {
      if (path(url) === `/repositories/${REPO}/runs` && isWrite(init)) return fail ? json(502, { error: 'bad_gateway' }) : json(201, startRun(bodyOf(init)['subject'] as typeof SUBJECT))
    })
    const subject = await openComposerWithSubject()
    fireEvent.click(within(subject).getByRole('button', { name: /Start workflow/ }))
    expect(await screen.findByText('Start request outcome unknown', {}, LOAD)).toBeTruthy()
    // The form is locked until the outcome is known.
    expect((screen.getByRole('combobox', { name: 'Workflow profile' }) as HTMLSelectElement).disabled).toBe(true)
    const recovery = (await screen.findByRole('heading', { name: 'Pending operation recovery' }, LOAD)).closest('section')!
    expect(within(recovery).getByText(/Workflow start outcome unknown/)).toBeTruthy()
    const firstId = String(writes(sent)[0]!.body!['request_id'])
    expect(within(recovery).getByText(firstId)).toBeTruthy()

    fail = false
    fireEvent.click(within(recovery).getByRole('button', { name: /Retry exact request/ }))
    expect(await within(recovery).findByText('Workflow start returned a conclusive result.', {}, LOAD)).toBeTruthy()
    expect(within(recovery).getByText('ws-run-new')).toBeTruthy()
    // Focus moves to the outcome rather than being dropped with the retry button.
    expect(recovery.contains(document.activeElement) && document.activeElement !== document.body).toBe(true)
    const [a, b] = writes(sent)
    expect(b!.body).toEqual(a!.body) // same fields, same request ID
    expectAllowedWrites(sent)
    expect(sessionStorage.length).toBe(0)
  })

  it('a success that does not match the retained request keeps it for retry', async () => {
    stubHub((url, init) => (path(url) === `/repositories/${REPO}/runs` && isWrite(init) ? json(201, { ...startRun(), actor_id: 'someone-else' }) : undefined))
    const subject = await openComposerWithSubject()
    fireEvent.click(within(subject).getByRole('button', { name: /Start workflow/ }))
    expect((await screen.findAllByText(/did not match the retained actor, repository, profile and exact subject/, {}, LOAD)).length).toBeGreaterThan(0)
    expect(await screen.findByRole('heading', { name: 'Pending operation recovery' }, LOAD)).toBeTruthy()
  })
})

describe('cancel a run', () => {
  it('asks for confirmation, then POSTs cancel with the run generation', async () => {
    const running = { ...startRun(SUBJECT, 'running'), run_id: 'active-run', generation: 4, native_run_id: 'claude-review-passed' }
    const sent = stubHub((url, init) => {
      if (path(url) === `/repositories/${REPO}/runs` && !isWrite(init)) return json(200, { runs: [running], next_cursor: null })
      if (path(url) === `/repositories/${REPO}/runs/active-run/cancel`) return json(200, { ...running, state: 'cancelled' })
    })
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: /^Cancel$/ }, LOAD))
    const group = screen.getByRole('group', { name: 'Cancel run active-run?' })
    expect(document.activeElement).toBe(within(group).getByRole('button', { name: 'Cancel run' }))
    fireEvent.click(within(group).getByRole('button', { name: 'Keep' }))
    expect(writes(sent)).toEqual([])
    fireEvent.click(screen.getByRole('button', { name: /^Cancel$/ }))
    fireEvent.click(within(screen.getByRole('group', { name: 'Cancel run active-run?' })).getByRole('button', { name: 'Cancel run' }))
    expect(await screen.findByText(/Cancellation requested for active-run/, {}, LOAD)).toBeTruthy()
    expect(writes(sent)).toMatchObject([{ method: 'POST', url: `/v1/hub/workspace/repositories/${REPO}/runs/active-run/cancel`, body: { expected_generation: 4 } }])
    expectAllowedWrites(sent)
  })

  it('Escape backs out of the confirmation without sending', async () => {
    const queued = { ...startRun(SUBJECT, 'queued'), run_id: 'queued-run' }
    const sent = stubHub((url, init) => (path(url) === `/repositories/${REPO}/runs` && !isWrite(init) ? json(200, { runs: [queued], next_cursor: null }) : undefined))
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: /^Cancel$/ }, LOAD))
    fireEvent.keyDown(screen.getByRole('group', { name: 'Cancel run queued-run?' }), { key: 'Escape' })
    expect(screen.queryByRole('group', { name: 'Cancel run queued-run?' })).toBeNull()
    expect(document.activeElement).toBe(screen.getByRole('button', { name: /^Cancel$/ }))
    expect(writes(sent)).toEqual([])
  })
})

describe('approve and land', () => {
  const admission: Override = (url, init) => {
    if (!isWrite(init)) return undefined
    const b = bodyOf(init)
    if (path(url) === `/repositories/${REPO}/profiles/quality-gate/approve`) return json(200, { approval_id: `ws-approve-${ACTOR}-${String(b['request_id'])}`, actor_id: ACTOR, revision: SUBJECT.revision, context_digest: SUBJECT.context_digest })
    if (path(url) === `/repositories/${REPO}/profiles/quality-gate/integrate`) return json(200, { operation_id: `ws-integrate-${ACTOR}-${String(b['request_id'])}`, target: 'main', context_digest: SUBJECT.context_digest, approval_id: b['approval_id'], after: { revision: SUBJECT.revision, version: 9 } })
  }

  it('approval then landing, each confirmed, bound to the exact subject, with readiness and limitations shown', async () => {
    const sent = stubHub(withCaps({ approve_revisions: true, integrate_revisions: true }), admission)
    const subject = await openComposerWithSubject()
    expect(screen.getByText('configured')).toBeTruthy()
    expect(screen.getByText('target main')).toBeTruthy()
    expect(screen.getByText('Synthetic profile for interface development only.')).toBeTruthy()

    const land = within(subject).getByRole('button', { name: /Land revision to main/ }) as HTMLButtonElement
    expect(land.disabled).toBe(true) // no approval ID yet

    fireEvent.click(within(subject).getByRole('button', { name: /Approve revision/ }))
    fireEvent.click(within(subject).getByRole('button', { name: 'Approve' }))
    expect(await within(subject).findByText('Native approval recorded', {}, LOAD)).toBeTruthy()
    const approvalId = (within(subject).getByRole('textbox', { name: 'Native approval ID' }) as HTMLInputElement).value
    expect(approvalId).toMatch(new RegExp(`^ws-approve-${ACTOR}-`))

    fireEvent.click(within(subject).getByRole('button', { name: /Land revision to main/ }))
    const confirm = within(subject).getByRole('group', { name: /Land revision 222222222222 on main\?/ })
    fireEvent.click(within(confirm).getByRole('button', { name: 'Land to main' }))
    expect(await within(subject).findByText('Landed to main', {}, LOAD)).toBeTruthy()
    expect(within(subject).getByText(/Native target version 9/)).toBeTruthy()

    const [approve, integrate] = writes(sent)
    expect(approve).toMatchObject({ url: `/v1/hub/workspace/repositories/${REPO}/profiles/quality-gate/approve`, body: { subject: SUBJECT, expected_generation: 1 } })
    expect(integrate).toMatchObject({ url: `/v1/hub/workspace/repositories/${REPO}/profiles/quality-gate/integrate`, body: { subject: SUBJECT, expected_generation: 1, approval_id: approvalId } })
    expectAllowedWrites(sent)
  })

  it('an unknown landing outcome offers "Retry same landing" with the retained request', async () => {
    let fail = true
    const sent = stubHub(withCaps({ integrate_revisions: true }), (url, init) => (fail && isWrite(init) && path(url).endsWith('/integrate') ? json(503, { error: 'unavailable' }) : admission(url, init)))
    const subject = await openComposerWithSubject()
    fireEvent.change(within(subject).getByRole('textbox', { name: 'Native approval ID' }), { target: { value: 'appr-from-reviewer' } })
    fireEvent.click(within(subject).getByRole('button', { name: /Land revision to main/ }))
    fireEvent.click(within(subject).getByRole('button', { name: 'Land to main' }))
    expect(await within(subject).findByText('Landing outcome unknown', {}, LOAD)).toBeTruthy()
    fail = false
    fireEvent.click(within(subject).getByRole('button', { name: /Retry same landing/ }))
    fireEvent.click(within(subject).getByRole('button', { name: 'Land to main' }))
    expect(await within(subject).findByText('Landed to main', {}, LOAD)).toBeTruthy()
    const [a, b] = writes(sent)
    expect(b!.body).toEqual(a!.body)
    expect(a!.body).toMatchObject({ approval_id: 'appr-from-reviewer' })
  })
})

describe('members', () => {
  it('adds a member with PUT /members and the list generation; invalid input blocks saving', async () => {
    const sent = stubHub((url, init) => (path(url) === '/members' && isWrite(init) ? json(200, { ...bodyOf(init), generation: 2 }) : undefined))
    at('#workspace/members')
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: /Add member/ }, LOAD))
    const form = screen.getByRole('form', { name: 'Add forge member' })
    const save = within(form).getByRole('button', { name: 'Save member' }) as HTMLButtonElement
    expect(save.disabled).toBe(true)
    expect(within(form).getByText('Enter a positive, safe numeric user ID.')).toBeTruthy()
    fireEvent.change(within(form).getByRole('textbox', { name: /Numeric forge user ID/ }), { target: { value: '0042' } })
    expect(save.disabled).toBe(true)
    fireEvent.change(within(form).getByRole('textbox', { name: /Numeric forge user ID/ }), { target: { value: '4242' } })
    fireEvent.click(within(form).getByRole('checkbox', { name: 'contributor' }))
    fireEvent.change(within(form).getByRole('textbox', { name: 'Repository IDs' }), { target: { value: 'acme/widgets, acme/gadgets' } })
    expect(save.disabled).toBe(false)
    fireEvent.click(save)
    await waitFor(() => expect(screen.queryByRole('form')).toBeNull(), LOAD)
    expect(writes(sent)).toMatchObject([
      { method: 'PUT', url: '/v1/hub/workspace/members', body: { expected_generation: 1, identity: { provider: 'github', host: 'github.com', user_id: 4242 }, roles: ['reader', 'contributor'], repository_ids: ['acme/widgets', 'acme/gadgets'], enabled: true } },
    ])
    expectAllowedWrites(sent)
  })

  it('edits an existing member without changing its identity', async () => {
    const sent = stubHub((url, init) => (path(url) === '/members' && isWrite(init) ? json(200, bodyOf(init)) : undefined))
    at('#workspace/members')
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: 'Edit member 1002' }, LOAD))
    const form = screen.getByRole('form', { name: 'Edit member' })
    expect((within(form).getByRole('textbox', { name: /Numeric forge user ID/ }) as HTMLInputElement).disabled).toBe(true)
    fireEvent.click(within(form).getByRole('checkbox', { name: 'Membership enabled' }))
    fireEvent.click(within(form).getByRole('button', { name: 'Save member' }))
    await waitFor(() => expect(writes(sent)).toHaveLength(1), LOAD)
    expect(writes(sent)[0]!.body).toMatchObject({ identity: { provider: 'github', host: 'github.com', user_id: 1002 }, roles: ['reader', 'reviewer'], repository_ids: ['acme/widgets'], enabled: false })
  })

  it.each([
    [{ error: 'generation_conflict' }, /The workspace changed while you were editing/],
    [{ error: 'last_administrator' }, /Keep at least one enabled administrator/],
    [{ error: 'member_limit' }, /member limit has been reached/],
  ])('409 %o is explained and the form stays open', async (reply, text) => {
    stubHub((url, init) => (path(url) === '/members' && isWrite(init) ? json(409, reply) : undefined))
    at('#workspace/members')
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: 'Edit member 1001' }, LOAD))
    fireEvent.click(screen.getByRole('button', { name: 'Save member' }))
    expect(await screen.findByText(text, {}, LOAD)).toBeTruthy()
    expect(screen.getByRole('form', { name: 'Edit member' })).toBeTruthy()
    expect(screen.queryByRole('heading', { name: 'Pending operation recovery' })).toBeNull()
  })

  it('loads more members, and stops when the membership generation changed', async () => {
    const member = (id: number) => ({ identity: { provider: 'github', host: 'github.com', user_id: id }, roles: ['reader'], repository_ids: ['*'], enabled: true, generation: 1 })
    const sent = stubHub((url) => {
      if (path(url) !== '/members') return undefined
      const after = url.searchParams.get('after')
      if (!after) return json(200, { members: [member(1)], next_cursor: 'm1', generation: 5 })
      if (after === 'm1') return json(200, { members: [member(2)], next_cursor: 'm2', generation: 5 })
      return json(200, { members: [member(3)], next_cursor: null, generation: 6 })
    })
    at('#workspace/members')
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: 'Load more members' }, LOAD))
    await screen.findByText('github.com · user 2', {}, LOAD)
    expect(sent.some((r) => r.url === '/v1/hub/workspace/members?limit=50&after=m1')).toBe(true)
    fireEvent.click(screen.getByRole('button', { name: 'Load more members' }))
    expect(await screen.findByText(/Membership changed while loading/, {}, LOAD)).toBeTruthy()
    expect(screen.queryByText('github.com · user 3')).toBeNull()
  })
})

describe('settings', () => {
  it('saves the name with PUT /settings and the expected generation', async () => {
    const sent = stubHub((url, init) => (path(url) === '/settings' && isWrite(init) ? json(200, { ...bodyOf(init), id: 'synthetic-workspace', generation: 2 }) : undefined))
    at('#workspace/settings')
    render(<WorkspacePage />)
    const name = await screen.findByRole('textbox', { name: 'Workspace name' }, LOAD)
    const save = screen.getByRole('button', { name: 'Save settings' }) as HTMLButtonElement
    expect(save.disabled).toBe(true) // unchanged
    fireEvent.change(name, { target: { value: '   ' } })
    expect(save.disabled).toBe(true)
    expect(screen.getByText('Enter a workspace name.')).toBeTruthy()
    fireEvent.change(name, { target: { value: '  Billing team  ' } })
    fireEvent.click(save)
    await waitFor(() => expect(writes(sent)).toHaveLength(1), LOAD)
    expect(writes(sent)[0]).toMatchObject({ method: 'PUT', url: '/v1/hub/workspace/settings', body: { name: 'Billing team', max_concurrent_runs: 1, expected_generation: 1 } })
    expectAllowedWrites(sent)
  })

  it('a 409 is explained', async () => {
    stubHub((url, init) => (path(url) === '/settings' && isWrite(init) ? json(409, { error: 'generation_conflict' }) : undefined))
    at('#workspace/settings')
    render(<WorkspacePage />)
    fireEvent.change(await screen.findByRole('textbox', { name: 'Workspace name' }, LOAD), { target: { value: 'New name' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save settings' }))
    expect(await screen.findByText(/The workspace changed while you were editing/, {}, LOAD)).toBeTruthy()
  })
})

describe('workspace workflow run deep link', () => {
  it('shows the retained workflow for a workspace run, with step links that stay in the workspace', async () => {
    stubHub()
    at(`#workspace/workflow/${REPO}/synthetic-workspace-run`)
    render(<WorkspacePage />)
    expect(await screen.findByText('Checks and bindings are current', {}, LOAD)).toBeTruthy()
    const step = screen.getByRole('link', { name: /^claude-review/ })
    expect(step.getAttribute('href')).toBe(`#workspace/workflow/${REPO}/synthetic-workspace-run/steps/claude-review`)
    at(step.getAttribute('href')!)
    expect(await screen.findByText('Step: claude-review', {}, LOAD)).toBeTruthy()
    at(`#workspace/workflow/${REPO}/synthetic-workspace-run/steps/no-such-step`)
    expect(await screen.findByText('This retained step was not found.', {}, LOAD)).toBeTruthy()
  })

  it('an invalid link and an unavailable journal are explained', async () => {
    stubHub((url) => (path(url).endsWith('/workflow') ? json(200, { version: 1, availability: 'unavailable', reason: 'native_workflow_not_configured' }) : undefined))
    at('#workspace/workflow/%E0%A4%A/x')
    render(<WorkspacePage />)
    expect(await screen.findByText('Invalid workspace run link.', {}, LOAD)).toBeTruthy()
    at(`#workspace/workflow/${REPO}/synthetic-workspace-run`)
    expect(await screen.findByText('Retained workflow is not available yet', {}, LOAD)).toBeTruthy()
  })
})

describe('sign out', () => {
  it('DELETE /session with the CSRF token, then the entrance', async () => {
    let signedIn = true
    const sent = stubHub((url, init) => {
      if (path(url) === '/session' && init.method === 'DELETE') {
        signedIn = false
        return json(204, null)
      }
      if (path(url) === '/session' && !signedIn) return json(401, { error: 'unauthorized' })
    })
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: /Sign out/ }, LOAD))
    expect(await screen.findByText('Workspace sign-in is not configured on this hub.', {}, LOAD)).toBeTruthy()
    expect(writes(sent)).toEqual([{ method: 'DELETE', url: '/v1/hub/workspace/session', headers: { 'x-csrf-token': CSRF } }])
    expectAllowedWrites(sent)
  })

  it('a refused sign-out is reported and the session stays', async () => {
    stubHub((url, init) => (path(url) === '/session' && init.method === 'DELETE' ? json(403, { error: 'csrf_required' }) : undefined))
    render(<WorkspacePage />)
    fireEvent.click(await screen.findByRole('button', { name: /Sign out/ }, LOAD))
    expect(await screen.findByText(/security token is no longer accepted/, {}, LOAD)).toBeTruthy()
    expect(screen.getByText(/signed in to/)).toBeTruthy()
  })
})

describe('errors', () => {
  it.each([
    [403, /membership does not allow this action/],
    [503, /Workspace access is unavailable/],
    [429, /hub is busy/],
  ])('session %i → a readable notice with retry, no data', async (status, text) => {
    stubHub((url) => (path(url) === '/session' ? json(status, { error: 'x' }) : undefined))
    render(<WorkspacePage />)
    expect(await screen.findByText(text, {}, LOAD)).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Retry' })).toBeTruthy()
    expect(screen.queryByRole('navigation', { name: 'Workspace sections' })).toBeNull()
  })

  it('401 shows the sign-in entrance, not an error', async () => {
    stubHub((url) => (path(url) === '/session' ? json(401, { error: 'unauthorized' }) : undefined))
    render(<WorkspacePage />)
    expect(await screen.findByText('Sign in to your workspace', {}, LOAD)).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
  })

  it('section errors stay in their section', async () => {
    stubHub((url) => {
      if (path(url) === '/repositories') return json(403, { error: 'forbidden' })
      if (path(url).startsWith('/members')) return json(503, { error: 'unavailable' })
    })
    render(<WorkspacePage />)
    expect(await screen.findByText(/membership does not allow this action/, {}, LOAD)).toBeTruthy()
    at('#workspace/members')
    expect(await screen.findByText(/Workspace access is unavailable/, {}, LOAD)).toBeTruthy()
    expect(within(screen.getByRole('heading', { name: 'You' }).closest('section')!).getByText('administrator')).toBeTruthy()
  })

  it('a failing runs request is shown inside Recent runs', async () => {
    stubHub((url, init) => (path(url) === `/repositories/${REPO}/runs` && !isWrite(init) ? json(503, { error: 'unavailable' }) : undefined))
    render(<WorkspacePage />)
    const runs = (await screen.findByRole('heading', { name: 'Recent runs' }, LOAD)).closest('section')!
    expect(await within(runs).findByText(/Workspace access is unavailable/, {}, LOAD)).toBeTruthy()
  })
})
