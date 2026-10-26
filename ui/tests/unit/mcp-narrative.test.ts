import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const html = readFileSync(resolve(process.cwd(), '../src/hub/mcp_views/work-overview.html'), 'utf8')
const script = new DOMParser().parseFromString(html, 'text/html').querySelector('script')!.textContent!.replace('findBuild();\nstart();', '')
function setup() {
  document.body.innerHTML = '<div id="app"></div><div id="find"></div>'
  const load = new Function('window', 'document', `${script};return {NARRATIVE,narrativeValue,renderNarrative};`) as (window: Window, document: Document) => {
    NARRATIVE: { value: unknown; error: boolean }
    narrativeValue: (value: unknown) => unknown
    renderNarrative: (root: HTMLElement) => void
  }
  return load(window, document)
}
const value = {
  id: `nar_${'a'.repeat(32)}`, digest: 'b'.repeat(64), status: 'ready', generated_at: 150, generated_by: 'synthetic agent',
  window: { subscriber: 'human', event_from: 0, event_through: 3, merge_from: 0, merge_through: 1, complete: false },
  sources: [{ id: 'merge:observation', label: 'Merge observation 1', work_id: null, commit: 'c'.repeat(40), pull_number: 17 }],
  warnings: ['Forge coverage unavailable.'],
  output: { headline: 'Retry behavior changed.', headline_references: ['merge:observation'], facts: [{ text: '<img src=x onerror=alert(1)> was the reported title.', references: ['merge:observation'] }], implications: [{ text: 'If you change retries, inspect this commit.', references: ['merge:observation'] }] },
}
describe('MCP agent narrative', () => {
  it('renders cited facts and implications as text and keeps coverage visible', () => {
    const api = setup()
    api.NARRATIVE.value = api.narrativeValue(value)
    const root = document.getElementById('app')!
    api.renderNarrative(root)
    expect(root.textContent).toContain('Retry behavior changed.')
    expect(root.textContent).toContain('Reported facts')
    expect(root.textContent).toContain('What this may mean for your work')
    expect(root.textContent).toContain('Forge coverage unavailable.')
    expect(root.textContent).toContain('bounded portion')
    expect(root.textContent).toContain('PR #17')
    expect(root.textContent).toContain('not independent verification or edit authority')
    expect(root.querySelectorAll('details.source')).toHaveLength(3)
    expect(root.querySelector('img')).toBeNull()
    expect(root.querySelector('button')).toBeNull()
  })
  it('renders the reported failure reason without inventing attempt exhaustion', () => {
    const api = setup()
    api.NARRATIVE.value = api.narrativeValue({ ...value, status: 'failed', generated_at: null, generated_by: null, output: null, warnings: ['Source facts changed.'] })
    const root = document.getElementById('app')!
    api.renderNarrative(root)
    expect(root.textContent).toContain('Source facts changed.')
    expect(root.textContent).toContain('keeps checking for recovery')
    expect(root.textContent).not.toContain('attempt limit was reached')
  })
  it('rejects invented references, malformed window and executable source links', () => {
    const { narrativeValue } = setup()
    expect(narrativeValue({ ...value, output: { ...value.output, facts: [{ text: 'A claim', references: ['unknown'] }] } })).toBeNull()
    expect(narrativeValue({ ...value, output: { ...value.output, headline_references: ['unknown'] } })).toBeNull()
    expect(narrativeValue({ ...value, output: { ...value.output, headline_references: [] } })).toBeNull()
    expect(narrativeValue({ ...value, window: { ...value.window, event_from: 4 } })).toBeNull()
    expect(narrativeValue({ ...value, sources: [{ ...value.sources[0], work_id: 'javascript:alert(1)' }] })).toBeNull()
    expect(narrativeValue({ ...value, output: { ...value.output, headline: 'hidden\u202e claim' } })).toBeNull()
  })
  it('shows an honest waiting state and permits only source-free empty output', () => {
    const api = setup()
    api.NARRATIVE.value = api.narrativeValue({ ...value, status: 'pending', generated_at: null, generated_by: null, output: null })
    const root = document.getElementById('app')!
    api.renderNarrative(root)
    expect(root.textContent).toContain('Waiting for a connected agent')
    expect(api.narrativeValue({ ...value, output: { ...value.output, facts: [] } })).toBeNull()
    expect(api.narrativeValue({ ...value, sources: [], output: { headline: 'No retained facts in this bounded snapshot.', headline_references: [], facts: [], implications: [] } })).not.toBeNull()
  })
})
