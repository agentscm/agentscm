import type { WorkItem } from './standup.ts'
import { Box, Empty, Notice, Pill, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'
import { fetchReconciliation, type ReconciliationSummary } from './workReconciliation.ts'

export function ReconciliationFacts({ value }: { value: ReconciliationSummary }) {
  const current = value.current
  if (!current) {
    return <Empty icon="question-circle" title={value.stale_reasons.includes('not_reconciled') ? 'Not reconciled yet' : 'Prerequisite evidence needs updating'}>
      Ask the lead agent to refresh prerequisite evidence. Earlier results remain history.
    </Empty>
  }
  const reservation = {
    held: 'Scope is still held',
    released: 'Scope is released',
    explicitly_retained: 'Scope was explicitly retained',
    held_owner_decision_required: 'The owner must decide what happens to the held scope',
  }[current.reservation_disposition]
  return <div className="stack">
    <div>
      <strong>{reservation}</strong>
      <p>{current.next_action}</p>
    </div>
    {current.conditions.length > 0 && <div>
      <h3>Prerequisites</h3>
      <ul className="plain-list">
        {current.conditions.map((condition) => <li key={condition.id}>
          <strong>{condition.id}</strong>{' '}
          <Pill tone={condition.state === 'satisfied' ? 'ok' : 'warn'}>
            {condition.state === 'not_satisfied' ? 'Not satisfied' : condition.state === 'satisfied' ? 'Satisfied' : 'Unknown'}
          </Pill>
          <div className="small muted">
            {condition.obstructing ? 'Still obstructing this task.' : 'This condition no longer obstructs this task.'}
            {condition.limits.length > 0 && <> Evidence limits: {condition.limits.join(', ')}.</>}
          </div>
        </li>)}
      </ul>
    </div>}
    {current.links.length > 0 && <details className="disclosure">
      <summary>Declared PR conditions</summary>
      <ul className="plain-list">
        {current.links.map((link) => <li key={link.id}>{link.id}: {link.state.replaceAll('_', ' ')}</li>)}
      </ul>
    </details>}
    {current.reassessments.length > 0 && <Notice tone="warn" title="Reassess the reported blocker">
      {current.reassessments.length} prerequisite condition{current.reassessments.length === 1 ? ' is' : 's are'} satisfied. The original blocker is preserved below; other blockers and task completion remain separate.
    </Notice>}
    <p className="small muted">Retained provider observations support these conditions. A merged PR never proves the whole task completed or releases its scope.</p>
  </div>
}

export function WorkReconciliation({ item, token, refreshWork }: { item: WorkItem; token: string; refreshWork: () => void }) {
  const poll = usePoll((signal) => fetchReconciliation(item, token, signal), 15_000, `reconciliation|${token}|${item.id}|${item.generation}|${item.updated_seq}`)
  return <Box icon="signpost-split" title="Prerequisites and held scope" explain="The same stored reconciliation agents read, separate from the agent's report.">
    {poll.kind === 'loading' ? <Skeleton rows={2} />
      : poll.kind === 'error' || poll.refreshError ? <Notice tone="warn" title="Reconciliation is unavailable" action={<button className="btn btn-sm btn-outline-secondary" onClick={poll.retry}>Retry</button>}>
        Current evidence could not be checked. Earlier results are hidden until this read succeeds.
      </Notice>
        : poll.value.stale_reasons.includes('work_detail_changed') ? <Notice tone="warn" title="Work changed since this page loaded" action={<button className="btn btn-sm btn-outline-secondary" onClick={refreshWork}>Reload Work detail</button>}>Reload the Work item before inspecting its current prerequisite evidence.</Notice>
          : <ReconciliationFacts value={poll.value} />}
  </Box>
}
