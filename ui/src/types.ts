// Wire types for the hub's read-only status endpoints. Fields the page does not
// need are left out; anything teammate-supplied is treated as untrusted text.
export interface AgentRef {
  token: string
  name: string
}
export interface Owner {
  login: string
}
export interface Whoami {
  token: { id: string; name: string; repository: string; expires_at?: number | null; read_only: boolean }
  owner: { provider: 'github' | 'gitlab' | string; host?: string; user_id?: string | number; login: string }
}
export interface Intent {
  id: string
  task: string | null
  branch: string | null
  base?: string | null
  machine: string | null
  paths: string[]
  via: string[]
  expires_at?: number
  created_at: number
  updated_at: number
  agent: AgentRef
  owner: Owner
}
export interface PullRequest {
  number: number
  title: string
  state: string
  draft: boolean
  head_ref: string
  base_ref: string
  author: { login: string }
  html_url?: string | null
  files: string[]
  files_truncated?: boolean
}
export interface Work {
  intents: Intent[]
  intents_truncated: boolean
  pull_requests: PullRequest[]
  connected: boolean
}
export interface Session {
  /** Opaque and stable per session. */
  key: string
  agent: AgentRef
  owner: Owner
  intent: string | null
  task: string | null
  via: string[]
  created_at: number
  updated_at: number
}
export interface HubEvent {
  seq: number
  id: string
  at: number
  kind: string
  agent: AgentRef
  owner: Owner
  data: Record<string, unknown>
}
export interface Activity {
  sessions: Session[]
  events: HubEvent[]
}
export interface Health {
  status: 'ok' | 'unavailable'
  storage?: 'sqlite' | 'postgres' | string
  /** Present (true) only when the hub runs under `agentscm demo`. */
  demo?: true
  coordination?: {
    denied: number
    repeated: number
    rate_limited: number
    fail_open: number
    hook_p95_ms: number
  }
}
