import {
  ArrowLeftOutlined,
  BranchesOutlined,
  CheckCircleOutlined,
  ClockCircleOutlined,
  ExportOutlined,
  LinkOutlined,
  ReloadOutlined,
  TeamOutlined,
  WarningOutlined,
} from '@ant-design/icons'
import { Alert, Breadcrumb, Button, Card, Descriptions, Empty, List, Skeleton, Space, Tag, Typography, type DescriptionsProps } from 'antd'
import { useEffect, useMemo, useState, type ReactNode } from 'react'
import { ApiError } from '../api.ts'
import {
  briefingPath,
  currentLeadParticipant,
  fetchBriefing,
  fetchStandupWork,
  safeCommitHref,
  safeForgeIssueHref,
  workHref,
  workIdFromHash,
  type BriefingQuery,
  type StandupBriefing,
  type WorkCheckpoint,
  type WorkItem,
  type WorkSourceRef,
} from '../standup.ts'
import { Chips, ExternalLink } from './bits.tsx'

type Load<T> = { kind: 'loading' } | { kind: 'ready'; value: T; fetchedAt: number } | { kind: 'error'; message: string; missing?: boolean }

const STATE_COLOUR: Record<WorkItem['state'], string> = {
  active: 'processing',
  blocked: 'error',
  paused: 'warning',
  finished: 'success',
  handover: 'purple',
}

const CLAIM_COLOUR: Record<WorkItem['claim_status'], string> = {
  active: 'success',
  stale: 'warning',
  unknown: 'default',
}

const EVENT_LABEL: Record<string, string> = {
  work_started: 'started work',
  work_joined: 'joined work',
  work_takeover: 'took over the claim',
  work_checkpoint: 'reported a checkpoint',
}

const localDate = (at: number): string => new Date(at * 1000).toLocaleString()

function WorkTags({ item }: { item: WorkItem }) {
  return (
    <Space size={[4, 4]} wrap>
      <Tag color={STATE_COLOUR[item.state]}>{item.state}</Tag>
      <Tag color={CLAIM_COLOUR[item.claim_status]}>claim {item.claim_status}</Tag>
      {item.claim_status !== 'active' && <Tag icon={<WarningOutlined />}>still claimed</Tag>}
    </Space>
  )
}

function WorkLink({ id, children }: { id: string; children?: ReactNode }) {
  return <a href={workHref(id)}>{children ?? <code>{id}</code>}</a>
}

function WorkSource({ item }: { item: WorkItem }) {
  const href = safeForgeIssueHref(item.source)
  if (item.source.kind === 'native') return <>Plain Git · <code>{item.source.request_key}</code></>
  const text = <>{item.source.provider} · {item.source.repository} · issue <code>{item.source.issue}</code></>
  return href ? <ExternalLink href={href}>{text} <ExportOutlined /></ExternalLink> : text
}

function SourceReference({ reference, item }: { reference: WorkSourceRef; item: WorkItem }) {
  if (reference.kind === 'work') return <WorkLink id={reference.work_id} />
  if (reference.kind === 'repository_path') return <code>{reference.path}{reference.line === null ? '' : `:${reference.line}`}</code>
  const href = safeCommitHref(item.source, reference.oid)
  const text = <code>{reference.oid.slice(0, 12)}{reference.path ? ` · ${reference.path}` : ''}</code>
  return href ? <ExternalLink href={href}>{text} <ExportOutlined /></ExternalLink> : text
}

function Checkpoint({ checkpoint, item }: { checkpoint: WorkCheckpoint; item: WorkItem }) {
  return (
    <Card size="small" title={<Space><Tag>Agent report</Tag><span>{checkpoint.kind}</span></Space>}>
      <Typography.Paragraph>{checkpoint.summary}</Typography.Paragraph>
      {checkpoint.next_action && <Typography.Paragraph><strong>Next action:</strong> {checkpoint.next_action}</Typography.Paragraph>}
      {checkpoint.source_refs.length > 0 && (
        <Space size={[12, 6]} wrap>
          <LinkOutlined />
          {checkpoint.source_refs.map((reference, index) => <SourceReference key={`${reference.kind}-${index}`} reference={reference} item={item} />)}
        </Space>
      )}
      <Typography.Text type="secondary">Reported by {checkpoint.agent} · {checkpoint.session} · {localDate(checkpoint.at)}</Typography.Text>
    </Card>
  )
}

function WorkSummary({ item }: { item: WorkItem }) {
  const lead = currentLeadParticipant(item)
  return (
    <List.Item>
      <List.Item.Meta
        title={<Space size={[8, 4]} wrap><WorkLink id={item.id}>{item.title}</WorkLink><WorkTags item={item} /></Space>}
        description={
          <Space direction="vertical" size={4}>
            <span>Current lead: {lead.login} · {item.lead_agent}</span>
            <span>Created by {item.owner_login}</span>
            {item.blocker && <span><Tag color="error">Agent-reported blocker</Tag>{item.blocker.summary}</span>}
            {item.next_action && <span><Tag>Agent-reported next action</Tag>{item.next_action}</span>}
            <span>Last check-in {localDate(item.last_checkin_at)}</span>
            {item.scope.length > 0 && <Chips items={item.scope} />}
          </Space>
        }
      />
    </List.Item>
  )
}

function BriefingView({ token }: { token: string }) {
  const [queries, setQueries] = useState<BriefingQuery[]>([{ limit: 50 }])
  const [page, setPage] = useState(0)
  const query = queries[page]!
  const requestPath = useMemo(() => briefingPath(query), [query])
  const [load, setLoad] = useState<Load<StandupBriefing>>({ kind: 'loading' })
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    const controller = new AbortController()
    void fetchBriefing(token, controller.signal, query).then(
      (value) => setLoad({ kind: 'ready', value, fetchedAt: Date.now() }),
      (error: unknown) => {
        if (!controller.signal.aborted) setLoad({ kind: 'error', message: error instanceof Error ? error.message : 'Unknown error' })
      },
    )
    return () => controller.abort()
  }, [token, query, requestPath, attempt])

  if (load.kind === 'loading') return <div className="workflow-loading"><Skeleton active paragraph={{ rows: 8 }} /></div>
  if (load.kind === 'error') return <Alert showIcon type="error" message="Could not load the stand-up" description={load.message} action={<Button icon={<ReloadOutlined />} onClick={() => { setLoad({ kind: 'loading' }); setAttempt((value) => value + 1) }}>Retry</Button>} />

  const briefing = load.value
  const next = () => {
    if (briefing.complete) return
    const nextQuery: BriefingQuery = {
      after: briefing.next_after,
      ...(briefing.next_work_after ? { workAfter: briefing.next_work_after } : {}),
      workDone: briefing.next_work_done,
      through: briefing.through,
      limit: query.limit,
    }
    setQueries((all) => [...all.slice(0, page + 1), nextQuery])
    setLoad({ kind: 'loading' })
    setPage((value) => value + 1)
  }

  return (
    <Space direction="vertical" size="large" style={{ width: '100%' }}>
      <Typography.Text type="secondary"><ClockCircleOutlined /> Briefing fetched {new Date(load.fetchedAt).toLocaleString()}</Typography.Text>
      {briefing.retention_gap && <Alert showIcon type="warning" message="Part of the acknowledged history is no longer retained" description="The current work snapshot is complete for this page traversal, but the change history has a retention gap." />}

      <section aria-labelledby="standup-current-heading">
        <div className="workflow-section-heading">
          <div>
            <h2 id="standup-current-heading">Current unfinished work</h2>
            <p>Includes old, stale and unknown claims. Stale or unknown means the work remains claimed; it does not make the work available.</p>
          </div>
          <span>{briefing.items.length} on this page</span>
        </div>
        {briefing.items.length === 0
          ? <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No unfinished work on this page." />
          : <List dataSource={briefing.items} renderItem={(item) => <WorkSummary key={item.id} item={item} />} />}
      </section>

      <section aria-labelledby="standup-changes-heading">
        <div className="workflow-section-heading">
          <div>
            <h2 id="standup-changes-heading">Changes since the last acknowledged briefing</h2>
            <p>Ordered through a fixed event watermark. This read-only view inspects the boundary and never acknowledges it.</p>
          </div>
          <span>{briefing.events.length} on this page</span>
        </div>
        {briefing.events.length === 0 ? (
          <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No retained changes on this page." />
        ) : (
          <List
            dataSource={briefing.events}
            renderItem={(event) => (
              <List.Item key={event.seq}>
                <List.Item.Meta
                  title={<><WorkLink id={event.work_id}>{event.item.title}</WorkLink> · {EVENT_LABEL[event.kind]}</>}
                  description={<Space direction="vertical" size={2}><span>Lead at this change: {currentLeadParticipant(event.item).login} · {event.item.lead_agent}</span><span>Created by {event.item.owner_login}</span><span>{localDate(event.at)} · sequence {event.seq}</span></Space>}
                />
              </List.Item>
            )}
          />
        )}
      </section>

      <div className="workflow-pagination" aria-label="Stand-up pages">
        <Button disabled={page === 0} onClick={() => { setLoad({ kind: 'loading' }); setPage((value) => value - 1) }}>Previous</Button>
        <span>Page {page + 1}</span>
        <Button disabled={briefing.complete} onClick={next}>Next</Button>
      </div>
    </Space>
  )
}

type DescriptionItem = NonNullable<DescriptionsProps['items']>[number]

function relations(key: string, label: string, ids: string[]): DescriptionItem[] {
  if (ids.length === 0) return []
  return [{ key, label, children: <Space size={[8, 4]} wrap>{ids.map((id) => <WorkLink key={id} id={id} />)}</Space> }]
}

export function WorkMetadata({ item }: { item: WorkItem }) {
  const lead = currentLeadParticipant(item)
  const items: DescriptionItem[] = [
    { key: 'source', label: 'Source', children: <WorkSource item={item} /> },
    { key: 'repository', label: 'Repository', children: <code>{item.repository}</code> },
    { key: 'creator', label: 'Creator', children: item.owner_login },
    { key: 'lead', label: 'Current lead', children: <>{lead.login} · {item.lead_agent} · <code>{item.lead_session}</code></> },
    { key: 'checkin', label: 'Last check-in', children: localDate(item.last_checkin_at) },
    { key: 'generation', label: 'Generation', children: item.generation },
    ...(item.branch ? [{ key: 'branch', label: 'Branch', children: <><BranchesOutlined /> <code>{item.branch}</code></> }] : []),
    ...(item.worktree ? [{ key: 'worktree', label: 'Worktree', children: <code>{item.worktree}</code> }] : []),
    ...(item.next_action ? [{ key: 'next-action', label: 'Agent-reported next action', span: 'filled' as const, children: item.next_action }] : []),
    ...relations('depends-on', 'Depends on', item.depends_on),
    ...relations('related-work', 'Related work', item.related_work),
    { key: 'created', label: 'Created', children: localDate(item.created_at) },
    { key: 'updated', label: 'Updated', children: localDate(item.updated_at) },
  ]
  return <Descriptions bordered size="small" column={{ xs: 1, sm: 1, md: 2, lg: 2, xl: 2, xxl: 2 }} items={items} />
}

function WorkDetail({ id, token }: { id: string; token: string }) {
  const [load, setLoad] = useState<Load<WorkItem>>({ kind: 'loading' })
  const [attempt, setAttempt] = useState(0)
  useEffect(() => {
    const controller = new AbortController()
    void fetchStandupWork(id, token, controller.signal).then(
      (value) => setLoad({ kind: 'ready', value, fetchedAt: Date.now() }),
      (error: unknown) => {
        if (!controller.signal.aborted) setLoad({ kind: 'error', missing: error instanceof ApiError && error.status === 404, message: error instanceof Error ? error.message : 'Unknown error' })
      },
    )
    return () => controller.abort()
  }, [id, token, attempt])

  if (load.kind === 'loading') return <div className="workflow-loading"><Skeleton active paragraph={{ rows: 10 }} /></div>
  if (load.kind === 'error') {
    return <Alert showIcon type={load.missing ? 'warning' : 'error'} message={load.missing ? 'Work item not found' : 'Could not load this work item'} description={load.missing ? 'It may not exist or this token may no longer be allowed to inspect it.' : load.message} action={<Button onClick={() => { setLoad({ kind: 'loading' }); setAttempt((value) => value + 1) }}>Try again</Button>} />
  }
  const item = load.value
  return (
    <>
      <div className="workflow-page-heading">
        <div>
          <Button className="workflow-back" type="link" href="#coordination" icon={<ArrowLeftOutlined />}>Coordination</Button>
          <h1>{item.title}</h1>
          <p><code>{item.id}</code></p>
        </div>
        <WorkTags item={item} />
      </div>

      {item.claim_status !== 'active' && <Alert style={{ marginBottom: 20 }} showIcon type="warning" message={`Claim status: ${item.claim_status}`} description="This work remains claimed. Coordinate or use the authorized takeover flow before treating it as available." />}

      <WorkMetadata item={item} />

      <section style={{ marginTop: 24 }} aria-labelledby="standup-scope-heading">
        <div className="workflow-section-heading"><div><h2 id="standup-scope-heading">Declared scope</h2><p>A coordination claim, not a filesystem lock.</p></div><span>{item.scope.length}</span></div>
        {item.scope.length ? <Chips items={item.scope} max={12} /> : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No repository paths were declared." />}
      </section>

      <section style={{ marginTop: 24 }} aria-labelledby="standup-participants-heading">
        <div className="workflow-section-heading"><div><h2 id="standup-participants-heading"><TeamOutlined /> Participants</h2></div><span>{item.participants.length}</span></div>
        <List dataSource={item.participants} renderItem={(participant) => <List.Item><List.Item.Meta title={`${participant.login} · ${participant.agent}`} description={<><Tag>{participant.role}</Tag><code>{participant.session}</code></>} /></List.Item>} />
      </section>

      <section style={{ marginTop: 24 }} aria-labelledby="standup-checkpoint-heading">
        <div className="workflow-section-heading"><div><h2 id="standup-checkpoint-heading"><CheckCircleOutlined /> Latest checkpoint</h2><p>Self-reported coordination context; it is not proof of execution, review, acceptance or landing.</p></div></div>
        {item.blocker && (item.current?.kind !== 'blocker' || item.current.at !== item.blocker.at) && (
          <div style={{ marginBottom: 16 }}>
            <Alert showIcon type="error" message="Active agent-reported blocker" description={item.blocker.summary} />
          </div>
        )}
        {item.current ? <Checkpoint checkpoint={item.current} item={item} /> : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No checkpoint has been reported." />}
      </section>
    </>
  )
}

export function Standup({ token, repository }: { token: string; repository: string }) {
  const [workId, setWorkId] = useState(() => workIdFromHash(window.location.hash))
  const [refresh, setRefresh] = useState(0)
  useEffect(() => {
    const changed = () => setWorkId(workIdFromHash(window.location.hash))
    window.addEventListener('hashchange', changed)
    return () => window.removeEventListener('hashchange', changed)
  }, [])

  return (
    <div className="workflow-page" aria-label="Agent stand-up">
      <Breadcrumb className="workflow-breadcrumb" items={workId ? [
        { title: <a href="#coordination">Coordination</a> },
        { title: <span className="workflow-breadcrumb-repo">{repository}</span> },
        { title: <code>{workId}</code> },
      ] : [
        { title: 'Coordination' },
        { title: <span className="workflow-breadcrumb-repo">{repository}</span> },
      ]} />
      {workId ? <WorkDetail key={`${workId}-${refresh}`} id={workId} token={token} /> : (
        <>
          <div className="workflow-page-heading">
            <div>
              <h1>Agent stand-up</h1>
              <p>Current shared engineering work and a bounded catch-up through one fixed watermark.</p>
            </div>
            <Space>
              <Tag className="workflow-readonly" icon={<ClockCircleOutlined />}>Read-only · viewer-local time</Tag>
              <Button icon={<ReloadOutlined />} onClick={() => setRefresh((value) => value + 1)}>Refresh</Button>
            </Space>
          </div>
          <BriefingView key={refresh} token={token} />
        </>
      )}
    </div>
  )
}
