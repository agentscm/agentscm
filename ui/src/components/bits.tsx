import { ApiOutlined, DeploymentUnitOutlined, ExperimentOutlined } from '@ant-design/icons'
import { Avatar, Card, Tag, Tooltip } from 'antd'
import type { CSSProperties, ReactNode } from 'react'
import { useNow } from '../useHub.ts'

/** Human text only: callers pass teammate-supplied strings as React children, never as HTML. */
export const initials = (s: string): string => {
  const parts = s.split(/[\s._-]+/).filter(Boolean)
  const two = parts.length > 1 ? (parts[0]![0]! + parts[1]![0]!) : s.slice(0, 2)
  return (two || '?').toUpperCase()
}

const hue = (s: string): number => {
  let h = 0
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0
  return h % 360
}

/** Initials avatar with a deterministic soft colour (theme-aware via CSS). */
export function Av({ name, size = 32, title }: { name: string; size?: number; title?: string }) {
  return (
    <Avatar className="av" size={size} style={{ '--h': hue(name), fontSize: size * 0.4 } as CSSProperties} aria-label={title ?? name}>
      {initials(name)}
    </Avatar>
  )
}

const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto', style: 'narrow' })

export function relative(nowMs: number, atSec: number): string {
  const s = Math.max(0, Math.round(nowMs / 1000 - atSec))
  if (s < 5) return 'just now'
  if (s < 60) return rtf.format(-s, 'second')
  if (s < 3600) return rtf.format(-Math.floor(s / 60), 'minute')
  if (s < 86400) return rtf.format(-Math.floor(s / 3600), 'hour')
  return rtf.format(-Math.floor(s / 86400), 'day')
}

export const absolute = (atSec: number): string => new Date(atSec * 1000).toLocaleString()
export const clock = (atSec: number): string => new Date(atSec * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })

export function RelTime({ at }: { at: number }) {
  const now = useNow()
  return (
    <Tooltip title={absolute(at)}>
      <time dateTime={new Date(at * 1000).toISOString()} className="rel">
        {relative(now, at)}
      </time>
    </Tooltip>
  )
}

const VIA_NAMES: Record<string, string> = { claude: 'Claude', codex: 'Codex', cursor: 'Cursor', copilot: 'Copilot', generic: 'CLI' }
export function viaLabel(via: string): string {
  if (via === 'mcp') return 'MCP'
  if (via.startsWith('hook:')) {
    const k = via.slice(5)
    return `Hook · ${VIA_NAMES[k] ?? k}`
  }
  return via
}

export function ViaTag({ via }: { via: string }) {
  return (
    <Tag bordered={false} className="pill via" icon={via === 'mcp' ? <DeploymentUnitOutlined /> : <ApiOutlined />}>
      {viaLabel(via)}
    </Tag>
  )
}

/** Quiet marker on handoff steps and rows. */
export function Experimental() {
  return (
    <Tooltip title="Handoff is experimental: agents may not ask or answer, and the hub only relays the request.">
      <Tag bordered={false} className="pill exp" icon={<ExperimentOutlined />} tabIndex={0}>
        Experimental
      </Tag>
    </Tooltip>
  )
}

export function Chip({ children, tone }: { children: ReactNode; tone?: 'amber' }) {
  return <code className={`chip${tone ? ` chip-${tone}` : ''}`}>{children}</code>
}

/** Up to `max` chips, then a "+N" chip whose tooltip lists the rest. */
export function Chips({ items, max = 4, tone }: { items: string[]; max?: number; tone?: (s: string) => 'amber' | undefined }) {
  const shown = items.slice(0, max)
  const rest = items.slice(max)
  return (
    <span className="chips">
      {shown.map((p) => (
        <Chip key={p} tone={tone?.(p)}>
          {p}
        </Chip>
      ))}
      {rest.length > 0 && (
        <Tooltip title={rest.slice(0, 12).join('\n') + (rest.length > 12 ? `\n+${rest.length - 12} more` : '')} styles={{ body: { whiteSpace: 'pre-line', fontFamily: 'var(--cb-mono)' } }}>
          <code className="chip chip-more" tabIndex={0}>
            +{rest.length}
          </code>
        </Tooltip>
      )}
    </span>
  )
}

export const safeHttps = (u: string | null | undefined): string | null => (typeof u === 'string' && u.startsWith('https://') ? u : null)

export function ExternalLink({ href, children, className }: { href: string; children: ReactNode; className?: string }) {
  return (
    <a href={href} target="_blank" rel="noreferrer noopener" className={className}>
      {children}
    </a>
  )
}

export function Section({ title, count, index, className, children }: { title: string; count?: number | string; index: number; className?: string; children: ReactNode }) {
  return (
    <Card
      className={`cb-card rise ${className ?? ''}`}
      style={{ '--i': index } as CSSProperties}
      variant="borderless"
      title={
        <span className="card-title">
          <h2>{title}</h2>
          {count !== undefined && (
            <Tag bordered={false} className="pill count">
              {count}
            </Tag>
          )}
        </span>
      }
    >
      {children}
    </Card>
  )
}
