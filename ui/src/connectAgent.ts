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

const REPO = /^[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)+$/

/** With `repo`, the agent starts in that repository's checkout. Kept short: the prompt rides in a launch link. */
export function agentSetupPrompt(rawOrigin: string, repo?: string | null): string | null {
  const origin = safeHubOrigin(rawOrigin)
  const where = repo && REPO.test(repo) ? ` Work in my checkout of ${repo}, cloning it if needed.` : ''
  return origin
    ? `Set up AgentSCM on this computer for the Hub at ${origin}.${where} Run: npx agentscm --yes --json --hub-url ${shellQuote(origin)}. If it prints a sign-in link, show it to me and wait while I sign in. It sets up every coding agent it finds, once for all my projects. Tell me which agents it set up and which failed. Then tell me to fully quit and reopen my agents, and, for Codex, to run /hooks once and trust the AgentSCM hooks. Leave trust approvals to me; do not edit trust records, create accounts, expose tokens or deploy a Hub.`
    : null
}

/** For a Hub with no GitHub or GitLab sign-in: the agent helps its runner turn sign-in on, or helps a viewer ask for a link. */
export function signInHelpPrompt(rawOrigin: string): string | null {
  const origin = safeHubOrigin(rawOrigin)
  return origin
    ? `The AgentSCM Hub at ${origin} has no GitHub or GitLab sign-in, so its web page only opens with a read-only token. If I run this Hub, help me turn sign-in on: follow "Sign-in configuration" in https://github.com/agentscm/agentscm/blob/main/docs/hub.md and walk me through creating the OAuth app; I will handle the client secret myself. If I don't run it, help me write a short message asking whoever does for a read-only link. Do not edit trust records, expose tokens or deploy a Hub.`
    : null
}
