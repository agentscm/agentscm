import { afterAll, beforeAll, describe, expect, it } from 'vitest'
import { fetchActivity, fetchHealth, fetchSkillWorkflows, fetchWhoami, fetchWork } from '../../src/api.ts'
import { fetchBriefing, fetchCoordination, fetchOverview, fetchStandupWork } from '../../src/standup.ts'
import { READ_TOKEN, freshHandoff, hubReachable, marker } from '../hub.ts'
import { requests } from './live.ts'

const live = await hubReachable()
if (!live) console.warn('Skipping live-hub API tests: start the dev hub with ui/dev-seed/start.sh (127.0.0.1:7332).')
const signal = () => new AbortController().signal

describe.skipIf(!live)('data layer against the live hub', () => {
  let handoff: Awaited<ReturnType<typeof freshHandoff>>
  beforeAll(async () => {
    handoff = await freshHandoff(marker())
  })
  afterAll(() => handoff?.release())

  it('whoami: read-only token scoped to one repository', async () => {
    const w = await fetchWhoami(READ_TOKEN, signal())
    expect(w.token.read_only).toBe(true)
    expect(w.token.repository).toMatch(/^[\w.-]+\/[\w.-]+$/)
    expect(w.owner.login).not.toBe('unknown')
  })

  it('health: ok with coordination counters', async () => {
    const h = await fetchHealth(signal())
    expect(h.status).toBe('ok')
    expect(h.storage).toBe('sqlite')
    expect(h.coordination!.denied).toBeGreaterThanOrEqual(1)
    expect(h.coordination!.hook_p95_ms).toBeGreaterThanOrEqual(0)
  })

  it('work and activity: live intents, sessions and handoff events', async () => {
    const repo = (await fetchWhoami(READ_TOKEN, signal())).token.repository
    const work = await fetchWork(repo, READ_TOKEN, signal())
    expect(work.intents.length).toBeGreaterThan(0)
    for (const i of work.intents) {
      expect(i.agent.name).toBeTruthy()
      expect(Array.isArray(i.paths)).toBe(true)
    }
    // The seeder records a synthetic GitHub connection (for #62's CI/deploy view); nothing reaches GitHub.
    expect(work.connected).toBe(true)
    const activity = await fetchActivity(repo, READ_TOKEN, signal())
    expect(activity.sessions.length).toBeGreaterThan(0)
    const kinds = new Set(activity.events.map((e) => e.kind))
    expect(kinds.has('handoff_request')).toBe(true)
    const request = activity.events.find((e) => e.kind === 'handoff_request' && (e.data['paths'] as string[]).includes(handoff.file))
    expect(request?.data).toMatchObject({ paths: [handoff.file], to: { agent: 'gemini-pax' } })
    expect(activity.events.some((e) => e.kind === 'handoff_grant' && (e.data['paths'] as string[]).includes(handoff.file))).toBe(true)
    expect(kinds.has('edit_denied')).toBe(true)
  })

  it('stand-up overview: all four reported states, recent outcomes, complete slices', async () => {
    const o = await fetchOverview(READ_TOKEN, signal())
    const states = new Set(o.current.map((w) => w.state))
    for (const s of ['active', 'blocked', 'handover', 'paused'] as const) expect(states.has(s)).toBe(true)
    expect(o.recent.length).toBeGreaterThan(0)
    expect(o.current_complete && o.recent_complete && o.overlaps_complete).toBe(true)
    expect(o.until).toBeGreaterThan(o.recent_since)
  })

  it('coordination requests: pending, responded and resolved stages', async () => {
    const c = await fetchCoordination(READ_TOKEN, signal(), { activeOnly: false })
    const statuses = new Set(c.requests.map((r) => r.status))
    expect(statuses.has('pending')).toBe(true)
    expect(statuses.has('responded')).toBe(true)
    expect(statuses.has('resolved')).toBe(true)
    const active = await fetchCoordination(READ_TOKEN, signal(), { activeOnly: true })
    expect(active.requests.every((r) => r.status === 'pending' || r.status === 'responded')).toBe(true)
  })

  it('briefing (change history) and work detail', async () => {
    const b = await fetchBriefing(READ_TOKEN, signal(), { limit: 10 })
    expect(b.events.length).toBeGreaterThan(0)
    expect(b.events.length).toBeLessThanOrEqual(10)
    const id = b.events[0]!.work_id
    const item = await fetchStandupWork(id, READ_TOKEN, signal())
    expect(item.id).toBe(id)
    expect(item.participants.length).toBeGreaterThan(0)
  })

  it('skill workflows report unavailable when native workflows are not configured', async () => {
    const env = await fetchSkillWorkflows(READ_TOKEN, signal())
    expect(env).toMatchObject({ availability: 'unavailable', reason: expect.any(String) })
  })

  it('the data layer only ever issues GETs', () => {
    expect(requests.length).toBeGreaterThan(0)
    expect(requests.filter((r) => r.method !== 'GET')).toEqual([])
  })
})
