import { explorerRead, type ExplorerEntryKind } from './repoExplorer.ts'

export interface MapFile { path: string; oid: string; kind: ExplorerEntryKind; inspection: 'inspected' | 'unsupported' | 'too_large' | 'not_inspected' | 'unavailable' | 'binary' }
export interface MapRelation { from: number; to: number; line: number; kind: 'import_candidate' | 'module_candidate' | 'use_candidate' }
export interface MapSnapshot {
  commit: string; tree: string; files: MapFile[]; relations: MapRelation[]
  unresolved: { file: number; line: number; reference: string; reason: 'incomplete_tree' | 'unsupported_syntax' | 'ambiguous' | 'outside_view' | 'external_or_alias' | 'unresolved' }[]
  coverage: { discovered_files: number; tree_complete: boolean; omitted_files: number; inspected_files: number; omitted_relations: number; omitted_unresolved: number }
}
export interface RepositoryMap { version: 1; repository_id: string; scope: string; extractor: 'static-reference-candidates-v1'; head: MapSnapshot; base: MapSnapshot | null }
const fail = (): never => { throw new Error('The repository map is malformed or belongs to another source selection.') }
const object = (v: unknown): Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v) ? v as Record<string, unknown> : fail()
const text = (v: unknown, max = 4096): string => typeof v === 'string' && new TextEncoder().encode(v).byteLength <= max ? v : fail()
const oid = (v: unknown): string => /^[a-f0-9]{40}([a-f0-9]{24})?$/.test(text(v, 64)) ? v as string : fail()
const count = (v: unknown): number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0 ? v : fail()
const flag = (v: unknown): boolean => typeof v === 'boolean' ? v : fail()
const one = <T extends string>(v: unknown, choices: readonly T[]): T => choices.includes(v as T) ? v as T : fail()
const list = <T>(v: unknown, max: number, read: (v: unknown) => T): T[] => Array.isArray(v) && v.length <= max ? v.map(read) : fail()
const path = (v: unknown): string => { const s = text(v); return s && !s.includes('\\') && !s.includes('\0') && !s.split('/').some(p => p === '' || p === '.' || p === '..') ? s : fail() }
function snapshot(v: unknown): MapSnapshot {
  const s = object(v), c = object(s.coverage)
  const files = list(s.files, 384, v => { const f = object(v); return { path: path(f.path), oid: oid(f.oid), kind: one(f.kind, ['file', 'executable', 'symlink', 'gitlink'] as const), inspection: one(f.inspection, ['inspected', 'unsupported', 'too_large', 'not_inspected', 'unavailable', 'binary'] as const) } })
  if (new Set(files.map(f => f.path)).size !== files.length) fail()
  const index = (v: unknown) => { const i = count(v); return i < files.length ? i : fail() }
  const line = (v: unknown) => { const n = count(v); return n > 0 && n <= 65536 ? n : fail() }
  const relations = list(s.relations, 1024, v => { const e = object(v); return { from: index(e.from), to: index(e.to), line: line(e.line), kind: one(e.kind, ['import_candidate', 'module_candidate', 'use_candidate'] as const) } })
  if (relations.some(e => files[e.from]?.inspection !== 'inspected')) fail()
  const result: MapSnapshot = { commit: oid(s.commit), tree: oid(s.tree), files, relations,
    unresolved: list(s.unresolved, 128, v => { const e = object(v); return { file: index(e.file), line: line(e.line), reference: text(e.reference, 1024), reason: one(e.reason, ['incomplete_tree', 'unsupported_syntax', 'ambiguous', 'outside_view', 'external_or_alias', 'unresolved'] as const) } }),
    coverage: { discovered_files: count(c.discovered_files), tree_complete: flag(c.tree_complete), omitted_files: count(c.omitted_files), inspected_files: count(c.inspected_files), omitted_relations: count(c.omitted_relations), omitted_unresolved: count(c.omitted_unresolved) },
  }
  if (result.coverage.discovered_files !== files.length + result.coverage.omitted_files || result.coverage.inspected_files > files.length) fail()
  return result
}
export function decodeRepositoryMap(v: unknown): RepositoryMap {
  const m = object(v)
  if (m.version !== 1 || m.extractor !== 'static-reference-candidates-v1') fail()
  const scope = m.scope === '' ? '' : path(m.scope)
  const result: RepositoryMap = { version: 1, repository_id: text(m.repository_id, 256), scope, extractor: 'static-reference-candidates-v1', head: snapshot(m.head), base: m.base === null ? null : snapshot(m.base) }
  if ([result.head, ...(result.base ? [result.base] : [])].some(s => s.files.some(f => scope && f.path !== scope && !f.path.startsWith(`${scope}/`)))) fail()
  return result
}
export async function fetchRepositoryMap(repository: string, commit: string, base: string | undefined, scope: string, signal: AbortSignal): Promise<RepositoryMap> {
  const m = await explorerRead('map', { commit, base, path: scope || undefined }, decodeRepositoryMap, signal)
  if (m.repository_id !== repository || m.head.commit !== commit || (m.base?.commit ?? undefined) !== base || m.scope !== scope) fail()
  return m
}
export function component(path: string, scope: string): string {
  const relative = scope && path.startsWith(`${scope}/`) ? path.slice(scope.length + 1) : path
  const part = relative.split('/')[0]!
  return relative.includes('/') ? (scope ? `${scope}/${part}` : part) : scope || '(root files)'
}
export function mapChanges(base: MapSnapshot, head: MapSnapshot) {
  const before = new Map(base.files.map(f => [f.path, f]))
  const after = new Map(head.files.map(f => [f.path, f]))
  const complete = base.coverage.tree_complete && head.coverage.tree_complete && base.coverage.omitted_files === 0 && head.coverage.omitted_files === 0
  return [...new Set([...before.keys(), ...after.keys()])].sort().flatMap(path => {
    const a = before.get(path), b = after.get(path)
    if (a && b) return a.oid === b.oid && a.kind === b.kind ? [] : [{ path, state: 'Changed' }]
    return [{ path, state: complete ? a ? 'Removed' : 'Added' : 'Outside the other snapshot’s coverage' }]
  })
}
export function relationChanges(base: MapSnapshot, head: MapSnapshot) {
  const key = (s: MapSnapshot, e: MapRelation) => `${s.files[e.from]!.path}\0${s.files[e.to]!.path}\0${e.kind}`
  const before = new Map(base.relations.map(e => [key(base, e), e]))
  const after = new Map(head.relations.map(e => [key(head, e), e]))
  const covered = (s: MapSnapshot, path: string) => s.coverage.tree_complete && s.coverage.omitted_files === 0 && s.coverage.omitted_relations === 0 && s.coverage.omitted_unresolved === 0 && s.files.find(f => f.path === path)?.inspection === 'inspected' && !s.unresolved.some(e => s.files[e.file]?.path === path)
  return [...new Set([...before.keys(), ...after.keys()])].sort().flatMap(k => {
    if (before.has(k) && after.has(k)) return []
    const [from, to] = k.split('\0') as [string, string]
    return [{ from, to, state: covered(base, from) && covered(head, from) ? after.has(k) ? 'Newly observed' : 'No longer observed' : 'Comparison incomplete' }]
  })
}
