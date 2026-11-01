// Sample data for the lifecycle prototypes (#proto/...). Static on purpose: no Hub endpoint serves stages, proof
// kinds or doors yet, so every screen here is a proposal rendered over invented records, never a live reading.
// All three variations read this same file so they can be compared on identical tasks.

export type Tone = 'brand' | 'ok' | 'warn' | 'bad' | 'muted'

/** How a fact is known. Kept apart on purpose: a signature or a report is not the same as an observation. */
export type Proof = 'agent' | 'command' | 'forge' | 'human' | 'waiting' | 'stale' | 'failed' | 'missing' | 'skipped'

export const PROOF: Record<Proof, { label: string; tone: Tone; icon: string; tip: string }> = {
  agent: { label: 'Agent reported', tone: 'muted', icon: 'robot', tip: 'An agent said so. Useful, but nothing else checked it.' },
  command: { label: 'Command result', tone: 'ok', icon: 'terminal', tip: 'A command ran on this exact commit and its result was kept. A signature says who ran it, not that it ran honestly.' },
  forge: { label: 'Forge observed', tone: 'ok', icon: 'github', tip: 'GitHub reported it for this exact commit.' },
  human: { label: 'Human approved', tone: 'ok', icon: 'person-check', tip: 'A person approved this exact commit.' },
  waiting: { label: 'Waiting on a person', tone: 'warn', icon: 'hourglass-split', tip: 'Nothing can move until a person decides.' },
  stale: { label: 'Stale', tone: 'warn', icon: 'clock-history', tip: 'It passed on an earlier commit. It does not apply to this one.' },
  failed: { label: 'Failed', tone: 'bad', icon: 'x-circle', tip: 'It ran on this commit and did not pass.' },
  missing: { label: 'Not yet', tone: 'muted', icon: 'circle', tip: 'No result exists for this commit yet.' },
  skipped: { label: 'Not needed', tone: 'muted', icon: 'dash-circle', tip: 'Two-way door: this step is only required for one-way doors.' },
}

export type StageId = 'building' | 'checks' | 'review' | 'ready' | 'merge' | 'follow'

/** The default "Get changes ready for review" journey, left to right. Configurable; not every transition is automated. */
export const STAGES: { id: StageId; label: string; short: string; plain: string; icon: string; step?: string }[] = [
  { id: 'building', label: 'Building', short: 'Building', plain: 'An agent is implementing', icon: 'hammer' },
  { id: 'checks', label: 'Checks', short: 'Checks', plain: 'Run on the dev machine first', icon: 'terminal', step: 'checks' },
  { id: 'review', label: 'Review and repair', short: 'Review', plain: 'Independent review, findings back to the author', icon: 'eye', step: 'review' },
  { id: 'ready', label: 'Ready for a person', short: 'Ready', plain: 'Evidence prepared; a person decides', icon: 'person-check', step: 'evidence' },
  { id: 'merge', label: 'PR and merge', short: 'Merge', plain: 'Forge checks, approval, merge', icon: 'git', step: 'ci' },
  { id: 'follow', label: 'Follow-up', short: 'Follow-up', plain: 'Release notes, docs, deploy check', icon: 'send' },
]

/** Which stage each kind of evidence belongs to, so a journey can show proof where it was earned. */
export const CHECK_STAGE: Record<string, StageId> = {
  'Request and criteria': 'building', Tests: 'checks', 'Link check': 'checks', 'Component tests': 'checks', 'Security scan': 'checks',
  'Independent review': 'review', Approval: 'ready', CI: 'merge', Merge: 'merge', 'Release notes': 'follow',
}

export interface Check { name: string; proof: Proof; detail: string }
export interface Agent { who: string; tool: string }
/** A Work item can have several runs (retries, follow-ups) and several PRs; neither is the Work itself. */
export interface Run { workflow: string; n: number; state: string; tone: Tone }
export interface Pr { n: number; state: string; tone: Tone }

export interface Work {
  id: string
  title: string
  purpose: string
  owner: string
  agents: Agent[]
  stage: StageId
  /** What a person reads first: the state in words. */
  state: string
  why: string
  next: string
  tone: Tone
  door: 'one-way' | 'two-way'
  blast: string
  source: string
  prs: Pr[]
  runs: Run[]
  paths: string[]
  checks: Check[]
  requirements: string[]
  activity: { at: string; text: string }[]
  fresh: string
  followUps?: string[]
}

export const WORK: Work[] = [
  {
    id: 'w1', title: 'Split tax calculation out of invoice totals', purpose: 'Totals and tax move independently, so per-region tax can change without touching rounding.',
    owner: 'maria', agents: [{ who: 'maria', tool: 'claude-code' }, { who: 'review', tool: 'codex' }], stage: 'ready',
    state: 'Ready for your decision', why: 'Checks are current on the head commit, both review findings are fixed, and the security scan found nothing in its scope.', next: 'Approve, or send it back with a note',
    tone: 'warn', door: 'one-way', blast: 'src/billing, invoices', source: 'a41c9e2', paths: ['src/billing/tax.rs', 'src/billing/totals.rs'],
    prs: [{ n: 42, state: 'Open · CI passed', tone: 'ok' }],
    runs: [{ workflow: 'Get changes ready for review', n: 2, state: 'Waiting on a person', tone: 'warn' }, { workflow: 'Get changes ready for review', n: 1, state: 'Stopped at review findings', tone: 'muted' }],
    checks: [
      { name: 'Request and criteria', proof: 'human', detail: 'maria wrote three acceptance criteria' },
      { name: 'Tests', proof: 'command', detail: 'npm test passed on a41c9e2, on maria’s computer' },
      { name: 'Independent review', proof: 'agent', detail: 'codex found 2 issues; both fixed in a41c9e2' },
      { name: 'Security scan', proof: 'command', detail: 'security-audit skill: 0 confirmed, scope src/billing only' },
      { name: 'CI', proof: 'forge', detail: 'build and e2e passed on a41c9e2' },
      { name: 'Approval', proof: 'waiting', detail: 'a maintainer must approve a one-way door' },
    ],
    requirements: ['Totals unchanged for every existing fixture', 'Tax rounding per region, not per line', 'No public API changes'],
    activity: [{ at: '4 min ago', text: 'PR evidence prepared from the record' }, { at: '12 min ago', text: 'Tests rerun after the review fix: passed' }, { at: '40 min ago', text: 'codex review: 2 findings, sent back to the author' }, { at: '2 hr ago', text: 'maria claimed the task from her agent' }],
    fresh: '4 min ago',
  },
  {
    id: 'w2', title: 'Fix invoice rounding for JPY', purpose: 'Zero-decimal currencies stop showing .00 totals.',
    owner: 'dev', agents: [{ who: 'dev', tool: 'codex' }, { who: 'review', tool: 'claude-code' }], stage: 'review',
    state: 'Author fixing two findings', why: 'The independent review found a missing currency case and an untested path.', next: 'Nothing for you; dev’s codex is on it',
    tone: 'brand', door: 'two-way', blast: 'src/invoices/round.rs', source: '7be03f1', paths: ['src/invoices/round.rs', 'src/billing/totals.rs'],
    prs: [{ n: 45, state: 'Draft', tone: 'muted' }],
    runs: [{ workflow: 'Get changes ready for review', n: 1, state: 'At review and repair', tone: 'brand' }],
    checks: [
      { name: 'Request and criteria', proof: 'agent', detail: 'Criteria written by the agent from issue #91' },
      { name: 'Tests', proof: 'stale', detail: 'Passed on 51d2a90. Run them for this change.' },
      { name: 'Independent review', proof: 'agent', detail: 'claude-code: 2 findings open' },
      { name: 'Security scan', proof: 'skipped', detail: 'Two-way door' },
      { name: 'CI', proof: 'missing', detail: 'Waits for the author’s fix' },
      { name: 'Approval', proof: 'skipped', detail: 'Two-way door: merges on green checks' },
    ],
    requirements: ['JPY, KRW and CLP show whole numbers', 'Existing EUR fixtures unchanged'],
    activity: [{ at: '9 min ago', text: 'Review found 2 issues; sent back to dev’s codex' }, { at: '30 min ago', text: 'New commit 7be03f1; tests now stale' }, { at: '1 hr ago', text: 'Overlap noticed with maria’s billing change on totals.rs' }],
    fresh: '9 min ago',
  },
  {
    id: 'w3', title: 'Document webhook retries', purpose: 'Integrators know the retry budget and backoff.',
    owner: 'maria', agents: [{ who: 'maria', tool: 'codex' }], stage: 'building',
    state: 'Blocked on your answer', why: 'The agent needs the retry policy before writing it down: 5 tries or 8?', next: 'Answer the agent’s question',
    tone: 'bad', door: 'two-way', blast: 'docs/webhooks.md', source: 'e09b4c1', paths: ['docs/webhooks.md'], prs: [], runs: [],
    checks: [{ name: 'Request and criteria', proof: 'human', detail: 'From issue #88' }, { name: 'Link check', proof: 'missing', detail: 'Runs at Checks' }],
    requirements: ['Retry count and backoff are stated', 'Example payload included'],
    activity: [{ at: '15 min ago', text: 'maria’s codex asked: 5 tries or 8?' }, { at: '40 min ago', text: 'Claimed from issue #88' }],
    fresh: '15 min ago',
  },
  {
    id: 'w4', title: 'Tidy the settings page layout', purpose: 'Settings fit on a phone without sideways scrolling.',
    owner: 'dev', agents: [{ who: 'dev', tool: 'cursor' }], stage: 'building',
    state: 'Handed over', why: 'dev’s cursor finished the desktop pass and offered the responsive pass to another agent.', next: 'Pick it up with your agent',
    tone: 'warn', door: 'two-way', blast: 'ui/settings', source: '3f7a0d8', paths: ['ui/src/pages/Settings.tsx'], prs: [], runs: [],
    checks: [{ name: 'Request and criteria', proof: 'human', detail: 'From the design review' }, { name: 'Component tests', proof: 'command', detail: 'Passed on 3f7a0d8' }],
    requirements: ['No horizontal scroll at 375px'],
    activity: [{ at: '30 min ago', text: 'Handover note: responsive pass remains' }],
    fresh: '30 min ago',
  },
  {
    id: 'w5', title: 'Update the image pipeline dependencies', purpose: 'Pick up the libvips security fix.',
    owner: 'sam', agents: [{ who: 'sam', tool: 'copilot' }], stage: 'checks',
    state: 'Checks running on sam’s computer', why: 'Local tests first; the slow e2e suite stays in CI.', next: 'Nothing yet',
    tone: 'brand', door: 'two-way', blast: 'jobs/images', source: 'c55e2a7', paths: ['jobs/images/Cargo.toml'], prs: [],
    runs: [{ workflow: 'Get changes ready for review', n: 1, state: 'At checks', tone: 'brand' }],
    checks: [{ name: 'Request and criteria', proof: 'agent', detail: 'From the libvips advisory' }, { name: 'Tests', proof: 'missing', detail: 'Running now' }],
    requirements: ['libvips ≥ 8.16.1'],
    activity: [{ at: '2 min ago', text: 'Local check started' }],
    fresh: '2 min ago',
  },
  {
    id: 'w6', title: 'Queue retry budget', purpose: 'Workers stop retrying forever.',
    owner: 'sam', agents: [{ who: 'sam', tool: 'claude-code' }], stage: 'follow',
    state: 'Merged; release notes drafted', why: 'GitHub observed the merge of #40, and the after-merge workflow kept a release-notes draft.', next: 'Read and publish when you like',
    tone: 'ok', door: 'two-way', blast: 'src/queue', source: '90ac1b3', paths: ['src/queue/retry.rs', 'CHANGELOG.md'],
    prs: [{ n: 40, state: 'Merged', tone: 'ok' }],
    runs: [{ workflow: 'Get changes ready for review', n: 1, state: 'Finished', tone: 'ok' }, { workflow: 'After merge: release notes', n: 1, state: 'Draft kept', tone: 'ok' }],
    checks: [{ name: 'Tests', proof: 'command', detail: 'Passed on 90ac1b3' }, { name: 'CI', proof: 'forge', detail: 'Passed on the merge commit' }, { name: 'Merge', proof: 'forge', detail: '#40 merged 2 hr ago' }, { name: 'Release notes', proof: 'agent', detail: 'Draft kept as an artifact' }],
    requirements: ['Retry budget configurable per queue'],
    activity: [{ at: '1 hr ago', text: 'Follow-up produced a CHANGELOG draft' }, { at: '2 hr ago', text: 'GitHub observed the merge of #40' }],
    fresh: '1 hr ago', followUps: ['Release notes for 0.1.4'],
  },
  {
    id: 'w7', title: 'Rate-limit the export API', purpose: 'One tenant cannot starve exports for the rest.',
    owner: 'sam', agents: [{ who: 'sam', tool: 'claude-code' }], stage: 'merge',
    state: 'Waiting for CI on GitHub', why: 'Approved; GitHub is running the required checks on the merge commit.', next: 'Nothing for you',
    tone: 'brand', door: 'two-way', blast: 'src/api/export', source: 'd1e7f55', paths: ['src/api/export/limit.rs'],
    prs: [{ n: 44, state: 'Approved · CI running', tone: 'brand' }],
    runs: [{ workflow: 'Get changes ready for review', n: 1, state: 'At PR and merge', tone: 'brand' }],
    checks: [{ name: 'Tests', proof: 'command', detail: 'Passed on d1e7f55' }, { name: 'Independent review', proof: 'agent', detail: 'No findings' }, { name: 'Approval', proof: 'human', detail: 'maria approved d1e7f55' }, { name: 'CI', proof: 'missing', detail: 'Running on GitHub' }],
    requirements: ['Per-tenant limit with a clear 429 message'],
    activity: [{ at: '6 min ago', text: 'Approved by maria' }],
    fresh: '6 min ago',
  },
]

export const work = (id: string) => WORK.find((w) => w.id === id)

export type Role = 'author' | 'reviewer' | 'lead'
export const ROLES: { id: Role; label: string; who: string }[] = [
  { id: 'author', label: 'Author', who: 'maria, writing code with her agents' },
  { id: 'reviewer', label: 'Reviewer', who: 'a maintainer who approves one-way doors' },
  { id: 'lead', label: 'Lead', who: 'whoever settles ownership and setup' },
]

/** Inbox owns these threads; every surface shows the same item and resolving it once resolves it everywhere.
 *  Ordered by what a person can act on, risk, what it blocks and how long it has waited, never by amount of evidence. */
export interface Need { id: string; roles: Role[]; title: string; reason: string; action: string; work: string[]; icon: string; tone: 'warn' | 'bad'; waiting: string; blocks?: string; risk?: string }
export const NEEDS: Need[] = [
  { id: 'n3', roles: ['author'], title: 'Your codex is waiting on an answer', reason: 'Webhook docs: retry 5 times or 8?', action: 'Answer the agent', work: ['w3'], icon: 'pause-circle', tone: 'bad', waiting: '15 min', blocks: 'Blocks the docs change' },
  { id: 'n1', roles: ['reviewer'], title: 'PR #42 needs a decision', reason: 'Billing tax split. Every required check is current on the head commit.', action: 'Review the change', work: ['w1'], icon: 'person-check', tone: 'warn', waiting: '4 min', risk: 'One-way door' },
  { id: 'n2', roles: ['lead', 'author'], title: 'Two changes plan to edit src/billing/totals.rs', reason: 'maria’s claude-code holds it; dev’s codex wants it for the JPY fix.', action: 'Decide who takes it', work: ['w1', 'w2'], icon: 'arrow-left-right', tone: 'warn', waiting: '1 hr', blocks: 'Blocks the JPY fix' },
  { id: 'n4', roles: ['lead'], title: 'Settings layout is ready for another agent', reason: 'Desktop pass done; responsive pass remains.', action: 'Hand it to an agent', work: ['w4'], icon: 'box-arrow-in-right', tone: 'warn', waiting: '30 min' },
  { id: 'n5', roles: ['lead'], title: 'copilot hasn’t been seen editing on sam’s computer', reason: 'Its edit hook is installed but has never fired, so its holds are invisible.', action: 'Copy the fix for sam', work: ['w5'], icon: 'plug', tone: 'warn', waiting: '2 days' },
]
export const needsFor = (role: Role) => NEEDS.filter((n) => n.roles.includes(role))

export const CHANGED = [
  { icon: 'git', tone: 'ok' as Tone, text: 'GitHub observed the merge of #40', sub: 'The follow-up kept a release-notes draft', work: 'w6' },
  { icon: 'clock-history', tone: 'warn' as Tone, text: 'Tests on the JPY fix went stale after a new commit', sub: 'They passed on 51d2a90, not on 7be03f1', work: 'w2' },
  { icon: 'eye', tone: 'brand' as Tone, text: 'Review on #42 found 2 issues; both fixed', sub: 'Tests rerun and passed on the fix', work: 'w1' },
]

/** Contributions first, each linked to its record; counts only where the event was actually recorded. */
export const VALUE = {
  stories: [
    { icon: 'arrow-left-right', text: 'An overlap on src/billing/totals.rs was noticed before either PR opened.', work: 'w2' },
    { icon: 'terminal', text: 'A local test caught a rounding defect on the JPY fix before review.', work: 'w2' },
    { icon: 'clock-history', text: 'A stale test result on #42 was rerun for the new commit.', work: 'w1' },
    { icon: 'send', text: 'The merge of #40 produced release notes as a follow-up.', work: 'w6' },
  ],
  counts: [
    { n: '6', label: 'overlaps noticed', sub: '4 settled before either PR merged' },
    { n: '18', label: 'local checks', sub: '3 failed before review' },
    { n: '2', label: 'local results CI accepted', sub: 'recorded by the ci.yml verifier' },
    { n: '2.1M', label: 'tokens reported', sub: '3 of 5 agents report usage' },
  ],
}

export const CONNECTIONS = [
  { name: 'claude-code', kind: 'Agent', state: 'ok' as Tone, note: 'Edit hook and MCP verified' },
  { name: 'codex', kind: 'Agent', state: 'ok' as Tone, note: 'Edit hook and MCP verified' },
  { name: 'cursor', kind: 'Agent', state: 'ok' as Tone, note: 'Reports after edits; cannot refuse one' },
  { name: 'copilot', kind: 'Agent', state: 'warn' as Tone, note: 'Edit hook not seen yet on sam’s computer' },
  { name: 'GitHub', kind: 'Forge', state: 'ok' as Tone, note: 'Reads PRs and checks. Posting is off.' },
  { name: 'This computer', kind: 'Computer', state: 'ok' as Tone, note: 'Local checks and the source explorer' },
  { name: 'Slack', kind: 'Optional', state: 'muted' as Tone, note: 'Optional. Sends decisions to people.' },
  { name: 'Native source control', kind: 'Advanced', state: 'muted' as Tone, note: 'Off. Immutable revisions and native gates when enabled.' },
]

/** Steps read as four plain answers before any engine terminology. */
export type Where = 'Existing agent' | 'Local command' | 'Configured runner' | 'External CI observed'
export interface Step { id: string; title: string; doWhat: string; where: Where; done: string; next: string; icon: string }
export type FlowState = 'Draft' | 'Previewed' | 'Ready to activate' | 'Active'
export interface Flow { id: string; name: string; sentence: string; state: FlowState; kit: number; steps: Step[]; loop?: { from: string; to: string; label: string } }

export const FLOWS: Flow[] = [
  {
    id: 'f0', name: 'Coordinate existing agents', state: 'Active', kit: 1,
    sentence: 'Every connected agent announces the files it is about to edit, asks before taking a file someone else holds, checkpoints its task, and hands work over with a note.',
    steps: [
      { id: 'announce', title: 'Announce the edit', doWhat: 'The edit hook in each agent', where: 'Existing agent', done: 'The hold is recorded before the first edit', next: 'Edit, or ask the holder', icon: 'megaphone' },
      { id: 'ask', title: 'Ask before taking a held file', doWhat: 'An Inbox request to the holder', where: 'Existing agent', done: 'The holder, or their person, hands it over or keeps it', next: 'Carry on with the answer', icon: 'arrow-left-right' },
      { id: 'handover', title: 'Checkpoint and hand over', doWhat: 'A checkpoint with the next step', where: 'Existing agent', done: 'Another session can pick it up without the transcript', next: 'Done', icon: 'box-arrow-in-right' },
    ],
  },
  {
    id: 'f1', name: 'Get changes ready for review', state: 'Active', kit: 2,
    sentence: 'When a change is ready for review, run the repository’s checks locally, request an independent review, return findings to the author, then prepare the PR evidence.',
    loop: { from: 'review', to: 'checks', label: 'Findings go back to the author' },
    steps: [
      { id: 'checks', title: 'Run checks', doWhat: 'npm test and npm run lint', where: 'Local command', done: 'Both pass on the current commit', next: 'Request review', icon: 'terminal' },
      { id: 'review', title: 'Independent review', doWhat: 'The review skill in .claude/skills/review', where: 'Existing agent', done: 'Findings recorded on the exact commit; blocking ones fixed or accepted with a reason', next: 'Back to the author if there are findings, otherwise prepare evidence', icon: 'eye' },
      { id: 'evidence', title: 'Prepare PR evidence', doWhat: 'Write Summary, Evidence and Merge danger', where: 'Existing agent', done: 'The PR description matches the current commit', next: 'Wait for a person', icon: 'file-earmark-text' },
      { id: 'ci', title: 'PR checks and approval', doWhat: 'ci.yml: build and e2e', where: 'External CI observed', done: 'GitHub reports both passing on the head; one-way doors also need an approval', next: 'Merge', icon: 'github' },
    ],
  },
  {
    id: 'f2', name: 'After merge: release notes', state: 'Ready to activate', kit: 0,
    sentence: 'After a PR merges into main, ask an available agent to update the release notes and keep the draft for a person to publish.',
    steps: [
      { id: 'merge', title: 'Merge observed', doWhat: 'Watch main for merges', where: 'External CI observed', done: 'GitHub reports the merge commit', next: 'Write notes', icon: 'git' },
      { id: 'notes', title: 'Write release notes', doWhat: 'The changelog skill', where: 'Existing agent', done: 'A CHANGELOG draft is kept as an artifact', next: 'A person publishes it', icon: 'journal-text' },
    ],
  },
  {
    id: 'f3', name: 'Add a security review', state: 'Draft', kit: 3,
    sentence: 'Before review on one-way doors, run the selected security audit skill and treat confirmed findings as blocking.',
    steps: [
      { id: 'scan', title: 'Security audit', doWhat: 'cloudflare/security-audit-skill, pinned at a1b2c3d', where: 'Existing agent', done: 'findings.json and its coverage ledger validate; no confirmed findings in scope', next: 'Independent review', icon: 'shield-check' },
      { id: 'review', title: 'Independent review', doWhat: 'The review skill', where: 'Existing agent', done: 'Findings recorded on the exact commit', next: 'Prepare evidence', icon: 'eye' },
    ],
  },
]

export const DETECTED = [
  { icon: 'github', what: '.github/workflows/ci.yml', found: 'build, test, e2e and deploy-preview jobs' },
  { icon: 'terminal', what: 'package.json', found: 'test and lint scripts' },
  { icon: 'file-earmark-text', what: '.github/pull_request_template.md', found: 'a Testing section' },
  { icon: 'people', what: 'CODEOWNERS', found: 'owners for src/billing and src/auth' },
  { icon: 'journal-code', what: '.claude/skills/review', found: 'a review skill your agents already use' },
  { icon: 'robot', what: 'Connected agents', found: 'claude-code, codex, cursor and copilot' },
]

/** In order: each kit builds on the one before it. */
export const KITS = [
  { n: 1, name: 'Coordinate existing agents', icon: 'people', does: 'Agents announce what they edit, hand work over and leave a record anyone can catch up from.', detected: '4 connected agents, CODEOWNERS', unsupported: 'Cursor reports after an edit; it cannot be refused one.' },
  { n: 2, name: 'Get changes ready for review', icon: 'terminal', does: 'Checks run on the dev machine first, review findings go back to the author, and the PR carries current evidence.', detected: 'npm test and lint, the review skill, the PR template', unsupported: 'CI only skips work it has been configured to accept. Nothing is skipped by default.' },
  { n: 3, name: 'Add a security review', icon: 'shield-check', does: 'A pinned scan skill runs before review on one-way doors; its findings become evidence on the change.', detected: 'No security tooling found', unsupported: 'A scan covers its stated scope at one commit. It is not continuous security coverage.' },
]

/** Where each existing CI job can run. "Unknown" stays unknown until someone checks. */
export const CI_FIT = [
  { job: 'build', fit: 'Can run locally', tone: 'ok' as Tone },
  { job: 'test', fit: 'Reusable with a verifier', tone: 'ok' as Tone },
  { job: 'e2e', fit: 'Unknown: needs browsers and services', tone: 'muted' as Tone },
  { job: 'deploy-preview', fit: 'Must stay in CI', tone: 'warn' as Tone },
]

export type Lens = 'work' | 'lifecycle' | 'evidence' | 'maintenance' | 'security'
export const LENSES: { id: Lens; label: string; icon: string; caution: string }[] = [
  { id: 'work', label: 'Work', icon: 'people', caution: 'No declared work does not mean unused.' },
  { id: 'lifecycle', label: 'Lifecycle', icon: 'diagram-3', caution: 'Code can be in several versions and stages at once.' },
  { id: 'evidence', label: 'Evidence', icon: 'patch-check', caution: 'Old evidence stays true history but may not apply now.' },
  { id: 'maintenance', label: 'Maintenance', icon: 'tools', caution: '“Dormant” only from declared owners and change frequency.' },
  { id: 'security', label: 'Security', icon: 'shield-check', caution: 'A clean scan covers its stated scope, nothing more.' },
]

export interface Fact { text: string; tone: Tone }
export interface Area { path: string; work: string[]; uses: string[]; facts: Record<Lens, Fact[]> }
export const AREAS: Area[] = [
  { path: 'src/billing', work: ['w1', 'w2'], uses: ['src/invoices', 'src/api/export'], facts: {
    work: [{ text: '2 changes planned', tone: 'brand' }, { text: '1 overlap', tone: 'warn' }],
    lifecycle: [{ text: 'Candidate in #42', tone: 'brand' }, { text: 'Last merged 3 days ago', tone: 'muted' }],
    evidence: [{ text: 'Tests current on #42', tone: 'ok' }, { text: 'Review: 2 fixed', tone: 'ok' }],
    maintenance: [{ text: 'Owner maria (CODEOWNERS)', tone: 'muted' }, { text: 'Changes weekly', tone: 'muted' }],
    security: [{ text: 'Scanned 1 hr ago on #42', tone: 'ok' }, { text: '0 confirmed in scope', tone: 'ok' }],
  } },
  { path: 'src/invoices', work: ['w2'], uses: ['src/billing'], facts: {
    work: [{ text: '1 change in review', tone: 'brand' }],
    lifecycle: [{ text: 'Draft PR #45', tone: 'brand' }, { text: 'Deployed: 0.1.3', tone: 'ok' }],
    evidence: [{ text: 'Tests stale on 7be03f1', tone: 'warn' }],
    maintenance: [{ text: 'No declared owner', tone: 'muted' }],
    security: [{ text: 'Not scanned in 30 days', tone: 'warn' }],
  } },
  { path: 'src/webhooks', work: ['w3'], uses: ['src/queue'], facts: {
    work: [{ text: 'Docs change blocked', tone: 'bad' }],
    lifecycle: [{ text: 'Deployed: 0.1.3', tone: 'ok' }],
    evidence: [{ text: 'No tests found', tone: 'bad' }],
    maintenance: [{ text: 'Owner sam', tone: 'muted' }],
    security: [{ text: 'Never scanned', tone: 'warn' }],
  } },
  { path: 'src/auth', work: [], uses: [], facts: {
    work: [{ text: 'No declared work', tone: 'muted' }],
    lifecycle: [{ text: 'Deployed: 0.1.3', tone: 'ok' }],
    evidence: [{ text: 'Tests current on main', tone: 'ok' }],
    maintenance: [{ text: 'Owner sam (CODEOWNERS)', tone: 'muted' }, { text: 'Last change 41 days ago', tone: 'muted' }],
    security: [{ text: 'Scanned 21 days ago', tone: 'ok' }, { text: 'oauth/ excluded from scope', tone: 'warn' }],
  } },
  { path: 'src/queue', work: ['w6'], uses: [], facts: {
    work: [{ text: 'Follow-up drafted', tone: 'ok' }],
    lifecycle: [{ text: 'Merged in #40', tone: 'ok' }, { text: 'Not yet deployed', tone: 'muted' }],
    evidence: [{ text: 'CI passed on merge commit', tone: 'ok' }],
    maintenance: [{ text: 'Owner sam', tone: 'muted' }],
    security: [{ text: 'Not scanned', tone: 'muted' }],
  } },
  { path: 'jobs/images', work: ['w5'], uses: [], facts: {
    work: [{ text: '1 change in checks', tone: 'brand' }],
    lifecycle: [{ text: 'Deployed: 0.1.3', tone: 'ok' }],
    evidence: [{ text: 'Local checks running', tone: 'brand' }],
    maintenance: [{ text: 'Owner sam', tone: 'muted' }],
    security: [{ text: 'Advisory: libvips < 8.16.1', tone: 'bad' }],
  } },
]

/** The five comparison tasks, identical for every variation. */
export const TASKS = [
  'Find what needs your decision',
  'Understand a change before its PR exists (the webhook docs)',
  'Explain why PR #42 is, or isn’t, ready',
  'Activate an existing process',
  'Name one useful thing AgentSCM contributed this week',
]
