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
export function Box({ id, icon, title, explain, count, tone = 'brand', actions, children, className = '', flush = false }: { id?: string; icon: string; title: string; explain?: string; count?: number | string; tone?: Tone; actions?: ReactNode; children: ReactNode; className?: string; flush?: boolean }) {
  return (
    <section id={id} className={`box ${className}`} aria-labelledby={id ? `${id}-title` : undefined}>
      <header className="box-head">
        <span className={`box-badge tone-${tone}`}>
          <Icon name={icon} />
        </span>
        <h2 id={id ? `${id}-title` : undefined} className="box-title">
          {title}
        </h2>
        {count !== undefined && <span className="box-count">{count}</span>}
        {explain && <Explain text={explain} />}
        {actions && <div className="box-actions">{actions}</div>}
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

export function Skeleton({ rows = 3 }: { rows?: number }) {
  return (
    <div className="placeholder-glow" aria-label="Loading" role="status">
      {Array.from({ length: rows }, (_, i) => (
        <span key={i} className="placeholder skeleton-line" style={{ width: `${92 - i * 13}%` }} />
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

export function CopyButton({ text, label = 'Copy', className = 'btn btn-sm btn-outline-secondary' }: { text: string; label?: string; className?: string }) {
  const [state, setState] = useState<'idle' | 'ok' | 'fail'>('idle')
  return (
    <button
      type="button"
      className={className}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(
          () => setState('ok'),
          () => setState('fail'),
        )
        window.setTimeout(() => setState('idle'), 2000)
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

/** A simple agent mark: initials on a hue derived from the name, so the same agent reads the same everywhere. */
export function Avatar({ name, size = 32 }: { name: string; size?: number }) {
  let h = 0
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) % 360
  const initials = name.replace(/[^a-z0-9]+/gi, ' ').trim().split(' ').map((w) => w[0]).join('').slice(0, 2).toUpperCase() || '?'
  return (
    <span className="avatar" style={{ width: size, height: size, ['--h' as string]: String(h) }} aria-hidden="true">
      {initials}
    </span>
  )
}
