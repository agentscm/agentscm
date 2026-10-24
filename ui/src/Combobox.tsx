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
}

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
  footer?: ReactNode
  describedBy?: string
  invalid?: boolean
  disabled?: boolean
  emptyText?: string
  'data-tour'?: string
}

const DEBOUNCE_MS = 200
type Remote = { key: string; result: ComboResult | null }

export function Combobox({ id, label, placeholder, onSelect, options, search, parseId, keepText = false, footer, describedBy, invalid, disabled, emptyText = 'No matches', ...rest }: Props) {
  const uid = useId()
  const listId = `${id}-listbox`
  const [query, setQuery] = useState('')
  const [typed, setTyped] = useState(false)
  const [open, setOpen] = useState(false)
  const [active, setActive] = useState(-1)
  const [remote, setRemote] = useState<Remote | null>(null)
  const [attempt, setAttempt] = useState(0)
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
  const shown: ComboOption[] = search ? (current?.result?.options ?? []) : (options ?? []).filter((o) => !typed || !text || o.text.toLowerCase().includes(text.toLowerCase()))
  const items: ComboOption[] = pasted ? [{ value: pasted, label: <>Open <code>{pasted}</code></>, text: pasted }, ...shown.filter((o) => o.value !== pasted)] : shown
  const loading = !!search && !current
  const failed = !!search && !!current && current.result === null

  const pick = (o: ComboOption) => {
    setOpen(false)
    setTyped(false)
    setQuery(keepText ? o.text : '')
    onSelect(o.value, o)
  }
  const move = (to: number) => {
    setOpen(true)
    setActive(items.length === 0 ? -1 : ((to % items.length) + items.length) % items.length)
  }
  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    const go = (to: number) => {
      e.preventDefault()
      move(to)
    }
    if (e.key === 'ArrowDown') go(open ? active + 1 : 0)
    else if (e.key === 'ArrowUp') go(open ? (active < 0 ? items.length - 1 : active - 1) : items.length - 1)
    else if (e.key === 'Home' && open) go(0)
    else if (e.key === 'End' && open) go(items.length - 1)
    else if (e.key === 'Enter' && open) {
      const o = items[active] ?? (pasted ? items[0] : undefined)
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
    <div className="combo" data-tour={rest['data-tour']}>
      <label className="visually-hidden" htmlFor={id}>{label}</label>
      <input
        id={id}
        role="combobox"
        className={`form-control${invalid ? ' is-invalid' : ''}`}
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={open && active >= 0 && items[active] ? `${uid}-opt-${active}` : undefined}
        aria-describedby={describedBy}
        placeholder={placeholder}
        value={query}
        disabled={disabled}
        spellCheck={false}
        autoComplete="off"
        onChange={(e) => {
          setQuery(e.target.value)
          setTyped(true)
          setOpen(true)
          setActive(-1)
        }}
        onFocus={(e) => {
          setOpen(true)
          e.target.select()
        }}
        onClick={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={onKeyDown}
      />
      {open && (
        <div className="combo-pop" onMouseDown={(e) => e.preventDefault()}>
          {items.length > 0 && (
            <ul id={listId} role="listbox" aria-label={label} className="combo-list">
              {items.map((o, i) => (
                <li key={o.value} id={`${uid}-opt-${i}`} role="option" aria-selected={i === active} className="combo-opt" onMouseEnter={() => setActive(i)} onClick={() => pick(o)}>
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
          {!loading && !failed && items.length === 0 && <div className="combo-note small muted" role="status">{emptyText}</div>}
          {!loading && !failed && items.length > 0 && (
            <div className="visually-hidden" role="status">{items.length} result{items.length === 1 ? '' : 's'}</div>
          )}
          {search && current?.result?.more && <div className="combo-note small muted">More matches exist. Keep typing to narrow them.</div>}
          {footer && <div className="combo-note">{footer}</div>}
        </div>
      )}
    </div>
  )
}
