import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { decodeSkillWorkflowEnvelope, validSkillWorkflowRunId, validSkillWorkflowStepId } from '../../src/skillWorkflow.ts'
import { RunView } from '../../src/pages/Workflows.tsx'
import type { SkillWorkflowAction, SkillWorkflowProjection } from '../../src/types.ts'

const revision = 'a'.repeat(64)
const action = (): SkillWorkflowAction => ({
  profile_digest: 'b'.repeat(64), outcome: 'succeeded', observed_model: '<script>untrusted</script>',
  verification: 'not_independently_verified',
  artifact: { name: 'release-notes', media_type: 'text/markdown', sha256: 'c'.repeat(64), bytes: 230, run_id: 'release-run', step_id: 'draft', revision },
})
const projection = (): SkillWorkflowProjection => ({
  version: 1,
  run: {
    version: 1, run_id: 'release-run', plan_digest: `sha256:${'d'.repeat(64)}`, skill_digest: `sha256:${'e'.repeat(64)}`,
    source: { repository: 'f'.repeat(32), revision, policy_epoch: 1 }, state: 'succeeded', admission: 'not_assessed',
    steps: [{ id: 'draft', needs: [], required: true, state: 'succeeded', observations: [], detail: null, execution: null, review: null, action: action() }],
  },
  present_applicability: { basis: 'current_authority_repository_policy_epoch_and_immutable_revision', state: 'current', reason: null },
  present_landing_binding: null,
})
const envelope = (p = projection()) => ({ version: 1, availability: 'available', projection: p })

describe('productive workflow results', () => {
  it('accepts supported ID boundaries and rejects prompt-breaking IDs', () => {
    expect(validSkillWorkflowRunId('a')).toBe(true)
    expect(validSkillWorkflowRunId('a'.repeat(64))).toBe(true)
    expect(validSkillWorkflowRunId('a'.repeat(65))).toBe(false)
    expect(validSkillWorkflowRunId('published\nstep')).toBe(false)
    expect(validSkillWorkflowStepId('step:one/sub')).toBe(true)
    expect(validSkillWorkflowStepId('a'.repeat(65))).toBe(false)
    expect(validSkillWorkflowStepId('step\u2028injected')).toBe(false)
  })

  it('shows typed output without promoting generation into independent verification', () => {
    const decoded = decodeSkillWorkflowEnvelope(envelope())
    if (decoded.availability !== 'available') throw new Error('fixture unavailable')
    render(<RunView projection={decoded.projection} stepId="draft" stepHref={(id) => `#${id}`} />)
    expect(screen.getByText('Declared outputs were produced')).toBeTruthy()
    expect(screen.getByText('release-notes')).toBeTruthy()
    expect(screen.getByText(/Content has not been independently verified, approved, or published/)).toBeTruthy()
    expect(screen.queryByText('Recorded source checks passed')).toBeNull()
    expect(document.querySelector('script')).toBeNull()
  })

  it('keeps an interrupted action visibly uncertain even when the top-level run claims success', () => {
    const p = projection()
    p.run.steps[0]!.state = 'blocked'
    p.run.steps[0]!.action = { ...action(), outcome: 'uncertain', artifact: null }
    render(<RunView projection={p} stepId="draft" stepHref={(id) => `#${id}`} />)
    expect(screen.getByText('Inspect the interrupted action')).toBeTruthy()
    expect(screen.queryByText('Declared outputs were produced')).toBeNull()
  })

  it('does not present historical outputs as current source evidence', () => {
    const p = projection()
    p.present_applicability.state = 'stale'
    render(<RunView projection={p} stepId="draft" stepHref={(id) => `#${id}`} />)
    expect(screen.getByText('These outputs belong to an earlier source binding')).toBeTruthy()
    expect(screen.queryByText('Declared outputs were produced')).toBeNull()
  })

  it.each(['run_id', 'step_id', 'revision'] as const)('rejects artifact %s belonging to a different subject', (field) => {
    const p = projection()
    p.run.steps[0]!.action!.artifact![field] = field === 'revision' ? '0'.repeat(64) : 'other-subject'
    expect(() => decodeSkillWorkflowEnvelope(envelope(p))).toThrow(/binding/)
  })

  it.each([
    { outcome: 'succeeded', artifact: null },
    { outcome: 'uncertain' },
    { verification: 'independently_verified' },
    { outcome: 'new_unknown_state' },
    { artifact: { ...action().artifact!, bytes: 256 * 1024 + 1 } },
    { artifact: { ...action().artifact!, media_type: 'text/html' } },
  ])('rejects malformed or overclaimed action results %j', (override) => {
    const p = projection()
    const bad = envelope(p)
    Object.assign(bad.projection.run.steps[0]!.action!, override)
    expect(() => decodeSkillWorkflowEnvelope(bad)).toThrow()
  })

  it('continues decoding historical runs without an action field', () => {
    const p = projection()
    delete p.run.steps[0]!.action
    expect(decodeSkillWorkflowEnvelope(envelope(p)).availability).toBe('available')
  })
})
