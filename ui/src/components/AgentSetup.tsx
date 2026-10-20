import { Alert, Button, Card, Space, Tag, Typography } from 'antd'
import { useState } from 'react'
import { setupState, blockerLabel, type SetupStatus } from '../onboarding.ts'
import { fetchSetup } from '../api.ts'
import { useLoop, useNow } from '../useHub.ts'
import { ConnectAgent } from './ConnectAgent.tsx'

export function AgentSetup({ token }: { token: string }) {
  const [rows, setRows] = useState<SetupStatus[]>([])
  const [available, setAvailable] = useState(false)
  const [open, setOpen] = useState(false)
  const now = useNow()
  useLoop(async (signal) => {
    const next = await fetchSetup(token, signal)
    setRows(next)
    setAvailable(true)
  }, 5000, () => setAvailable(false))
  return <>
    <Card size="small" title="Agent setup" extra={<Button onClick={() => setOpen(true)}>Connect an agent</Button>} style={{ marginBottom: 16 }}>
      <Space direction="vertical" style={{ width: '100%' }}>
        {!available && <Alert type="info" showIcon message="Setup verification unavailable. No agent is marked ready." />}
        {available && rows.length === 0 && <Typography.Text type="secondary">No verified setup yet. Copy instructions to your agent to connect, resolve sign-in or approval blockers, and verify a native edit.</Typography.Text>}
        {rows.slice(0, 30).map((row) => {
          const state = setupState(row, now, available)
          return <div key={row.key}>
            <Space wrap><Typography.Text strong>{row.name}</Typography.Text><Typography.Text type="secondary">{row.owner} · {row.agent}</Typography.Text><Tag color={state === 'Ready' ? 'success' : state === 'Blocked' ? 'warning' : 'default'}>{state}</Tag></Space>
            <div><Typography.Text type="secondary">Connection {row.key.slice(0, 8)} · MCP {row.mcp_verified && available && row.expires_at > now / 1000 ? 'verified' : 'unverified'} · native edit hook {row.native_verified && available && row.expires_at > now / 1000 ? 'verified' : 'unverified'}{row.agent === 'cursor' ? ' (after edit)' : ''}</Typography.Text></div>
            <Typography.Paragraph style={{ marginBottom: 8 }}>{!available ? 'Verification unavailable. Reconnect to the Hub before relying on setup status.' : row.expires_at <= now / 1000 ? 'Evidence expired. Ask your agent to verify MCP and a native edit again.' : row.blocker ? `${blockerLabel(row.blocker)}. ${row.next_action}` : row.next_action}</Typography.Paragraph>
          </div>
        })}
        {rows.length > 30 && <Typography.Text type="secondary">Showing 30 setup sessions.</Typography.Text>}
        <Typography.Text type="secondary">Ready means recent MCP and edit-hook observations for one checkout/session. It does not guarantee future quota or every edit tool. Proof expires after 15 minutes.</Typography.Text>
      </Space>
    </Card>
    <ConnectAgent open={open} onClose={() => setOpen(false)} />
  </>
}
