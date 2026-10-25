import { loadLocalBase, loadLocalToken, localCredential, validLocalBase } from './localApi.ts'
import { withTimeout } from './timeout.ts'

const MAX_RESPONSE_BYTES = 4 * 1024 * 1024
const MAX_FILE_TEXT_BYTES = 1024 * 1024
const MAX_PATCH_TEXT_BYTES = 2 * 1024 * 1024
const MAX_CONVERSATION_TEXT_BYTES = 1024 * 1024
const encoder = new TextEncoder()

export type ExplorerLimitState = 'complete' | 'more_results' | 'output_limited'
export type ExplorerEntryKind = 'directory' | 'file' | 'executable' | 'symlink' | 'gitlink'
export type ExplorerFileState = 'text' | 'binary' | 'too_large' | 'unavailable' | 'unsupported'
export type ExplorerDiffKind = 'added' | 'deleted' | 'modified' | 'renamed' | 'copied' | 'type_changed' | 'unmerged' | 'unknown'
export type ExplorerPatchState = 'not_requested' | ExplorerFileState

export interface ExplorerSourceIdentity {
  version: 1
  repository_id: string
  commit: string
  tree: string
}

export interface ExplorerSummary {
  version: 1
  repository_id: string
  repository_label: string
  selected_ref: string
  commit: string
  tree: string
  observed_head: string | null
  head_ref: string | null
  dirty: true | null
  observed_at: number
  refs: { name: string; commit: string; kind: 'local_branch' | 'tag' | 'remote_tracking' }[]
  refs_omitted: boolean
  shallow: boolean
}

export interface ExplorerTreePage extends ExplorerSourceIdentity {
  path: string
  entries: { path: string; name: string; kind: ExplorerEntryKind; oid: string; size: number | null }[]
  next: string | null
  complete: boolean
  limit_state: ExplorerLimitState
}

export interface ExplorerFile extends ExplorerSourceIdentity {
  path: string
  kind: ExplorerEntryKind
  oid: string
  size: number | null
  state: ExplorerFileState
  content: string | null
}

export interface ExplorerHistoryPage extends ExplorerSourceIdentity {
  path: string | null
  path_interpretation: 'exact'
  entries: { oid: string; parents: string[]; author_name: string; author_time: number; subject: string }[]
  next: string | null
  complete: boolean
  limit_state: ExplorerLimitState
  shallow: boolean
}

export interface ExplorerDiffPage {
  version: 1
  repository_id: string
  left: string
  left_tree: string
  right: string
  right_tree: string
  path: string | null
  changes: {
    old_path: string | null
    path: string | null
    kind: ExplorerDiffKind
    similarity: number | null
    old_oid: string | null
    oid: string | null
    old_mode: string | null
    mode: string | null
  }[]
  patch: { state: ExplorerPatchState; content: string | null; truncated: boolean }
  next: string | null
  complete: boolean
  limit_state: ExplorerLimitState
}

export interface ExplorerSearchPage extends ExplorerSourceIdentity {
  query: string
  matches: { path: string; line: number; text: string }[]
  next: string | null
  complete: boolean
  limit_state: ExplorerLimitState
}

export type ExplorerCoverage = 'complete' | 'partial' | 'unknown'
export type ExplorerRetention = 'pending' | 'acknowledged' | 'compacted'
export type ExplorerApplicability = 'matching_recorded_clean_source' | 'different_recorded_source' | 'unknown'
export type ExplorerBodyAvailability = 'available' | 'not_captured' | 'not_applicable' | 'missing' | 'expired' | 'deleted' | 'denied' | 'too_large' | 'integrity_failure' | 'invalid_body'

export type ExplorerSourceBinding =
  | { kind: 'native'; repository: string; revision: string; change_id: string; context_digest: string; source_generation: number }
  | {
      kind: 'git'
      repository: string
      worktree_id: string
      head: string
      tree: string
      base: string
      dirty: boolean
      untracked_count: number
      omissions: ('untracked_content' | 'ignored_content' | 'working_tree_content' | 'index_state' | 'submodule_content' | 'symlink_target')[]
      captured_snapshot: {
        digest: string
        size_bytes: number
        media: 'json' | 'text' | 'json_lines' | 'binary'
        retention: 'ephemeral_local' | 'work_local' | 'shared_approved'
        availability: 'local' | 'shared' | 'external' | 'missing' | 'expired' | 'deleted' | 'denied' | 'integrity_failure' | 'mixed'
      } | null
      snapshot_coverage: ExplorerCoverage
    }

export interface ExplorerActionEvidence {
  entry: 'action'
  relation: 'exact_source_binding'
  path_relation: 'not_requested' | 'not_recorded'
  work_id: string
  generation: number
  session_id: string
  action_id: string
  operation_id: string
  action_kind: 'execution' | 'verification'
  recipe_provenance: 'observed_native_input' | 'configured_exact_match'
  recipe_digest: string
  setup_digest: string
  started_at: number
  source: ExplorerSourceBinding
  applicability: ExplorerApplicability
  start_retention: ExplorerRetention
  result: {
    operation_id: string
    outcome: 'succeeded' | 'failed' | 'incomplete' | 'unknown'
    result: 'passed' | 'failed' | 'incomplete' | 'unknown'
    assurance: 'caller_asserted' | 'observed_trusted_local' | 'protected_execution' | 'independent_verification'
    coverage: ExplorerCoverage
    drift: 'none_observed' | 'source_changed' | 'setup_changed' | 'recipe_changed' | 'unknown'
    finished_at: number
    retention: ExplorerRetention
  } | null
}

export interface ExplorerSessionEvidence {
  entry: 'session'
  relation: 'same_work_session'
  work_id: string
  generation: number
  session_id: string
  harness: 'claude_code' | 'codex'
  capture_mode: 'metadata_only' | 'local_conversation'
  first_observed_at: number
  last_observed_at: number
  event_count: number
  unavailable_body_count: number
  capture_gap_count: number
}

export interface ExplorerProjectionCoverage {
  retained_envelopes: number
  compacted_event_batches_unattributed: number
  unavailable_bodies: number
  unverified_bodies: number
  capture_gaps: number
  complete: boolean
}

export interface ExplorerEvidencePage {
  version: 1
  repository: string
  commit: string
  path: string | null
  requested_work_id: string | null
  entries: (ExplorerActionEvidence | ExplorerSessionEvidence)[]
  projection_coverage: ExplorerProjectionCoverage
  next_cursor: string | null
  complete: boolean
}

export interface ExplorerConversationEvent {
  work_id: string
  generation: number
  session_id: string
  event_id: string
  sequence: number
  observed_at: number
  kind: 'session_started' | 'user_prompt' | 'assistant_message' | 'tool_started' | 'tool_finished' | 'check_started' | 'check_finished' | 'session_ended' | 'gap'
  tool: { kind: 'read' | 'edit' | 'write' | 'search' | 'shell' | 'test' | 'model' | 'other'; request_id: string | null } | null
  relation: 'same_work_session'
  body: { availability: ExplorerBodyAvailability; media: 'json' | 'text' | 'json_lines' | 'binary' | null; content: string | null }
  gap: { first_missing: number; last_missing: number; reason: 'provider_history_unavailable' | 'adapter_started_late' | 'identity_uncertain' | 'sequence_missing' | 'local_retention_expired' | 'redacted' | 'capture_interrupted' } | null
  retention: ExplorerRetention
}

export interface ExplorerConversationPage {
  version: 1
  repository: string
  work_id: string
  session_id: string
  relation: 'same_work_session'
  events: ExplorerConversationEvent[]
  projection_coverage: ExplorerProjectionCoverage
  next_cursor: string | null
  complete: boolean
}

export class ExplorerResponseError extends Error {
  status?: number
  constructor(message: string, status?: number) {
    super(message)
    this.name = 'ExplorerResponseError'
    if (status !== undefined) this.status = status
  }
}

const fail = (path: string): never => {
  throw new ExplorerResponseError(`The local explorer returned malformed data at ${path}.`)
}

function object(value: unknown, path: string, keys: readonly string[]): Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) fail(path)
  const result = value as Record<string, unknown>
  const actual = Object.keys(result).sort()
  const expected = [...keys].sort()
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) fail(path)
  return result
}

function array(value: unknown, path: string, max: number): unknown[] {
  if (!Array.isArray(value) || value.length > max) return fail(path)
  return value
}

function string(value: unknown, path: string, maxBytes = 4096): string {
  if (typeof value !== 'string' || encoder.encode(value).byteLength > maxBytes || value.includes('\0')) return fail(path)
  return value
}

function jsonString(value: unknown, path: string, maxRawBytes: number, maxJsonBytes: number): string {
  const result = string(value, path, maxRawBytes)
  if (encoder.encode(JSON.stringify(result)).byteLength > maxJsonBytes) fail(path)
  return result
}

function nonempty(value: unknown, path: string, maxBytes = 4096): string {
  const result = string(value, path, maxBytes)
  if (!result) fail(path)
  return result
}

function nullableString(value: unknown, path: string, maxBytes = 4096): string | null {
  return value === null ? null : string(value, path, maxBytes)
}

function bool(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') return fail(path)
  return value
}

function integer(value: unknown, path: string, min = 0, max = Number.MAX_SAFE_INTEGER): number {
  if (!Number.isSafeInteger(value) || (value as number) < min || (value as number) > max) fail(path)
  return value as number
}

function enumValue<T extends string>(value: unknown, values: readonly T[], path: string): T {
  if (typeof value !== 'string' || !values.includes(value as T)) fail(path)
  return value as T
}

function oid(value: unknown, path: string): string {
  const result = string(value, path, 64)
  if (!/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(result)) fail(path)
  return result
}

function nullableOid(value: unknown, path: string): string | null {
  return value === null ? null : oid(value, path)
}

function nullableCursor(value: unknown, path: string): string | null {
  if (value === null) return null
  return nonempty(value, path, 2 * 1024)
}

function versionAndRepository(value: Record<string, unknown>, path: string): { version: 1; repository_id: string } {
  if (value['version'] !== 1) fail(`${path}.version`)
  return { version: 1, repository_id: nonempty(value['repository_id'], `${path}.repository_id`, 256) }
}

function sourceIdentity(value: Record<string, unknown>, path: string): ExplorerSourceIdentity {
  return { ...versionAndRepository(value, path), commit: oid(value['commit'], `${path}.commit`), tree: oid(value['tree'], `${path}.tree`) }
}

const ENTRY_KINDS = ['directory', 'file', 'executable', 'symlink', 'gitlink'] as const
const LIMIT_STATES = ['complete', 'more_results', 'output_limited'] as const
const FILE_STATES = ['text', 'binary', 'too_large', 'unavailable', 'unsupported'] as const

export function decodeExplorerSummary(value: unknown): ExplorerSummary {
  const path = 'summary'
  const raw = object(value, path, ['version', 'repository_id', 'repository_label', 'selected_ref', 'commit', 'tree', 'observed_head', 'head_ref', 'dirty', 'observed_at', 'refs', 'refs_omitted', 'shallow'])
  const identity = sourceIdentity(raw, path)
  return {
    ...identity,
    repository_label: nonempty(raw['repository_label'], `${path}.repository_label`, 255),
    selected_ref: nonempty(raw['selected_ref'], `${path}.selected_ref`, 1024),
    observed_head: nullableOid(raw['observed_head'], `${path}.observed_head`),
    head_ref: nullableString(raw['head_ref'], `${path}.head_ref`, 1024),
    dirty: raw['dirty'] === null ? null : raw['dirty'] === true ? true : fail(`${path}.dirty`),
    observed_at: integer(raw['observed_at'], `${path}.observed_at`),
    refs: array(raw['refs'], `${path}.refs`, 1024).map((item, index) => {
      const refPath = `${path}.refs[${index}]`
      const ref = object(item, refPath, ['name', 'commit', 'kind'])
      return {
        name: nonempty(ref['name'], `${refPath}.name`, 1024),
        commit: oid(ref['commit'], `${refPath}.commit`),
        kind: enumValue(ref['kind'], ['local_branch', 'tag', 'remote_tracking'] as const, `${refPath}.kind`),
      }
    }),
    refs_omitted: bool(raw['refs_omitted'], `${path}.refs_omitted`),
    shallow: bool(raw['shallow'], `${path}.shallow`),
  }
}

export function decodeExplorerTree(value: unknown): ExplorerTreePage {
  const path = 'tree'
  const raw = object(value, path, ['version', 'repository_id', 'commit', 'tree', 'path', 'entries', 'next', 'complete', 'limit_state'])
  return {
    ...sourceIdentity(raw, path),
    path: string(raw['path'], `${path}.path`),
    entries: array(raw['entries'], `${path}.entries`, 200).map((item, index) => {
      const entryPath = `${path}.entries[${index}]`
      const entry = object(item, entryPath, ['path', 'name', 'kind', 'oid', 'size'])
      return {
        path: nonempty(entry['path'], `${entryPath}.path`),
        name: nonempty(entry['name'], `${entryPath}.name`, 1024),
        kind: enumValue(entry['kind'], ENTRY_KINDS, `${entryPath}.kind`),
        oid: oid(entry['oid'], `${entryPath}.oid`),
        size: entry['size'] === null ? null : integer(entry['size'], `${entryPath}.size`),
      }
    }),
    next: nullableCursor(raw['next'], `${path}.next`),
    complete: bool(raw['complete'], `${path}.complete`),
    limit_state: enumValue(raw['limit_state'], LIMIT_STATES, `${path}.limit_state`),
  }
}

export function decodeExplorerFile(value: unknown): ExplorerFile {
  const path = 'file'
  const raw = object(value, path, ['version', 'repository_id', 'commit', 'tree', 'path', 'kind', 'oid', 'size', 'state', 'content'])
  const state = enumValue(raw['state'], FILE_STATES, `${path}.state`)
  const content = raw['content'] === null ? null : jsonString(raw['content'], `${path}.content`, MAX_FILE_TEXT_BYTES, MAX_PATCH_TEXT_BYTES)
  if ((state === 'text') !== (content !== null)) fail(`${path}.content`)
  return {
    ...sourceIdentity(raw, path),
    path: nonempty(raw['path'], `${path}.path`),
    kind: enumValue(raw['kind'], ENTRY_KINDS, `${path}.kind`),
    oid: oid(raw['oid'], `${path}.oid`),
    size: raw['size'] === null ? null : integer(raw['size'], `${path}.size`),
    state,
    content,
  }
}

export function decodeExplorerHistory(value: unknown): ExplorerHistoryPage {
  const path = 'history'
  const raw = object(value, path, ['version', 'repository_id', 'commit', 'tree', 'path', 'path_interpretation', 'entries', 'next', 'complete', 'limit_state', 'shallow'])
  if (raw['path_interpretation'] !== 'exact') fail(`${path}.path_interpretation`)
  return {
    ...sourceIdentity(raw, path),
    path: nullableString(raw['path'], `${path}.path`),
    path_interpretation: 'exact',
    entries: array(raw['entries'], `${path}.entries`, 100).map((item, index) => {
      const entryPath = `${path}.entries[${index}]`
      const entry = object(item, entryPath, ['oid', 'parents', 'author_name', 'author_time', 'subject'])
      return {
        oid: oid(entry['oid'], `${entryPath}.oid`),
        parents: array(entry['parents'], `${entryPath}.parents`, 256).map((parent, parentIndex) => oid(parent, `${entryPath}.parents[${parentIndex}]`)),
        author_name: jsonString(entry['author_name'], `${entryPath}.author_name`, 1024, 1024),
        author_time: integer(entry['author_time'], `${entryPath}.author_time`),
        subject: jsonString(entry['subject'], `${entryPath}.subject`, 8 * 1024, 8 * 1024),
      }
    }),
    next: nullableCursor(raw['next'], `${path}.next`),
    complete: bool(raw['complete'], `${path}.complete`),
    limit_state: enumValue(raw['limit_state'], LIMIT_STATES, `${path}.limit_state`),
    shallow: bool(raw['shallow'], `${path}.shallow`),
  }
}

export function decodeExplorerDiff(value: unknown): ExplorerDiffPage {
  const path = 'diff'
  const raw = object(value, path, ['version', 'repository_id', 'left', 'left_tree', 'right', 'right_tree', 'path', 'changes', 'patch', 'next', 'complete', 'limit_state'])
  const identity = versionAndRepository(raw, path)
  const patch = object(raw['patch'], `${path}.patch`, ['state', 'content', 'truncated'])
  const patchState = enumValue(patch['state'], ['not_requested', ...FILE_STATES] as const, `${path}.patch.state`)
  const patchContent = patch['content'] === null ? null : jsonString(patch['content'], `${path}.patch.content`, MAX_PATCH_TEXT_BYTES, MAX_PATCH_TEXT_BYTES)
  if ((patchState === 'text') !== (patchContent !== null)) fail(`${path}.patch.content`)
  return {
    ...identity,
    left: oid(raw['left'], `${path}.left`),
    left_tree: oid(raw['left_tree'], `${path}.left_tree`),
    right: oid(raw['right'], `${path}.right`),
    right_tree: oid(raw['right_tree'], `${path}.right_tree`),
    path: nullableString(raw['path'], `${path}.path`),
    changes: array(raw['changes'], `${path}.changes`, 200).map((item, index) => {
      const changePath = `${path}.changes[${index}]`
      const change = object(item, changePath, ['old_path', 'path', 'kind', 'similarity', 'old_oid', 'oid', 'old_mode', 'mode'])
      return {
        old_path: nullableString(change['old_path'], `${changePath}.old_path`),
        path: nullableString(change['path'], `${changePath}.path`),
        kind: enumValue(change['kind'], ['added', 'deleted', 'modified', 'renamed', 'copied', 'type_changed', 'unmerged', 'unknown'] as const, `${changePath}.kind`),
        similarity: change['similarity'] === null ? null : integer(change['similarity'], `${changePath}.similarity`, 0, 100),
        old_oid: nullableOid(change['old_oid'], `${changePath}.old_oid`),
        oid: nullableOid(change['oid'], `${changePath}.oid`),
        old_mode: nullableString(change['old_mode'], `${changePath}.old_mode`, 16),
        mode: nullableString(change['mode'], `${changePath}.mode`, 16),
      }
    }),
    patch: { state: patchState, content: patchContent, truncated: bool(patch['truncated'], `${path}.patch.truncated`) },
    next: nullableCursor(raw['next'], `${path}.next`),
    complete: bool(raw['complete'], `${path}.complete`),
    limit_state: enumValue(raw['limit_state'], LIMIT_STATES, `${path}.limit_state`),
  }
}

export function decodeExplorerSearch(value: unknown): ExplorerSearchPage {
  const path = 'search'
  const raw = object(value, path, ['version', 'repository_id', 'commit', 'tree', 'query', 'matches', 'next', 'complete', 'limit_state'])
  return {
    ...sourceIdentity(raw, path),
    query: string(raw['query'], `${path}.query`, 1024),
    matches: array(raw['matches'], `${path}.matches`, 100).map((item, index) => {
      const matchPath = `${path}.matches[${index}]`
      const match = object(item, matchPath, ['path', 'line', 'text'])
      return {
        path: nonempty(match['path'], `${matchPath}.path`),
        line: integer(match['line'], `${matchPath}.line`, 1),
        text: jsonString(match['text'], `${matchPath}.text`, 16 * 1024, 16 * 1024),
      }
    }),
    next: nullableCursor(raw['next'], `${path}.next`),
    complete: bool(raw['complete'], `${path}.complete`),
    limit_state: enumValue(raw['limit_state'], LIMIT_STATES, `${path}.limit_state`),
  }
}

const COVERAGE = ['complete', 'partial', 'unknown'] as const
const RETENTION = ['pending', 'acknowledged', 'compacted'] as const

function decodeProjectionCoverage(value: unknown, path: string): ExplorerProjectionCoverage {
  const raw = object(value, path, ['retained_envelopes', 'compacted_event_batches_unattributed', 'unavailable_bodies', 'unverified_bodies', 'capture_gaps', 'complete'])
  return {
    retained_envelopes: integer(raw['retained_envelopes'], `${path}.retained_envelopes`),
    compacted_event_batches_unattributed: integer(raw['compacted_event_batches_unattributed'], `${path}.compacted_event_batches_unattributed`),
    unavailable_bodies: integer(raw['unavailable_bodies'], `${path}.unavailable_bodies`),
    unverified_bodies: integer(raw['unverified_bodies'], `${path}.unverified_bodies`),
    capture_gaps: integer(raw['capture_gaps'], `${path}.capture_gaps`),
    complete: bool(raw['complete'], `${path}.complete`),
  }
}

function decodeArtifact(value: unknown, path: string): NonNullable<Extract<ExplorerSourceBinding, { kind: 'git' }>['captured_snapshot']> {
  const raw = object(value, path, ['digest', 'size_bytes', 'media', 'retention', 'availability'])
  return {
    digest: nonempty(raw['digest'], `${path}.digest`, 128),
    size_bytes: integer(raw['size_bytes'], `${path}.size_bytes`),
    media: enumValue(raw['media'], ['json', 'text', 'json_lines', 'binary'] as const, `${path}.media`),
    retention: enumValue(raw['retention'], ['ephemeral_local', 'work_local', 'shared_approved'] as const, `${path}.retention`),
    availability: enumValue(raw['availability'], ['local', 'shared', 'external', 'missing', 'expired', 'deleted', 'denied', 'integrity_failure', 'mixed'] as const, `${path}.availability`),
  }
}

function decodeSourceBinding(value: unknown, path: string): ExplorerSourceBinding {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) fail(path)
  const kind = (value as Record<string, unknown>)['kind']
  if (kind === 'native') {
    const raw = object(value, path, ['kind', 'repository', 'revision', 'change_id', 'context_digest', 'source_generation'])
    return {
      kind,
      repository: nonempty(raw['repository'], `${path}.repository`, 256),
      revision: nonempty(raw['revision'], `${path}.revision`, 128),
      change_id: nonempty(raw['change_id'], `${path}.change_id`, 255),
      context_digest: nonempty(raw['context_digest'], `${path}.context_digest`, 128),
      source_generation: integer(raw['source_generation'], `${path}.source_generation`),
    }
  }
  if (kind === 'git') {
    const raw = object(value, path, ['kind', 'repository', 'worktree_id', 'head', 'tree', 'base', 'dirty', 'untracked_count', 'omissions', 'captured_snapshot', 'snapshot_coverage'])
    return {
      kind,
      repository: nonempty(raw['repository'], `${path}.repository`, 256),
      worktree_id: nonempty(raw['worktree_id'], `${path}.worktree_id`, 255),
      head: oid(raw['head'], `${path}.head`),
      tree: oid(raw['tree'], `${path}.tree`),
      base: oid(raw['base'], `${path}.base`),
      dirty: bool(raw['dirty'], `${path}.dirty`),
      untracked_count: integer(raw['untracked_count'], `${path}.untracked_count`),
      omissions: array(raw['omissions'], `${path}.omissions`, 6).map((item, index) => enumValue(item, ['untracked_content', 'ignored_content', 'working_tree_content', 'index_state', 'submodule_content', 'symlink_target'] as const, `${path}.omissions[${index}]`)),
      captured_snapshot: raw['captured_snapshot'] === null ? null : decodeArtifact(raw['captured_snapshot'], `${path}.captured_snapshot`),
      snapshot_coverage: enumValue(raw['snapshot_coverage'], COVERAGE, `${path}.snapshot_coverage`),
    }
  }
  return fail(`${path}.kind`)
}

function decodeActionEvidence(value: unknown, path: string): ExplorerActionEvidence {
  const raw = object(value, path, ['entry', 'relation', 'path_relation', 'work_id', 'generation', 'session_id', 'action_id', 'operation_id', 'action_kind', 'recipe_provenance', 'recipe_digest', 'setup_digest', 'started_at', 'source', 'applicability', 'start_retention', 'result'])
  if (raw['entry'] !== 'action' || raw['relation'] !== 'exact_source_binding') fail(`${path}.relation`)
  let result: ExplorerActionEvidence['result'] = null
  if (raw['result'] !== null) {
    const resultPath = `${path}.result`
    const resultRaw = object(raw['result'], resultPath, ['operation_id', 'outcome', 'result', 'assurance', 'coverage', 'drift', 'finished_at', 'retention'])
    result = {
      operation_id: nonempty(resultRaw['operation_id'], `${resultPath}.operation_id`, 255),
      outcome: enumValue(resultRaw['outcome'], ['succeeded', 'failed', 'incomplete', 'unknown'] as const, `${resultPath}.outcome`),
      result: enumValue(resultRaw['result'], ['passed', 'failed', 'incomplete', 'unknown'] as const, `${resultPath}.result`),
      assurance: enumValue(resultRaw['assurance'], ['caller_asserted', 'observed_trusted_local', 'protected_execution', 'independent_verification'] as const, `${resultPath}.assurance`),
      coverage: enumValue(resultRaw['coverage'], COVERAGE, `${resultPath}.coverage`),
      drift: enumValue(resultRaw['drift'], ['none_observed', 'source_changed', 'setup_changed', 'recipe_changed', 'unknown'] as const, `${resultPath}.drift`),
      finished_at: integer(resultRaw['finished_at'], `${resultPath}.finished_at`),
      retention: enumValue(resultRaw['retention'], RETENTION, `${resultPath}.retention`),
    }
  }
  return {
    entry: 'action',
    relation: 'exact_source_binding',
    path_relation: enumValue(raw['path_relation'], ['not_requested', 'not_recorded'] as const, `${path}.path_relation`),
    work_id: nonempty(raw['work_id'], `${path}.work_id`, 255),
    generation: integer(raw['generation'], `${path}.generation`, 1),
    session_id: nonempty(raw['session_id'], `${path}.session_id`, 255),
    action_id: nonempty(raw['action_id'], `${path}.action_id`, 255),
    operation_id: nonempty(raw['operation_id'], `${path}.operation_id`, 255),
    action_kind: enumValue(raw['action_kind'], ['execution', 'verification'] as const, `${path}.action_kind`),
    recipe_provenance: enumValue(raw['recipe_provenance'], ['observed_native_input', 'configured_exact_match'] as const, `${path}.recipe_provenance`),
    recipe_digest: nonempty(raw['recipe_digest'], `${path}.recipe_digest`, 128),
    setup_digest: nonempty(raw['setup_digest'], `${path}.setup_digest`, 128),
    started_at: integer(raw['started_at'], `${path}.started_at`),
    source: decodeSourceBinding(raw['source'], `${path}.source`),
    applicability: enumValue(raw['applicability'], ['matching_recorded_clean_source', 'different_recorded_source', 'unknown'] as const, `${path}.applicability`),
    start_retention: enumValue(raw['start_retention'], RETENTION, `${path}.start_retention`),
    result,
  }
}

function decodeSessionEvidence(value: unknown, path: string): ExplorerSessionEvidence {
  const raw = object(value, path, ['entry', 'relation', 'work_id', 'generation', 'session_id', 'harness', 'capture_mode', 'first_observed_at', 'last_observed_at', 'event_count', 'unavailable_body_count', 'capture_gap_count'])
  if (raw['entry'] !== 'session' || raw['relation'] !== 'same_work_session') fail(`${path}.relation`)
  return {
    entry: 'session',
    relation: 'same_work_session',
    work_id: nonempty(raw['work_id'], `${path}.work_id`, 255),
    generation: integer(raw['generation'], `${path}.generation`, 1),
    session_id: nonempty(raw['session_id'], `${path}.session_id`, 255),
    harness: enumValue(raw['harness'], ['claude_code', 'codex'] as const, `${path}.harness`),
    capture_mode: enumValue(raw['capture_mode'], ['metadata_only', 'local_conversation'] as const, `${path}.capture_mode`),
    first_observed_at: integer(raw['first_observed_at'], `${path}.first_observed_at`),
    last_observed_at: integer(raw['last_observed_at'], `${path}.last_observed_at`),
    event_count: integer(raw['event_count'], `${path}.event_count`),
    unavailable_body_count: integer(raw['unavailable_body_count'], `${path}.unavailable_body_count`),
    capture_gap_count: integer(raw['capture_gap_count'], `${path}.capture_gap_count`),
  }
}

export function decodeExplorerEvidence(value: unknown): ExplorerEvidencePage {
  const path = 'evidence'
  const raw = object(value, path, ['version', 'repository', 'commit', 'path', 'requested_work_id', 'entries', 'projection_coverage', 'next_cursor', 'complete'])
  if (raw['version'] !== 1) fail(`${path}.version`)
  const decoded: ExplorerEvidencePage = {
    version: 1,
    repository: nonempty(raw['repository'], `${path}.repository`, 256),
    commit: oid(raw['commit'], `${path}.commit`),
    path: nullableString(raw['path'], `${path}.path`),
    requested_work_id: nullableString(raw['requested_work_id'], `${path}.requested_work_id`, 255),
    entries: array(raw['entries'], `${path}.entries`, 100).map((item, index) => {
      const entryPath = `${path}.entries[${index}]`
      if (item === null || typeof item !== 'object' || Array.isArray(item)) fail(entryPath)
      const kind = (item as Record<string, unknown>)['entry']
      if (kind === 'action') return decodeActionEvidence(item, entryPath)
      if (kind === 'session') return decodeSessionEvidence(item, entryPath)
      return fail(`${entryPath}.entry`)
    }),
    projection_coverage: decodeProjectionCoverage(raw['projection_coverage'], `${path}.projection_coverage`),
    next_cursor: nullableCursor(raw['next_cursor'], `${path}.next_cursor`),
    complete: bool(raw['complete'], `${path}.complete`),
  }
  for (const entry of decoded.entries) if (entry.entry === 'action' && entry.source.repository !== decoded.repository) fail(`${path}.entries.source.repository`)
  return decoded
}

function decodeConversationEvent(value: unknown, path: string): ExplorerConversationEvent {
  const raw = object(value, path, ['work_id', 'generation', 'session_id', 'event_id', 'sequence', 'observed_at', 'kind', 'tool', 'relation', 'body', 'gap', 'retention'])
  if (raw['relation'] !== 'same_work_session') fail(`${path}.relation`)
  let tool: ExplorerConversationEvent['tool'] = null
  if (raw['tool'] !== null) {
    const toolRaw = object(raw['tool'], `${path}.tool`, ['kind', 'request_id'])
    tool = {
      kind: enumValue(toolRaw['kind'], ['read', 'edit', 'write', 'search', 'shell', 'test', 'model', 'other'] as const, `${path}.tool.kind`),
      request_id: nullableString(toolRaw['request_id'], `${path}.tool.request_id`, 255),
    }
  }
  const bodyRaw = object(raw['body'], `${path}.body`, ['availability', 'media', 'content'])
  const availability = enumValue(bodyRaw['availability'], ['available', 'not_captured', 'not_applicable', 'missing', 'expired', 'deleted', 'denied', 'too_large', 'integrity_failure', 'invalid_body'] as const, `${path}.body.availability`)
  const media = bodyRaw['media'] === null ? null : enumValue(bodyRaw['media'], ['json', 'text', 'json_lines', 'binary'] as const, `${path}.body.media`)
  const content = bodyRaw['content'] === null ? null : string(bodyRaw['content'], `${path}.body.content`, MAX_CONVERSATION_TEXT_BYTES)
  if ((availability === 'available') !== (content !== null && media !== null)) fail(`${path}.body.content`)
  let gap: ExplorerConversationEvent['gap'] = null
  if (raw['gap'] !== null) {
    const gapRaw = object(raw['gap'], `${path}.gap`, ['first_missing', 'last_missing', 'reason'])
    gap = {
      first_missing: integer(gapRaw['first_missing'], `${path}.gap.first_missing`),
      last_missing: integer(gapRaw['last_missing'], `${path}.gap.last_missing`),
      reason: enumValue(gapRaw['reason'], ['provider_history_unavailable', 'adapter_started_late', 'identity_uncertain', 'sequence_missing', 'local_retention_expired', 'redacted', 'capture_interrupted'] as const, `${path}.gap.reason`),
    }
    if (gap.first_missing > gap.last_missing) fail(`${path}.gap`)
  }
  return {
    work_id: nonempty(raw['work_id'], `${path}.work_id`, 255),
    generation: integer(raw['generation'], `${path}.generation`, 1),
    session_id: nonempty(raw['session_id'], `${path}.session_id`, 255),
    event_id: nonempty(raw['event_id'], `${path}.event_id`, 255),
    sequence: integer(raw['sequence'], `${path}.sequence`),
    observed_at: integer(raw['observed_at'], `${path}.observed_at`),
    kind: enumValue(raw['kind'], ['session_started', 'user_prompt', 'assistant_message', 'tool_started', 'tool_finished', 'check_started', 'check_finished', 'session_ended', 'gap'] as const, `${path}.kind`),
    tool,
    relation: 'same_work_session',
    body: { availability, media, content },
    gap,
    retention: enumValue(raw['retention'], RETENTION, `${path}.retention`),
  }
}

export function decodeExplorerConversation(value: unknown): ExplorerConversationPage {
  const path = 'conversation'
  const raw = object(value, path, ['version', 'repository', 'work_id', 'session_id', 'relation', 'events', 'projection_coverage', 'next_cursor', 'complete'])
  if (raw['version'] !== 1 || raw['relation'] !== 'same_work_session') fail(`${path}.version`)
  const decoded: ExplorerConversationPage = {
    version: 1,
    repository: nonempty(raw['repository'], `${path}.repository`, 256),
    work_id: nonempty(raw['work_id'], `${path}.work_id`, 255),
    session_id: nonempty(raw['session_id'], `${path}.session_id`, 255),
    relation: 'same_work_session',
    events: array(raw['events'], `${path}.events`, 100).map((event, index) => decodeConversationEvent(event, `${path}.events[${index}]`)),
    projection_coverage: decodeProjectionCoverage(raw['projection_coverage'], `${path}.projection_coverage`),
    next_cursor: nullableCursor(raw['next_cursor'], `${path}.next_cursor`),
    complete: bool(raw['complete'], `${path}.complete`),
  }
  for (const event of decoded.events) if (event.work_id !== decoded.work_id || event.session_id !== decoded.session_id) fail(`${path}.events.identity`)
  return decoded
}

function adviceFor(status: number): string {
  if (status === 400) return 'The local explorer could not use that request. Return to the current source and try again.'
  if (status === 401) return 'The local explorer needs its read token. Add it in Settings → This computer.'
  if (status === 403) return 'The local explorer refused this page’s origin. Open the Hub through its configured browser origin.'
  if (status === 404) return 'The source is not available. Start agentscm serve with --explorer-repo and --explorer-repository, or choose another retained item.'
  if (status === 409) return 'The source changed while the explorer opened it. Refresh the source identity and try again.'
  if (status === 421) return 'The local explorer refused this page’s address. Open the Hub UI on localhost.'
  if (status === 503) return 'The local explorer is busy. Try again in a moment.'
  return `The local explorer could not complete the read (HTTP ${status}).`
}

async function boundedJson(response: Response): Promise<unknown> {
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_RESPONSE_BYTES) {
    await response.body?.cancel()
    throw new ExplorerResponseError('The local explorer response is too large to read safely.')
  }
  if (!response.body) throw new ExplorerResponseError('The local explorer returned an empty response.')
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    while (true) {
      const next = await reader.read()
      if (next.done) break
      total += next.value.byteLength
      if (total > MAX_RESPONSE_BYTES) {
        await reader.cancel()
        throw new ExplorerResponseError('The local explorer response is too large to read safely.')
      }
      chunks.push(next.value)
    }
  } finally {
    reader.releaseLock()
  }
  const bytes = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  try {
    return JSON.parse(new TextDecoder().decode(bytes)) as unknown
  } catch {
    throw new ExplorerResponseError('The local explorer returned invalid JSON.')
  }
}

async function explorerRead<T>(endpoint: string, params: Record<string, string | number | undefined>, decode: (value: unknown) => T, signal: AbortSignal): Promise<T> {
  const configured = loadLocalBase()
  if (!validLocalBase(configured)) throw new ExplorerResponseError('The saved local server address is not a loopback address. Update it in Settings → This computer.')
  const crossOrigin = configured !== ''
  const query = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) if (value !== undefined) query.set(key, String(value))
  const token = crossOrigin ? loadLocalToken() : localCredential() || loadLocalToken()
  const url = `${crossOrigin ? configured : ''}/v1/local/explorer/${endpoint}?${query}`
  const busyDelays = [150, 400, 900, 1800]
  let response: Response
  for (let attempt = 0; ; attempt++) {
    try {
      response = await fetch(url, {
        signal: withTimeout(signal),
        cache: 'no-store',
        credentials: 'omit',
        redirect: 'error',
        headers: token ? { Authorization: `Bearer ${token}` } : {},
      })
    } catch (error) {
      if (signal.aborted) throw error
      throw new ExplorerResponseError(`Nothing answered at ${configured || 'this page’s local explorer address'}. Start or reconnect agentscm serve, then try again.`)
    }
    if (response.status !== 503 || attempt === busyDelays.length) break
    await response.body?.cancel()
    await new Promise((resolve) => window.setTimeout(resolve, busyDelays[attempt]))
    if (signal.aborted) throw signal.reason
  }
  if (!response.ok) {
    // On this computer's own Hub page, a repository that is no longer connected is its own case.
    const code = response.status === 409 ? ((await response.json().catch(() => null)) as { error?: unknown } | null)?.error : undefined
    if (code === undefined) await response.body?.cancel()
    throw new ExplorerResponseError(code === 'repository_unavailable' ? 'That repository is no longer connected on this computer. Choose another one at the top of the page, or run agentscm open in its checkout.' : adviceFor(response.status), response.status)
  }
  return decode(await boundedJson(response))
}

function expectIdentity(value: ExplorerSourceIdentity, expected: { commit: string; repositoryId?: string }): void {
  if (value.commit !== expected.commit) throw new ExplorerResponseError('The local explorer returned a different immutable commit than requested.')
  if (expected.repositoryId !== undefined && value.repository_id !== expected.repositoryId) throw new ExplorerResponseError('The local explorer returned data for a different repository.')
}

export async function fetchExplorerSummary(selectedRef: string, signal: AbortSignal): Promise<ExplorerSummary> {
  return explorerRead('summary', { ref: selectedRef }, decodeExplorerSummary, signal)
}

export async function fetchExplorerTree(commit: string, path: string, signal: AbortSignal, after?: string, repositoryId?: string): Promise<ExplorerTreePage> {
  const value = await explorerRead('tree', { commit, path: path || undefined, after, limit: 100 }, decodeExplorerTree, signal)
  expectIdentity(value, { commit, repositoryId })
  if (value.path !== path) throw new ExplorerResponseError('The local explorer returned a different directory than requested.')
  return value
}

export async function fetchExplorerFile(commit: string, path: string, signal: AbortSignal, repositoryId?: string): Promise<ExplorerFile> {
  const value = await explorerRead('file', { commit, path }, decodeExplorerFile, signal)
  expectIdentity(value, { commit, repositoryId })
  if (value.path !== path) throw new ExplorerResponseError('The local explorer returned a different file than requested.')
  return value
}

export async function fetchExplorerHistory(commit: string, path: string | null, signal: AbortSignal, cursor?: string, repositoryId?: string): Promise<ExplorerHistoryPage> {
  const value = await explorerRead('history', { commit, path: path || undefined, cursor, limit: 25 }, decodeExplorerHistory, signal)
  expectIdentity(value, { commit, repositoryId })
  if (value.path !== path) throw new ExplorerResponseError('The local explorer returned history for a different path than requested.')
  return value
}

export async function fetchExplorerDiff(left: string, right: string, path: string | null, signal: AbortSignal, cursor?: string, repositoryId?: string): Promise<ExplorerDiffPage> {
  const value = await explorerRead('diff', { left, right, path: path || undefined, cursor, limit: 100 }, decodeExplorerDiff, signal)
  if (value.left !== left || value.right !== right || value.path !== path) throw new ExplorerResponseError('The local explorer returned a different comparison than requested.')
  if (repositoryId !== undefined && value.repository_id !== repositoryId) throw new ExplorerResponseError('The local explorer returned data for a different repository.')
  return value
}

export async function fetchExplorerSearch(commit: string, query: string, signal: AbortSignal, cursor?: string, repositoryId?: string): Promise<ExplorerSearchPage> {
  if (!query || encoder.encode(query).byteLength > 1024) throw new ExplorerResponseError('Search text must be between 1 byte and 1 KiB.')
  const value = await explorerRead('search', { commit, q: query, cursor, limit: 50 }, decodeExplorerSearch, signal)
  expectIdentity(value, { commit, repositoryId })
  if (value.query !== query) throw new ExplorerResponseError('The local explorer returned results for a different search.')
  return value
}

export async function fetchExplorerEvidence(commit: string, path: string | null, signal: AbortSignal, workId?: string, cursor?: string, repositoryId?: string): Promise<ExplorerEvidencePage> {
  const value = await explorerRead('evidence', { commit, path: path || undefined, work_id: workId, cursor, limit: 25 }, decodeExplorerEvidence, signal)
  if (value.commit !== commit || value.path !== path || value.requested_work_id !== (workId ?? null)) throw new ExplorerResponseError('The local explorer returned evidence for a different source selection.')
  if (repositoryId !== undefined && value.repository !== repositoryId) throw new ExplorerResponseError('The local explorer returned data for a different repository.')
  return value
}

export async function fetchExplorerConversation(workId: string, sessionId: string, signal: AbortSignal, cursor?: string, repositoryId?: string): Promise<ExplorerConversationPage> {
  const value = await explorerRead('conversation', { work_id: workId, session_id: sessionId, cursor, limit: 25 }, decodeExplorerConversation, signal)
  if (value.work_id !== workId || value.session_id !== sessionId) throw new ExplorerResponseError('The local explorer returned a different conversation than requested.')
  if (repositoryId !== undefined && value.repository !== repositoryId) throw new ExplorerResponseError('The local explorer returned data for a different repository.')
  return value
}
