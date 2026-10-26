// Why the Hub said no, in the words the agent and the menu bar app print for the same refusal.
// When the Hub writes the sentence (`message`), this page shows it as is. For a bare code it uses
// the shared sentence for that code, built from the same fields in the same order as the CLI.

export interface Refusal {
  code: string
  /** One sentence: what is wrong; the one fix. */
  message: string
  /** A further step, when the Hub names one. */
  fix: string | null
}

const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {})
// Server text is shown as plain text, but still bounded and kept to one line.
const line = (v: unknown, max = 300): string | null => {
  if (typeof v !== 'string') return null
  // eslint-disable-next-line no-control-regex
  const t = v.replace(/[\u0000-\u001f\u007f]+/g, ' ').replace(/\s+/g, ' ').trim()
  return t ? (t.length > max ? `${t.slice(0, max - 1)}…` : t) : null
}
const CODE = /^[a-z][a-z0-9_]{0,63}$/
const NAME = /^[A-Za-z0-9._-]{1,100}$/
const REPO = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._/-]+$/
/** `minimal_access` → `Minimal access`, as the forge names its roles. */
const role = (v: unknown): string | null => {
  const r = line(v, 40)
  if (!r || !/^[a-z_]+$/.test(r)) return null
  const words = r.replace(/_/g, ' ')
  return words[0]!.toUpperCase() + words.slice(1)
}
const forge = (v: unknown): string => (v === 'gitlab' ? 'GitLab' : v === 'github' ? 'GitHub' : 'Your forge')

type Fields = { repo: string | null; provider: unknown }
/** The shared sentence for each code that arrives without the Hub's own message. */
const KNOWN: Record<string, (f: Fields) => string> = {
  not_connected: () => 'This repository isn’t connected to this Hub yet; ask a Hub admin to connect it.',
  forbidden: () => 'This Hub won’t open that repository; it may have been renamed or moved, so check the name.',
  no_repository_role: () => 'Your account has no role on a repository connected to this Hub; ask a repository owner to add you, then sign in again.',
  not_allowed: () => 'This Hub only lets in people its admins have allowed; ask a Hub admin to add you.',
  user_disabled: () => 'Your account is turned off on this Hub; ask a Hub admin to turn it back on.',
  login_failed: () => 'Sign-in didn’t finish; choose Sign In in the AgentSCM menu bar app to try again.',
  sign_in_required: () => 'This computer isn’t signed in to this Hub any more; choose Sign In in the AgentSCM menu bar app.',
  access_check_unavailable: (f) => `The Hub can’t check your access to ${f.repo ?? 'that repository'}; ask a Hub admin to check that repository’s connection.`,
  permission_unavailable: (f) => `${forge(f.provider)} didn’t answer the Hub’s access check; nothing changed, so try again in a minute.`,
  device_revoked: () => 'This computer was removed from your account; sign in again to add it back.',
}

/** The sentence for a refusal reason shown without a response body, e.g. in the admin list. */
export const reasonSentence = (code: string, provider?: string): string | null =>
  Object.hasOwn(KNOWN, code) ? KNOWN[code]!({ repo: null, provider }) : null

/** `login has Role on visibility repository owner/name; ask a repository owner for Role or higher`. */
export function roleSentence(f: { login: string; has: string; visibility: string | null; repo: string; needs: string }): string {
  const what = f.has === 'None' ? `no access to ${f.visibility ? `${f.visibility} ` : ''}repository ${f.repo}` : `${f.has} on ${f.visibility ? `${f.visibility} ` : ''}repository ${f.repo}`
  return `${f.login} has ${what}; ask a repository owner for ${f.needs} or higher`
}

/** The refusal in an error response body, or null when there is nothing a person can act on. */
export function refusalFrom(body: unknown): Refusal | null {
  const b = rec(body)
  const nested = rec(b['error'])
  // `{"error":"code", …fields}` from the Hub; `{"error":{"code":…}}` is accepted too.
  const code = line(typeof b['error'] === 'string' ? b['error'] : (nested['code'] ?? b['code']), 64)
  if (!code || !CODE.test(code)) return null
  const fields = Object.keys(nested).length ? nested : b
  const fix = line(fields['fix'])
  const written = line(fields['message'])
  if (written) return { code, message: written, fix }
  const repoRaw = line(fields['repository'] ?? fields['repo'], 200)
  const repo = repoRaw && REPO.test(repoRaw) ? repoRaw : null
  const login = line(fields['login'], 100)
  const has = role(fields['has'] ?? fields['role'])
  const needs = role(fields['needs'])
  const visibility = line(fields['visibility'], 20)
  if (login && NAME.test(login) && repo && has && needs)
    return { code, message: roleSentence({ login, has, visibility: visibility && /^[a-z]+$/.test(visibility) ? visibility : null, repo, needs }), fix }
  return Object.hasOwn(KNOWN, code) ? { code, message: KNOWN[code]!({ repo, provider: fields['provider'] }), fix } : null
}
