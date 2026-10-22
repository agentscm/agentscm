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

export type SkillWorkflowRunState = 'pending' | 'running' | 'succeeded' | 'completed_with_omissions' | 'failed' | 'blocked' | 'cancelled'
export type SkillWorkflowStepState = 'pending' | 'running' | 'succeeded' | 'failed' | 'blocked' | 'cancelled'
export type SkillWorkflowApplicabilityState = 'current' | 'stale' | 'unavailable'
export type SkillWorkflowReviewStatus = 'succeeded' | 'failed' | 'blocked' | 'cancelled'
export type SkillWorkflowReviewSeverity = 'note' | 'warning' | 'error'

export interface SkillWorkflowLandingBinding {
  basis: 'current_change_version_review_context_target_and_policy_epoch'
  state: SkillWorkflowApplicabilityState
  reason: string | null
  change_id: string | null
  change_version: number | null
  context_digest: string | null
  target: string | null
}

export interface SkillWorkflowObservation {
  path: string
  sha256: string
  bytes: number
}

export interface SkillWorkflowCommandExecution {
  attempt_id: string
  generation: number
  state: string
  outcome: string | null
  record_digest: string | null
  failure: string | null
}

export interface SkillWorkflowReviewFinding {
  path: string | null
  start_line: number | null
  end_line: number | null
  title: string
  detail: string
  severity: SkillWorkflowReviewSeverity
}

export interface SkillWorkflowReview {
  model_requested: string
  model_observed: string | null
  status: SkillWorkflowReviewStatus
  stop: string | null
  findings: SkillWorkflowReviewFinding[]
  unresolved: string[]
  covered_paths: string[]
}

export interface SkillWorkflowStep {
  id: string
  needs: string[]
  required: boolean
  state: SkillWorkflowStepState
  observations: SkillWorkflowObservation[]
  detail: string | null
  execution: SkillWorkflowCommandExecution | null
  review: SkillWorkflowReview | null
}

export interface SkillWorkflowProjection {
  version: 1
  run: {
    version: 1
    run_id: string
    plan_digest: string
    skill_digest: string
    source: { repository: string; revision: string; policy_epoch: number }
    state: SkillWorkflowRunState
    steps: SkillWorkflowStep[]
    admission: 'not_assessed'
  }
  present_applicability: {
    basis: string
    state: SkillWorkflowApplicabilityState
    reason: string | null
  }
  present_landing_binding: SkillWorkflowLandingBinding | null
}

export interface SkillWorkflowRunSummary {
  run_id: string
  revision: string
  state: SkillWorkflowRunState
  name: string
  sequence: string
}

export type SkillWorkflowListEnvelope =
  | { version: 1; availability: 'available'; runs: SkillWorkflowRunSummary[]; next_cursor: string | null }
  | { version: 1; availability: 'unavailable'; reason: string }

export type SkillWorkflowEnvelope =
  | { version: 1; availability: 'available'; projection: SkillWorkflowProjection }
  | { version: 1; availability: 'unavailable'; reason: string }

export interface ViewSettings { enabled: boolean; work_overview: boolean; coordination_inbox: boolean }
