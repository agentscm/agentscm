// Shared ARIA 1.2 combobox (editable, list autocomplete). Two modes: `options` filters in the browser;
// `search(q, signal)` asks the server, debounced 200 ms, and aborts the previous request.
import { useEffect, useId, useState, type ReactNode } from 'react'
import { Icon } from './ui.tsx'

export interface ComboOption {
  value: string
  /** The human label (title, branch). */
  label: ReactNode
  /** Plain text the client filter matches against, and what the input shows after a pick when `keepText`. */
  text: string
  /** Right-hand facts: a status pill, a relative time. */
  meta?: ReactNode
  /** The ID, shown small and monospace under the label. */
  secondary?: string
}
export interface ComboResult {
  options: ComboOption[]
  /** More matches exist than were returned. */
  more?: boolean
  /** Fetches the next page; when set with `more`, a last "show more" option appends it. */
  loadMore?: (signal: AbortSignal) => Promise<ComboResult>
  moreLabel?: string
  /** Shown instead of "No matches" when nothing matched yet but more pages remain. */
  emptyMoreText?: string
}
const MORE = '\u0000more'

interface Props {
  id: string
  label: string
  placeholder?: string
  onSelect: (value: string, option?: ComboOption) => void
  options?: ComboOption[]
  search?: (q: string, signal: AbortSignal) => Promise<ComboResult>
  /** Returns the ID when the text is a complete ID; the first option then becomes "Open <id>". */
  parseId?: (text: string) => string | null
  /** Keep the picked option's text in the input instead of clearing it. */
  keepText?: boolean
  /** The ID kind has no distinctive shape (any word could be one): "Open <text>" goes last and is never chosen by Enter while results exist. */
  looseId?: boolean
  /** Fired on every edit of the text (not on a pick), so callers can drop a selection the text no longer names. */
  onTextChange?: (text: string) => void
  footer?: ReactNode
  describedBy?: string
  invalid?: boolean
  disabled?: boolean
  emptyText?: string
  'data-tour'?: string
}

const DEBOUNCE_MS = 200
type Remote = { key: string; result: ComboResult | null }

export function Combobox({ id, label, placeholder, onSelect, options, search, parseId, keepText = false, looseId = false, onTextChange, footer, describedBy, invalid, disabled, emptyText = 'No matches', ...rest }: Props) {
  const uid = useId()
  const listId = `${id}-listbox`
  const [query, setQuery] = useState('')
  const [typed, setTyped] = useState(false)
  const [open, setOpen] = useState(false)
  const [active, setActive] = useState(-1)
  const [remote, setRemote] = useState<Remote | null>(null)
  const [attempt, setAttempt] = useState(0)
  const [loadingMore, setLoadingMore] = useState(false)
  const text = query.trim()
  const term = typed ? text : ''
  const key = `${term}|${attempt}`

  useEffect(() => {
    if (!search || !open) return
    const ctrl = new AbortController()
    const timer = window.setTimeout(
      () => {
        search(term, ctrl.signal).then(
          (result) => {
            if (!ctrl.signal.aborted) setRemote({ key, result })
          },
          () => {
            if (!ctrl.signal.aborted) setRemote({ key, result: null })
          },
        )
      },
      text && typed ? DEBOUNCE_MS : 0,
    )
    return () => {
      window.clearTimeout(timer)
      ctrl.abort()
    }
    // `search` is intentionally not a dependency: callers pass a fresh closure each render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, open])

  const pasted = parseId?.(text) ?? null
  const current = remote?.key === key ? remote : null
  const shown: ComboOption[] = search ? ((current ?? remote)?.result?.options ?? []) : (options ?? []).filter((o) => !typed || !text || o.text.toLowerCase().includes(text.toLowerCase()))
  const result = current?.result ?? null
  const canMore = !!result?.more && !!result.loadMore
  const moreOption: ComboOption[] = canMore ? [{ value: MORE, text: '', label: loadingMore ? 'Searching…' : (result.moreLabel ?? 'Show more results') }] : []
  // A result that already is the pasted ID stands in for the "Open" option.
  const pastedOption: ComboOption | null = pasted && (!looseId || !shown.some((o) => o.value === pasted)) ? { value: pasted, label: <>Open <code>{pasted}</code></>, text: pasted } : null
  const rest2 = pasted && !looseId ? shown.filter((o) => o.value !== pasted) : shown
  const items: ComboOption[] = [...(pastedOption && !looseId ? [pastedOption] : []), ...rest2, ...(pastedOption && looseId ? [pastedOption] : []), ...moreOption]
  const strictPasted = !!pastedOption && !looseId
  const loading = !!search && !current
  // Keep the last results visible (full contrast, but inert) until the new ones land, so typing doesn't flash the list away.
  const stale = loading && shown.length > 0
  const live = stale ? [] : items
  const failed = !!search && !!current && current.result === null

  const loadMore = () => {
    if (!result?.loadMore || loadingMore || !remote) return
    const base = remote
    setLoadingMore(true)
    result.loadMore(new AbortController().signal).then(
      (next) => setRemote((r) => (r === base ? { key: base.key, result: { ...next, options: [...base.result!.options, ...next.options] } } : r)),
      () => setRemote((r) => (r === base ? { key: base.key, result: null } : r)),
    ).finally(() => setLoadingMore(false))
  }
  const pick = (o: ComboOption) => {
    if (o.value === MORE) return loadMore()
    setOpen(false)
    setTyped(false)
    setQuery(keepText ? o.text : '')
    onSelect(o.value, o)
  }
  const activeId = open && active >= 0 && live[active] ? `${uid}-opt-${active}` : undefined
  useEffect(() => {
    document.getElementById(activeId ?? '')?.scrollIntoView?.({ block: 'nearest' })
  }, [activeId])
  const move = (to: number) => {
    setOpen(true)
    setActive(live.length === 0 ? -1 : ((to % live.length) + live.length) % live.length)
  }
  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    const go = (to: number) => {
      e.preventDefault()
      move(to)
    }
    if (e.key === 'ArrowDown') go(open ? active + 1 : 0)
    else if (e.key === 'ArrowUp') go(open ? (active < 0 ? live.length - 1 : active - 1) : live.length - 1)
    else if (e.key === 'Home' && open) go(0)
    else if (e.key === 'End' && open) go(live.length - 1)
    else if (e.key === 'Enter' && open) {
      const o = live[active] ?? (pastedOption && (!looseId || rest2.length === 0) ? pastedOption : undefined)
      if (o) {
        e.preventDefault()
        pick(o)
      }
    } else if (e.key === 'Escape' && open) {
      e.preventDefault()
      setOpen(false)
    }
  }

  return (
    <div
      className="combo"
      data-tour={rest['data-tour']}
      onBlur={(e) => {
        // Focus may move from the input to the popup's buttons (Tab); close only when it leaves both.
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOpen(false)
      }}
      onKeyDown={(e) => {
        if (e.key === 'Escape' && e.target !== e.currentTarget.querySelector('input')) {
          setOpen(false)
          e.currentTarget.querySelector('input')?.focus()
        }
      }}
    >
      <label className="visually-hidden" htmlFor={id}>{label}</label>
      <input
        id={id}
        role="combobox"
        className={`form-control${invalid ? ' is-invalid' : ''}`}
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-autocomplete="list"
        aria-activedescendant={activeId}
        aria-describedby={describedBy}
        placeholder={placeholder}
        value={query}
        disabled={disabled}
        spellCheck={false}
        autoComplete="off"
        onChange={(e) => {
          setQuery(e.target.value)
          onTextChange?.(e.target.value)
          setTyped(true)
          setOpen(true)
          setActive(-1)
        }}
        onFocus={(e) => {
          // Returning from the popup (Escape on a footer button) must not reopen it.
          if (!e.currentTarget.parentElement?.contains(e.relatedTarget as Node | null)) setOpen(true)
          e.target.select()
        }}
        onClick={() => setOpen(true)}
        onKeyDown={onKeyDown}
      />
      {open && (
        <div className="combo-pop" onMouseDown={(e) => e.preventDefault()}>
          {items.length > 0 && (
            <ul id={listId} role="listbox" aria-label={label} aria-busy={stale} className="combo-list">
              {items.map((o, i) => (
                <li key={o.value} id={`${uid}-opt-${i}`} role="option" aria-selected={!stale && i === active} className="combo-opt" onMouseEnter={() => !stale && setActive(i)} onClick={() => !stale && pick(o)}>
                  <span className="combo-main">
                    <span className="combo-label">{o.label}</span>
                    {o.meta}
                  </span>
                  {o.secondary && <code className="combo-id">{o.secondary}</code>}
                </li>
              ))}
            </ul>
          )}
          {items.length === 0 && <ul id={listId} role="listbox" aria-label={label} hidden />}
          {loading && <div className="combo-note small muted" role="status"><Icon name="arrow-repeat" /> Searching…</div>}
          {failed && (
            <div className="combo-note small" role="alert">
              Could not search. <button type="button" className="btn btn-link btn-sm p-0" onClick={() => setAttempt((n) => n + 1)}>Try again</button>
            </div>
          )}
          {!loading && !failed && shown.length === 0 && !strictPasted && <div className="combo-note small muted" role="status">{canMore ? (result.emptyMoreText ?? 'No matches in the results searched so far') : emptyText}</div>}
          {!loading && !failed && items.length > 0 && (
            <div className="visually-hidden" role="status">{items.length} result{items.length === 1 ? '' : 's'}</div>
          )}
          {search && current?.result?.more && !current.result.loadMore && <div className="combo-note small muted">More matches exist. Keep typing to narrow them.</div>}
          {footer && <div className="combo-note">{footer}</div>}
        </div>
      )}
    </div>
  )
}
