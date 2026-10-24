import { useEffect, useState } from 'react'
import { fetchDiff, fetchEntry, fetchTimeline, type DiffEntry, type EvidenceState, type TimelineEntry, type Trigger } from '../localApi.ts'
import { Ago, Box, CopyButton, Empty, Explain, Icon, Notice, Pill, PreviewBanner, Skeleton, type Tone } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import { hashPath, Pager, useHashCursor } from '../Pager.tsx'

export type TimelineRoute = { kind: 'list' } | { kind: 'entry'; id: number } | { kind: 'compare'; left: number; right: number }

const ID = /^[1-9][0-9]{0,15}$/
/** `#timeline`, `#timeline/entry/<id>` and `#timeline/compare/<older>/<newer>`; anything else is the list. */
export function timelineRoute(hash = window.location.hash): TimelineRoute {
  const parts = hashPath(hash).slice(1).split('/')
  if (parts[0] !== 'timeline') return { kind: 'list' }
  if (parts[1] === 'entry' && parts.length === 3 && ID.test(parts[2]!)) return { kind: 'entry', id: Number(parts[2]) }
  if (parts[1] === 'compare' && parts.length === 4 && ID.test(parts[2]!) && ID.test(parts[3]!) && parts[2] !== parts[3]) {
    const [a, b] = [Number(parts[2]), Number(parts[3])].sort((x, y) => x - y)
    return { kind: 'compare', left: a!, right: b! }
  }
  return { kind: 'list' }
}
export const entryHref = (id: number) => `#timeline/entry/${id}`
export const compareHref = (a: number, b: number) => `#timeline/compare/${Math.min(a, b)}/${Math.max(a, b)}`

const TRIGGER: Record<Trigger, { icon: string; word: string }> = {
  manual: { icon: 'camera', word: 'Saved by hand' },
  turn_end: { icon: 'robot', word: 'Saved when an agent finished a turn' },
  check_start: { icon: 'play-circle', word: 'Saved as a check started' },
  mcp_explicit: { icon: 'plug', word: 'Saved when an agent asked' },
  unknown: { icon: 'camera', word: 'Saved' },
}

export function entryTitle(e: TimelineEntry): { icon: string; title: string } {
  if (e.kind === 'check_link') return { icon: 'patch-check', title: 'A check tested this snapshot' }
  return { icon: TRIGGER[e.trigger].icon, title: TRIGGER[e.trigger].word }
}

const EVIDENCE: Record<EvidenceState, { tone: Tone; word: string; text: string }> = {
  not_run: { tone: 'muted', word: 'No check', text: 'This is a plain snapshot. No check was linked to it.' },
  available: { tone: 'ok', word: 'Check found', text: 'The check this entry points to is still on this computer and matches what was tested.' },
  binding_mismatch: { tone: 'warn', word: 'Check doesn’t match', text: 'A check with this ID exists, but it tested something else. Don’t rely on it for this snapshot.' },
  unavailable: { tone: 'warn', word: 'Check not found', text: 'The linked check is no longer on this computer, so this page can’t show it.' },
}

function useRoute(): TimelineRoute {
  const [route, setRoute] = useState(() => timelineRoute())
  useEffect(() => {
    const on = () => setRoute(timelineRoute())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

function List() {
  const pager = useHashCursor()
  const before = /^[1-9][0-9]{0,15}$/.test(pager.cursor ?? '') ? Number(pager.cursor) : undefined
  const [picked, setPicked] = useState<number[]>([])
  const load = usePoll((signal) => fetchTimeline(signal, before), before === undefined ? 15_000 : 0, String(before ?? ''))
  if (load.kind === 'loading') return <Skeleton rows={5} />
  if (load.kind === 'error') return <Notice tone="bad" title="Could not read the timeline">{load.message}</Notice>
  const { value, source, reason } = load.value
  const toggle = (id: number) => setPicked((p) => (p.includes(id) ? p.filter((x) => x !== id) : [...p.slice(-1), id]))
  const newest = value.entries[0]
  return (
    <>
      {source === 'preview' && <PreviewBanner reason={reason} />}
      <p className="answer-text">
        {value.entries.length === 0 ? 'No snapshots yet. They appear after the first save.' : <>{value.entries.length} saved moment{value.entries.length === 1 ? '' : 's'} on this page; the newest was saved {newest ? <Ago at={newest.recorded_at} /> : null}.</>}
      </p>
      <Box
        icon="clock-history"
        tour="timeline-list"
        title="Saved moments"
        count={value.entries.length}
        explain="Newest first. Tick two to compare them."
        actions={picked.length === 2 ? <a data-tour="timeline-compare" className="btn btn-sm btn-primary" href={compareHref(picked[0]!, picked[1]!)}><Icon name="arrow-left-right" /> Compare these two</a> : <span data-tour="timeline-compare" className="small muted">{picked.length === 1 ? 'Tick one more to compare' : 'Tick two to compare'}</span>}
      >
        {value.entries.length === 0 ? (
          <Empty icon="camera" title="Nothing saved yet">Agents save a snapshot at the end of each turn once the timeline is on.</Empty>
        ) : (
          <ol className="snaps">
            {value.entries.map((e) => {
              const t = entryTitle(e)
              const on = picked.includes(e.id)
              return (
                <li key={e.id} className={`snap kind-${e.kind}${on ? ' is-picked' : ''}`}>
                  <span className="snap-dot" aria-hidden="true"><Icon name={t.icon} /></span>
                  <a href={entryHref(e.id)} className="snap-main">
                    <strong>{t.title}</strong>
                    <span className="small muted">
                      <Ago at={e.recorded_at} /> · #{e.id}
                      {e.kind === 'snapshot' && <> · {e.changed_paths} file{e.changed_paths === 1 ? '' : 's'} changed</>}
                      {e.kind === 'check_link' && <> · run <code title={e.run_id}>{e.run_id.length > 16 ? `${e.run_id.slice(0, 16)}…` : e.run_id}</code></>}
                      {e.scope && <> · only {e.scope.join(', ')}</>}
                    </span>
                  </a>
                  <label className="snap-pick">
                    <input type="checkbox" className="form-check-input" checked={on} onChange={() => toggle(e.id)} aria-label={`Pick #${e.id} to compare`} /> Compare
                  </label>
                </li>
              )
            })}
          </ol>
        )}
        <Pager onNewer={before !== undefined ? pager.newer : undefined} onOlder={value.next_before !== null ? () => pager.older(String(value.next_before)) : undefined} />
      </Box>
      {!value.coverage.includes_pre_feature_history && (
        <p className="small muted"><Icon name="info-circle" /> Saving began when the timeline was turned on{value.coverage.earliest_recorded_id ? ` (with #${value.coverage.earliest_recorded_id})` : ''}. Work from before then isn’t here.</p>
      )}
    </>
  )
}

function EntryDetail({ id }: { id: number }) {
  const load = usePoll((signal) => fetchEntry(id, signal), 0, String(id))
  if (load.kind === 'loading') return <Skeleton rows={5} />
  if (load.kind === 'error') return <Notice tone="bad" title={`Could not read #${id}`}>{load.message}</Notice>
  const { value, source, reason } = load.value
  const e = value.entry
  const t = entryTitle(e)
  const ev = EVIDENCE[value.evidence]
  const forkCmd = `agentscm --data-dir DATA workspace fork ROOT ${e.id} CHILD CHANGE --fork-root FORK_ROOT`
  return (
    <>
      {source === 'preview' && <PreviewBanner reason={reason} />}
      <Box icon={t.icon} title={`#${e.id} · ${t.title}`} explain="One saved moment of this workspace.">
        <dl className="facts">
          <div><dt>Saved</dt><dd><Ago at={e.recorded_at} /></dd></div>
          {e.kind === 'snapshot' ? (
            <>
              <div><dt>Files changed</dt><dd>{e.changed_paths}</dd></div>
              <div><dt>Revision</dt><dd><code>{e.revision.slice(0, 12)}</code></dd></div>
            </>
          ) : (
            <>
              <div><dt>Check run</dt><dd><code>{e.run_id}</code></dd></div>
              <div><dt>Change</dt><dd><code>{e.change}</code> v{e.change_version}</dd></div>
              <div><dt>Tested</dt><dd><code>{e.tested_revision.slice(0, 12)}</code>{e.tested_revision !== e.source_revision && <Pill tone="warn" tip="The check tested different bytes from the ones open when it started.">differs from start</Pill>}</dd></div>
            </>
          )}
          <div><dt>Covers</dt><dd>{e.scope ? e.scope.join(', ') : 'The whole workspace'}</dd></div>
        </dl>
        <p className="mt-3 mb-0"><Pill tone={ev.tone}>{ev.word}</Pill> <span className="small">{ev.text}</span></p>
      </Box>
      <Box icon="diagram-2" title="Start again from here" explain="Forking copies this saved moment into a new folder so you can try a different direction without touching the current one.">
        <p className="mb-2">To start fresh from #{e.id}, run this on this computer. This page only reads, so it can’t do it for you.</p>
        <div className="cmd"><code>{forkCmd}</code><CopyButton text={forkCmd} /></div>
      </Box>
    </>
  )
}

const CHANGE: Record<DiffEntry['change'], { tone: Tone; icon: string; word: string }> = {
  added: { tone: 'ok', icon: 'plus-circle', word: 'Added' },
  changed: { tone: 'brand', icon: 'pencil', word: 'Changed' },
  removed: { tone: 'bad', icon: 'dash-circle', word: 'Removed' },
}

function Compare({ left, right }: { left: number; right: number }) {
  const [pages, setPages] = useState<(string | undefined)[]>([undefined])
  const after = pages.at(-1)
  const [seen, setSeen] = useState<DiffEntry[]>([])
  const load = usePoll((signal) => fetchDiff(left, right, signal, after), 0, `${left}|${right}|${after ?? ''}`)
  // Merge each reply once, keyed by the reply itself: right after "Show more" the previous page is still
  // the loaded value for one render, and must not be appended again.
  const [merged, setMerged] = useState<object | null>(null)
  if (load.kind === 'ready' && load.value !== merged) {
    setSeen((s) => (after === undefined ? load.value.value.entries : [...s, ...load.value.value.entries]))
    setMerged(load.value)
  }
  if (load.kind === 'error') return <Notice tone="bad" title="Could not compare these two">{load.message}</Notice>
  const next = load.kind === 'ready' ? load.value.value.next : null
  const counts = { added: 0, changed: 0, removed: 0 }
  for (const d of seen) counts[d.change]++
  return (
    <>
      {load.kind === 'ready' && load.value.source === 'preview' && <PreviewBanner reason={load.value.reason} />}
      <Box icon="arrow-left-right" title={`What changed from #${left} to #${right}`} count={seen.length} explain="Files that differ between the older and newer saved moment, limited to paths both of them cover.">
        {load.kind === 'loading' && seen.length === 0 ? (
          <Skeleton rows={5} />
        ) : (
          <>
            <div className="pills mb-3">
              {(['added', 'changed', 'removed'] as const).map((k) => <Pill key={k} tone={CHANGE[k].tone} icon={CHANGE[k].icon}>{counts[k]} {CHANGE[k].word.toLowerCase()}</Pill>)}
            </div>
            {seen.length === 0 && !next ? (
              <Empty icon="check2-circle" title="No differences">These two saved moments hold the same files.</Empty>
            ) : (
              <ul className="run-list">
                {seen.map((d) => (
                  <li key={d.path} className="run-row">
                    <span className={`run-icon tone-${CHANGE[d.change].tone}`}><Icon name={CHANGE[d.change].icon} /></span>
                    <span className="run-main"><code>{d.path}</code></span>
                    <Pill tone={CHANGE[d.change].tone}>{CHANGE[d.change].word}</Pill>
                  </li>
                ))}
              </ul>
            )}
            {load.kind === 'ready' && load.value.value.missing > 0 && (
              <Notice title="Some files couldn’t be compared">{load.value.value.missing} file content{load.value.value.missing === 1 ? ' isn’t' : 's aren’t'} on this computer, so those differences may be missing from this list.</Notice>
            )}
            {next && (
              <div className="more-row">
                <span className="small muted"><Icon name="exclamation-circle" /> More differences exist. These are the first {seen.length}.</span>
                <button type="button" className="btn btn-sm btn-outline-secondary" disabled={load.kind === 'loading'} onClick={() => setPages((p) => [...p, next])}>Show more</button>
              </div>
            )}
          </>
        )}
      </Box>
    </>
  )
}

export function TimelinePage() {
  const route = useRoute()
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">What did this workspace look like earlier?</h1>
        <p className="answer-text">A <strong>snapshot</strong> is a saved picture of your files at one moment, so you can look back, compare two moments, or start again from one.</p>
      </div>
      {route.kind !== 'list' && <a href="#timeline" className="back"><Icon name="arrow-left" /> All saved moments</a>}
      {route.kind === 'list' && <List />}
      {route.kind === 'entry' && <EntryDetail key={route.id} id={route.id} />}
      {route.kind === 'compare' && <Compare key={`${route.left}|${route.right}`} left={route.left} right={route.right} />}
      <p className="small muted"><Icon name="laptop" /> Read from <code>agentscm serve</code> on this computer (the workspace it was started with), not the hub. Saving and forking happen in the CLI or your agent; this page only reads. <Explain text="Paging shows what was saved since the timeline was turned on. It isn't proof of everything that happened before." /></p>
    </div>
  )
}
