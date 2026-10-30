import { RollCall, openingSteps, type OpeningTrouble } from '../RollCall.tsx'
import { SheetedFrame } from './SignIn.tsx'

export type { OpeningTrouble }

/** The sign-in layout while a GitHub web sign-in is opened: a roll call of the real steps on the sheet. */
export function Opening({ login, repos, trouble }: { login?: string; repos?: number; trouble?: OpeningTrouble | null }) {
  // The step now under way; earlier ones are done.
  const at = login === undefined ? 0 : repos === undefined ? 1 : 2
  return (
    <SheetedFrame className="signin-opening" headline={login ? `Welcome back, ${login}` : undefined}>
      <RollCall steps={openingSteps(login, repos)} at={at} trouble={trouble} />
    </SheetedFrame>
  )
}
