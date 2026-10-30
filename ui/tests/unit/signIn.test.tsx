import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import App from '../../src/App.tsx'
import { adoptFragmentToken } from '../../src/fragmentToken.ts'
import { signInHelpPrompt } from '../../src/connectAgent.ts'
import { pastedToken, SignIn } from '../../src/pages/SignIn.tsx'

const door = vi.hoisted(() => ({ front: false }))
vi.mock('../../src/frontDoor.ts', async (original) => ({ ...(await original<typeof import('../../src/frontDoor.ts')>()), onFrontDoor: () => door.front }))

const stubLogin = (status: number, body: unknown) => {
  const calls: string[] = []
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(String(url))
    return new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
  })
  return calls
}
const front = () => render(<SignIn notice={null} onSignedIn={() => {}} />)

const decoded = (href: string) => decodeURIComponent(/[?&](?:q|prompt|text)=([^&]*)/.exec(href)![1]!)
const askLinks = () => ['claude://code/new?q=', 'codex://', 'cursor://'].map((scheme) => document.querySelector<HTMLAnchorElement>(`.signin-agents a[href^="${scheme}"]`)!)

afterEach(() => {
  vi.unstubAllGlobals()
  door.front = false
  window.history.replaceState(null, '', '/')
})

describe('the sign-in front door', () => {
  it('with providers shows one card: the buttons, the account line and the read-only-link line', async () => {
    const calls = stubLogin(200, { providers: ['github', 'gitlab'], browser_only: [] })
    const { container } = front()
    const gh = await screen.findByRole('link', { name: /Sign in with GitHub/ })
    expect(gh.getAttribute('href')).toBe('/v1/hub/login/github?web=1')
    expect(screen.getByRole('link', { name: /Sign in with GitLab/ }).getAttribute('href')).toBe('/v1/hub/login/gitlab?web=1')
    expect(screen.getByRole('region', { name: 'Sign in' })).toBeTruthy()
    expect(screen.getByText('Use the GitHub or GitLab account that can see your team’s repository.')).toBeTruthy()
    expect(screen.getByText('No access? Ask a teammate for a read-only link, then open it in this browser.')).toBeTruthy()
    expect(container.querySelectorAll('.signin-card')).toHaveLength(1)
    expect(document.querySelector('#token')).toBeNull()
    expect(screen.queryByRole('button', { name: /Setting up your own computer/ })).toBeNull()
    expect(screen.queryByText('Set up this computer')).toBeNull()
    expect(screen.queryByText(/has no GitHub or GitLab sign-in/)).toBeNull()
    expect(container.querySelector('.signin-points')).toBeNull()
    // Making a link for someone is tucked away, with the one command.
    const make = screen.getByText('Make a read-only link for someone').closest('details')!
    expect(make.open).toBe(false)
    expect(make.textContent).toContain('agentscm hub login')
    expect(make.textContent).toContain('agentscm hub token create --read-only')
    expect(calls).toEqual(['/v1/hub/login'])
  })

  it('while the provider list is pending shows only the headline and lead', async () => {
    vi.stubGlobal('fetch', () => new Promise(() => {}))
    const { container } = front()
    expect(screen.getByRole('heading', { level: 1 })).toBeTruthy()
    expect(container.querySelector('form')).toBeNull()
    expect(container.querySelector('.signin-card')).toBeNull()
    expect(screen.queryByRole('link')).toBeNull()
  })

  it.each([
    ['an empty list', 200, { providers: [], browser_only: [] }],
    ['a 404', 404, { error: 'not_found' }],
  ])('with no providers (%s) shows the read-only token form and says why', async (_name, status, body) => {
    stubLogin(status, body)
    const { container } = front()
    expect(await screen.findByText(/This hub has no GitHub or GitLab sign-in, so it opens with a read-only token\. Ask whoever runs this hub for one\./)).toBeTruthy()
    expect(screen.queryByRole('link', { name: /Sign in with/ })).toBeNull()
    expect(screen.getByRole('form', { name: 'Open with a read-only token' })).toBeTruthy()
    expect(screen.getByLabelText('Read-only link or token')).toBeTruthy()
    expect(screen.getByRole('button', { name: /Open repository/ })).toBeTruthy()
    expect(container.querySelectorAll('.signin-card')).toHaveLength(1)
    expect(container.querySelector('details')).toBeNull()
    expect(container.textContent).not.toContain('Kept only in this browser.')
  })

  it('offers no token-creation command or computer set-up where the hub has no sign-in', async () => {
    stubLogin(200, { providers: [], browser_only: [] })
    const { container } = front()
    await screen.findByRole('form', { name: 'Open with a read-only token' })
    expect(container.textContent).not.toContain('agentscm hub token create')
    expect(container.textContent).not.toContain('agentscm hub login')
    expect(screen.queryByRole('button', { name: /Setting up your own computer/ })).toBeNull()
    expect(screen.queryByRole('region', { name: 'Set up this computer' })).toBeNull()
    expect(container.querySelector('.install')).toBeNull()
  })

  it('with providers adds a "Or let your agent set this up" row whose links carry the setup prompt', async () => {
    stubLogin(200, { providers: ['github'], browser_only: [] })
    front()
    expect(await screen.findByRole('heading', { name: 'Or let your agent set this up' })).toBeTruthy()
    const links = askLinks()
    for (const a of links) {
      expect(a).toBeTruthy()
      expect(decoded(a.getAttribute('href')!)).toContain('npx agentscm')
      expect(a.getAttribute('href')).not.toContain('token=')
    }
    expect(screen.getByRole('button', { name: 'Copy the prompt' })).toBeTruthy()
    expect(screen.getByText('Opens your agent with the request typed in, or copies it for you to paste. Nothing is sent until you press Enter.')).toBeTruthy()
  })

  it('puts a brand icon on every agent pill', async () => {
    stubLogin(200, { providers: ['github'], browser_only: [] })
    front()
    await screen.findByRole('heading', { name: 'Or let your agent set this up' })
    const pill = (name: string) => (screen.queryByRole('link', { name }) ?? screen.getByRole('button', { name })) as HTMLElement
    for (const n of ['Claude Code', 'Codex', 'Cursor', 'Copilot', 'Gemini', 'Grok']) expect(pill(n).querySelector('svg[aria-hidden="true"] path')).toBeTruthy()
    // The copy button is a sentence link under the pills, not a pill.
    expect(screen.getByRole('button', { name: 'Copy the prompt' }).querySelector('.bi-clipboard')).toBeTruthy()
    expect(document.querySelector('.signin-agents-actions')!.contains(screen.getByRole('button', { name: 'Copy the prompt' }))).toBe(false)
    expect(document.querySelector('.signin-agents-copy')!.textContent).toMatch(/^Using a different agent\?/)
    const labels = [...document.querySelectorAll('.signin-agents-actions > a.signin-agent, .signin-agents-actions > .signin-split > a, .signin-agents-actions > button')].map((e) => e.textContent!.trim())
    expect(labels).toEqual(['Claude Code', 'Codex', 'Cursor', 'Copilot', 'Gemini', 'Grok'])
  })

  it('Copilot and Grok copy the prompt and say where to paste it; they are not links', async () => {
    stubLogin(200, { providers: ['github'], browser_only: [] })
    const writeText = vi.fn<(text: string) => Promise<void>>(async () => {})
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } })
    front()
    await screen.findByRole('heading', { name: 'Or let your agent set this up' })
    for (const name of ['Copilot', 'Grok']) {
      expect(screen.queryByRole('link', { name })).toBeNull()
      const btn = screen.getByRole('button', { name })
      expect(btn.getAttribute('title')).toBe(`Copies the prompt. Paste it into ${name}.`)
      expect(btn.classList.contains('signin-agent-open')).toBe(false)
    }
    fireEvent.click(screen.getByRole('button', { name: 'Grok' }))
    expect(await screen.findByRole('button', { name: 'Copied, paste in Grok' })).toBeTruthy()
    expect(writeText).toHaveBeenCalledOnce()
    expect(writeText.mock.calls[0]![0]).toContain('npx agentscm')
    expect([...document.querySelectorAll('.signin-agents [role="status"]')].map((e) => e.textContent)).toEqual(['', '', 'Copied, paste in Grok'])
  })

  it('Gemini copies the prompt, then opens the app with the scheme that carries no prompt', async () => {
    stubLogin(200, { providers: ['github'], browser_only: [] })
    const writeText = vi.fn<(text: string) => Promise<void>>(async () => {})
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } })
    const opened = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    front()
    await screen.findByRole('heading', { name: 'Or let your agent set this up' })
    const btn = screen.getByRole('button', { name: 'Gemini' })
    expect(btn.getAttribute('title')).toBe('Copies the prompt and opens the Gemini app. Paste it there.')
    fireEvent.click(btn)
    expect(await screen.findByRole('button', { name: 'Copied, paste in Gemini' })).toBeTruthy()
    expect(writeText.mock.calls[0]![0]).toContain('npx agentscm')
    expect(opened).toHaveBeenCalledOnce()
    expect((opened.mock.contexts[0] as HTMLAnchorElement).href).toBe('googlegemini://')
    opened.mockRestore()
  })

  describe('noticing that nothing opened', () => {
    const noNav = (e: Event) => e.preventDefault()
    const note = () => document.querySelector<HTMLElement>('.signin-agents-miss')!
    const setup = async () => {
      document.addEventListener('click', noNav) // jsdom cannot follow a custom scheme
      stubLogin(200, { providers: ['github'], browser_only: [] })
      front()
      await screen.findByRole('heading', { name: 'Or let your agent set this up' })
      vi.useFakeTimers()
    }
    afterEach(() => {
      document.removeEventListener('click', noNav)
      vi.useRealTimers()
      vi.restoreAllMocks()
    })

    it('says which app did not open, with a download link, when the page keeps focus', async () => {
      await setup()
      fireEvent.click(document.querySelector('.signin-agents a[href^="cursor://"]')!)
      act(() => { vi.advanceTimersByTime(1499) })
      expect(document.querySelector('.signin-agents-miss')).toBeNull()
      act(() => { vi.advanceTimersByTime(2) })
      const miss = note()
      expect(miss.textContent).toBe('Cursor didn’t open. Is it installed? Get Cursor, or copy the prompt below.')
      const get = within(miss).getByRole('link', { name: 'Get Cursor' })
      expect(get.getAttribute('href')).toBe('https://cursor.com/download')
      expect(get.getAttribute('target')).toBe('_blank')
      expect(get.getAttribute('rel')).toBe('noreferrer')
    })

    it('stays quiet when the window blurs, or the page is hidden, before the wait ends', async () => {
      await setup()
      fireEvent.click(document.querySelector('.signin-agents a[href^="codex://"]')!)
      act(() => { window.dispatchEvent(new Event('blur')) })
      act(() => { vi.advanceTimersByTime(3000) })
      expect(document.querySelector('.signin-agents-miss')).toBeNull()
      fireEvent.click(document.querySelector('.signin-agents a[href^="cursor://"]')!)
      vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden')
      act(() => { document.dispatchEvent(new Event('visibilitychange')) })
      act(() => { vi.advanceTimersByTime(3000) })
      expect(document.querySelector('.signin-agents-miss')).toBeNull()
    })

    it('covers Claude and Gemini too, and the next click replaces the message and the timer', async () => {
      await setup()
      fireEvent.click(document.querySelector('.signin-agents a[href^="claude://"]')!)
      act(() => { vi.advanceTimersByTime(1600) })
      const claude = note()
      expect(claude.textContent).toContain('Claude didn’t open.')
      expect(within(claude).getByRole('link', { name: 'Get Claude' }).getAttribute('href')).toBe('https://claude.ai/download')
      fireEvent.click(document.querySelector('.signin-agents a[href^="codex://"]')!)
      expect(document.querySelector('.signin-agents-miss')).toBeNull()
      act(() => { vi.advanceTimersByTime(1600) })
      expect(within(note()).getByRole('link', { name: 'Get Codex' }).getAttribute('href')).toBe('https://openai.com/codex/')
      fireEvent.click(screen.getByRole('button', { name: 'Gemini' }))
      act(() => { vi.advanceTimersByTime(1600) })
      expect(within(note()).getByRole('link', { name: 'Get Gemini' }).getAttribute('href')).toBe('https://gemini.google/mac/')
    })

    it('clears its timer and listeners when the row goes away', async () => {
      await setup()
      const off = vi.spyOn(window, 'removeEventListener')
      const clear = vi.spyOn(window, 'clearTimeout')
      fireEvent.click(document.querySelector('.signin-agents a[href^="cursor://"]')!)
      cleanup()
      expect(off).toHaveBeenCalledWith('blur', expect.any(Function))
      expect(clear).toHaveBeenCalled()
    })
  })

  describe('leaving for the provider', () => {
    afterEach(() => vi.useRealTimers())

    it('morphs the sheet to "Taking you to GitHub", then goes to the login URL after the beat', async () => {
      stubLogin(200, { providers: ['github'], browser_only: [] })
      const loc = { href: 'http://localhost/', origin: 'http://localhost' }
      vi.stubGlobal('location', loc)
      front()
      const btn = await screen.findByRole('link', { name: /Sign in with GitHub/ })
      vi.useFakeTimers()
      fireEvent.click(btn)
      const item = screen.getByText('Taking you to GitHub').closest('li')!
      expect(item.getAttribute('aria-current')).toBe('step')
      expect(screen.getByText('Checking your sign-in')).toBeTruthy()
      expect(screen.queryByRole('link', { name: /Sign in with GitHub/ })).toBeNull()
      expect(document.querySelectorAll('.signin-pane-sheet')).toHaveLength(1)
      expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('See what every agent is working on.')
      expect(loc.href).toBe('http://localhost/')
      act(() => { vi.advanceTimersByTime(299) })
      expect(loc.href).toBe('http://localhost/')
      act(() => { vi.advanceTimersByTime(2) })
      expect(loc.href).toBe('/v1/hub/login/github?web=1')
      // Back from the bfcache: the normal sign-in view returns.
      act(() => { window.dispatchEvent(Object.assign(new Event('pageshow'), { persisted: true })) })
      expect(screen.getByRole('link', { name: /Sign in with GitHub/ })).toBeTruthy()
      expect(screen.queryByText('Taking you to GitHub')).toBeNull()
    })

    it('leaves modified clicks to the browser', async () => {
      stubLogin(200, { providers: ['github'], browser_only: [] })
      front()
      const btn = await screen.findByRole('link', { name: /Sign in with GitHub/ })
      expect(fireEvent.click(btn, { metaKey: true })).toBe(true)
      expect(screen.queryByText('Taking you to GitHub')).toBeNull()
    })
  })

  it('opens Claude Code in the desktop app, with the Terminal one menu away', async () => {
    stubLogin(200, { providers: ['github'], browser_only: [] })
    front()
    await screen.findByRole('heading', { name: 'Or let your agent set this up' })
    const main = screen.getByRole('link', { name: 'Claude Code' })
    expect(main.getAttribute('href')).toMatch(/^claude:\/\/code\/new\?q=/)
    expect(decoded(main.getAttribute('href')!)).toContain('npx agentscm --yes --json --hub-url')
    expect(screen.queryByRole('menu')).toBeNull()
    const toggle = screen.getByRole('button', { name: 'Other ways to open Claude Code' })
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    fireEvent.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    const menu = screen.getByRole('menu')
    expect(within(menu).getByRole('menuitem', { name: /Desktop app/ }).getAttribute('href')).toBe(main.getAttribute('href'))
    const terminal = within(menu).getByRole('menuitem', { name: /Terminal/ })
    expect(terminal.getAttribute('href')).toMatch(/^claude-cli:\/\/open\?q=/)
    expect(decoded(terminal.getAttribute('href')!)).toBe(decoded(main.getAttribute('href')!))
    expect(terminal.textContent).toContain('Opens a new Terminal window. macOS asks once to let Claude Code control it.')
    const vscode = within(menu).getByRole('menuitem', { name: /VS Code/ })
    expect(vscode.getAttribute('href')).toMatch(/^vscode:\/\/anthropic\.claude-code\/open\?prompt=/)
    expect(decoded(vscode.getAttribute('href')!)).toBe(decoded(main.getAttribute('href')!))
    expect(vscode.querySelector('.bi-code-slash')).toBeTruthy()
    expect(within(menu).getByRole('menuitem', { name: /Desktop app/ }).querySelector('.bi-window')).toBeTruthy()
    expect(terminal.querySelector('.bi-terminal')).toBeTruthy()
    for (const a of document.querySelectorAll('.signin-agents a')) expect(a.getAttribute('href')).not.toContain('token=')
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(screen.queryByRole('menu')).toBeNull()
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
  })

  it('with no providers adds an "Or ask your agent for help" row whose links carry the sign-in help prompt', async () => {
    stubLogin(200, { providers: [], browser_only: [] })
    front()
    expect(await screen.findByRole('heading', { name: 'Or ask your agent for help' })).toBeTruthy()
    for (const a of askLinks()) {
      expect(a).toBeTruthy()
      expect(decoded(a.getAttribute('href')!)).toContain('has no GitHub or GitLab sign-in')
      expect(a.getAttribute('href')).not.toContain('token=')
    }
    expect(screen.getByRole('button', { name: 'Copy the prompt' })).toBeTruthy()
    expect(screen.queryByRole('heading', { name: 'Or let your agent set this up' })).toBeNull()
  })

  it('on this computer’s front door shows no agent row', () => {
    door.front = true
    const { container } = front()
    expect(screen.getByRole('heading', { name: 'Connect this computer' })).toBeTruthy()
    expect(container.querySelector('.signin-agents')).toBeNull()
  })

  it('refuses a pasted link to another hub before sending anything, and an empty field', async () => {
    const calls = stubLogin(200, { providers: [], browser_only: [] })
    front()
    const input = await screen.findByLabelText('Read-only link or token')
    fireEvent.click(screen.getByRole('button', { name: /Open repository/ }))
    expect((await screen.findByRole('alert')).textContent).toContain('Paste a read-only token to continue.')
    expect(input.getAttribute('aria-describedby')).toBe('token-error')
    fireEvent.change(input, { target: { value: 'https://other.example/ui/#token=secret' } })
    fireEvent.click(screen.getByRole('button', { name: /Open repository/ }))
    expect((await screen.findByRole('alert')).textContent).toContain('That link is for another hub (https://other.example).')
    expect(calls).toEqual(['/v1/hub/login'])
  })
})

describe('signInHelpPrompt', () => {
  it('names the hub and what to do, and is null for an origin the Hub CLI would refuse', () => {
    const prompt = signInHelpPrompt('https://hub.example.com')
    expect(prompt).toContain('The AgentSCM Hub at https://hub.example.com has no GitHub or GitLab sign-in')
    expect(prompt).toContain('Do not edit trust records, expose tokens or deploy a Hub.')
    expect(prompt).not.toContain('token=')
    expect(signInHelpPrompt('http://hub.example.com')).toBeNull()
    expect(signInHelpPrompt('https://hub.example.com/?token=secret')).toBeNull()
  })
})

describe('pastedToken', () => {
  const here = 'https://hub.example'
  it('passes a bare token through', () => {
    expect(pastedToken('abc123', here)).toEqual({ token: 'abc123' })
  })
  it('takes the token from a link to this hub', () => {
    expect(pastedToken('https://hub.example/ui/#token=abc123', here)).toEqual({ token: 'abc123' })
  })
  it('refuses a link to another hub, naming it', () => {
    expect(pastedToken('https://evil.example/ui/#token=abc123', here)).toEqual({ error: 'That link is for another hub (https://evil.example). Open the link itself instead.' })
  })
  it('refuses a link with no token in it', () => {
    expect(pastedToken('https://hub.example/ui/', here)).toEqual({ error: 'That link has no read-only token in it. Paste the whole link, or just the token.' })
    expect(pastedToken('https://other.example/ui/#home', here)).toEqual({ error: expect.stringContaining('no read-only token') })
  })
})

describe('a #token= link the hub refuses', () => {
  it('lands on the front door with the one-line notice, even over a saved token the hub also refuses', async () => {
    localStorage.clear()
    sessionStorage.clear()
    localStorage.setItem('agentscm.token', 'stale-saved')
    window.history.replaceState(null, '', '/#token=bad-link')
    vi.stubGlobal('fetch', async (url: string) => {
      const u = String(url)
      if (u.endsWith('/v1/hub/login')) return new Response(JSON.stringify({ providers: ['github'] }), { status: 200, headers: { 'content-type': 'application/json' } })
      if (u.includes('/health')) return new Response(JSON.stringify({ status: 'ok' }), { status: 200, headers: { 'content-type': 'application/json' } })
      return new Response(JSON.stringify({ error: 'unauthorized' }), { status: 401, headers: { 'content-type': 'application/json' } })
    })
    await adoptFragmentToken()
    render(<App />)
    expect(await screen.findByText('That read-only link isn’t valid any more. Ask for a new one.', {}, { timeout: 5000 })).toBeTruthy()
    expect(await screen.findByRole('link', { name: /Sign in with GitHub/ })).toBeTruthy()
    expect(screen.queryByText(/Paste it here/)).toBeNull()
  })
})
