import { useState } from 'react'
import { loadLocalBase, loadLocalToken, localStatus, saveLocalBase, saveLocalToken, validLocalBase } from '../localApi.ts'
import { usePoll } from '../usePoll.ts'
import { loadSingleKeys, saveSingleKeys, type Density, type ThemePref } from '../storage.ts'
import type { Whoami } from '../types.ts'
import { ViewSettingsBox } from '../ViewSettings.tsx'
import { Box, Icon, Pill, Skeleton, localDate } from '../ui.tsx'
import { SECTIONS } from '../App.tsx'
import { useTour } from '../tour/Tour.tsx'
import { tourFor } from '../tour/content.ts'
import { setAutoplay, setWelcome } from '../tour/state.ts'

const CADENCE = [
  ['broadcast', 'Live agents, held files and activity', 'Every 5 seconds'],
  ['kanban', 'Work overview and coordination requests', 'Every 15 seconds'],
  ['heart-pulse', 'Hub health', 'Every 15 seconds'],
] as const

export function SettingsPage({ token, whoami, pref, onPref, density, onDensity, railCollapsed, onRail, onForget }: { token: string; whoami: Whoami | null; pref: ThemePref; onPref: (p: ThemePref) => void; density: Density; onDensity: (d: Density) => void; railCollapsed: boolean; onRail: (collapsed: boolean) => void; onForget: () => void }) {
  const themes: { v: ThemePref; icon: string; label: string; hint: string }[] = [
    { v: 'light', icon: 'sun', label: 'Light', hint: 'Always light' },
    { v: 'system', icon: 'circle-half', label: 'System', hint: 'Follows your device' },
    { v: 'dark', icon: 'moon-stars', label: 'Dark', hint: 'Always dark' },
  ]
  const densities: { v: Density; icon: string; label: string; hint: string }[] = [
    { v: 'spacious', icon: 'arrows-expand', label: 'Spacious', hint: 'More room between rows and sections.' },
    { v: 'compact', icon: 'arrows-collapse', label: 'Compact', hint: 'Fits more on screen.' },
  ]
  const [singleKeys, setSingleKeys] = useState(loadSingleKeys)
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">How is this page set up for me?</h1>
        <p className="answer-text">Appearance and token settings live in this browser only and never change the hub. Inline agent views are a hub setting shared by everyone.</p>
      </div>
      <div className="stack">
        <Box icon="palette" title="Appearance" explain="Saved in this browser. Other people see their own choice.">
          <div className="settings-row">
            <div>
              <div className="settings-label">Theme</div>
              <div className="muted small">Pick a fixed theme or follow your device’s setting.</div>
            </div>
            <div className="choice-cards" data-tour="settings-theme" role="radiogroup" aria-label="Theme">
              {themes.map((t) => (
                <button key={t.v} type="button" role="radio" aria-checked={pref === t.v} className={`choice${pref === t.v ? ' on' : ''}`} onClick={() => onPref(t.v)}>
                  <Icon name={t.icon} />
                  <span>
                    <strong>{t.label}</strong>
                    <span className="muted small">{t.hint}</span>
                  </span>
                </button>
              ))}
            </div>
          </div>
          <div className="settings-row">
            <div>
              <div className="settings-label">Density</div>
              <div className="muted small">How much room rows, boxes and sections get.</div>
            </div>
            <div className="choice-cards" data-tour="settings-density" role="radiogroup" aria-label="Density">
              {densities.map((t) => (
                <button key={t.v} type="button" role="radio" aria-checked={density === t.v} className={`choice${density === t.v ? ' on' : ''}`} onClick={() => onDensity(t.v)}>
                  <Icon name={t.icon} />
                  <span>
                    <strong>{t.label}</strong>
                    <span className="muted small">{t.hint}</span>
                  </span>
                </button>
              ))}
            </div>
          </div>
          <div className="settings-row">
            <div>
              <div className="settings-label">Compact sidebar</div>
              <div className="muted small">Show only icons in the sidebar. Hover an icon to see its name. Shortcut: <kbd>[</kbd></div>
            </div>
            <div className="form-check form-switch m-0">
              <input id="rail-compact" className="form-check-input" type="checkbox" role="switch" checked={railCollapsed} onChange={(e) => onRail(e.target.checked)} />
              <label className="visually-hidden" htmlFor="rail-compact">Compact sidebar</label>
            </div>
          </div>
          <div className="settings-row">
            <div>
              <div className="settings-label">Single-key shortcuts</div>
              <div className="muted small">Let <kbd>/</kbd> open search and <kbd>[</kbd> collapse the sidebar. Switch off if they trigger by accident; <kbd>Ctrl</kbd> <kbd>K</kbd> or <kbd>⌘</kbd> <kbd>K</kbd> still opens search.</div>
            </div>
            <div className="form-check form-switch m-0">
              <input id="single-keys" className="form-check-input" type="checkbox" role="switch" checked={singleKeys} onChange={(e) => { setSingleKeys(e.target.checked); saveSingleKeys(e.target.checked) }} />
              <label className="visually-hidden" htmlFor="single-keys">Single-key shortcuts</label>
            </div>
          </div>
        </Box>

        <Walkthrough />

        <LocalServer />

        <Box tour="settings-token" icon="key" title="Your token" explain="The token decides which repository you see. A read-only token can look but never change coordination.">
          {!whoami ? (
            <Skeleton rows={2} />
          ) : (
            <dl className="facts">
              <div><dt>Signed in as</dt><dd><Icon name={whoami.owner.provider === 'gitlab' ? 'gitlab' : 'github'} /> {whoami.owner.login}{whoami.owner.host ? <span className="muted"> · {whoami.owner.host}</span> : null}</dd></div>
              <div><dt>Token name</dt><dd>{whoami.token.name}</dd></div>
              <div><dt>Repository</dt><dd><code>{whoami.token.repository}</code></dd></div>
              <div><dt>Access</dt><dd><Pill tone="ok" icon="eye">Read-only</Pill></dd></div>
              <div><dt>Expires</dt><dd>{whoami.token.expires_at ? localDate(whoami.token.expires_at) : 'No expiry recorded'}</dd></div>
            </dl>
          )}
          <div className="settings-row settings-danger">
            <div>
              <div className="settings-label">Forget this token</div>
              <div className="muted small">Removes it from this browser. The hub still accepts it until it expires or is revoked.</div>
            </div>
            <button type="button" className="btn btn-outline-danger" onClick={onForget}>
              <Icon name="box-arrow-right" /> Forget token
            </button>
          </div>
        </Box>

        <ViewSettingsBox token={token} />

        <Box icon="arrow-repeat" title="Refresh" explain="Refreshing pauses while this tab is hidden and slows down after errors. A failed refresh keeps the last data and labels it.">
          <ul className="cadence">
            {CADENCE.map(([icon, what, when]) => (
              <li key={what}>
                <Icon name={icon} />
                <span>{what}</span>
                <span className="muted">{when}</span>
              </li>
            ))}
          </ul>
        </Box>
      </div>
    </div>
  )
}

const STATUS = {
  live: { tone: 'ok', icon: 'check-circle', word: 'Connected' },
  preview: { tone: 'muted', icon: 'easel', word: 'Not found — showing preview data' },
  off: { tone: 'warn', icon: 'exclamation-circle', word: 'Connected, but Timeline is off' },
  refused: { tone: 'bad', icon: 'lock', word: 'Refused — check the address or token' },
  error: { tone: 'bad', icon: 'x-octagon', word: 'Not answering properly' },
} as const

/** Where Templates and Timeline read from: `agentscm serve` on this computer. Loopback only, its own token. */
function LocalServer() {
  const [saved, setSaved] = useState(loadLocalBase)
  const [draft, setDraft] = useState(saved)
  const [token, setToken] = useState(loadLocalToken)
  const [nonce, setNonce] = useState(0)
  const value = draft.trim().replace(/\/$/, '')
  const valid = validLocalBase(value)
  const status = usePoll((signal) => localStatus(signal), 0, `${saved}|${nonce}`)
  return (
    <Box tour="settings-local" icon="laptop" title="This computer" explain="Templates and Timeline read from agentscm serve on this machine, never from the hub.">
      <form
        className="settings-row"
        onSubmit={(e) => {
          e.preventDefault()
          if (!valid) return
          saveLocalBase(value)
          saveLocalToken(token.trim())
          setSaved(value)
          setNonce((n) => n + 1)
        }}
      >
        <div>
          <label className="settings-label" htmlFor="local-base">Local read API</label>
          <div className="muted small">Leave the address empty to use this page’s own address. Only addresses on this computer are allowed, such as http://127.0.0.1:7331. A token is needed only for a different address: it is agentscm serve’s own read token, never your hub token, and is kept for this tab only.</div>
          <div className="mt-2">
            {status.kind === 'ready' ? <Pill tone={STATUS[status.value].tone} icon={STATUS[status.value].icon}>{STATUS[status.value].word}</Pill> : status.kind === 'loading' ? <span className="small muted">Checking…</span> : <Pill tone="bad">Could not check</Pill>}
          </div>
        </div>
        <div className="settings-control d-flex flex-column gap-2">
          <div>
            <label className="visually-hidden" htmlFor="local-base">Local read API address</label>
            <input id="local-base" className={`form-control${valid ? '' : ' is-invalid'}`} placeholder="Same address as this page" value={draft} onChange={(e) => setDraft(e.target.value)} spellCheck={false} aria-describedby="local-base-help" />
            {!valid && <div id="local-base-help" className="invalid-feedback d-block">Use http://127.0.0.1, localhost or [::1], with an optional port.</div>}
          </div>
          <div>
            <label className="visually-hidden" htmlFor="local-token">Local read token</label>
            <input id="local-token" type="password" className="form-control" placeholder="Local read token (optional)" value={token} onChange={(e) => setToken(e.target.value)} autoComplete="off" disabled={value === ''} />
          </div>
          <button type="submit" className="btn btn-outline-secondary" disabled={!valid}>Save and check</button>
        </div>
      </form>
    </Box>
  )
}

/** Per-viewer tour switches; stored in this browser only, like the theme. */
function Walkthrough() {
  const tour = useTour()
  if (!tour) return null
  const welcomeOn = !tour.prefs.welcomeSeen && !tour.prefs.welcomeOff
  const pages = SECTIONS.filter((s) => tourFor(s.key))
  return (
    <Box id="feature-walkthrough" tour="settings-walkthrough" icon="signpost-split" title="Feature walkthrough" explain="Short guided tours that point at the real numbers on each page. Saved in this browser only.">
      {!tour.remember && (
        <p className="muted small">This browser is not keeping site data, so tours will not start by themselves. You can still run any of them below.</p>
      )}
      <div className="settings-row">
        <div>
          <div className="settings-label">Welcome tour</div>
          <div className="muted small">Shows the top bar and what each section is for. Turn it on to see it again the next time you open the hub.</div>
        </div>
        <div className="d-flex align-items-center gap-2">
          <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => tour.start('welcome')}>Run now</button>
          <div className="form-check form-switch m-0">
            <input id="tour-welcome" className="form-check-input" type="checkbox" role="switch" checked={welcomeOn} disabled={!tour.remember} onChange={(e) => tour.update((p) => setWelcome(p, e.target.checked))} />
            <label className="visually-hidden" htmlFor="tour-welcome">Show the welcome tour next time</label>
          </div>
        </div>
      </div>
      <div className="settings-label mt-3">Page tours</div>
      <div className="muted small">Switch one on to play it by itself the next time you open that page. Every page also has a Tour this page button beside the breadcrumbs.</div>
      <ul className="tour-list">
        {pages.map((s) => (
          <li key={s.key}>
            <Icon name={s.icon} />
            <span>
              {s.label}
              <span className="muted small"> · {s.question}</span>
            </span>
            <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => {
              window.location.hash = s.key
              tour.start(s.key)
            }} aria-label={`Run the ${s.label} tour now`}>Run now</button>
            <div className="form-check form-switch m-0">
              <input id={`tour-${s.key}`} className="form-check-input" type="checkbox" role="switch" checked={tour.prefs.autoplay.includes(s.key)} disabled={!tour.remember} onChange={(e) => tour.update((p) => setAutoplay(p, s.key, e.target.checked))} />
              <label className="visually-hidden" htmlFor={`tour-${s.key}`}>Play the {s.label} tour on my next visit</label>
            </div>
          </li>
        ))}
      </ul>
    </Box>
  )
}
