// Shared presentation primitives: Bootstrap tooltips wired into React, evidence boxes, state pills.
import { Tooltip } from 'bootstrap'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useNow } from './useHub.ts'

export const Icon = ({ name, className = '' }: { name: string; className?: string }) => <i className={`bi bi-${name} ${className}`} aria-hidden="true" />

/** A Bootstrap tooltip owned by React: created on mount, updated on text change, disposed on unmount. */
export function Tip({ text, children, placement = 'top', className = '', focusable = false }: { text: string; children: ReactNode; placement?: 'top' | 'bottom' | 'left' | 'right'; className?: string; focusable?: boolean }) {
  const ref = useRef<HTMLSpanElement>(null)
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const tip = new Tooltip(el, { title: text, placement, trigger: 'hover focus', container: 'body' })
    return () => tip.dispose()
  }, [text, placement])
  return (
    <span ref={ref} className={className} tabIndex={focusable ? 0 : undefined}>
      {children}
    </span>
  )
}

/** The (i) explainer used beside every term whose meaning is not obvious. */
export function Explain({ text }: { text: string }) {
  return (
    <Tip text={text} focusable className="explain">
      <Icon name="info-circle" />
      <span className="visually-hidden">{text}</span>
    </Tip>
  )
}

/** An evidence box: icon-badged header strip, optional count and explainer, then content. */
export function Box({ id, tour, icon, title, explain, count, tone = 'brand', actions, children, className = '', flush = false }: { id?: string; /** Stable walkthrough anchor (`data-tour`). */ tour?: string; icon: string; title: string; explain?: string; count?: number | string; tone?: Tone; actions?: ReactNode; children: ReactNode; className?: string; flush?: boolean }) {
  return (
    <section id={id} data-tour={tour} className={`box box-tone-${tone} ${className}`} aria-labelledby={id ? `${id}-title` : undefined}>
      <header className="box-head">
        <span className={`box-badge tone-${tone}`}>
          <Icon name={icon} />
        </span>
        <h2 id={id ? `${id}-title` : undefined} className="box-title">
          {title}
        </h2>
        {count !== undefined && <span className="box-count">{count}</span>}
        {actions && <div className="box-actions">{actions}</div>}
        {explain && <p className="box-explain">{explain}</p>}
      </header>
      <div className={flush ? 'box-body flush' : 'box-body'}>{children}</div>
    </section>
  )
}

export type Tone = 'brand' | 'ok' | 'warn' | 'bad' | 'muted'

/** Color never travels alone: every pill carries an icon and a text label. */
export function Pill({ tone, icon, children, tip }: { tone: Tone; icon?: string; children: ReactNode; tip?: string }) {
  const pill = (
    <span className={`pill tone-${tone}`}>
      {icon && <Icon name={icon} />}
      {children}
    </span>
  )
  return tip ? <Tip text={tip} focusable>{pill}</Tip> : pill
}

export function Empty({ icon, title, children }: { icon: string; title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <Icon name={icon} className="empty-icon" />
      <p className="empty-title">{title}</p>
      {children && <p className="empty-text">{children}</p>}
    </div>
  )
}

export function Notice({ tone = 'warn', icon = 'exclamation-triangle', title, children, action }: { tone?: Tone; icon?: string; title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className={`notice tone-${tone}`} role={tone === 'bad' ? 'alert' : 'status'}>
      <Icon name={icon} className="notice-icon" />
      <div className="notice-body">
        <strong>{title}</strong>
        {children && <div>{children}</div>}
      </div>
      {action}
    </div>
  )
}

/** Row-shaped, like the lists it stands in for, so the content lands where the skeleton was. Shown only after a short delay (CSS), so a fast load never flashes it. */
export function Skeleton({ rows = 3 }: { rows?: number }) {
  return (
    <div className="placeholder-glow skeleton" aria-label="Loading" role="status">
      {Array.from({ length: rows }, (_, i) => (
        <span key={i} className="skeleton-row">
          <span className="placeholder skeleton-dot" />
          <span className="skeleton-text">
            <span className="placeholder skeleton-line" style={{ width: `${78 - ((i * 17) % 34)}%` }} />
            <span className="placeholder skeleton-line is-sub" style={{ width: `${46 - ((i * 11) % 20)}%` }} />
          </span>
        </span>
      ))}
    </div>
  )
}

export function Path({ children }: { children: string }) {
  return <code className="path">{children}</code>
}

export function Paths({ items, max = 6 }: { items: string[]; max?: number }) {
  if (items.length === 0) return <span className="muted">No paths declared</span>
  return (
    <span className="paths">
      {items.slice(0, max).map((p) => (
        <Path key={p}>{p}</Path>
      ))}
      {items.length > max && <Tip text={items.slice(max).join('\n')} focusable className="more">+{items.length - max} more</Tip>}
    </span>
  )
}

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'short' })
export function relative(atSeconds: number, nowMs: number): string {
  const diff = Math.round(atSeconds - nowMs / 1000)
  const abs = Math.abs(diff)
  if (abs < 5) return 'just now'
  if (abs < 60) return rtf.format(diff, 'second')
  if (abs < 3600) return rtf.format(Math.round(diff / 60), 'minute')
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), 'hour')
  return rtf.format(Math.round(diff / 86400), 'day')
}
export const localDate = (at: number): string => new Date(at * 1000).toLocaleString()

/** Relative time that ticks with the shared clock; the absolute time rides in the tooltip. */
export function Ago({ at }: { at: number }) {
  const now = useNow()
  return (
    <Tip text={localDate(at)} focusable>
      <time dateTime={new Date(at * 1000).toISOString()} className="ago">
        {relative(at, now)}
      </time>
    </Tip>
  )
}

/** Older records can hold a zero time from a hook that reported none; never render that as an age. */
export function Reported({ at }: { at: number }) {
  return at > 0 ? <Ago at={at} /> : <span className="muted">time not reported</span>
}

export function CopyButton({ text, label = 'Copy', className = 'btn btn-sm btn-outline-secondary' }: { text: string; label?: string; className?: string }) {
  const [state, setState] = useState<'idle' | 'ok' | 'fail'>('idle')
  const timer = useRef<number>()
  useEffect(() => () => window.clearTimeout(timer.current), [])
  return (
    <button
      type="button"
      className={className}
      onClick={() => {
        // `navigator.clipboard` is missing on plain-http pages (a LAN hub), so a throw is a failed copy too.
        void Promise.resolve()
          .then(() => navigator.clipboard.writeText(text))
          .then(() => setState('ok'), () => setState('fail'))
        window.clearTimeout(timer.current)
        timer.current = window.setTimeout(() => setState('idle'), 2000)
      }}
    >
      <Icon name={state === 'ok' ? 'check2' : state === 'fail' ? 'x-lg' : 'clipboard'} /> {state === 'ok' ? 'Copied' : state === 'fail' ? 'Copy failed' : label}
    </button>
  )
}

const VIA_NAMES: Record<string, string> = { claude: 'Claude Code', codex: 'Codex', cursor: 'Cursor', copilot: 'Copilot', gemini: 'Gemini' }
export function viaLabel(via: string): string {
  if (via === 'mcp') return 'MCP'
  if (via.startsWith('hook:')) return `Hook · ${VIA_NAMES[via.slice(5)] ?? via.slice(5)}`
  return via
}
export function Via({ via }: { via: string }) {
  return (
    <Pill tone="muted" icon={via === 'mcp' ? 'plug' : 'link-45deg'} tip={via === 'mcp' ? 'Connected through the AgentSCM MCP server: the agent can read and announce work.' : 'Connected through the agent’s edit hook: the hub sees each file edit before it happens.'}>
      {viaLabel(via)}
    </Pill>
  )
}

// Client setup names tokens "client-<host>-<hex id>": every agent on one computer shares the host, so the id is what tells them apart.
const CLIENT_TOKEN = /^client-(.+)-([0-9a-f]{6,})$/i
/** The name people read: "Agent 6a36" for a generated client token (the full name stays in the title), anything else as given. */
export const agentLabel = (name: string) => {
  const m = CLIENT_TOKEN.exec(name)
  return m ? `Agent ${m[2]!.slice(0, 4)}` : name
}

/** A simple agent mark: initials on a hue derived from the name, so the same agent reads the same everywhere. */
export function Avatar({ name, size = 32 }: { name: string; size?: number }) {
  let h = 0
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) % 360
  const id = CLIENT_TOKEN.exec(name)?.[2]
  const initials = id ? id.slice(0, 2).toUpperCase() : name.replace(/[^a-z0-9]+/gi, ' ').trim().split(' ').map((w) => w[0]).join('').slice(0, 2).toUpperCase() || '?'
  return (
    <span className="avatar" style={{ width: size, height: size, ['--h' as string]: String(h) }} aria-hidden="true">
      {initials}
    </span>
  )
}

/** Shown whenever a local-feature page is rendering fixtures instead of a live local read API. */
export function PreviewBanner({ reason }: { reason?: string }) {
  return (
    <Notice tone="brand" icon="easel" title="Preview data — the local read API isn’t connected yet">
      {reason ? `${reason} ` : ''}Everything below is sample data so you can see how this page works. Set the local server address in Settings once <code>agentscm serve</code> offers these reads.
    </Notice>
  )
}
