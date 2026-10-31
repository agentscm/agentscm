// Deep links that open a coding agent with a prompt already typed in. The agent shows the prompt and waits:
// nothing is sent until the person presses Enter. Only schemes the vendors document are built here; anything
// else returns null and the page keeps Copy. Pure: no DOM, no network.
//
// API for other packages:
//   LaunchKind                          'claude-code' (Terminal) | 'claude-desktop' | 'claude-code-vscode' | 'codex' | 'cursor'
//   launchLink(kind, prompt, ctx?)      the URL, or null (unknown kind, a token-like prompt, or over LAUNCH_MAX_URL)
//   launchKindOfAgent(name)             the kind for a hub agent name such as "claude-code", else null
//   launchKindOfClient(agent)           the kind for a Setup row's agent ("claude", "codex", "cursor"), else null
//   LAUNCH_LABEL                        "Claude Code", "Claude desktop app", "Codex", "Cursor"
//   originUrlFor(provider, host, repo)  "https://github.com/acme/widgets" for Codex to find the checkout
//   Handoff / handoffOf(whoami)        who is looking and which repository an Open link opens, from the hub's whoami
//   OPENS_ONLY_LINKS                    apps with a scheme but no prompt parameter (Gemini): opens the app, the prompt is copied
//   LAUNCH_NOTE                         the button title: it fills the prompt, it does not send it
export type LaunchKind = 'claude-code' | 'claude-desktop' | 'claude-code-vscode' | 'codex' | 'cursor'

/** Claude Code warns above 1,000 characters; a longer prompt stays Copy-only. */
export const LAUNCH_MAX_URL = 1000
export const LAUNCH_NOTE = 'Opens the app with this prompt filled in. Nothing is sent until you press Enter there.'
export const LAUNCH_LABEL: Record<LaunchKind, string> = { 'claude-code': 'Claude Code', 'claude-desktop': 'Claude desktop app', 'claude-code-vscode': 'Claude Code in VS Code', codex: 'Codex', cursor: 'Cursor' }

/** Apps that register a URL scheme but document no prompt parameter: the link only opens the app, so the caller copies the prompt first. */
export const OPENS_ONLY_LINKS = { gemini: 'googlegemini://' } as const

const REPO = /^[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)+$/
// A deep link lands in browser history and logs: refuse anything that looks like a credential or a pairing fragment.
const SECRET = /#pair=|\bpair=|\bBearer\s|\b(?:ghp|gho|ghs|ghu|github_pat|glpat)_[A-Za-z0-9_]{8,}/i

/** The Setup row's agent ("claude", "codex", "cursor") as a launch kind; Copilot, Gemini and the rest have no confirmed link. */
export function launchKindOfClient(agent: string): LaunchKind | null {
  return agent === 'claude' ? 'claude-code' : agent === 'codex' ? 'codex' : agent === 'cursor' ? 'cursor' : null
}

/** A hub agent name ("claude-code", "codex", "cursor") as a launch kind. Generated "client-…" names carry no kind. */
export function launchKindOfAgent(name: string): LaunchKind | null {
  const n = name.toLowerCase()
  return n === 'claude' || n === 'claude-code' ? 'claude-code' : n === 'codex' ? 'codex' : n === 'cursor' ? 'cursor' : null
}

/** `https://<host>/<owner>/<name>` for a GitHub or GitLab repository; null for anything else. */
export function originUrlFor(provider: string | undefined, host: string | undefined, repo: string | null | undefined): string | null {
  if (!repo || !REPO.test(repo)) return null
  const h = host && /^[a-z0-9.-]+$/i.test(host) ? host : provider === 'gitlab' ? 'gitlab.com' : provider === 'github' ? 'github.com' : null
  return h ? `https://${h}/${repo}` : null
}

const safeOrigin = (u: string | null | undefined): string | null => {
  try {
    const url = new URL(u ?? '')
    return url.protocol === 'https:' && !url.username && !url.password && !url.search && !url.hash ? url.origin + url.pathname.replace(/\/$/, '') : null
  } catch {
    return null
  }
}

/** The link that opens `kind` with `prompt` typed in, or null when none is confirmed or it would be too long to review. */
export function launchLink(kind: string, prompt: string, ctx: { repo?: string | null; originUrl?: string | null } = {}): string | null {
  const text = prompt.trim()
  if (!text || SECRET.test(text)) return null
  const q = encodeURIComponent(text)
  const repo = ctx.repo && REPO.test(ctx.repo) ? encodeURIComponent(ctx.repo).replace(/%2F/g, '/') : null
  const origin = safeOrigin(ctx.originUrl)
  let url: string
  switch (kind) {
    case 'claude-code':
      url = `claude-cli://open?${repo ? `repo=${repo}&` : ''}q=${q}`
      break
    case 'claude-desktop':
      url = `claude://code/new?q=${q}`
      break
    case 'claude-code-vscode':
      url = `vscode://anthropic.claude-code/open?prompt=${q}`
      break
    case 'codex':
      url = `codex://threads/new?prompt=${q}${origin ? `&originUrl=${encodeURIComponent(origin)}` : ''}`
      break
    case 'cursor':
      url = `cursor://anysphere.cursor-deeplink/prompt?text=${q}`
      break
    default:
      return null
  }
  return url.length > LAUNCH_MAX_URL ? null : url
}

/** Who is looking, and which repository an "Open in" link should open: the viewer's own agents only. */
export interface Handoff {
  me: string | null
  repo: string | null
  originUrl: string | null
}

export function handoffOf(w: { token: { repository: string }; owner: { login: string; provider?: string; host?: string } } | null | undefined): Handoff {
  const repo = w?.token.repository ?? null
  return { me: w?.owner.login ?? null, repo, originUrl: originUrlFor(w?.owner.provider, w?.owner.host, repo) }
}
