// What state a directed request is in, read from the Hub's records. Pure; the Inbox and next-step prompts both use it.
//
// API for other packages:
//   RESPONSE_LABEL   a reply kind in words ("Proposed a scope split")
//   currentAck(r, t), currentResponse(r, t)   a target's current receipt and reply
//   stage(r)         a request's stage: label, tone and icon
//   pending(r)       true while a request is neither resolved nor withdrawn
import type { CoordinationRequest, CoordinationResponseKind, CoordinationTarget } from './standup.ts'
import type { Tone } from './ui.tsx'

/** A reply kind in words. */
export const RESPONSE_LABEL: Record<CoordinationResponseKind, string> = { propose_split: 'Proposed a scope split', join: 'Offered to join', handover: 'Offered a handover', decline: 'Declined' }

/** The target's receipt for this request at its current generation, newest first. */
export function currentAck(r: CoordinationRequest, t: CoordinationTarget) {
  return r.acknowledgements.filter((a) => a.target_work_id === t.work_id && a.generation === t.generation && a.through_seq >= r.created_seq).sort((x, y) => y.seq - x.seq)[0]
}
/** The target's reply at its current generation, newest first. */
export function currentResponse(r: CoordinationRequest, t: CoordinationTarget) {
  return r.responses.filter((x) => x.target_work_id === t.work_id && x.target_generation === t.generation).sort((x, y) => y.seq - x.seq)[0]
}
/** Where a request is: resolved, withdrawn, replied, awaiting replies or receipt, or history incomplete. */
export function stage(r: CoordinationRequest): { label: string; tone: Tone; icon: string } {
  if (r.status === 'resolved') return { label: 'Resolved', tone: 'ok', icon: 'check2-all' }
  if (r.status === 'withdrawn') return { label: 'Withdrawn', tone: 'muted', icon: 'x-circle' }
  if (r.status === 'responded') return { label: 'Replied', tone: 'brand', icon: 'reply' }
  if (!r.acknowledgements_complete || !r.responses_complete) return { label: 'Pending · history incomplete', tone: 'warn', icon: 'question-circle' }
  if (r.targets.every((t) => currentAck(r, t) || currentResponse(r, t))) return { label: 'Awaiting replies', tone: 'warn', icon: 'hourglass-split' }
  return { label: 'Awaiting receipt', tone: 'warn', icon: 'envelope' }
}
/** True while a request is neither resolved nor withdrawn. */
export const pending = (r: CoordinationRequest) => r.status !== 'resolved' && r.status !== 'withdrawn'
