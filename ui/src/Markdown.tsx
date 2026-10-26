import type { ReactNode } from 'react'

/** The few markdown forms skill procedures use: headings, lists, paragraphs, fences, `code`, **bold**, *em* and http(s) links.
 * It builds React nodes, never HTML strings, so text from a workflow file cannot inject markup. Nested lists are flattened. */
type Block =
  | { kind: 'h'; level: number; text: string }
  | { kind: 'p'; text: string }
  | { kind: 'pre'; text: string }
  | { kind: 'ol' | 'ul'; start: number; items: string[] }

export function parseBlocks(source: string): Block[] {
  const lines = source.replace(/\r\n?/g, '\n').replace(/^---\n[\s\S]*?\n---\n/, '').split('\n')
  const out: Block[] = []
  let open: Block | null = null
  const close = () => { if (open) out.push(open); open = null }
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!
    if (/^\s*```/.test(line)) {
      close()
      const body: string[] = []
      while (++i < lines.length && !/^\s*```/.test(lines[i]!)) body.push(lines[i]!)
      out.push({ kind: 'pre', text: body.join('\n') })
      continue
    }
    if (!line.trim()) { close(); continue }
    const h = /^(#{1,6})\s+(.*)$/.exec(line)
    if (h) { close(); out.push({ kind: 'h', level: h[1]!.length, text: h[2]! }); continue }
    const li = /^\s*(\d+\.|[-*+])\s+(.*)$/.exec(line)
    if (li) {
      const kind = li[1]!.endsWith('.') ? 'ol' : 'ul'
      if (!open || open.kind !== kind) { close(); open = { kind, start: kind === 'ol' ? parseInt(li[1]!, 10) : 1, items: [] } }
      open.items.push(li[2]!)
      continue
    }
    if (open && (open.kind === 'ol' || open.kind === 'ul')) {
      if (/^\s/.test(line)) { open.items[open.items.length - 1] += ` ${line.trim()}`; continue }
      close()
    }
    if (open && open.kind === 'p') open.text += ` ${line.trim()}`
    else { close(); open = { kind: 'p', text: line.trim() } }
  }
  close()
  return out
}

const INLINE = /`([^`]+)`|\*\*([^*]+)\*\*|\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)|\*([^*\s][^*]*)\*/g

export function inline(text: string): ReactNode[] {
  const out: ReactNode[] = []
  let last = 0
  for (const m of text.matchAll(INLINE)) {
    const at = m.index ?? 0
    if (at > last) out.push(text.slice(last, at))
    const k = out.length
    if (m[1] !== undefined) out.push(<code key={k}>{m[1]}</code>)
    else if (m[2] !== undefined) out.push(<strong key={k}>{m[2]}</strong>)
    else if (m[3] !== undefined) out.push(<a key={k} href={m[4]} target="_blank" rel="noreferrer noopener">{m[3]}</a>)
    else out.push(<em key={k}>{m[5]}</em>)
    last = at + m[0].length
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

export function Markdown({ source, className = '' }: { source: string; className?: string }) {
  return (
    <div className={`md ${className}`}>
      {parseBlocks(source).map((b, i) => {
        if (b.kind === 'h') return <p key={i} className={`md-h md-h${Math.min(b.level, 3)}`} role="heading" aria-level={Math.min(b.level + 2, 6)}>{inline(b.text)}</p>
        if (b.kind === 'pre') return <pre key={i}><code>{b.text}</code></pre>
        if (b.kind === 'p') return <p key={i}>{inline(b.text)}</p>
        const items = b.items.map((t, j) => <li key={j}>{inline(t)}</li>)
        return b.kind === 'ol' ? <ol key={i} start={b.start}>{items}</ol> : <ul key={i}>{items}</ul>
      })}
    </div>
  )
}
