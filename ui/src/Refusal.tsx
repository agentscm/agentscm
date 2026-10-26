import type { Refusal } from './refusal.ts'
import { CopyButton, Notice } from './ui.tsx'

const COMMAND = /^(npx )?agentscm [a-z]/

/** A Hub refusal: the Hub's sentence as the title, then the one fix (copyable when it is a command). */
export function RefusalNotice({ refusal }: { refusal: Refusal }) {
  const { message, fix } = refusal
  return (
    <Notice tone="bad" icon="shield-exclamation" title={message}>
      {fix &&
        (COMMAND.test(fix) ? (
          <div className="command mt-2">
            <code>{fix}</code>
            <CopyButton text={fix} />
          </div>
        ) : (
          fix
        ))}
    </Notice>
  )
}
