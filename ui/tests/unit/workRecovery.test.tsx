import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { decodeStatus } from '../../src/actionableStatus.ts'
import type { BrowserSession } from '../../src/devices.ts'
import type { WorkItem } from '../../src/standup.ts'
import { clearPollCache } from '../../src/usePoll.ts'
import { WorkRecovery } from '../../src/WorkRecovery.tsx'

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
  clearPollCache()
})

const id = `wrk_${'b'.repeat(32)}`
const item = { id, repository: 'example/repo', generation: 3, updated_seq: 9 } as WorkItem
const session: BrowserSession = { login: 'owner', provider: 'github', admin: false, csrf: 'csrf-synthetic' }
const remedy = (name: string, permitted: boolean) => ({ remedy: name, permitted, ...(permitted ? {} : { refusal: { code: 'not_authorized', message: 'No.' } }) })
const recovery = (permitted: boolean) => ({
  schema_version: 1,
  work: [
    {
      work_id: id,
      bucket: 'needs_you',
      standing: permitted ? 'owner' : 'other',
      why: 'Unfinished and paused: Claude Code holds its scope with a lease that lapses at Unix 500 unless renewed.',
      follow_up: { unfinished: true, next_action: 'write the tests', blocker: null },
      requests: [],
      dependents: [`wrk_${'c'.repeat(32)}`],
      remedies: [remedy('abandon', permitted), remedy('release', permitted), remedy('resume', false)],
    },
  ],
})
const preview = {
  work_id: id,
  remedy: 'release',
  permitted: true,
  refusal: null,
  generation: 3,
  expected_updated_seq: 9,
  effects: { summary: 'Frees the scope and keeps the Work unfinished.', freed_paths: ['src/a.rs'], kept_paths: [], requests: [], dependents: [`wrk_${'c'.repeat(32)}`], follow_up: { unfinished: true } },
}

function hub(batch: unknown) {
  const sent: { url: string; body: Record<string, unknown> }[] = []
  vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
    const url = String(input)
    const body = JSON.parse(String(init?.body ?? '{}'))
    sent.push({ url, body })
    const reply = url.endsWith('/recovery') ? recovery(true) : url.endsWith('/preview') ? preview : batch
    return new Response(JSON.stringify(reply), { status: 200 })
  })
  return sent
}

describe('Work recovery panel', () => {
  it('renders nothing without a web sign-in', () => {
    const fetch = vi.spyOn(globalThis, 'fetch')
    const { container } = render(<WorkRecovery item={item} session={null} refreshWork={() => {}} />)
    expect(container.innerHTML).toBe('')
    expect(fetch).not.toHaveBeenCalled()
  })

  it('renders nothing when the person may apply nothing', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify(recovery(false)), { status: 200 }))
    const { container } = render(<WorkRecovery item={item} session={session} refreshWork={() => {}} />)
    await vi.waitFor(() => expect(container.querySelector('.skeleton, [aria-busy]')).toBeNull())
    expect(screen.queryByRole('button')).toBeNull()
  })

  it('previews a release, applies it with the preview fences and reloads the task', async () => {
    const sent = hub({ schema_version: 1, applied: 1, not_applied: 0, items: [{ work_id: id, operation_id: 'x', outcome: 'applied', code: null, message: null, retry: 'Nothing to retry.', result: null }] })
    const refresh = vi.fn()
    render(<WorkRecovery item={item} session={session} refreshWork={refresh} />)
    expect(await screen.findByText(/lapses at Unix 500/)).toBeTruthy()
    expect(screen.queryByRole('button', { name: /resume/i })).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Release its files' }))
    expect(await screen.findByText(/keeps the Work unfinished/)).toBeTruthy()
    expect(screen.getByText(/1 unfinished task depends/)).toBeTruthy()
    const apply = screen.getByRole('button', { name: 'Apply' })
    expect((apply as HTMLButtonElement).disabled).toBe(true)
    fireEvent.change(screen.getByLabelText('Why, in one line'), { target: { value: 'its session is gone' } })
    fireEvent.click(apply)
    expect(await screen.findByText('Done')).toBeTruthy()
    expect(refresh).toHaveBeenCalledOnce()
    const batch = sent.find((s) => s.url === '/v1/hub/session/work/transitions')!
    expect(batch.body).toMatchObject({ repository: 'example/repo', items: [{ work_id: id, generation: 3, expected_updated_seq: 9, transition: 'release', reason: 'its session is gone' }] })
  })

  it('says what happened and how to retry when the fences moved', async () => {
    hub({ schema_version: 1, applied: 0, not_applied: 1, items: [{ work_id: id, operation_id: 'x', outcome: 'refused', code: 'work_changed', message: 'The Work changed after it was read; nothing changed.', retry: 'Read the Work again, then retry.', result: null }] })
    const refresh = vi.fn()
    render(<WorkRecovery item={item} session={session} refreshWork={refresh} />)
    fireEvent.click(await screen.findByRole('button', { name: 'Release its files' }))
    await screen.findByText(/keeps the Work unfinished/)
    fireEvent.change(screen.getByLabelText('Why, in one line'), { target: { value: 'obsolete' } })
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    expect(await screen.findByText('Nothing changed')).toBeTruthy()
    expect(screen.getByText(/The Work changed after it was read/)).toBeTruthy()
    expect(refresh).not.toHaveBeenCalled()
  })
})

describe('shared status reservation', () => {
  it('reads a lapsed hold as lapsed, not held', () => {
    const fixture = JSON.parse(readFileSync(resolve(process.cwd(), '../src/hub/fixtures/actionable-status.json'), 'utf8'))
    fixture.work[0].operational.reservation.status = 'lapsed'
    expect(decodeStatus(fixture).work[0]!.operational.reservation.status).toBe('lapsed')
  })
})
