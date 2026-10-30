import { describe, expect, it } from 'vitest'
import { LAUNCH_MAX_URL, OPENS_ONLY_LINKS, launchKindOfAgent, launchKindOfClient, launchLink, originUrlFor } from '../../src/launchLink.ts'

const prompt = 'Use AgentSCM. Read agentscm_work_overview & plan: fix “login”.\nThen start.'
const enc = encodeURIComponent(prompt)

describe('launchLink', () => {
  it('builds each confirmed scheme and nothing else', () => {
    expect(launchLink('claude-code', prompt, { repo: 'acme/widgets' })).toBe(`claude-cli://open?repo=acme/widgets&q=${enc}`)
    expect(launchLink('claude-code', prompt)).toBe(`claude-cli://open?q=${enc}`)
    expect(launchLink('claude-desktop', prompt)).toBe(`claude://code/new?q=${enc}`)
    expect(launchLink('claude-desktop', prompt, { repo: 'acme/widgets' })).toBe(`claude://code/new?q=${enc}`)
    expect(launchLink('claude-code-vscode', prompt)).toBe(`vscode://anthropic.claude-code/open?prompt=${enc}`)
    expect(launchLink('codex', prompt, { originUrl: 'https://github.com/acme/widgets' })).toBe(`codex://threads/new?prompt=${enc}&originUrl=${encodeURIComponent('https://github.com/acme/widgets')}`)
    expect(launchLink('codex', prompt)).toBe(`codex://threads/new?prompt=${enc}`)
    expect(launchLink('cursor', prompt)).toBe(`cursor://anysphere.cursor-deeplink/prompt?text=${enc}`)
  })

  it('has no prompt link for Copilot, Gemini or anything it does not know', () => {
    for (const k of ['copilot', 'gemini', 'grok', 'windsurf', 'zed', '']) expect(launchLink(k, prompt)).toBeNull()
  })

  it('knows the one scheme that only opens an app (Gemini) and puts no prompt in it', () => {
    expect(OPENS_ONLY_LINKS).toEqual({ gemini: 'googlegemini://' })
  })

  it('refuses a URL over 1,000 characters, and an empty prompt', () => {
    expect(launchLink('cursor', 'x'.repeat(LAUNCH_MAX_URL))).toBeNull()
    expect(launchLink('cursor', 'x'.repeat(900))!.length).toBeLessThanOrEqual(LAUNCH_MAX_URL)
    expect(launchLink('cursor', '  ')).toBeNull()
    expect(launchLink('claude-desktop', 'x'.repeat(LAUNCH_MAX_URL))).toBeNull()
  })

  it('never carries a token, a pairing fragment or a malformed repository', () => {
    expect(launchLink('cursor', 'open http://127.0.0.1:7340/#pair=abc123')).toBeNull()
    expect(launchLink('cursor', 'Authorization: Bearer abcdef')).toBeNull()
    expect(launchLink('claude-desktop', 'open http://127.0.0.1:7340/#pair=abc123')).toBeNull()
    expect(launchLink('cursor', 'use ghp_abcdefghijklmnop')).toBeNull()
    expect(launchLink('claude-code', 'hi', { repo: 'acme/widgets&x=1' })).toBe('claude-cli://open?q=hi')
    expect(launchLink('codex', 'hi', { originUrl: 'https://user:pw@github.com/acme/widgets' })).toBe('codex://threads/new?prompt=hi')
    expect(launchLink('codex', 'hi', { originUrl: 'https://github.com/acme/widgets#pair=1' })).toBe('codex://threads/new?prompt=hi')
    expect(launchLink('codex', 'hi', { originUrl: 'http://github.com/acme/widgets' })).toBe('codex://threads/new?prompt=hi')
  })

  it('keeps a work id in the prompt (it is not a secret)', () => {
    expect(launchLink('cursor', `Resume wrk_${'a'.repeat(32)}`)).not.toBeNull()
  })
})

describe('agent kinds and origins', () => {
  it('maps hub agent names and Setup rows to the kinds that have a link', () => {
    expect(['claude-code', 'codex', 'cursor', 'copilot', 'gemini', 'client-host-6a36bc'].map(launchKindOfAgent)).toEqual(['claude-code', 'codex', 'cursor', null, null, null])
    expect(['claude', 'codex', 'cursor', 'copilot', 'gemini'].map(launchKindOfClient)).toEqual(['claude-code', 'codex', 'cursor', null, null])
  })
  it('builds a forge origin from the repository and sign-in host', () => {
    expect(originUrlFor('github', 'github.com', 'acme/widgets')).toBe('https://github.com/acme/widgets')
    expect(originUrlFor('gitlab', undefined, 'group/sub/app')).toBe('https://gitlab.com/group/sub/app')
    expect(originUrlFor('other', undefined, 'acme/widgets')).toBeNull()
    expect(originUrlFor('github', 'github.com', 'bad repo')).toBeNull()
  })
})
