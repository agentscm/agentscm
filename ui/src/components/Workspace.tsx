import { ArrowLeftOutlined, GithubOutlined, GitlabOutlined, PlusOutlined, ReloadOutlined, StopOutlined, TeamOutlined } from '@ant-design/icons'
import { Alert, Breadcrumb, Button, Checkbox, Collapse, Empty, Form, Input, InputNumber, Select, Skeleton, Space, Table, Tabs, Tag, Typography } from 'antd'
import { useCallback, useEffect, useRef, useState } from 'react'
import { WorkspaceWorkflow } from './SkillWorkflow.tsx'
import { Header } from './Header.tsx'
import type { ThemePref } from '../storage.ts'
import { identityKey, workspaceMutation, workspacePendingMutationEvent, workspacePendingMutations, WorkspaceError, workspaceError, workspaceRequest, type PendingWorkspaceMutation, type WorkspaceMember, type WorkspaceProfile, type WorkspaceRepository, type WorkspaceRun, type WorkspaceSession, type WorkspaceSettings, type WorkspaceSubject } from '../workspace.ts'

const ROLES = ['reader', 'contributor', 'reviewer', 'integrator', 'administrator']
const roleOptions = ROLES.map(value => ({ value, label: value.charAt(0).toUpperCase() + value.slice(1) }))
const activeRun = (state: string) => state === 'queued' || state === 'running'
const tone = (state: string) => state === 'succeeded' ? 'success' : ['failed', 'unknown', 'stale'].includes(state) ? 'error' : activeRun(state) ? 'processing' : 'default'
const sectionFromHash = (hash: string) => { const section = hash.split('/')[1]; return section === 'members' || section === 'settings' || section === 'workflow' ? section : 'repositories' }

export function WorkspaceLogin({ onBack }: { onBack?: () => void }) {
  const [providers, setProviders] = useState<string[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [refresh, setRefresh] = useState(0)
  useEffect(() => {
    const controller = new AbortController()
    workspaceRequest<{ providers: string[] }>('/login', { signal: controller.signal }).then(result => setProviders(result.providers.filter(provider => provider === 'github' || provider === 'gitlab'))).catch(error => { if (!controller.signal.aborted) setError(workspaceError(error)) })
    return () => controller.abort()
  }, [refresh])
  return <div className="workspace-login">
    <TeamOutlined className="workspace-login-icon" />
    <h2>Sign in to your workspace</h2>
    <p>Use your forge account to manage members and run workflows. Your operator must add your numeric forge identity first.</p>
    {error ? <Alert type="warning" showIcon message={error} action={<Button onClick={() => { setError(null); setRefresh(value => value + 1) }}>Retry</Button>} /> : providers === null ? <Skeleton active paragraph={{ rows: 2 }} /> : providers.length ? <Space wrap>{providers.map(provider => <Button key={provider} type="primary" icon={provider === 'github' ? <GithubOutlined /> : <GitlabOutlined />} href={`/v1/hub/login/${provider}?workspace=1`}>Continue with {provider === 'github' ? 'GitHub' : 'GitLab'}</Button>)}</Space> : <Alert type="info" showIcon message="Workspace sign-in is not configured on this hub." />}
    {onBack && <Button type="link" onClick={onBack}>Open read-only repository status</Button>}
  </div>
}

export function Workspace({ onSessionEnded }: { onSessionEnded?: () => void }) {
  const [session, setSession] = useState<WorkspaceSession | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [signedOut, setSignedOut] = useState(false)
  const [refresh, setRefresh] = useState(0)
  const [hash, setHash] = useState(() => window.location.hash)
  const section = sectionFromHash(hash)
  const parts = hash.split('/')
  const workflowRepository = (() => { try { return decodeURIComponent(parts[2] ?? '') } catch { return '' } })()
  const workflowRun = (() => { try { return decodeURIComponent(parts[3] ?? '') } catch { return '' } })()
  useEffect(() => { const listener = () => setHash(window.location.hash); window.addEventListener('hashchange', listener); return () => window.removeEventListener('hashchange', listener) }, [])
  useEffect(() => {
    const controller = new AbortController()
    const load = () => workspaceRequest<WorkspaceSession>('/session', { signal: controller.signal }).then(result => { if (!controller.signal.aborted) { setSession(result); setSignedOut(false); setError(null) } }).catch(error => {
      if (controller.signal.aborted) return
      if (error instanceof WorkspaceError && error.status === 401) { setSession(null); setSignedOut(true) }
      else {
        if (error instanceof WorkspaceError && (error.status === 403 || error.status === 404)) setSession(null)
        setError(workspaceError(error))
      }
    })
    void load(); const timer = window.setInterval(() => { void load() }, 15000)
    return () => { controller.abort(); window.clearInterval(timer) }
  }, [refresh])
  const reload = useCallback(() => setRefresh(value => value + 1), [])
  const signOut = async () => {
    if (!session) return
    try { await workspaceRequest('/session', { method: 'DELETE', csrf: session.csrf_token }); setSession(null); setSignedOut(true); onSessionEnded?.() } catch (error) { setError(workspaceError(error)) }
  }
  return <section className="workflow-page workspace-page" aria-label="Workspace">
    <Breadcrumb className="workflow-breadcrumb" items={[{ title: 'Hub' }, { title: 'Workspace' }, ...(session ? [{ title: section === 'workflow' ? 'Workflow run' : section === 'repositories' ? 'Repositories & execution' : section === 'members' ? 'Members' : 'Settings' }] : [])]} />
    {signedOut ? <WorkspaceLogin /> : <>
      <div className="workflow-page-heading">
        <div><h1>{session?.workspace.name ?? 'Workspace'}</h1><p className="workspace-subtitle">{session ? `Signed in as ${session.actor.user.login} · ${session.actor.roles.join(', ')}` : 'Shared repositories, members and workflow execution.'}</p></div>
        <Space><Button aria-label="Refresh workspace" icon={<ReloadOutlined />} onClick={reload} />{session && <Button onClick={() => { void signOut() }}>Sign out</Button>}</Space>
      </div>
      {error && <Alert className="workspace-alert" type="warning" showIcon message={error} />}
      {!session && !error && <Skeleton active paragraph={{ rows: 6 }} />}
      {session && <>
        {session.synthetic_fixture && <Alert type="warning" showIcon message="Synthetic development workspace" description="These are development fixtures. Workflow execution, forge sign-in and mutations are not performed." />}
        <PendingOperations session={session} />
        <Tabs activeKey={section === 'workflow' ? 'repositories' : section} onChange={value => { window.location.hash = `workspace/${value}` }} items={[{ key: 'repositories', label: 'Repositories & execution' }, { key: 'members', label: 'Members' }, { key: 'settings', label: 'Settings' }]} />
        {section === 'workflow' && (workflowRepository && workflowRun ? <WorkspaceWorkflow key={`${workflowRepository}/${workflowRun}`} repository={workflowRepository} workspaceRunId={workflowRun} /> : <Alert type="error" message="Invalid workspace run link. Open Repositories & execution to choose a run." />)}
        {section === 'members' && <Members key={refresh} session={session} onChanged={reload} />}
        {section === 'settings' && <Settings key={refresh} session={session} onChanged={reload} />}
        {section === 'repositories' && <Execution key={refresh} session={session} />}
      </>}
    </>}
  </section>
}

const pendingAction = (entry: PendingWorkspaceMutation): { label: string; capability: keyof WorkspaceSession['capabilities'] } => {
  if (entry.path.endsWith('/approve')) return { label: 'Native approval', capability: 'approve_revisions' }
  if (entry.path.endsWith('/integrate')) return { label: 'Native landing', capability: 'integrate_revisions' }
  if (entry.path.endsWith('/cancel')) return { label: 'Run cancellation', capability: 'execute_workflows' }
  if (entry.path.endsWith('/runs')) return { label: 'Workflow start', capability: 'execute_workflows' }
  if (entry.path === '/members') return { label: 'Membership change', capability: 'manage_workspace' }
  return { label: 'Workspace settings change', capability: 'manage_workspace' }
}

const pendingSubject = (entry: PendingWorkspaceMutation): WorkspaceSubject | null => {
  const subject = entry.body.subject
  if (!subject || typeof subject !== 'object' || Array.isArray(subject)) return null
  const fields = subject as Record<string, unknown>
  if (typeof fields.change_id !== 'string' || typeof fields.change_version !== 'number' || typeof fields.context_digest !== 'string' || typeof fields.revision !== 'string' || typeof fields.requirement_digest !== 'string') return null
  return fields as unknown as WorkspaceSubject
}

function PendingOperations({ session }: { session: WorkspaceSession }) {
  const [entries, setEntries] = useState(() => workspacePendingMutations(session))
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<{ message: string; returnedId: string | null } | null>(null)
  useEffect(() => {
    const load = () => setEntries(workspacePendingMutations(session))
    load(); window.addEventListener(workspacePendingMutationEvent, load)
    return () => window.removeEventListener(workspacePendingMutationEvent, load)
  }, [session])
  const retry = async (entry: PendingWorkspaceMutation) => {
    const action = pendingAction(entry)
    if (!session.capabilities[action.capability]) return
    setBusy(entry.requestId); setError(null); setNotice(null)
    try {
      const result = await workspaceMutation<Record<string, unknown> | undefined>(session, entry.path, entry.body, { method: entry.method, ...(entry.expectation ? { expectation: entry.expectation } : {}) })
      const returnedId = result ? (typeof result.approval_id === 'string' ? result.approval_id : typeof result.operation_id === 'string' ? result.operation_id : typeof result.run_id === 'string' ? result.run_id : null) : null
      setNotice({ message: `${action.label} returned a conclusive result.`, returnedId })
    } catch (error) { setError(workspaceError(error)) } finally { setBusy(null) }
  }
  if (entries.length === 0 && !notice) return null
  return <section className="workspace-editor" aria-label="Pending operation recovery">
    <h2>Pending operation recovery</h2>
    <p>These requests may have reached the Hub. This tab retains only their non-secret fields and request IDs. Nothing is retried automatically.</p>
    {error && <Alert type="error" showIcon message={error} />}
    {notice && <Alert type="success" showIcon message={notice.message} description={notice.returnedId ? <span>Returned ID <Typography.Text code copyable>{notice.returnedId}</Typography.Text></span> : undefined} closable onClose={() => setNotice(null)} />}
    <Space direction="vertical" size="middle" style={{ width: '100%' }}>
      {entries.map(entry => {
        const action = pendingAction(entry)
        const subject = pendingSubject(entry)
        const allowed = session.capabilities[action.capability]
        const generation = typeof entry.body.expected_generation === 'number' ? entry.body.expected_generation : null
        return <Alert key={`${entry.method}:${entry.path}`} type="warning" showIcon message={`${action.label} outcome unknown`} description={<div>
          {subject && <p>Exact revision <Typography.Text code copyable>{subject.revision}</Typography.Text> · Change version {subject.change_version}</p>}
          {entry.expectation?.kind === 'landing' && <p>Target <Typography.Text code>{entry.expectation.target}</Typography.Text></p>}
          <p>Request <Typography.Text code copyable>{entry.requestId}</Typography.Text>{generation !== null ? ` · Membership generation ${generation}` : ''}</p>
          <p>{allowed ? 'Retry sends the exact retained body with your current security token.' : `Your current roles do not allow this ${action.label.toLowerCase()} retry.`}</p>
        </div>} action={<Button disabled={!allowed || busy !== null} loading={busy === entry.requestId} onClick={() => { void retry(entry) }}>Retry exact request</Button>} />
      })}
    </Space>
  </section>
}

function Members({ session, onChanged }: { session: WorkspaceSession; onChanged: () => void }) {
  const [members, setMembers] = useState<WorkspaceMember[] | null>(null)
  const [cursor, setCursor] = useState<string | null>(null)
  const [generation, setGeneration] = useState(session.workspace.generation)
  const [editing, setEditing] = useState<WorkspaceMember | 'new' | null>(null)
  const [busy, setBusy] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [form] = Form.useForm()
  const canManage = session.capabilities.manage_workspace
  useEffect(() => {
    if (!canManage) return
    const controller = new AbortController()
    workspaceRequest<{ members: WorkspaceMember[]; next_cursor: string | null; generation: number }>('/members?limit=50', { signal: controller.signal }).then(result => { if (!controller.signal.aborted) { setMembers(result.members); setCursor(result.next_cursor); setGeneration(result.generation) } }).catch(error => { if (!controller.signal.aborted) setError(workspaceError(error)) })
    return () => controller.abort()
  }, [canManage])
  const edit = (member: WorkspaceMember | 'new') => {
    setEditing(member); setError(null)
    form.setFieldsValue(member === 'new' ? { provider: 'github', host: 'github.com', roles: ['reader'], repositories: '*', enabled: true, user_id: undefined } : { ...member.identity, roles: member.roles, repositories: member.repository_ids.join(', '), enabled: member.enabled })
  }
  const save = async (values: { provider: string; host: string; user_id: string; roles: string[]; repositories: string; enabled: boolean }) => {
    setBusy(true); setError(null)
    try {
      await workspaceMutation(session, '/members', { expected_generation: generation, identity: { provider: values.provider, host: values.host.trim(), user_id: Number(values.user_id) }, roles: values.roles, repository_ids: values.repositories.split(',').map(value => value.trim()).filter(Boolean), enabled: values.enabled }, { method: 'PUT' })
      onChanged()
    } catch (error) { setError(workspaceError(error)) } finally { setBusy(false) }
  }
  const loadMore = async () => {
    if (!cursor) return
    setLoading(true)
    try { const result = await workspaceRequest<{ members: WorkspaceMember[]; next_cursor: string | null; generation: number }>(`/members?limit=50&after=${encodeURIComponent(cursor)}`); if (result.generation !== generation) { setError('Membership changed while loading. Refresh the workspace to see the current list.'); return } setMembers(previous => [...(previous ?? []), ...result.members]); setCursor(result.next_cursor) } catch (error) { setError(workspaceError(error)) } finally { setLoading(false) }
  }
  if (!canManage) return <Alert type="info" showIcon message="Administrator access is required to view or change workspace members." />
  return <div className="workspace-section">
    <div className="workspace-toolbar"><div><h2>Members</h2><p>Roles grant separate permissions. Administrator access alone does not grant repository read or execution access.</p></div><Button type="primary" icon={<PlusOutlined />} disabled={!canManage || editing !== null} onClick={() => edit('new')}>Add member</Button></div>
    {error && <Alert type="error" showIcon message={error} />}
    {editing !== null && <div className="workspace-editor"><h3>{editing === 'new' ? 'Add forge member' : 'Edit member'}</h3>
      <Form form={form} layout="vertical" onFinish={values => { void save(values) }} disabled={busy || !canManage}>
        <div className="workspace-form-grid"><Form.Item name="provider" label="Forge" rules={[{ required: true }]}><Select disabled={editing !== 'new'} options={[{ value: 'github', label: 'GitHub' }, { value: 'gitlab', label: 'GitLab' }]} /></Form.Item><Form.Item name="host" label="Forge host" rules={[{ required: true, whitespace: true }]}><Input disabled={editing !== 'new'} placeholder="github.com" /></Form.Item><Form.Item name="user_id" label="Numeric forge user ID" rules={[{ required: true }, { validator: (_, value: unknown) => typeof value === 'string' || typeof value === 'number' ? /^[1-9][0-9]*$/.test(String(value)) && Number.isSafeInteger(Number(value)) ? Promise.resolve() : Promise.reject(new Error('Enter a positive, safe numeric user ID.')) : Promise.reject(new Error('Enter a numeric user ID.')) }]}><Input disabled={editing !== 'new'} inputMode="numeric" placeholder="12345" /></Form.Item></div>
        <Form.Item name="roles" label="Explicit roles" rules={[{ required: true, type: 'array', min: 1 }]} extra="Reader: inspect repositories. Contributor: start and cancel runs. Administrator: manage members and settings. Reviewer and integrator retain their native permissions."><Select mode="multiple" options={roleOptions} /></Form.Item>
        <Form.Item name="repositories" label="Repository IDs" rules={[{ required: true, whitespace: true }]} extra="Comma-separated repository IDs, or * for all configured repositories."><Input placeholder="owner/repository" /></Form.Item>
        <Form.Item name="enabled" valuePropName="checked"><Checkbox>Membership enabled</Checkbox></Form.Item>
        <Space wrap><Button type="primary" htmlType="submit" loading={busy}>Save member</Button><Button disabled={busy} onClick={() => setEditing(null)}>Cancel</Button></Space>
      </Form>
    </div>}
    <Table<WorkspaceMember> className="workflow-table workspace-desktop-table" rowKey={member => identityKey(member.identity)} loading={members === null && !error} dataSource={members ?? []} pagination={false} scroll={{ x: 690 }} locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No members available to display." /> }} columns={[
      { title: 'Forge identity', render: (_, member) => <div><strong>{member.identity.provider} · {member.identity.host}</strong><div><Typography.Text code>{member.identity.user_id}</Typography.Text></div></div> },
      { title: 'Roles', render: (_, member) => <Space wrap>{member.roles.map(role => <Tag key={role}>{role}</Tag>)}</Space> },
      { title: 'Repositories', render: (_, member) => member.repository_ids.join(', ') },
      { title: 'Status', render: (_, member) => <Tag color={member.enabled ? 'success' : 'default'}>{member.enabled ? 'Enabled' : 'Disabled'}</Tag> },
      { title: 'Action', render: (_, member) => <Button disabled={!canManage || editing !== null} onClick={() => edit(member)}>Edit</Button> },
    ]} />
    <div className="workspace-mobile-list">
      {members === null && !error ? <Skeleton active paragraph={{ rows: 3 }} /> : members?.length === 0 ? <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No members available to display." /> : <ul aria-label="Workspace members">{members?.map(member => <li key={identityKey(member.identity)}>
        <div className="workspace-mobile-row-heading"><div><strong>{member.identity.provider} · {member.identity.host}</strong><div><Typography.Text code>{member.identity.user_id}</Typography.Text></div></div><Tag color={member.enabled ? 'success' : 'default'}>{member.enabled ? 'Enabled' : 'Disabled'}</Tag></div>
        <Space wrap>{member.roles.map(role => <Tag key={role}>{role}</Tag>)}</Space>
        <p className="workspace-note">Repositories: {member.repository_ids.join(', ')}</p>
        <Button aria-label={`Edit member ${member.identity.user_id}`} disabled={!canManage || editing !== null} onClick={() => edit(member)}>Edit member</Button>
      </li>)}</ul>}
    </div>
    {cursor && <Button loading={loading} onClick={() => { void loadMore() }}>Load more members</Button>}
    <p className="workspace-note">Disable a member to revoke workspace access. The last enabled administrator must remain.</p>
  </div>
}

function Settings({ session, onChanged }: { session: WorkspaceSession; onChanged: () => void }) {
  const [settings, setSettings] = useState<WorkspaceSettings | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [form] = Form.useForm()
  useEffect(() => { if (!session.capabilities.manage_workspace) return; const controller = new AbortController(); workspaceRequest<WorkspaceSettings>('/settings', { signal: controller.signal }).then(result => { if (!controller.signal.aborted) { setSettings(result); form.setFieldsValue(result) } }).catch(error => { if (!controller.signal.aborted) setError(workspaceError(error)) }); return () => controller.abort() }, [form, session.capabilities.manage_workspace])
  const save = async (values: { name: string; max_concurrent_runs: number }) => {
    if (!settings) return
    setBusy(true); setError(null)
    try { await workspaceMutation(session, '/settings', { ...values, name: values.name.trim(), expected_generation: settings.generation }, { method: 'PUT' }); onChanged() } catch (error) { setError(workspaceError(error)) } finally { setBusy(false) }
  }
  if (!session.capabilities.manage_workspace) return <Alert type="info" showIcon message="Administrator access is required to view or change workspace settings." />
  return <div className="workspace-section"><h2>Workspace settings</h2><p>Set the shared name and limit concurrent workflow runs on this Hub.</p>
    {error && <Alert type="error" showIcon message={error} />}
    {!settings && !error ? <Skeleton active /> : settings && <Form form={form} className="workspace-settings-form" layout="vertical" disabled={!session.capabilities.manage_workspace || busy} onFinish={values => { void save(values) }}>
      <Form.Item name="name" label="Workspace name" rules={[{ required: true, whitespace: true, max: 120 }]}><Input maxLength={120} /></Form.Item>
      <Form.Item name="max_concurrent_runs" label="Maximum concurrent runs" rules={[{ required: true, type: 'number', min: 1, max: 1 }]} extra="This Hub currently runs one workflow at a time. Additional runs remain queued."><InputNumber min={1} max={1} precision={0} disabled /></Form.Item>
      <Button type="primary" htmlType="submit" loading={busy}>Save settings</Button>
    </Form>}
  </div>
}

function Execution({ session }: { session: WorkspaceSession }) {
  const [repositories, setRepositories] = useState<WorkspaceRepository[] | null>(null)
  const [runs, setRuns] = useState<WorkspaceRun[]>([])
  const [repositoryId, setRepositoryId] = useState<string>()
  const [profileId, setProfileId] = useState<string>()
  const [changeId, setChangeId] = useState('')
  const [availableSubjects, setAvailableSubjects] = useState<WorkspaceSubject[]>([])
  const [subjectCursor, setSubjectCursor] = useState<string | null>(null)
  const [subjectsLoading, setSubjectsLoading] = useState(false)
  const [runCursors, setRunCursors] = useState<string[]>([''])
  const [runNextCursor, setRunNextCursor] = useState<string | null>(null)
  const [runsRepository, setRunsRepository] = useState<string>()
  const [runsLoading, setRunsLoading] = useState(false)
  const runCursor = runCursors.at(-1) ?? ''
  const [subject, setSubject] = useState<WorkspaceSubject | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [pendingStart, setPendingStart] = useState<{ repositoryId: string; body: Record<string, unknown> } | null>(null)
  const pendingCancels = useRef(new Map<string, Record<string, unknown>>())
  const [notice, setNotice] = useState<string | null>(null)
  const [refresh, setRefresh] = useState(0)
  const selection = useRef(0)
  const repository = repositories?.find(repository => repository.repository_id === repositoryId)
  const profile = repository?.profiles.find(profile => profile.profile_id === profileId)
  const canRead = session.capabilities.read_repositories
  const canExecute = session.capabilities.execute_workflows
  useEffect(() => {
    if (!canRead) return
    const controller = new AbortController()
    workspaceRequest<{ repositories: WorkspaceRepository[] }>('/repositories', { signal: controller.signal }).then(result => { if (!controller.signal.aborted) { setRepositories(result.repositories); setRepositoryId(previous => previous ?? result.repositories[0]?.repository_id) } }).catch(error => { if (!controller.signal.aborted) setError(workspaceError(error)) })
    return () => controller.abort()
  }, [canRead])
  useEffect(() => {
    if (!canRead || !repositoryId) return
    const controller = new AbortController()
    const load = () => workspaceRequest<{ runs: WorkspaceRun[]; next_cursor: string | null }>(`/repositories/${encodeURIComponent(repositoryId)}/runs?limit=20${runCursor ? `&before=${encodeURIComponent(runCursor)}` : ''}`, { signal: controller.signal }).then(result => { if (!controller.signal.aborted) { setRuns(result.runs); setRunsRepository(repositoryId); setRunNextCursor(result.next_cursor) } }).catch(error => { if (!controller.signal.aborted) setError(workspaceError(error)) }).finally(() => { if (!controller.signal.aborted) setRunsLoading(false) })
    void load(); const timer = window.setInterval(() => { void load() }, 5000)
    return () => { controller.abort(); window.clearInterval(timer) }
  }, [canRead, repositoryId, runCursor, refresh])
  useEffect(() => {
    if (!repositoryId || !profileId) return
    const controller = new AbortController()
    workspaceRequest<{ subjects: WorkspaceSubject[]; next_cursor: string | null }>(`/repositories/${encodeURIComponent(repositoryId)}/profiles/${encodeURIComponent(profileId)}/subjects?limit=20`, { signal: controller.signal }).then(result => { if (!controller.signal.aborted) { setAvailableSubjects(result.subjects); setSubjectCursor(result.next_cursor) } }).catch(error => { if (!controller.signal.aborted) setError(workspaceError(error)) })
    return () => controller.abort()
  }, [repositoryId, profileId])
  const moreSubjects = async () => {
    if (!repositoryId || !profileId || !subjectCursor) return
    const current = selection.current; setSubjectsLoading(true)
    try { const result = await workspaceRequest<{ subjects: WorkspaceSubject[]; next_cursor: string | null }>(`/repositories/${encodeURIComponent(repositoryId)}/profiles/${encodeURIComponent(profileId)}/subjects?limit=20&after=${encodeURIComponent(subjectCursor)}`); if (selection.current === current) { setAvailableSubjects(previous => [...previous, ...result.subjects]); setSubjectCursor(result.next_cursor) } } catch (error) { if (selection.current === current) setError(workspaceError(error)) } finally { setSubjectsLoading(false) }
  }
  const clearSubject = () => { selection.current += 1; setSubject(null); setNotice(null) }
  const loadSubject = async () => {
    if (!repositoryId || !profileId || !changeId.trim()) return
    const current = selection.current
    setBusy('subject'); setError(null); setSubject(null)
    try { const result = await workspaceRequest<{ subject: WorkspaceSubject }>(`/repositories/${encodeURIComponent(repositoryId)}/profiles/${encodeURIComponent(profileId)}/subject?change_id=${encodeURIComponent(changeId.trim())}`); if (selection.current === current) setSubject(result.subject) } catch (error) { if (selection.current === current) setError(workspaceError(error)) } finally { setBusy(null) }
  }
  const start = async () => {
    if (!repositoryId || !profileId || !subject || !canExecute) return
    setBusy('start'); setError(null); setNotice(null)
    const pending = pendingStart ?? { repositoryId, body: { subject, profile_id: profileId, expected_generation: session.actor.generation } }
    const pendingSubject = pending.body.subject as WorkspaceSubject
    const pendingProfile = pending.body.profile_id as string
    const expectation = { kind: 'start' as const, repositoryId: pending.repositoryId, profileId: pendingProfile, subject: pendingSubject }
    try { const result = await workspaceMutation<WorkspaceRun>(session, `/repositories/${encodeURIComponent(pending.repositoryId)}/runs`, pending.body, { expectation }); setNotice(`Run ${result.run_id} accepted as ${result.state}.`); setPendingStart(null); setSubject(null); setRunCursors(['']); setRefresh(value => value + 1) } catch (error) { if (!(error instanceof WorkspaceError) || (error.status >= 500 && error.code !== 'busy')) setPendingStart(pending); else setPendingStart(null); setError(workspaceError(error)) } finally { setBusy(null) }
  }
  const cancel = async (run: WorkspaceRun) => {
    setBusy(run.run_id); setError(null)
    const body = pendingCancels.current.get(run.run_id) ?? { expected_generation: run.generation }
    try { await workspaceMutation(session, `/repositories/${encodeURIComponent(run.repository_id)}/runs/${encodeURIComponent(run.run_id)}/cancel`, body); pendingCancels.current.delete(run.run_id); setNotice(`Cancellation requested for ${run.run_id}. Refreshing its durable state.`); setRefresh(value => value + 1) } catch (error) { if (!(error instanceof WorkspaceError) || (error.status >= 500 && error.code !== 'busy')) pendingCancels.current.set(run.run_id, body); else pendingCancels.current.delete(run.run_id); setError(workspaceError(error)) } finally { setBusy(null) }
  }
  if (!canRead) return <Alert type="info" showIcon message="Your membership does not include repository read access. Ask an administrator to add the reader role for the repositories you need." />
  return <div className="workspace-section">
    <div className="workspace-toolbar"><div><h2>Workflow execution</h2><p>Inspect recent runs, or open New run to execute an existing native Change.</p></div><Button icon={<ReloadOutlined />} onClick={() => setRefresh(value => value + 1)}>Refresh runs</Button></div>
    {pendingStart && <Alert type="warning" showIcon message="Start request outcome unknown" description="The original revision and generation are retained. Inspect recent runs, or retry the same request; changing the form is disabled until its outcome is confirmed." />}
    {error && <Alert type="error" showIcon message={error} />}{notice && <Alert type="info" showIcon message={notice} />}
    {!canExecute && <Alert type="info" showIcon message="You can inspect these repositories. Starting or cancelling runs requires contributor access." />}
<div className="workspace-repository-picker"><label className="field-label" htmlFor="workspace-repository">Repository</label><Select id="workspace-repository" value={repositoryId} disabled={pendingStart !== null || busy !== null} placeholder="Choose repository" options={(repositories ?? []).map(repository => ({ value: repository.repository_id, label: repository.repository_id }))} onChange={value => { setRepositoryId(value); setProfileId(undefined); setAvailableSubjects([]); setSubjectCursor(null); setChangeId(''); setRunCursors(['']); clearSubject() }} /></div>
    <div className="workspace-toolbar"><div><h2>Recent runs</h2><p>Durable workspace execution state. Unknown and stale runs need operator review before a new attempt.</p></div></div>
    <Table<WorkspaceRun> className="workflow-table workspace-desktop-table" rowKey="run_id" loading={runsLoading || (runsRepository !== repositoryId && !!repositoryId && !error)} dataSource={runsRepository === repositoryId ? runs : []} pagination={false} scroll={{ x: 740 }} locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No workspace runs yet. Open New run to load a Change and start its configured workflow." /> }} columns={[
      { title: 'Run', render: (_, run) => <div><Typography.Text code copyable>{run.run_id}</Typography.Text><div className="workspace-note">{run.repository_id} · {run.profile_id}</div></div> },
      { title: 'Immutable revision', render: (_, run) => <code title={run.subject.revision}>{run.subject.revision.slice(0, 16)}…</code> },
      { title: 'State', render: (_, run) => <div><Tag color={tone(run.state)}>{run.state}</Tag>{run.detail && <p className="workspace-note">{run.detail}</p>}</div> },
      { title: 'Actions', render: (_, run) => <Space wrap>{run.native_run_id && <Button href={`#workspace/workflow/${encodeURIComponent(run.repository_id)}/${encodeURIComponent(run.run_id)}`}>Inspect workflow</Button>}<Button icon={<StopOutlined />} disabled={!canExecute || !activeRun(run.state) || busy !== null} loading={busy === run.run_id} onClick={() => { void cancel(run) }}>Cancel</Button></Space> },
    ]} />
    <div className="workspace-mobile-list">
      {runsLoading || (runsRepository !== repositoryId && !!repositoryId && !error) ? <Skeleton active paragraph={{ rows: 3 }} /> : runsRepository !== repositoryId || runs.length === 0 ? <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No workspace runs yet. Open New run to load a Change and start its configured workflow." /> : <ul aria-label="Recent workspace runs">{runs.map(run => <li key={run.run_id}>
        <div className="workspace-mobile-row-heading"><Typography.Text code copyable>{run.run_id}</Typography.Text><Tag color={tone(run.state)}>{run.state}</Tag></div>
        <p className="workspace-note">{run.repository_id} · {run.profile_id}</p>
        <p className="workspace-note">Revision <code title={run.subject.revision}>{run.subject.revision.slice(0, 16)}…</code></p>
        {run.detail && <p>{run.detail}</p>}
        <Space wrap>{run.native_run_id && <Button href={`#workspace/workflow/${encodeURIComponent(run.repository_id)}/${encodeURIComponent(run.run_id)}`}>Inspect workflow</Button>}<Button icon={<StopOutlined />} disabled={!canExecute || !activeRun(run.state) || busy !== null} loading={busy === run.run_id} onClick={() => { void cancel(run) }}>Cancel</Button></Space>
      </li>)}</ul>}
    </div>
    <Space><Button disabled={runCursors.length <= 1 || runsLoading} onClick={() => { setRunsLoading(true); setRunCursors(previous => previous.slice(0, -1)) }}>Previous runs</Button><Button disabled={!runNextCursor || runsLoading} onClick={() => { if (runNextCursor) { setRunsLoading(true); setRunCursors(previous => [...previous, runNextCursor]) } }}>Older runs</Button></Space>
    <p className="workspace-note">Showing up to 20 runs per page. Configuration readiness does not prove runtime availability or a passing admission receipt.</p>
    <Collapse className="workspace-composer" items={[{ key: 'new-run', label: pendingStart ? 'New run & native admission · request outcome unknown' : 'New run & native admission', forceRender: true, children: <>
      <p>Choose an operator-configured profile and load an existing native Change. Execution binds to its exact immutable revision.</p>
    {repositories === null && !error ? <Skeleton active /> : repositories?.length === 0 ? <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No execution repositories are configured. Ask the operator to map a native repository and workflow profile." /> : repositories && <div className="workspace-run-form">
      <div className="workspace-form-grid"><div><label className="field-label" htmlFor="workspace-profile">Workflow profile</label><Select id="workspace-profile" value={profileId} disabled={!repository || pendingStart !== null || busy !== null} placeholder="Choose profile" options={repository?.profiles.map((profile: WorkspaceProfile) => ({ value: profile.profile_id, label: `${profile.profile_id} · ${profile.target}` }))} onChange={value => { setProfileId(value); setAvailableSubjects([]); setSubjectCursor(null); setChangeId(''); clearSubject() }} /></div></div>
      {repository && repository.profiles.length === 0 && <Alert type="warning" showIcon message="This repository has no execution profiles. Ask the operator to configure one." />}
      {profile && <p className="workspace-note"><Tag>{profile.readiness}</Tag>{profile.limitations} <span>Target: <code>{profile.target}</code></span></p>}
      {profile && <div className="workspace-change-picker"><label className="field-label" htmlFor="workspace-known-change">Current native Changes</label><Select id="workspace-known-change" disabled={pendingStart !== null || busy !== null} showSearch value={changeId || undefined} placeholder="Choose a current Change" options={availableSubjects.map(subject => ({ value: subject.change_id, label: `${subject.change_id} · version ${subject.change_version} · ${subject.revision.slice(0, 12)}` }))} onChange={value => { setChangeId(value); clearSubject() }} notFoundContent="No current Changes found on this page." />{subjectCursor && <Button loading={subjectsLoading} onClick={() => { void moreSubjects() }}>Load more Changes</Button>}</div>}
      <label className="field-label" htmlFor="workspace-change">Native Change ID</label><div className="workspace-subject-control"><Input id="workspace-change" disabled={pendingStart !== null || busy !== null} value={changeId} placeholder="Existing Change ID" onChange={event => { setChangeId(event.target.value); clearSubject() }} onPressEnter={() => { void loadSubject() }} /><Button disabled={!profile || !changeId.trim() || busy !== null || pendingStart !== null} loading={busy === 'subject'} onClick={() => { void loadSubject() }}>Load revision</Button></div>
      {subject && <div className="workspace-subject"><h3>Revision to execute</h3><dl><dt>Revision</dt><dd><code>{subject.revision}</code></dd><dt>Change version</dt><dd>{subject.change_version}</dd><dt>Review context</dt><dd><code>{subject.context_digest}</code></dd><dt>Requirement</dt><dd><code>{subject.requirement_digest}</code></dd></dl><Button type="primary" disabled={!canExecute || busy !== null || profile?.readiness !== 'configured'} loading={busy === 'start'} onClick={() => { void start() }}>{pendingStart ? 'Retry same start request' : 'Start workflow'}</Button>
        {repositoryId && profile && <NativeAdmission key={`${repositoryId}/${profileId}/${JSON.stringify(subject)}`} session={session} repositoryId={repositoryId} profile={profile} subject={subject} blocked={pendingStart !== null || busy !== null} />}
      </div>}
    </div>}
    </> }]} />
  </div>
}

export function WorkspaceEntrance({ onBack, pref, onPref }: { onBack: () => void; pref: ThemePref; onPref: (pref: ThemePref) => void }) {
  return <><Header workspaceMode whoami={null} lastOk={null} failing={false} pref={pref} onPref={onPref} onForget={onBack} /><div className="workspace-entrance-nav"><Button icon={<ArrowLeftOutlined />} onClick={onBack}>Repository status</Button></div><main className="page hub-page"><Workspace /></main></>
}


function NativeAdmission({ session, repositoryId, profile, subject, blocked }: { session: WorkspaceSession; repositoryId: string; profile: WorkspaceProfile; subject: WorkspaceSubject; blocked: boolean }) {
  const [approvalId, setApprovalId] = useState('')
  const [busy, setBusy] = useState<'approve' | 'integrate' | null>(null)
  const [pending, setPending] = useState<{ action: 'approve' | 'integrate'; body: Record<string, unknown> } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [approval, setApproval] = useState<{ approval_id: string; revision: string; context_digest: string; actor_id: string } | null>(null)
  const [landing, setLanding] = useState<{ operation_id: string; target: string; after: { revision: string; version: number } } | null>(null)
  const canApprove = session.capabilities.approve_revisions === true
  const canIntegrate = session.capabilities.integrate_revisions === true
  const submit = async (action: 'approve' | 'integrate') => {
    if ((action === 'approve' && !canApprove) || (action === 'integrate' && (!canIntegrate || !approvalId.trim()))) return
    setBusy(action); setError(null)
    const request = pending ?? { action, body: { subject, expected_generation: session.actor.generation, ...(action === 'integrate' ? { approval_id: approvalId.trim() } : {}) } }
    try {
      const expectation = request.action === 'approve' ? { kind: 'approval' as const, revision: subject.revision, contextDigest: subject.context_digest } : { kind: 'landing' as const, revision: subject.revision, contextDigest: subject.context_digest, target: profile.target }
      const result = await workspaceMutation<{ approval_id: string; revision: string; context_digest: string; actor_id: string } | { operation_id: string; target: string; after: { revision: string; version: number } }>(session, `/repositories/${encodeURIComponent(repositoryId)}/profiles/${encodeURIComponent(profile.profile_id)}/${request.action}`, request.body, { expectation })
      if (request.action === 'approve') {
        const receipt = result as { approval_id: string; revision: string; context_digest: string; actor_id: string }
        setApproval(receipt); setApprovalId(receipt.approval_id)
      } else {
        const event = result as { operation_id: string; target: string; after: { revision: string; version: number } }
        setLanding(event)
      }
      setPending(null)
    } catch (error) { if (!(error instanceof WorkspaceError) || (error.status >= 500 && error.code !== 'busy')) setPending(request); else setPending(null); setError(workspaceError(error)) } finally { setBusy(null) }
  }
  return <section className="workspace-admission" aria-label="Native approval and landing">
    <h3>Approve and land this revision</h3>
    <p>Approval and landing are separate actions. The server revalidates membership, the exact subject and required workflow evidence for each action.</p>
    {error && <Alert type="error" showIcon message={error} />}
    {pending && <Alert type="warning" showIcon message={`${pending.action === 'approve' ? 'Approval' : 'Landing'} outcome unknown`} description="Retry keeps the original subject, generation and request ID. Inspect the native state before making a different change." />}
    {approval && <Alert type="success" showIcon message="Native approval recorded" description={<span>Approval <Typography.Text code copyable>{approval.approval_id}</Typography.Text> is bound to the displayed revision and review context.</span>} />}
    {landing && <Alert type="success" showIcon message={`Landed to ${landing.target}`} description={<span>Native target version {landing.after.version}. Operation <Typography.Text code copyable>{landing.operation_id}</Typography.Text></span>} />}
    <div className="workspace-admission-actions"><div><Button disabled={blocked || !canApprove || busy !== null || (pending !== null && pending.action !== 'approve') || landing !== null} loading={busy === 'approve'} onClick={() => { void submit('approve') }}>{pending?.action === 'approve' ? 'Retry same approval' : 'Approve revision'}</Button><p className="workspace-note">{canApprove ? 'Records a native approval for this revision and context.' : 'Reviewer role required to approve.'}</p></div>
      <div><label className="field-label" htmlFor="workspace-approval">Native approval ID</label><Input id="workspace-approval" value={approvalId} onChange={event => { setApprovalId(event.target.value); setLanding(null) }} disabled={blocked || busy !== null || pending !== null || !canIntegrate} placeholder="Approval from the reviewer" /><p className="workspace-note">Use the approval above or paste one issued for this exact subject.</p><Button type="primary" disabled={blocked || !canIntegrate || !approvalId.trim() || busy !== null || (pending !== null && pending.action !== 'integrate') || landing !== null} loading={busy === 'integrate'} onClick={() => { void submit('integrate') }}>{pending?.action === 'integrate' ? 'Retry same landing' : `Land revision to ${profile.target}`}</Button><p className="workspace-note">{canIntegrate ? 'Updates the native target only after its current gate passes.' : 'Integrator role required to land.'}</p></div>
    </div>
  </section>
}
