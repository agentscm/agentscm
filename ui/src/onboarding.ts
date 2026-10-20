import { CLIENT_AGENTS, type ClientAgent } from './connectAgent.ts'

export interface SetupStatus {
  agent: ClientAgent
  key: string
  name: string
  owner: string
  blocker: string | null
  mcp_verified: boolean
  native_verified: boolean
  expires_at: number
  next_action: string
}

export function decodeSetup(value: unknown): SetupStatus[] {
  if (!Array.isArray(value)) throw new Error('Invalid onboarding response')
  return value.slice(0, 256).flatMap((v: unknown) => {
    if (!v || typeof v !== 'object') return []
    const r = v as Record<string, unknown>
    if (!CLIENT_AGENTS.includes(r.agent as ClientAgent) ||
      !['key', 'name', 'owner', 'next_action'].every((key) => typeof r[key] === 'string') ||
      typeof r.expires_at !== 'number' || !Number.isFinite(r.expires_at)) return []
    return [{ agent: r.agent as ClientAgent, key: r.key as string,
      name: r.name as string, owner: r.owner as string,
      blocker: r.blocker === null ? null : typeof r.blocker === 'string' ? r.blocker : 'unknown',
      mcp_verified: r.mcp_verified === true, native_verified: r.native_verified === true,
      expires_at: r.expires_at, next_action: r.next_action as string }]
  })
}

export function setupState(row: SetupStatus, now: number, available: boolean): 'Ready' | 'Blocked' | 'Unverified' {
  if (!available || row.expires_at <= now / 1000) return 'Unverified'
  if (row.blocker !== null) return 'Blocked'
  return row.mcp_verified && row.native_verified ? 'Ready' : 'Unverified'
}

export const blockerLabel = (blocker: string | null): string => ({ login: 'Sign in to your agent', trust: 'Review native approvals', quota: 'Usage limit', unsupported: 'Account access unsupported', unknown: 'Inspect agent status' }[blocker ?? ''] ?? 'Verification needed')
