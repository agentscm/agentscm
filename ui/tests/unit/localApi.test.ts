// #64/#67 adapter: decoders against the local server's real replies, the live-or-preview rule, and routing.
import { afterEach, describe, expect, it, vi } from 'vitest'
import { FIXTURE_DIFF, FIXTURE_ENTRY, FIXTURE_HANDOFF, FIXTURE_INSPECT, FIXTURE_TIMELINE, LocalError, decodeDiff, decodeEntryView, decodeHandoff, decodeInspect, decodeTimeline, fetchEntry, fetchTimeline, inspectTemplate, saveLocalBase, saveLocalToken, validLocalBase, type TemplateAction } from '../../src/localApi.ts'
import { describeAction } from '../../src/pages/Templates.tsx'
import { compareHref, timelineRoute } from '../../src/pages/Timeline.tsx'

const signal = new AbortController().signal
const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
  sessionStorage.clear()
})

describe('decoders, against real replies', () => {
  it('reads a template recipe', () => {
    const t = decodeInspect(FIXTURE_INSPECT())
    expect(t.reference).toMatchObject({ name: 'verify', path: 'skills/checks/v2/workflow.json', workflow_version: 2 })
    expect(t.template.steps.map((s) => describeAction(s.action).text)).toEqual(['Check this path is in the change: src/lib.rs'])
    expect(decodeInspect({ template: { steps: [{ id: 'x', action: { kind: 'teleport' } }] } }).template.steps[0]!.action.kind).toBe('unknown')
  })

  it('describes every step kind in plain words', () => {
    const actions: TemplateAction[] = [
      { kind: 'require_capability', capability: 'docker' },
      { kind: 'command', runner: 'cargo-nextest' },
      { kind: 'claude_code_review', paths: ['a.rs', 'b.rs'], model: 'claude-opus-5-5' },
    ]
    expect(actions.map((a) => describeAction(a).text)).toEqual(['Needs the “docker” capability on the machine that runs it', 'Run the “cargo-nextest” command', 'Ask Claude (claude-opus-5-5) to review 2 files'])
  })

  it('reads a handoff, flattening a structured change id', () => {
    const h = decodeHandoff(FIXTURE_HANDOFF())
    expect(h.run.state).toBe('failed')
    expect(h.run.steps[0]).toMatchObject({ id: 'source', state: 'failed', detail: 'content digest does not match for src/lib.rs' })
    expect(h.subject_scope).toBe('source_revision_only')
    expect(decodeHandoff({ command_context_bindings: [{ change: { repository: 'r', id: 'c1' }, change_version: 2 }] }).command_context_bindings[0]!.change).toBe('c1')
    expect(decodeHandoff({ run: { state: 'dancing' } }).run.state).toBe('unknown')
  })

  it('reads timeline pages, entries and diffs', () => {
    const first = decodeTimeline(FIXTURE_TIMELINE())
    expect(first.entries.map((e) => [e.kind, e.id])).toEqual([['check_link', 3], ['snapshot', 2]])
    expect(first.next_before).toBe(2)
    expect(first.coverage.includes_pre_feature_history).toBe(false)
    const older = decodeTimeline(FIXTURE_TIMELINE(2))
    expect(older.entries.map((e) => e.id)).toEqual([1])
    expect(older.next_before).toBeNull()
    expect(decodeTimeline({ timeline: { entries: [{ kind: 'mystery' }] } }).entries).toEqual([])
    expect(decodeEntryView(FIXTURE_ENTRY(3))!).toMatchObject({ evidence: 'available', entry: { kind: 'check_link', change: 'demo-change' } })
    expect(decodeEntryView(FIXTURE_ENTRY(1))!.evidence).toBe('not_run')
    expect(decodeEntryView({ entry: { kind: 'snapshot', id: 1 }, evidence: { state: 'odd' } })!.evidence).toBe('unavailable')
    expect(decodeDiff(FIXTURE_DIFF(1, 2))).toEqual({ left: 1, right: 2, entries: [{ path: 'src/more.rs', change: 'added' }], next: null, missing: 0 })
    const partial = decodeDiff({ left: 1, right: 2, diff: { entries: [{ path: 'a', before: 'x', after: null }, { path: 'b', before: 'x', after: 'y' }], needed_objects: ['o1', 'o2'], next: 'c1' } })
    expect(partial.entries.map((e) => e.change)).toEqual(['removed', 'changed'])
    expect(partial).toMatchObject({ next: 'c1', missing: 2 })
  })
})

describe('live or preview', () => {
  it('uses the live server when it answers, on the agreed routes', async () => {
    const seen: string[] = []
    vi.stubGlobal('fetch', async (url: string) => (seen.push(url), json(200, url.includes('/timeline/') ? FIXTURE_ENTRY(3) : url.includes('library') ? FIXTURE_INSPECT() : FIXTURE_TIMELINE())))
    expect((await fetchTimeline(signal)).source).toBe('live')
    await fetchEntry(3, signal)
    await inspectTemplate('demo-change', 'skills/checks/v1/workflow.json', signal)
    expect(seen).toEqual(['/v1/local/workspace/timeline?limit=25', '/v1/local/workspace/timeline/3', '/v1/local/skill-workflow-library?change=demo-change&library=skills%2Fchecks%2Fv1%2Fworkflow.json'])
  })

  it('falls back to preview data when nothing answers, or something other than the local server does', async () => {
    vi.stubGlobal('fetch', async () => {
      throw new TypeError('connection refused')
    })
    expect((await fetchTimeline(signal)).source).toBe('preview')
    vi.stubGlobal('fetch', async () => new Response('<html>proxy error</html>', { status: 502 }))
    expect((await inspectTemplate('c', 'p', signal)).source).toBe('preview')
    vi.stubGlobal('fetch', async () => json(405, { error: 'method_not_allowed' }))
    expect((await fetchTimeline(signal)).source).toBe('preview')
  })

  it('shows the local server’s own errors as advice, never as sample data', async () => {
    vi.stubGlobal('fetch', async () => json(404, { error: 'timeline is off' }))
    await expect(fetchEntry(99, signal)).rejects.toMatchObject({ status: 404, message: expect.stringContaining('--timeline-workspace') })
    vi.stubGlobal('fetch', async () => json(421, { error: 'host' }))
    await expect(fetchTimeline(signal)).rejects.toMatchObject({ message: 'The local read API refused this page’s address; open the hub UI on localhost.' })
    vi.stubGlobal('fetch', async () => json(401, { error: 'unauthorized' }))
    await expect(fetchTimeline(signal)).rejects.toBeInstanceOf(LocalError)
  })

  it('sends the local token only to a different local address, never same-origin', async () => {
    const calls: [string, Record<string, string>][] = []
    vi.stubGlobal('fetch', async (url: string, init: RequestInit) => (calls.push([url, Object.fromEntries(new Headers(init.headers).entries())]), json(200, FIXTURE_TIMELINE())))
    saveLocalToken('local-secret')
    await fetchTimeline(signal)
    saveLocalBase('http://127.0.0.1:7331')
    await fetchTimeline(signal)
    expect(calls[0]).toEqual(['/v1/local/workspace/timeline?limit=25', {}])
    expect(calls[1]).toEqual(['http://127.0.0.1:7331/v1/local/workspace/timeline?limit=25', { authorization: 'Bearer local-secret' }])
  })

  it('only accepts addresses on this computer', () => {
    expect(validLocalBase('')).toBe(true)
    expect(validLocalBase('http://127.0.0.1:7331')).toBe(true)
    expect(validLocalBase('http://localhost')).toBe(true)
    expect(validLocalBase('https://example.com')).toBe(false)
    expect(validLocalBase('http://127.0.0.1.evil.com')).toBe(false)
  })
})

describe('timeline routes', () => {
  it('parses list, entry and compare, ordering the pair', () => {
    expect(timelineRoute('#timeline')).toEqual({ kind: 'list' })
    expect(timelineRoute('#timeline/entry/3')).toEqual({ kind: 'entry', id: 3 })
    expect(timelineRoute('#timeline/compare/3/1')).toEqual({ kind: 'compare', left: 1, right: 3 })
    expect(timelineRoute('#timeline/compare/4/4')).toEqual({ kind: 'list' })
    expect(timelineRoute('#timeline/entry/-1')).toEqual({ kind: 'list' })
    expect(compareHref(3, 1)).toBe('#timeline/compare/1/3')
  })
})

describe('busy local server', () => {
  it('retries a 503 before giving up, then shows the busy advice', async () => {
    let calls = 0
    vi.stubGlobal('fetch', async () => (++calls < 3 ? json(503, { error: 'busy' }) : json(200, FIXTURE_TIMELINE())))
    expect((await fetchTimeline(signal)).source).toBe('live')
    expect(calls).toBe(3)
    calls = 0
    vi.stubGlobal('fetch', async () => (calls++, json(503, { error: 'busy' })))
    await expect(fetchTimeline(signal)).rejects.toMatchObject({ status: 503, message: expect.stringContaining('busy') })
    expect(calls).toBe(5)
  }, 10_000)
})
