// An agent's brand mark when the name is a known one ("claude-code", "codex", "cursor", ...), else a plain cursor. Decorative:
// the text beside it always names the agent.
import { BrandIcon, brandOf } from './agentIcons.tsx'
import { Icon } from './ui.tsx'

export function AgentMark({ agent }: { agent: string }) {
  const brand = brandOf(agent)
  return <span className="in-mark" aria-hidden="true">{brand ? <BrandIcon name={brand} /> : <Icon name="cursor-fill" />}</span>
}
