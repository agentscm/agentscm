// The owner's recovery actions on one task's page: release, finish or keep a hold, after a preview
// of what it would change. The Hub refuses an apply whose fences moved, with nothing changed.
//
// API for other packages:
//   WorkRecovery({ item, session, refreshWork })   nothing without a web sign-in, or when the person may apply nothing
import { useState } from 'react'
import type { BrowserSession } from './devices.ts'
import { operationId, sendError } from './inboxApi.ts'
import type { WorkItem } from './standup.ts'
import { Box, Notice, Paths, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'
import { applyTransition, fetchRecovery, PERSON_REMEDIES, previewRemedy, type BatchItem, type RemedyKind, type RemedyPreview } from './workRecovery.ts'

const LABEL: Partial<Record<RemedyKind, string>> = {
  release: 'Release its files',
  complete: 'Mark it complete',
  abandon: 'Abandon it',
  retain: 'Keep holding its files',
}

type Step = { kind: 'idle' } | { kind: 'previewing'; remedy: RemedyKind } | { kind: 'preview'; preview: RemedyPreview; op: string } | { kind: 'sending'; preview: RemedyPreview; op: string } | { kind: 'done'; preview: RemedyPreview; op: string; result: BatchItem } | { kind: 'error'; message: string }

export function WorkRecovery({ item, session, refreshWork }: { item: WorkItem; session: BrowserSession | null; refreshWork: () => void }) {
  if (!session) return null
  return <Panel item={item} session={session} refreshWork={refreshWork} />
}

function Panel({ item, session, refreshWork }: { item: WorkItem; session: BrowserSession; refreshWork: () => void }) {
  const poll = usePoll((signal) => fetchRecovery(session, item.repository, item.id, signal), 0, `recovery|${item.id}|${item.generation}|${item.updated_seq}`)
  const [step, setStep] = useState<Step>({ kind: 'idle' })
  const [reason, setReason] = useState('')
  if (poll.kind === 'loading') return <Skeleton rows={1} />
  if (poll.kind === 'error') return null
  const work = poll.value
  const open = work ? work.remedies.filter((r) => r.permitted && PERSON_REMEDIES.includes(r.remedy)) : []
  if (!work || open.length === 0) return null

  const preview = async (remedy: RemedyKind) => {
    setStep({ kind: 'previewing', remedy })
    try {
      setStep({ kind: 'preview', preview: await previewRemedy(session, item.repository, item.id, remedy), op: operationId() })
    } catch (e) {
      setStep({ kind: 'error', message: sendError(e) })
    }
  }
  const apply = async (p: RemedyPreview, op: string) => {
    setStep({ kind: 'sending', preview: p, op })
    try {
      const result = await applyTransition(session, item.repository, p, op, reason.trim())
      setStep({ kind: 'done', preview: p, op, result })
      if (result.outcome === 'applied' || result.outcome === 'replayed') refreshWork()
    } catch (e) {
      setStep({ kind: 'error', message: sendError(e) })
    }
  }
  const shown = step.kind === 'preview' || step.kind === 'sending' || step.kind === 'done' ? step.preview : null
  return (
    <Box icon="tools" title="Release or end this task" explain="What you can do as its owner from the Hub. A preview shows what changes; nothing changes until you apply it.">
      <div className="stack">
        <p>{work.why}</p>
        {(work.next_action || work.blocker) && (
          <p className="small muted">
            {work.next_action && <>Reported next action: {work.next_action}. </>}
            {work.blocker && <>Reported blocker: {work.blocker}.</>} Releasing keeps these; ending the task keeps them as history.
          </p>
        )}
        <div className="d-flex flex-wrap gap-2">
          {open.map((r) => (
            <button key={r.remedy} className="btn btn-sm btn-outline-secondary" disabled={step.kind === 'previewing' || step.kind === 'sending'} onClick={() => void preview(r.remedy)}>
              {LABEL[r.remedy]}
            </button>
          ))}
        </div>
        {step.kind === 'error' && <Notice tone="bad" title="That did not go through">{step.message}</Notice>}
        {shown && (
          <div className="stack">
            <p>
              <strong>{LABEL[shown.remedy]}:</strong> {shown.summary}
            </p>
            {shown.freed_paths.length > 0 && (
              <div>
                <span className="label">Files it frees</span>
                <Paths items={shown.freed_paths} />
              </div>
            )}
            {(shown.requests.length > 0 || shown.dependents.length > 0) && (
              <p className="small muted">
                {shown.requests.length > 0 && <>{shown.requests.length} open request{shown.requests.length === 1 ? '' : 's'} will record this and stay open. </>}
                {shown.dependents.length > 0 && <>{shown.dependents.length} unfinished task{shown.dependents.length === 1 ? ' depends' : 's depend'} on this one.</>}
              </p>
            )}
            {!shown.permitted && shown.refusal && <Notice title="You cannot do this now">{shown.refusal.message}</Notice>}
            {shown.permitted && step.kind !== 'done' && (
              <>
                <label className="label" htmlFor={`recovery-reason-${item.id}`}>
                  Why, in one line
                </label>
                <input id={`recovery-reason-${item.id}`} className="form-control form-control-sm" maxLength={500} value={reason} onChange={(e) => setReason(e.target.value)} />
                <div className="d-flex gap-2">
                  <button className="btn btn-sm btn-primary" disabled={!reason.trim() || step.kind === 'sending'} onClick={() => step.kind === 'preview' && void apply(step.preview, step.op)}>
                    Apply
                  </button>
                  <button className="btn btn-sm btn-outline-secondary" disabled={step.kind === 'sending'} onClick={() => setStep({ kind: 'idle' })}>
                    Cancel
                  </button>
                </div>
              </>
            )}
            {step.kind === 'done' && <Outcome result={step.result} onRetry={() => void apply(step.preview, step.op)} />}
          </div>
        )}
      </div>
    </Box>
  )
}

function Outcome({ result, onRetry }: { result: BatchItem; onRetry: () => void }) {
  if (result.outcome === 'applied' || result.outcome === 'replayed') return <Notice tone="ok" icon="check-circle" title="Done">{result.outcome === 'replayed' ? 'This was already applied; nothing changed now.' : 'The task page reloads with the change.'}</Notice>
  return (
    <Notice title="Nothing changed" action={result.outcome === 'unavailable' ? <button className="btn btn-sm btn-outline-secondary" onClick={onRetry}>Try again</button> : undefined}>
      {result.message ?? 'The Hub refused this change.'} {result.retry}
    </Notice>
  )
}
