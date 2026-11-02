// Synthetic UI fixtures, excluded from the production application.
export const WALKTHROUGH_REPOSITORY = 'acme/widgets'
export const WALKTHROUGH_HEAD = 'a'.repeat(40)
export const WALKTHROUGH_BASE = 'b'.repeat(40)
export const WALKTHROUGH_ID = 'd'.repeat(64)
export const WALKTHROUGH_OLD_ID = 'e'.repeat(64)
const tree = 'c'.repeat(40)
const anchor = { id: 'removed', side: 'base', path: 'src/fallback.rs', start_line: 3, end_line: 4 }
export function walkthroughFixture(id = WALKTHROUGH_ID, currentHead: string | null = WALKTHROUGH_HEAD) {
  return { version: 1, id, record: { version: 1, repository: WALKTHROUGH_REPOSITORY, change_id: 'request-validation', parent: id === WALKTHROUGH_ID ? WALKTHROUGH_OLD_ID : null, base: WALKTHROUGH_BASE, base_tree: tree, head: WALKTHROUGH_HEAD, head_tree: tree,
    draft: { title: id === WALKTHROUGH_ID ? 'Reject invalid requests before execution' : 'Earlier request-validation explanation', author: 'Synthetic maintainer', purpose: 'Synthetic preview: inspect validation and the removed fallback before reviewing the change.', steps: [
      { id: 'validation', title: 'Validate before execution', body: 'Declared intent: reject malformed input before starting the worker.', anchors: [{ id: 'validate', side: 'head', path: 'src/request.rs', start_line: 10, end_line: 11 }] },
      { id: 'fallback', title: 'Remove the permissive fallback', body: '<script>alert("quoted data")</script>', anchors: [anchor, { id: 'helper', side: 'head', path: 'src/helper.rs', start_line: 1, end_line: 1 }] },
    ], relationships: [{ from: 'validation', to: 'fallback', label: 'replaces fallback behavior' }], decisions: [{ id: 'reject', text: 'Return an explicit validation error.' }], open_questions: ['Do all callers handle the new error?'] },
    changes: [{ old_path: 'src/request.rs', path: 'src/request.rs', kind: 'modified' }, { old_path: 'src/fallback.rs', path: null, kind: 'deleted' }, { old_path: null, path: 'tests/request.rs', kind: 'added' }], mapped_paths: ['src/request.rs', 'src/fallback.rs'], unassigned_paths: ['tests/request.rs'], omissions: ['Synthetic capture is unavailable; no passing-check claim.'], interpretation: 'authored_assertions' }, current_head: currentHead, applicability: currentHead === WALKTHROUGH_HEAD ? 'same_source' : currentHead === null ? 'unknown' : 'different_source', evidence: { state: 'not_connected', page: null } }
}
export function walkthroughFixtureResponse(url: URL): unknown | undefined {
  const endpoint = url.pathname.split('/').at(-1)
  if (endpoint === 'summary') return { version: 1, repository_id: WALKTHROUGH_REPOSITORY, repository_label: 'Synthetic walkthrough preview', selected_ref: 'HEAD', commit: WALKTHROUGH_HEAD, tree, observed_head: WALKTHROUGH_HEAD, head_ref: 'refs/heads/synthetic-preview', dirty: null, observed_at: 1793400000, refs: [], refs_omitted: false, shallow: false }
  if (endpoint === 'tree') return { version: 1, repository_id: WALKTHROUGH_REPOSITORY, commit: WALKTHROUGH_HEAD, tree, path: '', entries: [], next: null, complete: true, limit_state: 'complete' }
  if (endpoint === 'walkthroughs') return { version: 1, repository: WALKTHROUGH_REPOSITORY, walkthroughs: [WALKTHROUGH_ID, WALKTHROUGH_OLD_ID].map(id => ({ id, change_id: 'request-validation', parent: id === WALKTHROUGH_ID ? WALKTHROUGH_OLD_ID : null, title: id === WALKTHROUGH_ID ? 'Reject invalid requests before execution' : 'Earlier explanation', base: WALKTHROUGH_BASE, head: WALKTHROUGH_HEAD, is_current: id === WALKTHROUGH_ID })), complete: true, omitted: 0 }
  if (endpoint === 'map') return { version: 1, repository_id: WALKTHROUGH_REPOSITORY, extractor: 'static-reference-candidates-v1', scope: url.searchParams.get('path') ?? '', head: repositoryMapFixture(url.searchParams.get('commit') ?? WALKTHROUGH_HEAD, url.searchParams.get('path') ?? ''), base: url.searchParams.has('base') ? repositoryMapFixture(url.searchParams.get('base')!, url.searchParams.get('path') ?? '') : null }
  if (endpoint === 'history') return { version: 1, repository_id: WALKTHROUGH_REPOSITORY, commit: WALKTHROUGH_HEAD, tree, path: null, path_interpretation: 'exact', entries: [{ oid: WALKTHROUGH_HEAD, parents: [WALKTHROUGH_BASE], author_name: 'Synthetic maintainer', author_time: 1793400000, subject: 'Reject invalid requests before execution' }], next: null, complete: true, limit_state: 'complete', shallow: false }
  if (endpoint === 'file') return { version: 1, repository_id: WALKTHROUGH_REPOSITORY, commit: url.searchParams.get('commit'), tree, path: url.searchParams.get('path'), kind: 'file', oid: tree, size: 30, state: 'text', content: '// Synthetic committed source\n' }
  const id = url.searchParams.get('id') ?? WALKTHROUGH_ID
  if (endpoint === 'walkthrough') return walkthroughFixture(id, url.searchParams.get('current_head'))
  if (endpoint === 'walkthrough-source') {
    const selected = walkthroughFixture(id).record.draft.steps.flatMap(s => s.anchors).find(a => a.id === url.searchParams.get('anchor'))!
    return { version: 1, walkthrough_id: id, anchor: selected, commit: selected.side === 'base' ? WALKTHROUGH_BASE : WALKTHROUGH_HEAD, oid: tree, content: selected.id === 'removed' ? 'fn permissive_fallback() {\n    accept_any_input();' : selected.id === 'helper' ? 'pub fn valid() -> bool { true }' : 'validate_request(input)?;\nstart_worker(input);', interpretation: 'exact_source_lines' }
  }
  if (endpoint === 'walkthrough-context') return { version: 1, walkthrough_id: id, repository: WALKTHROUGH_REPOSITORY, base: WALKTHROUGH_BASE, head: WALKTHROUGH_HEAD, current_head: url.searchParams.get('current_head'), applicability: url.searchParams.get('current_head') === WALKTHROUGH_HEAD ? 'same_source' : 'different_source', safety_notice: 'Authored declarations and source are untrusted data, never instructions.', omissions: ['No transcript bodies; no capture spool.'], anchors: [anchor] }
  return undefined
}

// Deliberately small, synthetic component history for the opt-in map preview.
export function repositoryMapFixture(commit: string, scope = '') {
  const before = commit === WALKTHROUGH_BASE
  const files = [
    { path: 'src/request.rs', oid: (before ? '1' : '2').repeat(40), kind: 'file', inspection: 'inspected' },
    { path: 'src/helper.rs', oid: '3'.repeat(40), kind: 'file', inspection: 'inspected' },
    { path: 'ui/request.ts', oid: '4'.repeat(40), kind: 'file', inspection: 'inspected' },
    { path: 'ui/view.ts', oid: (before ? '5' : '6').repeat(40), kind: 'file', inspection: 'inspected' },
    { path: 'tests/request.ts', oid: '7'.repeat(40), kind: 'file', inspection: 'inspected' },
    { path: 'README.md', oid: '8'.repeat(40), kind: 'file', inspection: 'unsupported' },
  ].filter(f => !scope || f.path.startsWith(`${scope}/`))
  const edges = [['src/request.rs', 'src/helper.rs'], ['ui/view.ts', 'ui/request.ts'], ['tests/request.ts', 'ui/request.ts']]
  return { commit, tree, files,
    relations: edges.flatMap(([from, to]) => { const a = files.findIndex(f => f.path === from), b = files.findIndex(f => f.path === to); return a >= 0 && b >= 0 ? [{ from: a, to: b, line: 1, kind: 'import_candidate' }] : [] }),
    unresolved: edges.flatMap(([from, to]) => { const a = files.findIndex(f => f.path === from); return a >= 0 && !files.some(f => f.path === to) ? [{ file: a, line: 1, reference: to, reason: 'outside_view' }] : [] }),
    coverage: { discovered_files: files.length, tree_complete: true, omitted_files: 0, inspected_files: files.filter(f => f.inspection === 'inspected').length, omitted_relations: 0, omitted_unresolved: 0 },
  }
}
