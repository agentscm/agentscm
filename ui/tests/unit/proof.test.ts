import { describe, expect, it } from 'vitest'
import type { ChangeRecordAssurance } from '../../src/changeRecord.ts'
import { PROOF, PROOF_RANK, backedTone, runProof, proofFromChangeRecord, proofFromExternal, proofFromSkill, proofOfChangeRecord, proofOfExternal } from '../../src/proof.ts'
import type { External } from '../../src/publications.ts'

const ext = (state: External['state']) => ({ state, note: '', mergeCommit: '', reconciledAt: null, ci: [], deployments: [] }) as External

describe('proof ladder', () => {
  it('labels the five rungs exactly', () => {
    expect(Object.fromEntries(Object.entries(PROOF).map(([k, v]) => [k, [v.label, v.tone, v.icon]]))).toEqual({
      reported: ['Agent says', 'warn', 'person-check'],
      collector: ['Collector saw it', 'brand', 'laptop'],
      observed: ['Hub saw it', 'brand', 'eye'],
      verified: ['Verified', 'ok', 'patch-check'],
      unknown: ['Not known', 'muted', 'question-circle'],
    })
    expect(PROOF.unknown.tip).toBe('Nothing is recorded. This is not a pass.')
  })

  it('maps every skill assurance, and anything unrecognised to unknown', () => {
    expect(['self_reported', 'observed', 'receipt', null, 'signed_by_mars', ''].map(proofFromSkill)).toEqual(['reported', 'observed', 'verified', 'unknown', 'unknown', 'unknown'])
  })

  it('maps every change-record assurance, with the local-observation tip', () => {
    const all: ChangeRecordAssurance[] = ['caller_asserted', 'observed_trusted_local', 'protected_execution', 'independent_verification']
    // Only an independent verification is Verified; a collector on the author’s own computer is never Hub saw it or Verified.
    expect(all.map(proofFromChangeRecord)).toEqual(['reported', 'collector', 'collector', 'verified'])
    expect(proofOfChangeRecord('observed_trusted_local').tip).toMatch(/collector on the agent’s computer.*hub did not see it/)
    expect(proofOfChangeRecord('protected_execution').tip).toMatch(/Not independent/)
    expect(proofFromChangeRecord('made_up' as ChangeRecordAssurance)).toBe('unknown')
  })

  it('maps every external read state, and a missing read to unknown', () => {
    expect([ext('observed'), ext('incomplete'), ext('unobserved'), null].map(proofFromExternal)).toEqual(['observed', 'observed', 'unknown', 'unknown'])
    expect(proofOfExternal(ext('incomplete')).tip).toBe('The hub read only part of this.')
  })

  it('rates a run by its weakest gate that recorded an assurance', () => {
    expect(runProof([{ assurance: 'receipt' }, { assurance: 'self_reported' }, { assurance: null }])).toBe('reported')
    expect(runProof([{ assurance: 'receipt' }, { assurance: 'observed' }])).toBe('observed')
    expect(PROOF_RANK.reported < PROOF_RANK.collector && PROOF_RANK.collector < PROOF_RANK.observed && PROOF_RANK.observed < PROOF_RANK.verified).toBe(true)
    expect(backedTone('ok', 'collector')).toBe('brand')
    expect(backedTone('ok', 'reported')).toBe('warn')
    expect(backedTone('ok', 'verified')).toBe('ok')
    expect(runProof([{ assurance: null }])).toBe('unknown')
  })
})
