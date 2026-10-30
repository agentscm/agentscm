// #62 decoding and the five-step track, against the dev hub's real replies for its 7 seeded states.
import { afterEach, describe, expect, it, vi } from 'vitest'
import { landedIdFromHash } from '../../src/pages/Landed.tsx'
import { clean, dateSpan, decodePublication, headline, httpsOnly, loadRecent, outcomeStep, rememberRecent, track, type Publication } from '../../src/publications.ts'
import seeded from '../fixtures/publications.json'

const all = (seeded as unknown[]).map(decodePublication)
const byBranch = (b: string) => all.find((p) => p.branch.endsWith(b))!
const words = (p: Publication) => track(p).map((s) => s.word)

afterEach(() => {
  vi.restoreAllMocks()
  localStorage.clear()
})

describe('decodePublication', () => {
  it('decodes all seven seeded states', () => {
    expect(all.map((p) => [p.branchState, p.pr.state, p.external?.state ?? null])).toEqual([
      ['pending', 'pending', null],
      ['rejected', 'pending', null],
      ['created', 'pending', null],
      ['created', 'opened', null],
      ['created', 'merged', 'observed'],
      ['created', 'merged', 'incomplete'],
      ['created', 'merged', 'unobserved'],
    ])
    expect(byBranch('rename-config-keys').branchMessage).toBeTruthy()
  })

  it('refuses a reply without a valid id', () => {
    expect(() => decodePublication({ id: 'nope' })).toThrow()
  })

  it('treats provider text as untrusted: control characters stripped, non-https links dropped, unknown values never success', () => {
    const p = decodePublication({
      id: `pub_${'a'.repeat(32)}`,
      states: { branch: { state: 'created' }, pull_request: { state: 'merged', number: 7, html_url: 'javascript:alert(1)', merge_commit: 'f'.repeat(40) } },
      external: { state: 'weird', ci: [{ run_id: 1, name: 'build\u0007\u001b[31m', outcome: 'exploded', html_url: 'http://evil.example' }], deployments: [] },
    })
    expect(p.pr.url).toBeNull()
    expect(p.external!.state).toBe('unobserved')
    expect(p.external!.ci[0]!.name).toBe('build[31m')
    expect(p.external!.ci[0]!.outcome).toBe('unknown')
    expect(p.external!.ci[0]!.url).toBeNull()
    expect(httpsOnly('https://github.com/x')).toBe('https://github.com/x')
    expect(clean('x'.repeat(500))).toHaveLength(200)
  })
})

describe('track', () => {
  it('reads each seeded state as five plain words', () => {
    expect(words(byBranch('retry-backoff-jitter'))).toEqual(['Pending', 'Pending', 'Not yet', 'Not yet', 'Not yet'])
    expect(words(byBranch('rename-config-keys'))).toEqual(['Rejected', 'Not started', 'Not started', 'Not yet', 'Not yet'])
    expect(words(byBranch('status-page-empty-states'))).toEqual(['Created', 'Pending', 'Not yet', 'Not yet', 'Not yet'])
    expect(words(byBranch('activity-pagination')).slice(0, 3)).toEqual(['Created', expect.stringMatching(/^Opened/), 'Not yet'])
    expect(words(byBranch('token-ttl-docs')).slice(3)).toEqual(['Not checked yet', 'Not checked yet'])
  })

  it('gives every outcome combination an honest word', () => {
    const base = byBranch('handoff-expiry-sweep')
    const ext = base.external!
    const run = (outcome: string) => ({ ...ext.ci[0]!, outcome } as never)
    const withCi = (state: 'observed' | 'incomplete', ci: never[]) => ({ ...base, external: { ...ext, state, ci } })
    expect(outcomeStep('ci', withCi('observed', [run('succeeded')])).word).toBe('Passed')
    expect(outcomeStep('ci', withCi('observed', [run('succeeded'), run('failed')])).word).toBe('Failed')
    expect(outcomeStep('ci', withCi('incomplete', [run('failed')])).word).toBe('Failed')
    expect(outcomeStep('ci', withCi('observed', [run('running')])).word).toBe('Running')
    expect(outcomeStep('ci', withCi('incomplete', [run('succeeded')])).word).toBe('Partly checked')
    expect(outcomeStep('ci', withCi('observed', [])).word).toBe('None found')
    expect(outcomeStep('ci', withCi('incomplete', [])).word).toBe('Partly checked')
    expect(outcomeStep('ci', withCi('observed', [run('cancelled')])).word).toBe('Mixed')
    expect(outcomeStep('ci', { ...base, external: null }).word).toBe('Not tracked')
    expect(outcomeStep('ci', { ...base, external: { ...ext, state: 'unobserved' } }).tone).toBe('muted')
  })

  it('sums a change up in one phrase', () => {
    expect(headline(byBranch('rename-config-keys')).text).toBe('Branch rejected')
    // This page says Merged, never Landed: Workspace uses Land for AgentSCM's own native target.
    expect(all.map((p) => headline(p).text).join(' ')).not.toMatch(/Landed/)
    expect(dateSpan([])).toBeNull()
    expect(dateSpan(all.map((p, i) => ({ ...p, createdAt: 1_792_690_000 + i * 86_400 * 3 })))).toMatch(/ to /)
    expect(dateSpan([{ ...all[0]!, createdAt: 1_792_690_000 }, { ...all[1]!, createdAt: 1_792_690_100 }])).not.toMatch(/ to /)
    expect(headline(byBranch('schema-migration-lock')).tone).not.toBe('ok')
    expect(headline(byBranch('token-ttl-docs')).tone).not.toBe('ok')
  })
})

describe('routing and recent ids', () => {
  it('only opens valid ids', () => {
    const id = `pub_${'b'.repeat(32)}`
    expect(landedIdFromHash(`#landed/${id}`)).toBe(id)
    expect(landedIdFromHash('#landed/pub_short')).toBeNull()
    expect(landedIdFromHash('#landed/%E0%A4%A')).toBeNull()
    expect(landedIdFromHash('#landed')).toBeNull()
  })

  it('remembers recent ids and survives storage that throws', () => {
    const a = `pub_${'1'.repeat(32)}`
    const b = `pub_${'2'.repeat(32)}`
    rememberRecent(a)
    expect(rememberRecent(b)).toEqual([b, a])
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    expect(loadRecent()).toEqual([])
    expect(() => rememberRecent(a)).not.toThrow()
  })
})
