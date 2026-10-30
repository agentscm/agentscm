export interface OpeningTrouble {
  title: string
  message: string
  onRetry?: () => void
  retryLabel?: string
  onSignOut?: () => void
  signOutLabel?: string
}

export interface RollStep {
  done: string
  now: string
}

/** The three steps of opening the Hub after a web sign-in; the counts come from the Hub, never a guess. */
export const openingSteps = (login?: string, repos?: number): RollStep[] => [
  { done: `Signed in as ${login}`, now: 'Checking your sign-in' },
  { done: `Found ${repos} ${repos === 1 ? 'repository' : 'repositories'}`, now: 'Finding your repositories' },
  { done: 'Opened your work', now: 'Opening your work' },
]

/** The sheet's content while the Hub opens: a roll call of the real steps. Steps before `at` are done. */
export function RollCall({ steps, at, trouble }: { steps: RollStep[]; at: number; trouble?: OpeningTrouble | null }) {
  return (
    <div className="signin-sheet-inner">
      <h2 className="signin-sheet-title">Opening the Hub</h2>
      <div role="status" aria-label="Opening the Hub">
        <ol className="roll">
          {steps.map((s, i) => {
            const st = i < at ? 'done' : i === at ? (trouble ? 'bad' : 'now') : 'next'
            return (
              <li key={s.now} data-state={st} aria-current={i === at ? 'step' : undefined}>
                <span className="roll-mark" aria-hidden="true">
                  {st === 'done' && <svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>}
                </span>
                {st === 'done' && <span className="visually-hidden">Done: </span>}
                {st === 'done' ? s.done : s.now}
              </li>
            )
          })}
        </ol>
        {trouble && (
          <div className="roll-trouble">
            <p><strong>{trouble.title}</strong> {trouble.message}</p>
            <div className="roll-actions">
              {trouble.onRetry && <button type="button" className="btn btn-primary btn-sm" onClick={trouble.onRetry}>{trouble.retryLabel}</button>}
              {trouble.onSignOut && <button type="button" className="btn btn-link" onClick={trouble.onSignOut}>{trouble.signOutLabel}</button>}
            </div>
          </div>
        )}
      </div>
    </div>
  )
}
