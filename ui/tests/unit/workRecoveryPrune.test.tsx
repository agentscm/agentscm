import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { BrowserSession } from '../../src/devices.ts'
import type { WorkItem } from '../../src/standup.ts'
import { clearPollCache } from '../../src/usePoll.ts'
import { decodePreview, fetchInventory } from '../../src/workRecovery.ts'
import { WorkRecovery } from '../../src/WorkRecovery.tsx'

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
  clearPollCache()
})

const id = `wrk_${'b'.repeat(32)}`
const item = { id, repository: 'example/repo', generation: 3, updated_seq: 9 } as WorkItem
const session: BrowserSession = { login: 'admin', provider: 'github', admin: true, csrf: 'csrf-synthetic' }
const participants = [
  { token: 'tok_a', session: 'digest-a' },
  { token: 'tok_b', session: 'digest-b' },
]
const recovery = { schema_version: 1, work: [{ work_id: id, bucket: 'needs_you', standing: 'admin', why: 'Full list.', follow_up: { unfinished: true }, requests: [], dependents: [], remedies: [{ remedy: 'prune_participants', permitted: true }] }] }
const preview = (apply: unknown = { arguments: { participants } }) => ({
  work_id: id,
  remedy: 'prune_participants',
  permitted: true,
  refusal: null,
  generation: 3,
  expected_updated_seq: 9,
  effects: { summary: 'Removes the listed non-lead participants.', freed_paths: [], kept_paths: [], requests: [], dependents: [], follow_up: { unfinished: true }, fenced: [{ login: 'someone', agent: 'Codex', role: 'helper' }] },
  apply,
})

function hub(batchItem: Record<string, unknown>) {
  const sent: { url: string; body: Record<string, unknown> }[] = []
  const fetch = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
    const url = String(input)
    sent.push({ url, body: JSON.parse(String(init?.body ?? '{}')) })
    const reply = url.endsWith('/recovery') ? recovery : url.endsWith('/preview') ? preview() : { items: [batchItem] }
    return new Response(JSON.stringify(reply), { status: 200 })
  })
  return { sent, fetch }
}

async function toApply() {
  render(<WorkRecovery item={item} session={session} refreshWork={() => {}} />)
  fireEvent.click(await screen.findByRole('button', { name: 'Remove listed sessions' }))
  expect(await screen.findByText(/someone · Codex · helper/)).toBeTruthy()
  fireEvent.change(screen.getByLabelText('Why, in one line'), { target: { value: 'list is full' } })
  fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
}

describe('prune from the person UI', () => {
  it('sends exactly the preview participants', async () => {
    const { sent } = hub({ outcome: 'applied', code: null, message: null, retry: 'none' })
    await toApply()
    expect(await screen.findByText('Done')).toBeTruthy()
    const batch = sent.find((s) => s.url === '/v1/hub/session/work/transitions')!
    expect(batch.body).toMatchObject({ items: [{ transition: 'prune_participants', expected_updated_seq: 9, participants }] })
  })

  it('shows a work_changed refusal and does not retry', async () => {
    const { sent } = hub({ outcome: 'refused', code: 'work_changed', message: 'The Work changed after it was read; nothing changed.', retry: 'Read the Work again.' })
    await toApply()
    expect(await screen.findByText(/The Work changed after it was read/)).toBeTruthy()
    expect(sent.filter((s) => s.url.endsWith('/transitions'))).toHaveLength(1)
  })

  it('rejects malformed participants instead of dropping them', () => {
    for (const bad of [{ arguments: { participants: [{ token: 'tok_a' }] } }, { arguments: { participants: 'x' } }, { arguments: { participants: [] } }]) {
      expect(() => decodePreview(preview(bad))).toThrow()
    }
  })
})

describe('all-owners inventory', () => {
  it('asks with all_owners and keeps unknown finding kinds and the pull request', async () => {
    const fetch = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({
          work: [
            {
              work_id: id,
              title: 'Synthetic task',
              owner_login: 'owner',
              findings: [
                { kind: 'hidden_hold', evidence: 'Merged.', complete: true, pull_request: 7 },
                { kind: 'future_kind', evidence: 'Newer.', complete: false },
              ],
            },
          ],
          next_after: id,
        }),
        { status: 200 },
      ),
    )
    const page = await fetchInventory(session, 'example/repo', id)
    expect(JSON.parse(String(fetch.mock.calls[0]![1]!.body))).toEqual({ repository: 'example/repo', all_owners: true, after: id })
    expect(page.next_after).toBe(id)
    expect(page.work[0]).toMatchObject({ title: 'Synthetic task', owner_login: 'owner' })
    expect(page.work[0]!.findings).toEqual([
      { kind: 'hidden_hold', evidence: 'Merged.', complete: true, pull_request: 7 },
      { kind: 'future_kind', evidence: 'Newer.', complete: false, pull_request: null },
    ])
  })
})
