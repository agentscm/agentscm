// Sample records only the G prototype needs: repository components, people, and the feature map.
import type { Tone } from '../data.ts'

export type GLens = 'work' | 'evidence' | 'ownership' | 'security'
export const G_LENSES: { id: GLens; label: string; icon: string; note: string }[] = [
  { id: 'work', label: 'Work', icon: 'kanban', note: 'Who plans to change it. No declared work does not mean unused.' },
  { id: 'evidence', label: 'Evidence', icon: 'patch-check', note: 'Tests and reviews on the current code. Old evidence stays true history but may not apply now.' },
  { id: 'ownership', label: 'Ownership', icon: 'people', note: 'Declared owners from CODEOWNERS and how often the code changes.' },
  { id: 'security', label: 'Security', icon: 'shield-check', note: 'What each scan covered and when. A clean scan covers its stated scope, nothing more.' },
]

export interface Component {
  id: string
  name: string
  path: string
  icon: string
  desc: string
  owner: string
  files: number
  col: number
  row: number
  work: string[]
  uses: string[]
  facts: Record<GLens, { text: string; tone: Tone }>
  history: { at: string; text: string; icon: string; tone: Tone }[]
}

export const COMPONENTS: Component[] = [
  { id: 'api', name: 'Export API', path: 'src/api/export', icon: 'cloud-arrow-up', desc: 'Tenant exports and their limits', owner: 'sam', files: 18, col: 0, row: 0, work: ['w7'], uses: ['billing', 'queue'],
    facts: { work: { text: 'Merging #44', tone: 'brand' }, evidence: { text: 'Tests current', tone: 'ok' }, ownership: { text: 'sam · weekly', tone: 'muted' }, security: { text: 'Scanned 6 days ago', tone: 'ok' } },
    history: [{ at: '6 min ago', text: 'maria approved d1e7f55', icon: 'person-check', tone: 'ok' }, { at: '1 hr ago', text: 'Tests passed on d1e7f55', icon: 'terminal', tone: 'ok' }, { at: '6 days ago', text: 'Security scan: 0 confirmed', icon: 'shield-check', tone: 'ok' }] },
  { id: 'billing', name: 'Billing', path: 'src/billing', icon: 'credit-card', desc: 'Invoices, tax and totals', owner: 'maria', files: 24, col: 1, row: 0, work: ['w1', 'w2'], uses: ['invoices'],
    facts: { work: { text: '2 changes · 1 overlap', tone: 'warn' }, evidence: { text: 'Current on #42', tone: 'ok' }, ownership: { text: 'maria · weekly', tone: 'muted' }, security: { text: 'Scanned 1 hr ago', tone: 'ok' } },
    history: [{ at: '4 min ago', text: 'PR evidence prepared for #42', icon: 'file-earmark-text', tone: 'brand' }, { at: '12 min ago', text: 'Tests rerun after review fix', icon: 'terminal', tone: 'ok' }, { at: '1 hr ago', text: 'Security audit: 0 confirmed in scope', icon: 'shield-check', tone: 'ok' }, { at: '3 days ago', text: 'PR #38 merged', icon: 'git', tone: 'ok' }] },
  { id: 'invoices', name: 'Invoices', path: 'src/invoices', icon: 'receipt', desc: 'Rounding and currency display', owner: 'nobody declared', files: 9, col: 2, row: 0, work: ['w2'], uses: [],
    facts: { work: { text: '1 change in review', tone: 'brand' }, evidence: { text: 'Tests stale', tone: 'warn' }, ownership: { text: 'No declared owner', tone: 'warn' }, security: { text: 'Not scanned in 30 days', tone: 'warn' } },
    history: [{ at: '9 min ago', text: 'Review found 2 issues', icon: 'eye', tone: 'warn' }, { at: '30 min ago', text: 'New commit; tests now stale', icon: 'clock-history', tone: 'warn' }] },
  { id: 'webhooks', name: 'Webhooks', path: 'src/webhooks', icon: 'broadcast', desc: 'Delivery, retries and signing', owner: 'sam', files: 12, col: 0, row: 1, work: ['w3'], uses: ['queue'],
    facts: { work: { text: 'Docs blocked', tone: 'bad' }, evidence: { text: 'No tests found', tone: 'bad' }, ownership: { text: 'sam · monthly', tone: 'muted' }, security: { text: 'Never scanned', tone: 'warn' } },
    history: [{ at: '15 min ago', text: 'Agent asked: 5 tries or 8?', icon: 'pause-circle', tone: 'bad' }, { at: '2 weeks ago', text: 'Release 0.1.3 deployed', icon: 'rocket-takeoff', tone: 'ok' }] },
  { id: 'queue', name: 'Queue', path: 'src/queue', icon: 'stack', desc: 'Workers and retry budgets', owner: 'sam', files: 15, col: 1, row: 1, work: ['w6'], uses: [],
    facts: { work: { text: 'Follow-up drafted', tone: 'ok' }, evidence: { text: 'CI passed on merge', tone: 'ok' }, ownership: { text: 'sam · weekly', tone: 'muted' }, security: { text: 'Not scanned', tone: 'muted' } },
    history: [{ at: '1 hr ago', text: 'Release notes drafted', icon: 'journal-text', tone: 'brand' }, { at: '2 hr ago', text: 'PR #40 merged', icon: 'git', tone: 'ok' }] },
  { id: 'auth', name: 'Auth', path: 'src/auth', icon: 'key', desc: 'Sessions, OAuth and tokens', owner: 'sam', files: 31, col: 2, row: 1, work: [], uses: [],
    facts: { work: { text: 'No declared work', tone: 'muted' }, evidence: { text: 'Tests current on main', tone: 'ok' }, ownership: { text: 'sam · 41 days quiet', tone: 'muted' }, security: { text: 'oauth/ outside scan', tone: 'warn' } },
    history: [{ at: '21 days ago', text: 'Security scan; oauth/ excluded', icon: 'shield-exclamation', tone: 'warn' }, { at: '41 days ago', text: 'Last change merged', icon: 'git', tone: 'muted' }] },
]

export const PEOPLE = [
  { who: 'maria', agents: ['claude-code', 'codex'], doing: 'Billing tax split; webhook docs', tone: 'warn' as Tone, status: 'Waiting on decisions' },
  { who: 'dev', agents: ['codex', 'cursor'], doing: 'JPY rounding fix', tone: 'brand' as Tone, status: 'Fixing two findings' },
  { who: 'sam', agents: ['copilot', 'claude-code'], doing: 'Image deps; export limits', tone: 'brand' as Tone, status: 'Checks running' },
]

export type FStatus = 'Implemented' | 'Experimental' | 'Partial' | 'Draft' | 'Proposed'
export const F_TONE: Record<FStatus, Tone> = { Implemented: 'ok', Experimental: 'brand', Partial: 'warn', Draft: 'muted', Proposed: 'muted' }

/** What AgentSCM has built, grouped by the job it does, and where each piece lives in this layout. */
export const FEATURES: { job: string; icon: string; blurb: string; items: { name: string; status: FStatus; where: string; to: string }[] }[] = [
  { job: 'Coordinate before work collides', icon: 'arrow-left-right', blurb: 'Agents announce what they edit; overlaps become conversations, not merge conflicts.', items: [
    { name: 'File holds, refused once', status: 'Implemented', where: 'Now › Needs you', to: '' },
    { name: 'Scope overlaps and requests', status: 'Implemented', where: 'Inbox', to: 'inbox' },
    { name: 'Branch collision scan', status: 'Implemented', where: 'Repository › Work', to: 'repository' },
    { name: 'Handoffs between agents', status: 'Experimental', where: 'Work', to: 'work' },
    { name: 'Slack decisions', status: 'Experimental', where: 'Setup', to: 'setup' },
  ] },
  { job: 'Carry context between people and agents', icon: 'journal-text', blurb: 'Pick up any change without reading a transcript.', items: [
    { name: 'Work, checkpoints and briefings', status: 'Implemented', where: 'Work › Timeline', to: 'work' },
    { name: 'Change records', status: 'Experimental', where: 'Work › Checks', to: 'work/w1' },
    { name: 'Shared conversations (opt-in)', status: 'Experimental', where: 'Work', to: 'work/w1' },
    { name: 'Commitments (Promises)', status: 'Experimental', where: 'Repository', to: 'repository' },
    { name: 'Change walkthroughs', status: 'Experimental', where: 'Work › Source', to: 'work/w1' },
  ] },
  { job: 'Apply a repeatable process', icon: 'diagram-3', blurb: 'Set the process once; agents read it at session start.', items: [
    { name: 'Workflow library and skill imports', status: 'Experimental', where: 'Workflows', to: 'workflows' },
    { name: 'PR gate skill', status: 'Experimental', where: 'Workflows', to: 'workflows' },
    { name: 'Local validation, signed', status: 'Experimental', where: 'Work › Checks', to: 'work/w1' },
    { name: 'After-merge rules', status: 'Experimental', where: 'Workflows › Follow-up', to: 'workflows' },
    { name: 'Stop hooks', status: 'Partial', where: 'Setup', to: 'setup' },
  ] },
  { job: 'Explain whether a change is ready', icon: 'patch-check', blurb: 'Every fact says how it is known: reported, run, observed or approved.', items: [
    { name: 'PR context and linked work', status: 'Experimental', where: 'Work', to: 'work/w1' },
    { name: 'Forge checks and merge observed', status: 'Experimental', where: 'Work › Timeline', to: 'work/w7' },
    { name: 'Portable evidence verifier', status: 'Experimental', where: 'Work › Checks', to: 'work/w1' },
  ] },
  { job: 'Show what it contributed', icon: 'stars', blurb: 'Contributions with their records, never invented savings.', items: [
    { name: 'Work insights and usage', status: 'Implemented', where: 'Contributions', to: 'value' },
    { name: 'Contribution receipts', status: 'Draft', where: 'Contributions', to: 'value' },
  ] },
  { job: 'Native source control (opt-in)', icon: 'box', blurb: 'Immutable revisions and gates for teams that turn them on.', items: [
    { name: 'Ledger, approvals and landing', status: 'Experimental', where: 'Repository › Revisions', to: 'repository' },
    { name: 'Task workspaces and snapshots', status: 'Experimental', where: 'Repository › Revisions', to: 'repository' },
    { name: 'Native authority per target', status: 'Proposed', where: 'Not built', to: 'map' },
  ] },
]
