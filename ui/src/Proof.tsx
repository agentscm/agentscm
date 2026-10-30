// API for other packages:
//   <ProofBadge level="reported" tip?="…" />   the proof-ladder badge (Agent says / Collector saw it / Hub saw it / Verified / Not known)
import { PROOF, type ProofLevel } from './proof.ts'
import { Pill } from './ui.tsx'

/** One proof rung as a pill; `tip` replaces the rung's default tooltip. */
export function ProofBadge({ level, tip }: { level: ProofLevel; tip?: string }) {
  const p = PROOF[level]
  return (
    <Pill tone={p.tone} icon={p.icon} tip={tip ?? p.tip}>
      {p.label}
    </Pill>
  )
}
