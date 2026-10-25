// Conversation sharing for the checkouts on this computer, served by the local service
// (`/v1/local/capture`). The service owns the setting; this module only reads it and asks for a
// change. Turning sharing on needs `confirm_sharing`, which callers send only after the person has
// seen what is uploaded.
export type SharingMode = 'metadata_only' | 'local_conversation' | 'shared_conversation'

export interface SharingCheckout {
  /** Opaque name the service gives this checkout, so two checkouts of one repository stay apart. */
  id: string
  repository: string
  hub: string
  folder: string
  configured: boolean
  enabled: boolean
  /** The mode while the checkout's capture is enabled. */
  mode: SharingMode | null
  /** False when the saved capture settings could not be read, so nothing is offered. */
  readable: boolean
}

const MODES = ['metadata_only', 'local_conversation', 'shared_conversation'] as const
const MAX_CHECKOUTS = 200

function object(value: unknown, path: string): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`${path} must be an object`)
  return value as Record<string, unknown>
}
const text = (value: unknown, path: string): string => {
  if (typeof value !== 'string' || value.length > 2048 || [...value].some((c) => c.charCodeAt(0) < 32 || (c.charCodeAt(0) >= 127 && c.charCodeAt(0) < 160))) throw new Error(`${path} must be text`)
  return value
}
const flag = (value: unknown, path: string): boolean => {
  if (typeof value !== 'boolean') throw new Error(`${path} must be true or false`)
  return value
}

export function decodeCheckout(value: unknown, path = 'checkout'): SharingCheckout {
  const raw = object(value, path)
  const mode = raw['mode']
  if (mode !== null && !(MODES as readonly unknown[]).includes(mode)) throw new Error(`${path}.mode is not a known mode`)
  const id = text(raw['id'], `${path}.id`)
  if (!/^[0-9a-f]{8,64}$/.test(id)) throw new Error(`${path}.id must be an opaque id`)
  return {
    id,
    repository: text(raw['repository'], `${path}.repository`),
    hub: text(raw['hub'], `${path}.hub`),
    folder: text(raw['folder'], `${path}.folder`),
    configured: flag(raw['configured'], `${path}.configured`),
    enabled: flag(raw['enabled'], `${path}.enabled`),
    mode: mode as SharingMode | null,
    readable: flag(raw['readable'], `${path}.readable`),
  }
}

export function decodeCheckouts(value: unknown): SharingCheckout[] {
  const raw = object(value, 'capture')
  if (raw['version'] !== 1 || !Array.isArray(raw['checkouts']) || raw['checkouts'].length > MAX_CHECKOUTS) throw new Error('capture is not a version 1 list')
  return raw['checkouts'].map((item, index) => decodeCheckout(item, `capture.checkouts[${index}]`))
}

const headers = (token: string) => ({ Authorization: `Bearer ${token}` })

export async function fetchSharing(token: string, signal: AbortSignal): Promise<SharingCheckout[]> {
  const res = await fetch('/v1/local/capture', { signal, cache: 'no-store', credentials: 'omit', headers: headers(token) })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return decodeCheckouts(await res.json())
}

/** Sharing is on only while the checkout's capture is enabled in shared mode. */
export const isSharing = (checkout: SharingCheckout): boolean => checkout.enabled && checkout.mode === 'shared_conversation'

/** Turn sharing on (after the person confirmed) or off for one listed checkout, named by its id. Returns its new state. */
export async function setSharing(token: string, checkout: SharingCheckout, on: boolean, signal?: AbortSignal): Promise<SharingCheckout> {
  // The repository and Hub are what the person saw; the service refuses the change if the checkout
  // is connected to anything else by now.
  const shown = { id: checkout.id, repository: checkout.repository, hub: checkout.hub }
  const body = on ? { ...shown, mode: 'shared_conversation', confirm_sharing: true } : { ...shown, mode: 'metadata_only' }
  const res = await fetch('/v1/local/capture', {
    method: 'POST',
    signal,
    cache: 'no-store',
    credentials: 'omit',
    headers: { ...headers(token), 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  const reply: unknown = await res.json().catch(() => null)
  if (!res.ok) {
    const message = reply && typeof reply === 'object' ? (reply as Record<string, unknown>)['message'] : null
    throw new Error(typeof message === 'string' && message ? message.slice(0, 300) : `HTTP ${res.status}`)
  }
  return decodeCheckout(object(reply, 'reply')['checkout'])
}
