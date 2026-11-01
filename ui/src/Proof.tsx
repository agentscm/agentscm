// API for other packages:
//   <ProofBadge level="reported" tip?="…" tone?="muted" />   the proof-ladder badge (Agent says / Collector saw it / Hub saw it / Verified / Not known)
import { PROOF, type ProofLevel } from './proof.ts'
import { Pill, type Tone } from './ui.tsx'

/** One proof rung as a pill; `tip` replaces the rung's default tooltip, `tone` its colour (History mutes "Agent says", which every row there carries). */
export function ProofBadge({ level, tip, tone }: { level: ProofLevel; tip?: string; tone?: Tone }) {
  const p = PROOF[level]
  return (
    <Pill tone={tone ?? p.tone} icon={p.icon} tip={tip ?? p.tip}>
      {p.label}
    </Pill>
  )
}
