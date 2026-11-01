// G · the flagship prototype: A's frame with Now · Work · Workflows · Repository, utilities below, and one live
// sample state. The rail's highlight glides between items and the sheet slides between pages (view transitions).
import { useEffect, useRef, useState } from 'react'
import { Icon } from '../../ui.tsx'
import { Go, useProto } from '../kit.tsx'
import { TopBar } from '../shell.tsx'
import { FeatureMap, InboxG, SetupG, ValueG } from './Extras.tsx'
import { Now } from './Now.tsx'
import { RepositoryPage } from './Repository.tsx'
import { ReviewSheet, WorkDetail, WorkIndex } from './Work.tsx'
import { WorkflowsPage } from './Workflows.tsx'
import { StoreProvider, useStore } from './store.tsx'
import './g.css'

export const VariantG = () => <StoreProvider><GShell /></StoreProvider>

function GShell() {
  const { route, role } = useProto()
  const st = useStore()
  const [page = '', id, sub] = route.parts
  const open = st.openFor(role).length
  const [drawer, setDrawer] = useState(false)
  const shell = useRef<HTMLDivElement>(null)
  const sheetOpen = page === 'work' && !!id && sub === 'review'
  // While the review sheet is open, everything behind it is inert, so focus cannot leave the dialog.
  useEffect(() => {
    const parts = shell.current ? [...shell.current.querySelectorAll(':scope > .topbar, :scope > .side, :scope > .content')] : []
    for (const p of parts) {
      if (sheetOpen) p.setAttribute('inert', '')
      else p.removeAttribute('inert')
    }
  }, [sheetOpen])
  useEffect(() => {
    if (!drawer) return
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') setDrawer(false) }
    window.addEventListener('keydown', esc)
    return () => window.removeEventListener('keydown', esc)
  }, [drawer])
  const nav = [
    { key: '', icon: 'sun', label: 'Now', q: 'What needs me?', badge: open },
    { key: 'work', icon: 'layers', label: 'Work', q: 'Where is each change?' },
    { key: 'workflows', icon: 'diagram-3', label: 'Workflows', q: 'What process applies?' },
    { key: 'repository', icon: 'box', label: 'Repository', q: 'What do we know about this code?' },
  ]
  const util = [
    { key: 'inbox', icon: 'inbox', label: 'Inbox', badge: open },
    { key: 'setup', icon: 'sliders', label: 'Setup', dot: !st.copilotFixed },
    { key: 'map', icon: 'compass', label: 'Feature map' },
  ]
  return (
    <div className="app-shell g-shell" ref={shell}>
      <TopBar count={open} onMenu={() => setDrawer(!drawer)} menuOpen={drawer} />
      {drawer && <div className="drawer-scrim" onClick={() => setDrawer(false)} aria-hidden="true" />}
      <aside className={`side${drawer ? ' is-open' : ''}`} aria-label="Sections">
        <nav className="side-nav" onClick={() => setDrawer(false)}>
          <div className="side-group">
            {nav.map((n) => {
              const on = page === n.key
              return (
                <Go key={n.key} to={n.key} className={`side-link${on ? ' active' : ''}`}>
                  {on && <span className="g-pill" aria-hidden="true" />}
                  <span className="side-icon"><Icon name={n.icon} /></span>
                  <span className="side-text"><span className="side-label">{n.label}</span><span className="side-q">{n.q}</span></span>
                  {!!n.badge && <span key={n.badge} className="side-badge tone-warn g-badge">{n.badge}</span>}
                </Go>
              )
            })}
          </div>
          <div className="side-group g-util">
            {util.map((n) => {
              const on = page === n.key
              return (
                <Go key={n.key} to={n.key} className={`side-link${on ? ' active' : ''}`}>
                  {on && <span className="g-pill" aria-hidden="true" />}
                  <span className="side-icon"><Icon name={n.icon} /></span>
                  <span className="side-text"><span className="side-label">{n.label}</span></span>
                  {!!n.badge && <span key={n.badge} className="side-badge tone-warn g-badge">{n.badge}</span>}
                  {n.dot && <span className="g-util-dot" aria-label="Needs attention" />}
                </Go>
              )
            })}
          </div>
        </nav>
        <div className="side-foot"><p className="side-foot-note"><Icon name="easel" /> Sample data. Nothing leaves this page.</p></div>
      </aside>
      <div className="content">
        <main className="main g-main" id="main" tabIndex={-1} key={page}>
          {page === '' && <Now />}
          {page === 'work' && (id ? <WorkDetail id={id} /> : <WorkIndex />)}
          {page === 'workflows' && <WorkflowsPage />}
          {page === 'repository' && <RepositoryPage />}
          {page === 'inbox' && <InboxG />}
          {page === 'setup' && <SetupG />}
          {page === 'value' && <ValueG />}
          {page === 'map' && <FeatureMap />}
        </main>
      </div>
      {page === 'work' && id && sub === 'review' && <ReviewSheet id={id} />}
      {st.toast && (
        <div key={st.toast.id} className="g-toast" role="status" onMouseEnter={st.holdToast} onMouseLeave={st.releaseToast} onFocus={st.holdToast} onBlur={st.releaseToast}>
          <span className="g-ring tone-ok"><Icon name={st.toast.icon} /></span>
          <span>{st.toast.text}</span>
          <button type="button" className="g-x" aria-label="Dismiss" onClick={st.dismissToast}><Icon name="x-lg" /></button>
        </div>
      )}
    </div>
  )
}
