import { RollCall, openingSteps, type OpeningTrouble } from '../RollCall.tsx'
import { SheetedFrame } from './SignIn.tsx'

export type { OpeningTrouble }

/** What the headline calls the person: the first word of their profile name, else their login. */
export const greetingName = (login: string, name?: string): string => name?.trim().split(/\s+/)[0] || login

/** The sign-in layout while a GitHub web sign-in is opened: a roll call of the real steps on the sheet. */
export function Opening({ login, name, repos, trouble }: { login?: string; name?: string; repos?: number; trouble?: OpeningTrouble | null }) {
  // The step now under way; earlier ones are done.
  const at = login === undefined ? 0 : repos === undefined ? 1 : 2
  return (
    <SheetedFrame className="signin-opening" headline={login ? `Welcome back, ${greetingName(login, name)}` : undefined}>
      <RollCall steps={openingSteps(login, repos)} at={at} trouble={trouble} />
    </SheetedFrame>
  )
}
