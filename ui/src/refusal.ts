// Why the Hub said no, in the words the agent and the menu bar app print for the same refusal.
// The Hub writes the sentence; this page shows it as is. Only when an older Hub sends a bare code
// does the page compose the sentence itself, from the same fields and in the same order.

export interface Refusal {
  code: string
  /** One sentence: who, what they have, what is needed. */
  message: string
  /** The one thing to do about it, when the Hub names one. */
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
const role = (v: unknown): string | null => {
  const r = line(v, 40)
  return r && NAME.test(r) ? r[0]!.toUpperCase() + r.slice(1).toLowerCase() : null
}

/** Sentences for codes an older Hub sends without its own message. Never shown when the Hub wrote one. */
const KNOWN: Record<string, string> = {
  not_connected: 'This repository isn’t connected to this Hub yet; ask a Hub admin to connect it.',
  repo_not_found: 'This Hub can’t see that repository; check the name, or ask a Hub admin to connect it.',
  session_expired: 'Your sign-in on this computer has ended; sign in again.',
  device_revoked: 'This computer was removed from your account; sign in again to add it back.',
}

/** `login has Role on visibility repo owner/name; ask an owner for Role` — the shared refusal sentence. */
export function roleSentence(f: { login: string; role: string; visibility: string | null; repo: string; needs: string }): string {
  return `${f.login} has ${f.role} on ${f.visibility ? `${f.visibility} ` : ''}repo ${f.repo}; ask an owner for ${f.needs}`
}

/** The refusal in an error response body, or null when there is nothing a person can act on. */
export function refusalFrom(body: unknown): Refusal | null {
  const b = rec(body)
  const nested = rec(b['error'])
  // `{"error":"code"}` today; `{"error":{"code":…,"message":…}}` or flat fields are accepted too.
  const code = line(typeof b['error'] === 'string' ? b['error'] : (nested['code'] ?? b['code']), 64)
  if (!code || !CODE.test(code)) return null
  const fields = Object.keys(nested).length ? nested : b
  const fix = line(fields['fix'])
  const written = line(fields['message'])
  if (written) return { code, message: written, fix }
  const login = line(fields['login'], 100)
  const repo = line(fields['repo'] ?? fields['repository'], 200)
  const have = role(fields['role'])
  const needs = role(fields['needs'] ?? fields['required_role'])
  const visibility = line(fields['visibility'], 20)
  if (login && NAME.test(login) && repo && REPO.test(repo) && have && needs)
    return { code, message: roleSentence({ login, role: have, visibility: visibility && /^[a-z]+$/.test(visibility) ? visibility : null, repo, needs }), fix }
  return Object.hasOwn(KNOWN, code) ? { code, message: KNOWN[code]!, fix } : null
}
