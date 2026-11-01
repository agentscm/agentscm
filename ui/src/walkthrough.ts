import { decodeExplorerEvidence, explorerRead, type ExplorerEvidencePage } from './repoExplorer.ts'

export interface WalkthroughAnchor { id: string; side: 'base' | 'head'; path: string; start_line: number; end_line: number }
export interface WalkthroughStep { id: string; title: string; body: string; anchors: WalkthroughAnchor[] }
export interface WalkthroughVersion { id: string; change_id: string; parent: string | null; title: string; base: string; head: string; is_current: boolean }
export interface WalkthroughList { version: 1; repository: string; walkthroughs: WalkthroughVersion[]; complete: boolean; omitted: number }
export interface WalkthroughDetail {
  version: 1; id: string
  record: { version: 1; repository: string; change_id: string; parent: string | null; base: string; base_tree: string; head: string; head_tree: string
    draft: { title: string; author: string; purpose: string; steps: WalkthroughStep[]; relationships: { from: string; to: string; label: string }[]; decisions: { id: string; text: string }[]; open_questions: string[] }
    changes: { old_path: string | null; path: string | null; kind: string }[]; mapped_paths: string[]; unassigned_paths: string[]; omissions: string[]; interpretation: 'authored_assertions' }
  current_head: string | null; applicability: 'same_source' | 'different_source' | 'unknown'; evidence: { state: 'available' | 'not_connected' | 'unavailable'; page: ExplorerEvidencePage | null }
}
export interface WalkthroughSource { version: 1; walkthrough_id: string; anchor: WalkthroughAnchor; commit: string; oid: string; content: string; interpretation: 'exact_source_lines' }
const fail = (): never => { throw new Error('The local walkthrough response is malformed or does not match this selection.') }
const object = (v: unknown): Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v) ? v as Record<string, unknown> : fail()
const str = (v: unknown, max = 4096): string => typeof v === 'string' && new TextEncoder().encode(v).byteLength <= max ? v : fail()
const oid = (v: unknown): string => /^[a-f0-9]{40}([a-f0-9]{24})?$/.test(str(v, 64)) ? v as string : fail()
const identifier = (v: unknown): string => /^[A-Za-z0-9_.-]{1,128}$/.test(str(v, 128)) ? v as string : fail()
const digest = (v: unknown): string => /^[a-f0-9]{64}$/.test(str(v, 64)) ? v as string : fail()
const nullable = (v: unknown, decode = str): string | null => v === null ? null : decode(v)
const num = (v: unknown): number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0 ? v : fail()
const bool = (v: unknown): boolean => typeof v === 'boolean' ? v : fail()
const array = <T>(v: unknown, decode: (v: unknown) => T, max = 2000): T[] => Array.isArray(v) && v.length <= max ? v.map(decode) : fail()
const one = <T extends string>(v: unknown, options: T[]): T => options.includes(v as T) ? v as T : fail()
const path = (v: unknown): string => { const s = str(v, 4096); return s && !s.startsWith('/') && !s.split('/').some(p => p === '..' || p === '.' || p === '') ? s : fail() }
function anchor(v: unknown): WalkthroughAnchor {
  const a = object(v)
  const result = { id: identifier(a.id), side: one(a.side, ['base', 'head']), path: path(a.path), start_line: num(a.start_line), end_line: num(a.end_line) }
  if (result.start_line < 1 || result.end_line < result.start_line || result.end_line - result.start_line >= 200) fail()
  return result
}
function version(v: unknown): WalkthroughVersion {
  const a = object(v)
  return { id: digest(a.id), change_id: identifier(a.change_id), parent: nullable(a.parent, digest), title: str(a.title, 256), base: oid(a.base), head: oid(a.head), is_current: bool(a.is_current) }
}
export function decodeWalkthroughList(v: unknown): WalkthroughList {
  const a = object(v)
  if (a.version !== 1) fail()
  return { version: 1, repository: str(a.repository, 512), walkthroughs: array(a.walkthroughs, version, 100), complete: bool(a.complete), omitted: num(a.omitted) }
}
export function decodeWalkthrough(v: unknown): WalkthroughDetail {
  const a = object(v), r = object(a.record), d = object(r.draft), e = object(a.evidence)
  if (a.version !== 1 || r.version !== 1 || r.interpretation !== 'authored_assertions') fail()
  const steps = array(d.steps, v => { const s = object(v); return { id: identifier(s.id), title: str(s.title, 256), body: str(s.body), anchors: array(s.anchors, anchor, 128) } }, 32)
  const relationships = array(d.relationships, v => { const r = object(v); return { from: identifier(r.from), to: identifier(r.to), label: str(r.label, 256) } }, 64)
  const stepIds = new Set(steps.map(s => s.id)), anchors = steps.flatMap(s => s.anchors)
  if (anchors.length > 128 || stepIds.size !== steps.length || new Set(anchors.map(a => a.id)).size !== anchors.length || relationships.some(r => !stepIds.has(r.from) || !stepIds.has(r.to))) fail()
  const result: WalkthroughDetail = { version: 1, id: digest(a.id), record: { version: 1, repository: str(r.repository, 512), change_id: identifier(r.change_id), parent: nullable(r.parent, digest), base: oid(r.base), base_tree: oid(r.base_tree), head: oid(r.head), head_tree: oid(r.head_tree), draft: { title: str(d.title, 256), author: str(d.author, 256), purpose: str(d.purpose), steps, relationships, decisions: array(d.decisions, v => { const d = object(v); return { id: identifier(d.id), text: str(d.text) } }, 32), open_questions: array(d.open_questions, v => str(v), 32) }, changes: array(r.changes, v => { const c = object(v); return { old_path: nullable(c.old_path, path), path: nullable(c.path, path), kind: str(c.kind, 64) } }, 1000), mapped_paths: array(r.mapped_paths, path), unassigned_paths: array(r.unassigned_paths, path), omissions: array(r.omissions, v => str(v)), interpretation: 'authored_assertions' }, current_head: nullable(a.current_head, oid), applicability: one(a.applicability, ['same_source', 'different_source', 'unknown']), evidence: { state: one(e.state, ['available', 'not_connected', 'unavailable']), page: e.page === null ? null : decodeExplorerEvidence(e.page) } }
  if ((result.evidence.state === 'available') !== (result.evidence.page !== null)) fail()
  if (result.evidence.page && (result.evidence.page.repository !== result.record.repository || result.evidence.page.commit !== result.record.head)) fail()
  return result
}
export function decodeWalkthroughSource(v: unknown): WalkthroughSource {
  const a = object(v)
  if (a.version !== 1 || a.interpretation !== 'exact_source_lines') fail()
  return { version: 1, walkthrough_id: digest(a.walkthrough_id), anchor: anchor(a.anchor), commit: oid(a.commit), oid: oid(a.oid), content: str(a.content, 64 * 1024), interpretation: 'exact_source_lines' }
}
export async function fetchWalkthroughs(repository: string, signal: AbortSignal) {
  const v = await explorerRead('walkthroughs', { limit: 100 }, decodeWalkthroughList, signal)
  if (v.repository !== repository) fail()
  return v
}
export async function fetchWalkthrough(repository: string, id: string, currentHead: string, signal: AbortSignal) {
  const v = await explorerRead('walkthrough', { id, current_head: currentHead }, decodeWalkthrough, signal)
  if (v.record.repository !== repository || v.id !== id || v.current_head !== currentHead || (v.applicability === 'same_source') !== (v.record.head === currentHead)) fail()
  return v
}
export async function fetchWalkthroughSource(detail: WalkthroughDetail, expected: WalkthroughAnchor, signal: AbortSignal) {
  const v = await explorerRead('walkthrough-source', { id: detail.id, anchor: expected.id }, decodeWalkthroughSource, signal)
  if (v.walkthrough_id !== detail.id || v.commit !== detail.record[expected.side] || JSON.stringify(v.anchor) !== JSON.stringify(expected)) fail()
  return v
}
export async function fetchWalkthroughContext(detail: WalkthroughDetail, currentHead: string, signal: AbortSignal): Promise<string> {
  return explorerRead('walkthrough-context', { id: detail.id, current_head: currentHead }, value => {
    const v = object(value)
    if (v.version !== 1 || v.walkthrough_id !== detail.id || v.repository !== detail.record.repository || v.base !== detail.record.base || v.head !== detail.record.head || v.current_head !== currentHead || v.applicability !== detail.applicability) fail()
    const text = JSON.stringify(v)
    if (new TextEncoder().encode(text).byteLength > 32768) fail()
    return text
  }, signal)
}
