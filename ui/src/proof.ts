// The proof ladder: every claim of progress says who backs it, in one of five rungs.
// Only 'verified' is green. 'collector' is a collector on the author's own computer: the hub did not see it and the record is unsigned.
// No DOM, no React.
//
// API for other packages:
//   ProofLevel                    'reported' | 'collector' | 'observed' | 'verified' | 'unknown'
//   PROOF[level]                  { label, tone, icon, tip } for the badge
//   Proof                         { level, tip? }: a rung plus an optional tooltip override
//   proofFromSkill(assurance)     skills.ts Gate.assurance / Evidence.assurance
//   proofFromChangeRecord(a)      changeRecord.ts ChangeRecordAssurance
//   proofFromExternal(x)          publications.ts External (CI and deploys after merge)
//   PROOF_RANK                    orders the rungs, weakest first
//   runProof(gates)               one skill run's badge: its weakest gate that recorded an assurance
//   Render with <ProofBadge level=… tip=… /> from Proof.tsx.
import type { ChangeRecordAssurance } from './changeRecord.ts'
import type { External } from './publications.ts'
import type { Tone } from './ui.tsx'

export type ProofLevel = 'reported' | 'collector' | 'observed' | 'verified' | 'unknown'

/** A rung, with a tooltip that replaces the rung's default one when set. */
export interface Proof {
  level: ProofLevel
  tip?: string
}

/** Badge label, tone, icon and default tooltip for each rung. */
export const PROOF: Record<ProofLevel, { label: string; tone: Tone; icon: string; tip: string }> = {
  reported: { label: 'Agent says', tone: 'warn', icon: 'person-check', tip: 'Written by the agent. The hub did not check it.' },
  collector: { label: 'Collector saw it', tone: 'brand', icon: 'laptop', tip: 'Observed by AgentSCM’s collector on the author’s own computer. The hub did not see it, and the record is unsigned, so it is not independent.' },
  observed: { label: 'Hub saw it', tone: 'brand', icon: 'eye', tip: 'Recorded by the hub itself (its own claim record, an overlap check, or a read of GitHub or GitLab), not just reported by the agent.' },
  verified: { label: 'Verified', tone: 'ok', icon: 'patch-check', tip: 'Backed by a signed receipt or an independent check, not by the agent or the author’s computer.' },
  unknown: { label: 'Not known', tone: 'muted', icon: 'question-circle', tip: 'Nothing is recorded. This is not a pass.' },
}

/**
 * The tone a claim may wear: green only when the hub or a receipt backs it. An "ok" the agent merely reported, or nobody
 * recorded, is drawn amber; a collector's observation on the author's own computer is drawn blue, not green.
 */
export function backedTone(tone: Tone, proof: ProofLevel | null): Tone {
  if (tone !== 'ok') return tone
  return proof === 'reported' || proof === 'unknown' ? 'warn' : proof === 'collector' ? 'brand' : tone
}

/** Weakest to strongest; 'unknown' is below every recorded rung. */
export const PROOF_RANK: Record<ProofLevel, number> = { unknown: 0, reported: 1, collector: 2, observed: 3, verified: 4 }

/**
 * One skill run's proof: the weakest rung among gates that recorded an assurance, or 'unknown' when none has.
 * The weakest, not the strongest: a run is only as strong as its weakest gate, so one self-reported gate makes it "Agent says".
 */
export function runProof(gates: readonly { assurance: string | null }[]): ProofLevel {
  const levels = gates.filter((g) => g.assurance !== null).map((g) => proofFromSkill(g.assurance))
  return levels.length ? levels.reduce((a, b) => (PROOF_RANK[b] < PROOF_RANK[a] ? b : a)) : 'unknown'
}

/** A skill gate's or evidence's assurance: self_reported, observed or receipt; anything else is unknown. */
export function proofFromSkill(assurance: string | null): ProofLevel {
  if (assurance === 'self_reported') return 'reported'
  if (assurance === 'observed') return 'observed'
  if (assurance === 'receipt') return 'verified'
  return 'unknown'
}

/** A change record's assurance. Use `proofOfChangeRecord` when the tooltip override matters. */
export function proofFromChangeRecord(a: ChangeRecordAssurance): ProofLevel {
  return proofOfChangeRecord(a).level
}

/**
 * A change record's assurance with its tooltip, which names who observed. A collector's observation, including a
 * protected-execution run, stays below "Hub saw it": the collector is operator-run, the record is unsigned and the
 * hub does not authenticate it (docs/advanced/execution.md). Only an independent verification is "Verified".
 */
export function proofOfChangeRecord(a: ChangeRecordAssurance): Proof {
  if (a === 'caller_asserted') return { level: 'reported' }
  if (a === 'observed_trusted_local') return { level: 'collector', tip: 'Observed by AgentSCM’s collector on the agent’s computer. The hub did not see it, and the record is unsigned.' }
  if (a === 'protected_execution') return { level: 'collector', tip: 'Run by the collector in the configured protected execution boundary. The hub does not authenticate the collector, the record is unsigned and the boundary is not a sandbox for hostile code. Not independent.' }
  if (a === 'independent_verification') return { level: 'verified', tip: 'The change record names an independent verification of this action. Open the record to see who verified it.' }
  return { level: 'unknown' }
}

/** How much of CI and deploys after merge the hub read. Use `proofOfExternal` for the tooltip. */
export function proofFromExternal(x: External | null): ProofLevel {
  return proofOfExternal(x).level
}

/** The external read with its tooltip: a partial read says so. */
export function proofOfExternal(x: External | null): Proof {
  if (x?.state === 'observed') return { level: 'observed' }
  if (x?.state === 'incomplete') return { level: 'observed', tip: 'The hub read only part of this.' }
  return { level: 'unknown' }
}
