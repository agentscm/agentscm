// Hand a request to the person's own coding agent: deep links that fill the prompt, copy buttons for the
// rest, and a check that says when an app seemed not to open. Used by sign-in and All repositories.
import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from 'react'
import { BrandIcon } from './agentIcons.tsx'
import { LAUNCH_LABEL, LAUNCH_NOTE, OPENS_ONLY_LINKS, launchLink } from './launchLink.ts'
import { useDismiss } from './TopNav.tsx'
import { CopyButton, Icon } from './ui.tsx'

const OPEN_WAIT_MS = 1500
/** Where to get each app, shown when its link seemed to do nothing. */
const GET_APP = {
  Claude: 'https://claude.ai/download',
  Codex: 'https://openai.com/codex/',
  Cursor: 'https://cursor.com/download',
  Gemini: 'https://gemini.google/mac/',
} as const
type App = keyof typeof GET_APP

/** A browser cannot see installed apps. When an app opens, the page loses focus or is hidden; if neither happens soon after
 *  the click, assume the app is missing. `watch(app)` arms the check; the message names the app and links to its download. */
function useOpenedCheck() {
  const [missing, setMissing] = useState<App | null>(null)
  const stop = useRef<() => void>(() => {})
  const watch = useCallback((app: App) => {
    stop.current()
    setMissing(null)
    const done = () => { window.clearTimeout(timer); window.removeEventListener('blur', done); document.removeEventListener('visibilitychange', onHide); stop.current = () => {} }
    const onHide = () => { if (document.visibilityState === 'hidden') done() }
    const timer = window.setTimeout(() => { done(); setMissing(app) }, OPEN_WAIT_MS)
    window.addEventListener('blur', done)
    document.addEventListener('visibilitychange', onHide)
    stop.current = done
  }, [])
  useEffect(() => () => stop.current(), [])
  return { missing, watch }
}

/** Follow a custom-scheme link from a click handler, so it stays inside the user gesture. */
const openApp = (url: string) => Object.assign(document.createElement('a'), { href: url }).click()

const OpenedNote = ({ app }: { app: App | null }) => app && (
  <p className="signin-agents-miss" role="status">
    {app} didn’t open. Is it installed? <a href={GET_APP[app]} target="_blank" rel="noreferrer">Get {app}</a>, or copy the prompt below.
  </p>
)

/** Claude Code opens in the desktop app; the Terminal and VS Code are in the menu. A link that is too long or token-like is dropped. */
function ClaudeSplit({ prompt, repo, watch }: { prompt: string; repo: string | null; watch: (app: App) => void }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  const desktop = launchLink('claude-desktop', prompt)
  const terminal = launchLink('claude-code', prompt, { repo })
  const vscode = launchLink('claude-code-vscode', prompt)
  const main = desktop ?? terminal
  if (!main) return null
  const go = () => watch('Claude')
  if (!desktop || !terminal) return <a className="btn signin-agent signin-agent-open" href={main} rel="noreferrer" title={LAUNCH_NOTE} onClick={go}><BrandIcon name="claudecode" /> Claude Code</a>
  return (
    <div className="pop signin-split signin-agent-open" ref={ref}>
      <a className="btn signin-agent signin-split-main" href={desktop} rel="noreferrer" title={LAUNCH_NOTE} onClick={go}><BrandIcon name="claudecode" /> Claude Code</a>
      <button type="button" className="btn signin-agent signin-split-toggle" aria-haspopup="menu" aria-expanded={open} aria-label="Other ways to open Claude Code" onClick={() => setOpen((o) => !o)}>
        <Icon name="chevron-down" />
      </button>
      {open && (
        <div className="pop-menu pop-left" role="menu">
          <a role="menuitem" className="pop-item" href={desktop} rel="noreferrer" onClick={() => { go(); setOpen(false) }}>
            <Icon name="window" />
            <span className="pop-main">Desktop app</span>
          </a>
          <a role="menuitem" className="pop-item" href={terminal} rel="noreferrer" onClick={() => { go(); setOpen(false) }}>
            <Icon name="terminal" />
            <span className="pop-main">
              Terminal
              <span className="pop-sub">Opens a new Terminal window. macOS asks once to let Claude Code control it.</span>
            </span>
          </a>
          {vscode && (
            <a role="menuitem" className="pop-item" href={vscode} rel="noreferrer" onClick={() => { go(); setOpen(false) }}>
              <Icon name="code-slash" />
              <span className="pop-main">VS Code</span>
            </a>
          )}
        </div>
      )}
    </div>
  )
}

const DEEP_LINK_ICONS = { codex: <BrandIcon name="codex" />, cursor: <BrandIcon name="cursor" /> }
/** No vendor documents a link that opens these with a prompt, so they copy it. Gemini's app has a scheme that opens it empty. */
const PASTE_AGENTS: { name: string; icon: ReactNode }[] = [
  { name: 'Copilot', icon: <BrandIcon name="copilot" /> },
  { name: 'Gemini', icon: <BrandIcon name="gemini" /> },
  { name: 'Grok', icon: <BrandIcon name="grok" /> },
]

/** Hand the request to the person's own coding agent: the link fills its prompt, and nothing is sent until they press Enter there. */
export function AskAgent({ title, prompt, repo = null, className = '' }: { title: ReactNode; prompt: string | null; /** Claude Code opens in this repository's checkout. */ repo?: string | null; className?: string }) {
  const { missing, watch } = useOpenedCheck()
  const titleId = useId()
  if (!prompt) return null
  return (
    <section className={`signin-agents ${className}`.trim()} aria-labelledby={titleId}>
      <h2 id={titleId} className="signin-agents-title">{title}</h2>
      <div className="signin-agents-actions">
        <ClaudeSplit prompt={prompt} repo={repo} watch={watch} />
        {(['codex', 'cursor'] as const).map((k) => {
          const url = launchLink(k, prompt)
          return url && <a key={k} className="btn signin-agent signin-agent-open" href={url} rel="noreferrer" title={LAUNCH_NOTE} onClick={() => watch(k === 'codex' ? 'Codex' : 'Cursor')}>{DEEP_LINK_ICONS[k]} {LAUNCH_LABEL[k]}</a>
        })}
        {PASTE_AGENTS.map((a) => a.name === 'Gemini' ? (
          // The Gemini app takes no prompt in a link: copy it, then open the app empty. Hidden on a phone with the other app links.
          <CopyButton key={a.name} text={prompt} label={a.name} icon={a.icon} done={`Copied, paste in ${a.name}`} title="Copies the prompt and opens the Gemini app. Paste it there." className="btn signin-agent signin-agent-open" onCopy={() => { watch('Gemini'); openApp(OPENS_ONLY_LINKS.gemini) }} />
        ) : (
          <CopyButton key={a.name} text={prompt} label={a.name} icon={a.icon} done={`Copied, paste in ${a.name}`} title={`Copies the prompt. Paste it into ${a.name}.`} className="btn signin-agent" />
        ))}
      </div>
      <OpenedNote app={missing} />
      <p className="signin-agents-copy">Using a different agent? <CopyButton text={prompt} label="Copy the prompt" className="btn btn-link signin-agents-copylink" /></p>
      <p className="signin-agents-note">Opens your agent with the request typed in, or copies it for you to paste. Nothing is sent until you press Enter.</p>
    </section>
  )
}
