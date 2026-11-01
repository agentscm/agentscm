import { fetchSetup } from '../api.ts'
import { who } from '../nextStep.ts'
import { SectionTabs } from '../TopNav.tsx'
import { blockerLabel, setupState, type SetupStatus } from '../onboarding.ts'
import { useNow } from '../useHub.ts'
import { Avatar, Box, Empty, Icon, Notice, PageHead, Pill, Skeleton } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import { AGENT_NAMES, ConnectAgent, InstallCommand } from './Connect.tsx'

/** "maria’s Codex": the owner and the agent's product name, as the Dashboard names someone's agent. */
const named = (row: SetupStatus) => who(row.owner, AGENT_NAMES[row.agent])

const STATE_LABEL = { Ready: 'Ready', Blocked: 'Needs action', Unverified: 'Not yet verified' } as const
const STATE_TONE = { Ready: 'ok', Blocked: 'warn', Unverified: 'muted' } as const
const STATE_ICON = { Ready: 'patch-check', Blocked: 'exclamation-octagon', Unverified: 'question-circle' } as const

/** What stands between a row and Ready, as the end of "<login>’s <agent> …". */
const BLOCKED: Record<string, string> = { login: 'needs you to sign in to it', trust: 'needs its AgentSCM hooks approved', quota: 'has hit a usage limit', unsupported: 'can’t verify with this account' }
function waitingFor(row: SetupStatus, now: number): string {
  if (row.expires_at <= now / 1000) return 'needs to check in again'
  if (row.blocker !== null) return BLOCKED[row.blocker] ?? 'needs a look'
  return row.mcp_verified ? 'hasn’t checked a real edit yet' : 'hasn’t reached the Hub yet'
}

/** The page's answer, built from the rows already loaded: how many agents are ready, and what the one that isn't needs. */
export function setupAnswer(rows: SetupStatus[], now: number): string {
  if (rows.length === 0) return 'No agent has connected yet. Copy the setup instructions into your agent to start.'
  const waiting = rows.filter((row) => setupState(row, now, true) !== 'Ready')
  const ready = rows.length - waiting.length
  if (waiting.length === 0) return rows.length === 1 ? `${named(rows[0]!)} is ready.` : `All ${rows.length} agents are ready.`
  if (waiting.length > 1) return ready === 0 ? `None of the ${rows.length} agents is ready yet.` : `${ready} of ${rows.length} agents ${ready === 1 ? 'is' : 'are'} ready; ${waiting.length} need a step from you.`
  const one = `${named(waiting[0]!)} ${waitingFor(waiting[0]!, now)}.`
  return ready === 0 ? one : `${ready} of ${rows.length} agents ${ready === 1 ? 'is' : 'are'} ready; ${one}`
}

/** The two checks behind Ready, in one muted line: "MCP ✓ · Edit hook not yet". */
export function checksLine(row: SetupStatus, live: boolean): string {
  const mark = (ok: boolean) => (ok && live ? '✓' : 'not yet')
  return `MCP ${mark(row.mcp_verified)} · ${row.agent === 'cursor' ? 'Edit hook (after edit)' : 'Edit hook'} ${mark(row.native_verified)}`
}

export function SetupPage({ token }: { token: string }) {
  const load = usePoll((signal) => fetchSetup(token, signal), 5_000, token)
  const now = useNow()
  const available = load.kind === 'ready' && load.refreshError === null
  const rows: SetupStatus[] = load.kind === 'ready' ? load.value : []
  return (
    <div className="page">
      <PageHead question="Is my agent set up?" answer={load.kind === 'loading' ? 'Checking your agents…' : available ? setupAnswer(rows, now) : 'Setup status is unavailable until the Hub answers again.'} live />
      <SectionTabs />
      <div className="grid-2 align-start">
        <Box tour="setup-sessions" icon="patch-check" title="Verified sessions" count={rows.length} explain="Ready means one session reached the Hub over MCP and its edit hook reported a real edit. That proof lasts 15 minutes after the agent’s last check, and never longer than the agent’s token: 24 hours from a signed-in computer (it renews itself), 30 days on a Hub older than computer sign-in. Ready doesn’t promise future quota, or that every edit tool is covered.">
          {load.kind === 'loading' && <Skeleton rows={3} />}
          {!available && load.kind !== 'loading' && <Notice tone="muted" icon="info-circle" title="Setup verification unavailable">No agent is marked ready until the Hub answers again.</Notice>}
          {load.kind === 'ready' && rows.length === 0 && (
            <>
              <Empty icon="plug" title="No agent has checked in yet">
                Paste the setup instructions into your agent; it shows up here once it checks in.
              </Empty>
              <InstallCommand />
            </>
          )}
          <ul className="plain-list">
            {rows.slice(0, 30).map((row) => {
              const state = setupState(row, now, available)
              const live = available && row.expires_at > now / 1000
              return (
                <li key={row.key} className="setup-row">
                  <div className="setup-head">
                    <Avatar name={row.name} size={28} />
                    <span className="setup-main">
                      <strong className="setup-name" title={`${row.name} (session ${row.key.slice(0, 8)})`}>{named(row)}</strong>
                      <span className="setup-checks">{checksLine(row, live)}</span>
                    </span>
                    <Pill tone={STATE_TONE[state]} icon={STATE_ICON[state]}>
                      {STATE_LABEL[state]}
                    </Pill>
                  </div>
                  {state !== 'Ready' && <p className="setup-next">
                    <Icon name="arrow-right-circle" /> {!available ? 'Verification unavailable. Reconnect to the hub before relying on setup status.' : row.expires_at <= now / 1000 ? 'Evidence expired. Ask your agent to verify MCP and a native edit again.' : row.blocker ? `${blockerLabel(row.blocker)}. ${row.next_action}` : state === 'Unverified' ? `Ask your agent to: ${row.next_action}` : row.next_action}
                  </p>}
                </li>
              )
            })}
          </ul>
          {rows.length > 30 && <p className="small muted">Showing 30 setup sessions.</p>}
        </Box>
        <Box tour="setup-connect" icon="plug" title="Connect an agent" intro="Send this to each engineer: they copy these instructions into their agent." explain="Copies the instructions your agent follows to install AgentSCM.">
          <ConnectAgent />
        </Box>
      </div>
    </div>
  )
}
