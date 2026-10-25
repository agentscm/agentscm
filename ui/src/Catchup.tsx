import { useCallback, useEffect, useRef, useState } from 'react'
import {
  acknowledgeCatchup,
  acquireCatchupDeliveryClient,
  appendCatchupPage,
  fetchCatchup,
  releaseCatchupDeliveryClient,
  type CatchupDeliveryClientLease,
  type CatchupCoordinationEvent,
  type CatchupMergeObservation,
  type CatchupPage,
  type CatchupWorkEvent,
} from './catchup.ts'
import { Ago, Box, Icon, Notice, Skeleton, localDate } from './ui.tsx'

type LoadState =
  | { kind: 'loading' }
  | { kind: 'error'; token: string; message: string }
  | { kind: 'ready'; token: string; page: CatchupPage }

const message = (error: unknown) => error instanceof Error ? error.message : 'Unknown error'
const plural = (count: number, singular: string, many = `${singular}s`) => `${count} ${count === 1 ? singular : many}`
const shortCommit = (commit: string) => commit.slice(0, 12)

const WORK_LABEL: Record<string, string> = {
  work_started: 'Work started',
  work_joined: 'Agent joined',
  work_takeover: 'Work taken over',
  work_checkpoint: 'Checkpoint reported',
}

const COORDINATION_LABEL: Record<string, string> = {
  coordination_requested: 'Coordination requested',
  coordination_responded: 'Coordination response',
  coordination_resolved: 'Coordination resolved',
  coordination_withdrawn: 'Coordination withdrawn',
}

function WorkUpdate({ event }: { event: CatchupWorkEvent }) {
  const summary = event.item.current?.summary ?? event.item.title
  return (
    <li>
      <div><strong>{WORK_LABEL[event.kind] ?? 'Work updated'}</strong> · <a href={`#work/${encodeURIComponent(event.work_id)}`}>{event.item.title}</a></div>
      <div className="small muted">{event.item.lead_agent} · {summary} · <Ago at={event.at} /></div>
      <details className="disclosure">
        <summary>Source details</summary>
        <dl className="tech">
          <dt>Work</dt><dd><code className="text-break">{event.work_id}</code></dd>
          <dt>Event</dt><dd><code>{event.seq}</code> · {event.kind}</dd>
          <dt>Recorded</dt><dd>{localDate(event.at)}</dd>
        </dl>
      </details>
    </li>
  )
}

function CoordinationUpdate({ event }: { event: CatchupCoordinationEvent }) {
  return (
    <li>
      <div><strong>{COORDINATION_LABEL[event.kind] ?? 'Coordination updated'}</strong> · {event.title}</div>
      <div className="small muted">{event.status} · <Ago at={event.at} /></div>
      <details className="disclosure">
        <summary>Source details</summary>
        <dl className="tech">
          <dt>Request</dt><dd><code className="text-break">{event.request_id}</code></dd>
          <dt>Event</dt><dd><code>{event.seq}</code> · {event.kind}</dd>
          <dt>Recorded</dt><dd>{localDate(event.at)}</dd>
        </dl>
      </details>
    </li>
  )
}

function MergeUpdate({ merge }: { merge: CatchupMergeObservation }) {
  return (
    <li>
      <div><strong>{merge.title ?? `Pull request #${merge.pull_number}`}</strong></div>
      <div className="small muted">
        PR #{merge.pull_number} · <code>{shortCommit(merge.merge_commit)}</code> · observed <Ago at={merge.observed_at} />
        {merge.occurred_at !== null && <> · occurred <Ago at={merge.occurred_at} /></>}
      </div>
      <details className="disclosure">
        <summary>Source details</summary>
        <dl className="tech">
          <dt>Observation</dt><dd><code className="text-break">{merge.id}</code></dd>
          <dt>Source</dt><dd>{merge.source === 'forge_pull' ? 'Forge pull request' : 'AgentSCM publication'} · <code className="text-break">{merge.source_id}</code></dd>
          <dt>Branch → base</dt><dd><code className="text-break">{merge.branch}</code> → <code className="text-break">{merge.base}</code></dd>
          <dt>Head commit</dt><dd><code className="text-break">{merge.head_commit}</code></dd>
          <dt>Merge commit</dt><dd><code className="text-break">{merge.merge_commit}</code></dd>
          <dt>Observed</dt><dd>{localDate(merge.observed_at)}</dd>
          <dt>Occurred</dt><dd>{merge.occurred_at === null ? 'Not reported' : localDate(merge.occurred_at)}</dd>
          <dt>Paths</dt><dd>{merge.paths.length === 0 ? 'Not reported' : <>{merge.paths.slice(0, 8).map((path) => <code key={path} className="text-break">{path}</code>)}{(!merge.paths_complete || merge.paths.length > 8) && <span className="muted">More paths may exist</span>}</>}</dd>
        </dl>
      </details>
    </li>
  )
}

function Groups({ page }: { page: CatchupPage }) {
  return (
    <div className="stack">
      {page.merges.length > 0 && (
        <details className="disclosure" open>
          <summary>{plural(page.merges.length, 'merge observation')}</summary>
          <ul className="plain-list">{page.merges.map((merge) => <MergeUpdate key={merge.id} merge={merge} />)}</ul>
        </details>
      )}
      {page.work_events.length > 0 && (
        <details className="disclosure" open={page.merges.length === 0}>
          <summary>{plural(page.work_events.length, 'work update')}</summary>
          <ul className="plain-list">{page.work_events.map((event) => <WorkUpdate key={event.seq} event={event} />)}</ul>
        </details>
      )}
      {page.coordination_events.length > 0 && (
        <details className="disclosure" open={page.merges.length === 0 && page.work_events.length === 0}>
          <summary>{plural(page.coordination_events.length, 'coordination update')}</summary>
          <ul className="plain-list">{page.coordination_events.map((event) => <CoordinationUpdate key={event.seq} event={event} />)}</ul>
        </details>
      )}
    </div>
  )
}

export function CatchupPanel({ token }: { token: string }) {
  const [state, setState] = useState<LoadState>({ kind: 'loading' })
  const [busy, setBusy] = useState<{ token: string; kind: 'page' | 'ack' } | null>(null)
  const [notice, setNotice] = useState<{ token: string; tone: 'ok' | 'warn' | 'bad'; title: string; detail: string } | null>(null)
  const [deliveryClient, setDeliveryClient] = useState<CatchupDeliveryClientLease | null>(null)
  const generation = useRef(0)
  const deliveryClientEpoch = useRef(0)
  const deliveryClientPromise = useRef<Promise<CatchupDeliveryClientLease> | null>(null)
  const heldDeliveryClient = useRef<CatchupDeliveryClientLease | null>(null)
  const invalidateGeneration = useCallback(() => { generation.current++ }, [])
  const nextDeliveryClientEpoch = useCallback(() => ++deliveryClientEpoch.current, [])
  const isDeliveryClientEpoch = useCallback((epoch: number) => deliveryClientEpoch.current === epoch, [])

  useEffect(() => {
    const epoch = nextDeliveryClientEpoch()
    const pending = deliveryClientPromise.current ??= acquireCatchupDeliveryClient()
    void pending.then((lease) => {
      if (!isDeliveryClientEpoch(epoch)) return
      heldDeliveryClient.current = lease
      setDeliveryClient(lease)
    })
    return () => {
      const cleanupEpoch = nextDeliveryClientEpoch()
      if (heldDeliveryClient.current !== null) {
        releaseCatchupDeliveryClient(heldDeliveryClient.current)
        heldDeliveryClient.current = null
        deliveryClientPromise.current = null
        return
      }
      queueMicrotask(() => {
        if (!isDeliveryClientEpoch(cleanupEpoch)) return
        const abandoned = deliveryClientPromise.current
        deliveryClientPromise.current = null
        void abandoned?.then(releaseCatchupDeliveryClient)
      })
    }
  }, [isDeliveryClientEpoch, nextDeliveryClientEpoch])

  const loadInitial = useCallback(async (keepCurrent = false) => {
    if (deliveryClient === null) return
    const operation = generation.current
    const controller = new AbortController()
    try {
      const page = await fetchCatchup(token, controller.signal, { subscriber: 'human', deliveryClient: deliveryClient.client })
      if (generation.current === operation) setState({ kind: 'ready', token, page })
    } catch (error) {
      if (!controller.signal.aborted && generation.current === operation) {
        if (keepCurrent) setNotice({ token, tone: 'bad', title: 'Could not refresh catch-up', detail: message(error) })
        else setState({ kind: 'error', token, message: message(error) })
      }
    }
  }, [deliveryClient, token])

  useEffect(() => {
    if (deliveryClient === null) return
    const operation = ++generation.current
    const controller = new AbortController()
    void fetchCatchup(token, controller.signal, { subscriber: 'human', deliveryClient: deliveryClient.client }).then(
      (page) => { if (!controller.signal.aborted && generation.current === operation) setState({ kind: 'ready', token, page }) },
      (error: unknown) => { if (!controller.signal.aborted && generation.current === operation) setState({ kind: 'error', token, message: message(error) }) },
    )
    return () => {
      controller.abort()
      invalidateGeneration()
    }
  }, [deliveryClient, invalidateGeneration, token])

  const currentState: LoadState = state.kind !== 'loading' && state.token !== token ? { kind: 'loading' } : state
  const currentBusy = busy?.token === token ? busy.kind : null
  const currentNotice = notice?.token === token ? notice : null
  if (currentState.kind === 'loading') return <Box icon="clock-history" title="Since you last checked" tone="muted"><Skeleton rows={3} /></Box>
  if (currentState.kind === 'error') {
    return (
      <Notice tone="bad" icon="clock-history" title="Catch-up unavailable" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => { setState({ kind: 'loading' }); void loadInitial() }}>Retry</button>}>
        Current Work remains visible below. This failure is not an empty result or an all-clear. {currentState.message}
      </Notice>
    )
  }

  const { page } = currentState
  const total = page.work_events.length + page.coordination_events.length + page.merges.length
  const hasGap = page.retention.work_gap || page.retention.coordination_gap || page.retention.merge_gap
  const limitedSources = page.source_coverage.filter((coverage) => coverage.state !== 'observed_only')

  const loadMore = async () => {
    if (currentBusy || page.complete) return
    const operation = generation.current
    setBusy({ token, kind: 'page' })
    setNotice(null)
    const controller = new AbortController()
    try {
      const next = await fetchCatchup(token, controller.signal, {
        subscriber: page.subscriber,
        deliveryClient: deliveryClient!.client,
        delivery: page.delivery,
        eventAfter: page.next_event_after,
        eventThrough: page.event_through,
        mergeAfter: page.next_merge_after,
        mergeThrough: page.merge_through,
      })
      if (generation.current === operation) setState({ kind: 'ready', token, page: appendCatchupPage(page, next) })
    } catch (error) {
      if (generation.current === operation) setNotice({ token, tone: 'bad', title: 'Could not load the next page', detail: message(error) })
    } finally {
      if (generation.current === operation) setBusy(null)
    }
  }

  const acknowledge = async () => {
    if (currentBusy || !page.complete) return
    const operation = generation.current
    setBusy({ token, kind: 'ack' })
    setNotice(null)
    const controller = new AbortController()
    try {
      const result = await acknowledgeCatchup(token, controller.signal, page)
      if (generation.current !== operation) return
      if (result.status === 'conflict') {
        setNotice({ token, tone: 'warn', title: 'Review position changed', detail: 'Another device advanced this review position. Reloading the current delivery without moving it backwards.' })
        await loadInitial(true)
      } else {
        setNotice({ token, tone: 'ok', title: 'Marked retained records reviewed', detail: 'The server advanced this repository’s personal review position through the retained records in this fixed delivery.' })
        await loadInitial(true)
      }
    } catch (error) {
      if (generation.current === operation) setNotice({ token, tone: 'bad', title: 'Could not mark reviewed', detail: `The review position was not assumed to move. ${message(error)}` })
    } finally {
      if (generation.current === operation) setBusy(null)
    }
  }

  const actions = page.complete ? (
    <button type="button" className="btn btn-sm btn-primary" disabled={currentBusy !== null} onClick={() => void acknowledge()}>
      <Icon name="check2" /> {currentBusy === 'ack' ? 'Marking…' : hasGap ? 'Mark available records reviewed' : 'Mark reviewed'}
    </button>
  ) : (
    <button type="button" className="btn btn-sm btn-outline-secondary" disabled={currentBusy !== null} onClick={() => void loadMore()}>
      <Icon name="arrow-down-circle" /> {currentBusy === 'page' ? 'Loading…' : 'Load more'}
    </button>
  )

  return (
    <Box
      id="since-last-checked"
      icon="clock-history"
      title="Since you last checked"
      count={total}
      tone={hasGap || !page.complete ? 'warn' : total > 0 ? 'brand' : 'muted'}
      explain="A fixed, personal AgentSCM delivery. Opening, refreshing or paging this view does not mark it reviewed."
      actions={actions}
    >
      {currentNotice && <Notice tone={currentNotice.tone} title={currentNotice.title}>{currentNotice.detail}</Notice>}
      {hasGap && (
        <Notice title="Earlier records are no longer retained">
          This delivery starts at the available retention floor. Review current Work and source details before acting; the missing interval cannot be treated as quiet.
        </Notice>
      )}
      {!page.complete && (
        <Notice title="More changes remain">
          This fixed delivery is only partly loaded. Load every page before it can be marked reviewed.
        </Notice>
      )}
      {limitedSources.map((coverage) => (
        <Notice key={coverage.source} tone={coverage.state === 'unavailable' ? 'bad' : 'warn'} title={`${coverage.source === 'forge_pull' ? 'Forge pull request' : 'Tracked publication'} source ${coverage.state === 'unavailable' ? 'unavailable' : 'incomplete'}`}>
          {coverage.checked_at === null ? 'No successful source check is recorded.' : <>Last checked {localDate(coverage.checked_at)}.</>} {coverage.unread_pages > 0 && <>{plural(coverage.unread_pages, 'source page')} {coverage.unread_pages === 1 ? 'remains' : 'remain'} unread. </>}{coverage.deferred > 0 && <>{plural(coverage.deferred, 'observation')} {coverage.deferred === 1 ? 'was' : 'were'} deferred.</>}
        </Notice>
      ))}
      {total === 0 ? (
        <p>{page.complete ? 'No new retained AgentSCM records are in this delivery.' : 'No records are shown on this page; later pages remain.'}</p>
      ) : (
        <>
          <p>
            {plural(page.work_events.length, 'work update')}, {plural(page.coordination_events.length, 'coordination update')}, and {plural(page.merges.length, 'merge observation')} loaded.
          </p>
          <Groups page={page} />
        </>
      )}
      <p className="small muted">
        Merge entries are immutable observations recorded by AgentSCM. Source status is observational and does not claim all forge history. Current unfinished Work and coordination warnings remain below.
      </p>
      <details className="disclosure">
        <summary>Delivery details</summary>
        <dl className="tech">
          <dt>Delivery</dt><dd><code className="text-break">{page.delivery}</code></dd>
          <dt>Work events</dt><dd><code>{page.event_from}</code> → <code>{page.event_through}</code></dd>
          <dt>Merge observations</dt><dd><code>{page.merge_from}</code> → <code>{page.merge_through}</code></dd>
          <dt>Retention floors</dt><dd>events <code>{page.retention.event_available_from}</code> · merges <code>{page.retention.merge_available_from}</code></dd>
        </dl>
      </details>
    </Box>
  )
}
