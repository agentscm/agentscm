import { fetchSetup } from '../api.ts'
import { blockerLabel, setupState, type SetupStatus } from '../onboarding.ts'
import { useNow } from '../useHub.ts'
import { Avatar, Box, Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import { ConnectAgent } from './Connect.tsx'

const STATE_TONE = { Ready: 'ok', Blocked: 'warn', Unverified: 'muted' } as const
const STATE_ICON = { Ready: 'patch-check', Blocked: 'exclamation-octagon', Unverified: 'question-circle' } as const

function Check({ ok, label, tip }: { ok: boolean; label: string; tip: string }) {
  return (
    <Pill tone={ok ? 'ok' : 'muted'} icon={ok ? 'check2' : 'dash-circle'} tip={tip}>
      {label} {ok ? 'verified' : 'unverified'}
    </Pill>
  )
}

export function SetupPage({ token }: { token: string }) {
  const load = usePoll((signal) => fetchSetup(token, signal), 5_000, token)
  const now = useNow()
  const available = load.kind === 'ready' && load.refreshError === null
  const rows: SetupStatus[] = load.kind === 'ready' ? load.value : []
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Is my agent set up?</h1>
        <p className="answer-text">An agent is ready when it has proved, in one session, that it can reach the hub over MCP and that its edit hook fires on a real edit.</p>
      </div>
      <div className="grid-2 align-start">
        <Box tour="setup-sessions" icon="patch-check" title="Verified sessions" count={rows.length} explain="Proof lasts up to 15 minutes after the agent’s last check, and never outlasts the agent’s own token (8 hours for client setup). Ready does not guarantee future quota or that every edit tool is covered.">
          {load.kind === 'loading' && <Skeleton rows={3} />}
          {!available && load.kind !== 'loading' && <Notice tone="muted" icon="info-circle" title="Setup verification unavailable">No agent is marked ready until the hub answers again.</Notice>}
          {load.kind === 'ready' && rows.length === 0 && (
            <Empty icon="plug" title="No verified setup yet">
              Copy the instructions to your agent, resolve any sign-in or approval blockers, then verify a native edit. Only agents set up with <code>agentscm client setup</code> report here; agents set up with <code>agentscm quickstart</code> do not appear.
            </Empty>
          )}
          <ul className="plain-list">
            {rows.slice(0, 30).map((row) => {
              const state = setupState(row, now, available)
              const live = available && row.expires_at > now / 1000
              return (
                <li key={row.key} className="setup-row">
                  <div className="d-flex align-items-center gap-2 flex-wrap">
                    <Avatar name={row.name} size={28} />
                    <strong>{row.name}</strong>
                    <span className="muted small">
                      {row.owner} · {row.agent}
                    </span>
                    <span className="ms-auto">
                      <Pill tone={STATE_TONE[state]} icon={STATE_ICON[state]}>
                        {state}
                      </Pill>
                    </span>
                  </div>
                  <div className="pills mt-2">
                    <Check ok={row.mcp_verified && live} label="MCP" tip="The agent called AgentSCM over MCP from this session." />
                    <Check ok={row.native_verified && live} label={row.agent === 'cursor' ? 'Edit hook (after edit)' : 'Edit hook'} tip="The agent’s native edit hook reported a real edit to the hub in this session." />
                    <span className="small muted">
                      <Icon name="hash" /> {row.key.slice(0, 8)}
                    </span>
                  </div>
                  <p className="small mt-2 mb-0">
                    <Icon name="arrow-right-circle" /> {!available ? 'Verification unavailable. Reconnect to the hub before relying on setup status.' : row.expires_at <= now / 1000 ? 'Evidence expired. Ask your agent to verify MCP and a native edit again.' : row.blocker ? `${blockerLabel(row.blocker)}. ${row.next_action}` : state === 'Unverified' ? `Ask your agent to: ${row.next_action}` : row.next_action}
                  </p>
                </li>
              )
            })}
          </ul>
          {rows.length > 30 && <p className="small muted">Showing 30 setup sessions.</p>}
        </Box>
        <Box tour="setup-connect" icon="plug" title="Connect an agent" explain="Copies instructions your coding agent follows to install AgentSCM. Nothing runs from this page.">
          <ConnectAgent />
        </Box>
      </div>
    </div>
  )
}
