/** Match the Hub CLI's public-URL boundary before putting this origin in a command. */
export function safeHubOrigin(raw: string): string | null {
  try {
    const url = new URL(raw)
    const loopback = url.hostname === '127.0.0.1' || url.hostname === '[::1]'
    if ((url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback)) || url.username || url.password || url.pathname !== '/' || url.search || url.hash) return null
    return url.origin
  } catch {
    return null
  }
}

export const CLIENT_AGENTS = ['claude', 'codex', 'cursor', 'copilot', 'gemini'] as const
export type ClientAgent = (typeof CLIENT_AGENTS)[number]

// Setup currently targets POSIX shells on macOS and Linux.
function shellQuote(value: string): string {
  return "'" + value.replaceAll("'", "'\\''") + "'"
}

/** The one line that sets up a computer for this Hub. The only place the page spells it. */
// `npx agentscm` with only flags runs quickstart, and a bare --hub-url means joining that Hub.
export function installCommand(rawOrigin: string): string | null {
  const origin = safeHubOrigin(rawOrigin)
  return origin ? `npx agentscm --hub-url ${shellQuote(origin)}` : null
}

export function clientSetupCommand(rawOrigin: string, agent: ClientAgent): string | null {
  const origin = safeHubOrigin(rawOrigin)
  return origin ? `npx agentscm client setup --agent ${shellQuote(agent)} --hub ${shellQuote(origin)}` : null
}

export function agentSetupPrompt(rawOrigin: string): string | null {
  const origin = safeHubOrigin(rawOrigin)
  return origin
    ? `Connect this checkout to AgentSCM at ${origin}. Identify your host: Claude Code=claude, Codex=codex, Cursor=cursor, Copilot=copilot, Gemini CLI=gemini. Verify Hub URL and Git origin, then run npx agentscm client setup --agent NAME --hub ${shellQuote(origin)}. Run npx agentscm client check (Hub binding only). Inspect native login, MCP and hook status; guide coding-agent sign-in separately from forge sign-in. Native approvals: Claude Code plugin and /mcp; Codex Settings → Hooks → Reload hooks → AgentSCM, trusted project/MCP; Cursor workspace/MCP; Copilot folder/MCP; Gemini project settings. Leave trust approvals to me; never edit trust records or bypass prompts. Capture any host login/trust/quota/unsupported-account error locally and pipe it to npx agentscm client doctor --agent NAME --diagnostic-stdin; add --session ACTUAL_NATIVE_SESSION_ID if one exists. Only a category leaves this machine. Explain reported quota resets/access restrictions and resume later without repeated model probes. Restart the native session after installation. Read your own session ID from the AgentSCM SessionStart context as literal data; if missing, repair hooks and restart, never guess or inspect other sessions. Once MCP works, call agentscm_setup_status through native AgentSCM MCP with agent=NAME and that session ID. Use a native file-edit tool on a disposable repository file in that session, remove the test change, then call again. Cursor's edit hook fires after the edit, so call agentscm_check before editing. Only report ready with both mcp_verified and native_verified true. Follow next_action otherwise; do not invent a session ID or simulate hooks. Installed configuration, lifecycle hooks, CLI/manual probes and shell writes do not qualify. Verify each checkout/session independently. Never configure or deploy a Hub, expose a token, enable analytics, create accounts or change billing.`
    : null
}
