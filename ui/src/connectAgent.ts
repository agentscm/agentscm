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

export function clientSetupCommand(rawOrigin: string, agent: ClientAgent): string | null {
  const origin = safeHubOrigin(rawOrigin)
  return origin ? `npx agentscm client setup --agent ${shellQuote(agent)} --hub ${shellQuote(origin)}` : null
}

export function agentSetupPrompt(rawOrigin: string): string | null {
  const origin = safeHubOrigin(rawOrigin)
  return origin
    ? `Connect this checkout to AgentSCM at ${origin}. Identify which coding agent you are running in and select its setup name: Claude Code is claude, Codex is codex, Cursor is cursor, Copilot is copilot, and Gemini is gemini. Verify the Hub URL and Git origin, then run npx agentscm client setup --agent NAME --hub ${shellQuote(origin)} with the matching name. Pause for my forge sign-in and your host's AgentSCM hook or MCP trust prompts when required. Never configure or deploy a Hub, expose a token, or enable analytics. Run npx agentscm client check and verify a real agent session reaches the expected repository before claiming setup is ready.`
    : null
}
