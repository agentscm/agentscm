import { useState } from 'react'
import { fetchViewSettings } from './api.ts'
import type { ViewSettings, ViewSettingsReport } from './types.ts'
import { workspaceError, workspaceMutation, workspaceRequest, type WorkspaceSession } from './workspace.ts'
import { Box, Notice, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'

const SIGN_IN = 'Sign in to the workspace to change your views.'
const ADMIN_ONLY = 'Only a workspace administrator can change the hub default.'
const CHILD_OFF = 'Off because the master switch is off'
const FIELDS: { id: string; field: keyof ViewSettings; label: string; hint: string; child: boolean }[] = [
  { id: 'master', field: 'enabled', label: 'Show views in agent apps', hint: 'Turns inline views on or off everywhere.', child: false },
  { id: 'work', field: 'work_overview', label: 'Work overview', hint: 'Who is working on what, as a card.', child: true },
  { id: 'inbox', field: 'coordination_inbox', label: 'Coordination inbox', hint: 'Requests waiting for an answer, as a card.', child: true },
]

/** Per-user views. Reading needs the status-page token; any signed-in member changes their own, an administrator the hub default. */
export function ViewSettingsBox({ token }: { token: string }) {
  const load = usePoll((signal) => fetchViewSettings(token, signal), 15_000, token)
  const session = usePoll((signal) => workspaceRequest<WorkspaceSession>('/session', { signal }), 60_000, 'view-settings')
  const [saved, setSaved] = useState<{ value: ViewSettingsReport; at: number } | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const fetchedAt = load.kind === 'ready' ? load.fetchedAt : 0

  const member = session.kind === 'ready' ? session.value : null
  const admin = !!member?.capabilities.manage_workspace
  const current = (saved && saved.at === fetchedAt ? saved.value : null) ?? (load.kind === 'ready' ? load.value : null)

  const run = async (act: (m: WorkspaceSession, c: ViewSettingsReport) => Promise<ViewSettingsReport>) => {
    if (!member || !current) return
    setSaving(true)
    setError(null)
    try {
      setSaved({ value: await act(member, current), at: fetchedAt })
    } catch (e) {
      setError(workspaceError(e))
    } finally {
      setSaving(false)
    }
  }
  const mine = (body: Record<string, unknown>) => run((m) => workspaceMutation<ViewSettingsReport>(m, '/view-settings/me', body, { method: 'PUT' }))
  const setDefault = (patch: Partial<ViewSettings>) => run(async (m, c) => {
    const hub_default = await workspaceMutation<ViewSettings>(m, '/view-settings', { ...c.hub_default, ...patch }, { method: 'PUT' })
    const effective = { ...hub_default }
    for (const f of FIELDS) { const o = c.override?.[f.field]; if (typeof o === 'boolean') effective[f.field] = o }
    return { ...c, hub_default, effective }
  })

  const row = (prefix: string, values: ViewSettings, editable: boolean, onChange: (f: keyof ViewSettings, on: boolean) => void, source?: (f: keyof ViewSettings) => string) => FIELDS.map(({ id, field, label, hint, child }) => {
    const sid = `${prefix}-${id}`
    const off = child && !values.enabled
    return (
      <div className="settings-row" key={sid}>
        <div>
          <div className="settings-label" id={`${sid}-label`}>{label}</div>
          <div className="muted small" id={`${sid}-hint`}>{off ? CHILD_OFF : hint}</div>
          {source && <div className="muted small" id={`${sid}-source`}>{source(field)}</div>}
        </div>
        <div className="form-check form-switch m-0">
          <input id={sid} className="form-check-input" type="checkbox" role="switch" checked={values[field] && !off} disabled={!editable || saving || off} aria-describedby={`${sid}-hint${source ? ` ${sid}-source` : ''} ${prefix}-explain`} onChange={(e) => onChange(field, e.target.checked)} />
          <label className="visually-hidden" htmlFor={sid}>{prefix === 'default' ? `Hub default: ${label}` : label}</label>
        </div>
      </div>
    )
  })
  const settled = session.kind !== 'loading'
  return (
    <Box id="view-settings" icon="window-stack" title="Inline agent views">
      <p className="muted small" id="view-settings-explain">When an agent app supports it, the work overview and coordination inbox appear as cards inside the conversation. Turning this off keeps the plain text answers.</p>
      {load.kind === 'loading' && <Skeleton rows={3} />}
      {load.kind === 'error' && <Notice title="Could not load this setting" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>{load.message}</Notice>}
      {load.kind === 'ready' && load.refreshError && <Notice title="Showing the last value. The latest refresh failed.">{load.refreshError}</Notice>}
      {current && (
        <>
          <h3 className="h6 mt-2">Your views</h3>
          <p className="muted small" id="mine-explain">These choices apply only to your own agents. Agents can also change them with the <code>agentscm_view_settings</code> tool.</p>
          {row('mine', current.effective, !!member, (f, on) => void mine({ [f]: on }), (f) => (current.override?.[f] != null ? 'from your setting' : 'from the hub default'))}
          {current.override && Object.values(current.override).some((v) => v != null) && (
            <button type="button" className="btn btn-sm btn-outline-secondary mt-2" disabled={!member || saving} onClick={() => void mine({ reset: true })}>Use the hub default</button>
          )}
          {!member && settled && <p className="muted small mt-2 mb-0" id="view-settings-readonly">{SIGN_IN}</p>}
          <details className="mt-3">
            <summary className="small">Hub default</summary>
            <p className="muted small mt-2" id="default-explain">Used for anyone who hasn&apos;t chosen their own setting.</p>
            {row('default', current.hub_default, admin, (f, on) => void setDefault({ [f]: on }))}
            {!admin && settled && <p className="muted small mb-0" id="default-readonly">{ADMIN_ONLY}</p>}
          </details>
        </>
      )}
      {error && <Notice tone="bad" title="Could not save">{error}</Notice>}
    </Box>
  )
}
