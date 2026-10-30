// The Hub's browser sign-in: which forges are configured, and where each one's sign-in starts.
export type LoginProvider = 'github' | 'gitlab'
export const PROVIDER_NAMES: Record<LoginProvider, string> = { github: 'GitHub', gitlab: 'GitLab' }

/** `web` signs in this browser (the Hub then opens a repository); `workspace` signs in to the workspace. */
export const loginHref = (provider: LoginProvider, flow: 'web' | 'workspace' = 'web'): string => `/v1/hub/login/${provider}?${flow}=1`

export const knownProviders = (list: unknown): LoginProvider[] => (Array.isArray(list) ? list.filter((p): p is LoginProvider => p === 'github' || p === 'gitlab') : [])

/** GET /v1/hub/login. Any failure (404, offline, odd body) means no provider: the token form is then the way in. */
export async function fetchLoginProviders(signal: AbortSignal): Promise<LoginProvider[]> {
  try {
    const res = await fetch('/v1/hub/login', { signal, cache: 'no-store', credentials: 'omit' })
    if (!res.ok) return []
    return knownProviders(((await res.json()) as { providers?: unknown }).providers)
  } catch {
    return []
  }
}
