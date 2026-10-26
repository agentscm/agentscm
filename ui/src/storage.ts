// Browser storage can throw (private windows, blocked site data): never rely on it.
const TOKEN_KEY = 'agentscm.token'
const THEME_KEY = 'agentscm.theme'
const DENSITY_KEY = 'agentscm.density'

function stores(): Storage[] {
  const out: Storage[] = []
  try {
    out.push(window.localStorage)
  } catch {
    /* unavailable */
  }
  try {
    out.push(window.sessionStorage)
  } catch {
    /* unavailable */
  }
  return out
}

export function loadToken(): string | null {
  for (const s of stores()) {
    try {
      const v = s.getItem(TOKEN_KEY)
      if (v) return v
    } catch {
      /* ignore */
    }
  }
  return null
}

export function clearToken(): void {
  for (const s of stores()) {
    try {
      s.removeItem(TOKEN_KEY)
    } catch {
      /* ignore */
    }
  }
}

/**
 * Whether a `#token=` in the URL may replace what is stored: only `agentscm demo` produces such
 * links, so outside demo mode a stored token stays, and a link cannot swap it for someone else's.
 */
export function fragmentTokenAllowed(stored: string | null, demo: boolean): boolean {
  return demo || stored === null
}

/**
 * After the Hub refuses `refused`: the sign-in another tab saved since, if any, so a stale tab
 * adopts it instead of erasing it; `null` means sign out.
 */
export function signInAfterRefusal(refused: string | null, stored: string | null): string | null {
  return stored && stored !== refused ? stored : null
}

/** Unchecked "keep me signed in" uses sessionStorage, checked uses localStorage. */
export function saveToken(token: string, keep: boolean): void {
  clearToken()
  try {
    ;(keep ? window.localStorage : window.sessionStorage).setItem(TOKEN_KEY, token)
  } catch {
    /* the session still works in memory */
  }
}

export type ThemePref = 'light' | 'dark' | 'system'

export function loadTheme(): ThemePref {
  try {
    const v = window.localStorage.getItem(THEME_KEY)
    if (v === 'light' || v === 'dark' || v === 'system') return v
  } catch {
    /* ignore */
  }
  return 'system'
}

export function saveTheme(t: ThemePref): void {
  try {
    window.localStorage.setItem(THEME_KEY, t)
  } catch {
    /* ignore */
  }
}

export type Density = 'spacious' | 'condensed'

export function loadDensity(): Density {
  try {
    const v = window.localStorage.getItem(DENSITY_KEY)
    if (v === 'spacious' || v === 'condensed') return v
    if (v === 'compact') return 'condensed' // the earlier name
  } catch {
    /* ignore */
  }
  return 'spacious'
}

export function saveDensity(d: Density): void {
  try {
    window.localStorage.setItem(DENSITY_KEY, d)
  } catch {
    /* ignore */
  }
}

const KEYS_KEY = 'agentscm.shortcuts'

/** Single-key shortcuts (`/`, `[`) are on unless switched off in Settings (WCAG 2.1.4); ⌘K / Ctrl K always works. */
export function loadSingleKeys(): boolean {
  try {
    return window.localStorage.getItem(KEYS_KEY) !== 'off'
  } catch {
    return true
  }
}

export function saveSingleKeys(on: boolean): void {
  try {
    if (on) window.localStorage.removeItem(KEYS_KEY)
    else window.localStorage.setItem(KEYS_KEY, 'off')
  } catch {
    /* ignore */
  }
}
