import { useState } from 'react'
import { fetchViewSettings } from '../../../ui/src/api.ts'
import type { ViewSettings } from '../../../ui/src/types.ts'
import { workspaceError, workspaceMutation, workspaceRequest, type WorkspaceSession } from '../../../ui/src/workspace.ts'
import { Box, Notice, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'

const READ_ONLY = 'Sign in to the workspace as an administrator to change this.'
const CHILD_OFF = 'Off because the master switch is off'

/** Hub-wide setting. Reading needs the status-page token; changing needs an administrator workspace session. */
export function ViewSettingsBox({ token }: { token: string }) {
  const load = usePoll((signal) => fetchViewSettings(token, signal), 15_000, token)
  const session = usePoll((signal) => workspaceRequest<WorkspaceSession>('/session', { signal }), 60_000, 'view-settings')
  const [saved, setSaved] = useState<{ value: ViewSettings; at: number } | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const fetchedAt = load.kind === 'ready' ? load.fetchedAt : 0

  const admin = session.kind === 'ready' && session.value.capabilities.manage_workspace ? session.value : null
  const current = (saved && saved.at === fetchedAt ? saved.value : null) ?? (load.kind === 'ready' ? load.value : null)

  const change = async (patch: Partial<ViewSettings>) => {
    if (!admin || !current) return
    setSaving(true)
    setError(null)
    try {
      setSaved({ value: await workspaceMutation<ViewSettings>(admin, '/view-settings', { ...current, ...patch }, { method: 'PUT' }), at: fetchedAt })
    } catch (e) {
      setError(workspaceError(e))
    } finally {
      setSaving(false)
    }
  }
  const lock = !admin || saving
  const row = (id: string, label: string, hint: string, on: boolean, field: keyof ViewSettings, child: boolean) => {
    const off = child && current && !current.enabled
    return (
      <div className="settings-row">
        <div>
          <div className="settings-label" id={`${id}-label`}>{label}</div>
          <div className="muted small" id={`${id}-hint`}>{off ? CHILD_OFF : hint}</div>
        </div>
        <div className="form-check form-switch m-0">
          <input id={id} className="form-check-input" type="checkbox" role="switch" checked={on && !off} disabled={lock || !!off} aria-describedby={`${id}-hint view-settings-explain${admin ? '' : ' view-settings-readonly'}`} onChange={(e) => void change({ [field]: e.target.checked })} />
          <label className="visually-hidden" htmlFor={id}>{label}</label>
        </div>
      </div>
    )
  }
  return (
    <Box id="view-settings" icon="window-stack" title="Inline agent views">
      <p className="muted small" id="view-settings-explain">When an agent app supports it, the work overview and coordination inbox appear as cards inside the conversation. Turning this off keeps the plain text answers.</p>
      {load.kind === 'loading' && <Skeleton rows={3} />}
      {load.kind === 'error' && <Notice title="Could not load this setting" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>{load.message}</Notice>}
      {load.kind === 'ready' && load.refreshError && <Notice title="Showing the last value. The latest refresh failed.">{load.refreshError}</Notice>}
      {current && (
        <>
          {row('view-master', 'Show views in agent apps', 'Turns inline views on or off everywhere.', current.enabled, 'enabled', false)}
          {row('view-work', 'Work overview', 'Who is working on what, as a card.', current.work_overview, 'work_overview', true)}
          {row('view-inbox', 'Coordination inbox', 'Requests waiting for an answer, as a card.', current.coordination_inbox, 'coordination_inbox', true)}
        </>
      )}
      {current && !admin && session.kind !== 'loading' && <p className="muted small mb-0" id="view-settings-readonly">{READ_ONLY}</p>}
      {error && <Notice tone="bad" title="Could not save">{error}</Notice>}
      <p className="muted small mb-0">Agents can also change this with the <code>agentscm_view_settings</code> tool.</p>
    </Box>
  )
}
