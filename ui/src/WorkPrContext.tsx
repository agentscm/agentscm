import { ContextFacts } from './PrContextPanel.tsx'
import { fetchContextJson, fetchPrContext, validHead, type PrContext } from './prContext.ts'
import { validWorkId, type WorkItem } from './standup.ts'
import { Ago, Box, Empty, Notice, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'

export interface WorkPrLinkView {
  work_id: string
  repository: string
  repository_id: number
  generation: number
  updated_seq: number
  snapshot_status: 'matches_work' | 'work_changed'
  link: { target: { repository_id: number; number: number; head_sha: string }; author: string; agent: string; generation: number; linked_at: number; event_seq: number } | null
}
const record = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Invalid Work relationship')
  return value as Record<string, unknown>
}
const integer = (value: unknown): number => { if (typeof value !== 'number' || !Number.isSafeInteger(value) || value <= 0) throw new Error('Invalid Work relationship identity'); return value }
const text = (value: unknown): string => { if (typeof value !== 'string' || !value || value.length > 255 || /[\p{Cc}\p{Cf}]/u.test(value)) throw new Error('Invalid Work relationship label'); return value }

export function decodeWorkPrLink(value: unknown, item: Pick<WorkItem, 'id' | 'repository' | 'generation' | 'updated_seq'>): WorkPrLinkView {
  const row = record(value)
  if (!validWorkId(item.id) || row.work_id !== item.id || row.repository !== item.repository) throw new Error('Work relationship identity mismatch')
  const repository_id = integer(row.repository_id), generation = integer(row.generation), updated_seq = integer(row.updated_seq)
  if (row.snapshot_status !== 'matches_work' && row.snapshot_status !== 'work_changed') throw new Error('Work relationship was not compared')
  if ((row.snapshot_status === 'matches_work') !== (updated_seq === item.updated_seq) || (row.snapshot_status === 'matches_work' && generation !== item.generation)) throw new Error('Work relationship sequence mismatch')
  if (row.head_status !== 'not_compared') throw new Error('Unexpected Work relationship comparison')
  let link: WorkPrLinkView['link'] = null
  if (row.link !== null) {
    const raw = record(row.link), target = record(raw.target)
    if (integer(target.repository_id) !== repository_id || typeof target.head_sha !== 'string' || !validHead(target.head_sha) || target.head_sha !== target.head_sha.toLowerCase() || integer(raw.generation) > generation || integer(raw.event_seq) > updated_seq) throw new Error('Work relationship target mismatch')
    link = { target: { repository_id, number: integer(target.number), head_sha: target.head_sha }, author: text(raw.author), agent: text(raw.agent), generation: integer(raw.generation), linked_at: integer(raw.linked_at), event_seq: integer(raw.event_seq) }
  }
  return { work_id: item.id, repository: item.repository, repository_id, generation, updated_seq, snapshot_status: row.snapshot_status, link }
}

export async function fetchWorkPrContext(item: Pick<WorkItem, 'id' | 'repository' | 'generation' | 'updated_seq'>, token: string, signal: AbortSignal): Promise<{ relationship: WorkPrLinkView; context: PrContext | null }> {
  const path = `/v1/hub/standup/work/${encodeURIComponent(item.id)}/pr-link?expected_updated_seq=${item.updated_seq}`
  const relationship = decodeWorkPrLink(await fetchContextJson(path, token, signal), item)
  if (relationship.snapshot_status !== 'matches_work' || !relationship.link) return { relationship, context: null }
  const link = relationship.link.target
  const context = await fetchPrContext(item.repository, link.number, link.head_sha, token, signal)
  if (context.availability === 'available' && context.repository_id !== relationship.repository_id) throw new Error('Work PR repository identity mismatch')
  if (context.provider_evidence?.availability === 'observed' && context.provider_evidence.snapshot.repository_id !== relationship.repository_id) throw new Error('Work provider repository identity mismatch')
  return { relationship, context }
}

export function WorkPrContext({ item, token, refreshWork }: { item: WorkItem; token: string; refreshWork: () => void }) {
  const poll = usePoll((signal) => fetchWorkPrContext(item, token, signal), 0, `work-pr|${token}|${item.id}|${item.generation}|${item.updated_seq}`)
  return <Box icon="git" title="Linked pull request">
    {poll.kind === 'loading' ? <Skeleton rows={3} /> : poll.kind === 'error' && poll.status === 400 ? <Empty icon="git" title="PR context request was rejected">The Hub could not accept the request. <button className="btn btn-link p-0" onClick={refreshWork}>Reload Work detail</button> to check the current Work item.</Empty> : poll.kind === 'error' || poll.refreshError ? <Notice tone="warn" title="PR relationship is unavailable" action={<button className="btn btn-sm btn-outline-secondary" onClick={poll.retry}>Try again</button>}>{poll.kind === 'error' ? poll.message : poll.refreshError} Previous relationship and context are hidden until access can be checked again.</Notice> : <LinkedFacts relationship={poll.value.relationship} context={poll.value.context} refresh={poll.retry} refreshWork={refreshWork} />}
  </Box>
}
function LinkedFacts({ relationship, context, refresh, refreshWork }: { relationship: WorkPrLinkView; context: PrContext | null; refresh: () => void; refreshWork: () => void }) {
  if (relationship.snapshot_status === 'work_changed') return <Notice tone="warn" title="Work changed since this detail loaded" action={<button className="btn btn-sm btn-outline-secondary" onClick={refreshWork}>Reload Work detail</button>}>The relationship describes a different Work sequence. Reload Work before inspecting its linked PR.</Notice>
  const link = relationship.link
  if (!link) return <Empty icon="git" title="No explicit PR relationship">The lead agent can record a PR number and exact head. Branch names and shared paths do not establish a relationship.</Empty>
  return <>
    <p>Caller-asserted relationship · PR #{link.target.number} at head <code title={link.target.head_sha}>{link.target.head_sha.slice(0, 8)}</code>.</p>
    <p className="small muted">Linked by {link.author} · {link.agent} · <Ago at={link.linked_at} /> · Work sequence {relationship.updated_seq}. This is not a provider-head or approval assertion.</p>
    {context?.availability === 'available' ? <ContextFacts data={context} refresh={refresh} headBasis="work_link" /> : <Notice title="Linked PR context is unavailable">The PR is not currently available in the retained mirror. <button className="btn btn-link p-0" onClick={refresh}>Refresh relationship and context</button>.</Notice>}
  </>
}
