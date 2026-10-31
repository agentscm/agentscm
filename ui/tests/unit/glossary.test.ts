import { describe, expect, it } from 'vitest'
import { GLOSS, glossText, type Term } from '../../src/glossary.ts'

describe('glossary', () => {
  it('gives every term a non-empty, one-sentence-length gloss', () => {
    for (const [term, g] of Object.entries(GLOSS)) {
      expect(g.gloss.trim(), term).not.toBe('')
      expect(g.gloss.length, term).toBeLessThan(160)
    }
  })

  it('adds the docs pointer only where one is set', () => {
    expect(glossText('native')).toBe('Recorded in AgentSCM’s own history, separate from GitHub or GitLab. More: docs.agentscm.dev/concepts')
    expect(glossText('hold' satisfies Term)).toBe(GLOSS.hold.gloss)
  })
})
