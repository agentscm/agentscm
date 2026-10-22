// Test helpers for the live dev hub started by ui-next/dev-seed/start.sh.
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

// Both runners start in this package directory (jsdom's import.meta.url is not a file URL).
const seed = `${resolve(process.cwd(), '../dev-seed')}/`

function envFile(path: string): Record<string, string> {
  try {
    return Object.fromEntries(
      readFileSync(path, 'utf8')
        .split('\n')
        .filter((l) => l.includes('='))
        .map((l) => [l.slice(0, l.indexOf('=')).trim(), l.slice(l.indexOf('=') + 1).trim()]),
    )
  } catch {
    return {}
  }
}

// Captured before tests wrap fetch: helper writes must not count as requests the UI made.
const nativeFetch = globalThis.fetch

const hubEnv = envFile(`${seed}.hub.env`)
const tokens = envFile(`${seed}.data/tokens.env`)

export const HUB = hubEnv['AGENTSCM_HUB'] ?? 'http://127.0.0.1:7332'
export const READ_TOKEN = hubEnv['READ_TOKEN'] ?? ''
/** An agent (write) token for one synthetic agent; never used by the UI itself. */
export const AGENT_TOKEN = tokens['RUNE'] ?? ''
const HOLDER = tokens['PAX'] ?? ''
const ASKER = tokens['JUNO'] ?? ''
const ROOT = resolve(process.cwd(), '../..')

export async function hubReachable(): Promise<boolean> {
  if (!READ_TOKEN) return false
  try {
    const r = await nativeFetch(`${HUB}/v1/hub/whoami`, { headers: { Authorization: `Bearer ${READ_TOKEN}` }, signal: AbortSignal.timeout(2000) })
    return r.ok
  } catch {
    return false
  }
}

export interface McpReply {
  status: number
  body: { result?: { isError?: boolean; structuredContent?: Record<string, unknown> }; error?: unknown }
}

export async function mcp(token: string, name: string, args: Record<string, unknown>): Promise<McpReply> {
  const r = await nativeFetch(`${HUB}/mcp`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name, arguments: args } }),
  })
  return { status: r.status, body: (await r.json()) as McpReply['body'] }
}

interface Item {
  id: string
  title: string
  generation: number
  updated_seq: number
}

/** Starts a unique native work item over MCP and returns it; the title carries the marker. */
export async function startWork(marker: string): Promise<{ item: Item; session: string }> {
  const session = `test-${marker}`
  const reply = await mcp(AGENT_TOKEN, 'agentscm_start', {
    task: `E2E probe ${marker}`,
    paths: [`docs/test-probes/${marker}.md`],
    session,
    request_key: `test-${marker}`,
    next_action: `Verify marker ${marker} reaches the UI`,
  })
  const sc = reply.body.result?.structuredContent as { work?: { item?: Item } } | undefined
  const item = sc?.work?.item
  if (reply.status !== 200 || reply.body.result?.isError || !item) throw new Error(`agentscm_start failed: ${JSON.stringify(reply.body).slice(0, 400)}`)
  return { item, session }
}

/** Records a checkpoint over MCP against the exact current generation. */
export async function checkpoint(item: Item, session: string, kind: string, summary: string, next_action?: string): Promise<McpReply> {
  return mcp(AGENT_TOKEN, 'agentscm_work_checkpoint', {
    work_id: item.id,
    operation_id: `op-${item.id}-${item.updated_seq}-${kind}`,
    generation: item.generation,
    expected_updated_seq: item.updated_seq,
    session,
    kind,
    summary,
    ...(next_action ? { next_action } : {}),
  })
}

/** Reads a work item through the same HTTP endpoint the UI uses. */
export async function readWork(id: string): Promise<Item & { current: { summary: string } | null; state: string }> {
  const r = await nativeFetch(`${HUB}/v1/hub/standup/work/${encodeURIComponent(id)}`, { headers: { Authorization: `Bearer ${READ_TOKEN}` } })
  if (!r.ok) throw new Error(`work read ${r.status}`)
  return r.json() as never
}

async function hook(token: string, session: string, event: string, file?: string): Promise<unknown> {
  const body: Record<string, unknown> = { session_id: session, cwd: ROOT, hook_event_name: event }
  if (file) Object.assign(body, { tool_name: 'Edit', tool_input: { file_path: `${ROOT}/${file}` } })
  const r = await nativeFetch(`${HUB}/v1/hub/hook/claude`, { method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
  if (!r.ok) throw new Error(`hook ${event} ${r.status}: ${(await r.text()).slice(0, 200)}`)
  return r.json()
}
const denied = (reply: unknown) => (reply as { hookSpecificOutput?: { permissionDecision?: string } })?.hookSpecificOutput?.permissionDecision === 'deny'

/**
 * Creates a fresh refusal plus granted handoff on a unique file, the way two real agents would:
 * the holder edits it, the asker's edit is refused (which asks the holder), and the holder hands it over.
 * Returns a cleanup that releases both sessions' files.
 */
export async function freshHandoff(m: string): Promise<{ file: string; release: () => Promise<void> }> {
  const file = `docs/test-probes/${m}-handoff.md`
  const holder = `test-hold-${m}`
  const asker = `test-ask-${m}`
  await hook(HOLDER, holder, 'SessionStart')
  await hook(HOLDER, holder, 'PreToolUse', file)
  await hook(ASKER, asker, 'SessionStart')
  if (!denied(await hook(ASKER, asker, 'PreToolUse', file))) throw new Error('expected the second edit to be refused')
  const handed = await mcp(HOLDER, 'agentscm_handoff', { path: file, accept: true, session: holder })
  if (handed.status !== 200 || handed.body.result?.isError) throw new Error(`handoff failed: ${JSON.stringify(handed.body).slice(0, 300)}`)
  return {
    file,
    release: async () => {
      await mcp(HOLDER, 'agentscm_done', { session: holder })
      await mcp(ASKER, 'agentscm_done', { session: asker })
    },
  }
}

export const marker = () => `m${crypto.randomUUID().replaceAll('-', '').slice(0, 12)}`
