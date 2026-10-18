import { DesktopOutlined, DownOutlined, GithubOutlined, GitlabOutlined, LogoutOutlined, MoonOutlined, SunOutlined, TeamOutlined } from '@ant-design/icons'
import { Dropdown, Segmented } from 'antd'
import logo from '../assets/logo.png'
import type { ThemePref } from '../storage.ts'
import type { Whoami } from '../types.ts'
import { useNow } from '../useHub.ts'
import { Av, relative } from './bits.tsx'

export function Logo({ height = 36 }: { height?: number }) {
  return <img className="brand-logo" src={logo} alt="AgentSCM" height={height} width={Math.round((height * 407) / 96)} />
}

function Live({ lastOk, failing }: { lastOk: number | null; failing: boolean }) {
  const now = useNow()
  const ago = lastOk ? relative(now, lastOk / 1000) : null
  return (
    <span className={`live-ind${failing ? ' is-warn' : ''}`}>
      <span className="live-dot" aria-hidden="true" />
      <span className="live-text">
        {failing ? 'Reconnecting…' : lastOk ? `Live · updated ${ago === 'just now' ? 'just now' : ago}` : 'Connecting…'}
      </span>
      <span className="sr-only" role="status">
        {failing ? 'Reconnecting to the hub' : lastOk ? 'Connected to the hub' : ''}
      </span>
    </span>
  )
}

interface Props {
  workspaceMode?: boolean
  whoami: Whoami | null
  lastOk: number | null
  failing: boolean
  pref: ThemePref
  onPref: (p: ThemePref) => void
  onForget: () => void
}

export function Header({ whoami, lastOk, failing, pref, onPref, onForget, workspaceMode = false }: Props) {
  const gitlab = whoami?.owner.provider === 'gitlab'
  return (
    <header className="topbar">
      <div className="topbar-inner">
        <div className="brand">
          <Logo />
        </div>
        {workspaceMode && <span className="repo-pill"><TeamOutlined /> Workspace</span>}
        {whoami && (
          <span className="repo-pill" title={whoami.token.repository}>
            {gitlab ? <GitlabOutlined /> : <GithubOutlined />}
            <span className="clamp1">{whoami.token.repository}</span>
          </span>
        )}
        <div className="topbar-right">
          {!workspaceMode && <Live lastOk={lastOk} failing={failing} />}
          <Segmented
            className="theme-seg"
            value={pref}
            onChange={(v) => onPref(v as ThemePref)}
            options={[
              { value: 'light', icon: <SunOutlined />, label: <span className="seg-label">Light</span> },
              { value: 'dark', icon: <MoonOutlined />, label: <span className="seg-label">Dark</span> },
              { value: 'system', icon: <DesktopOutlined />, label: <span className="seg-label">System</span> },
            ]}
            aria-label="Theme"
          />
          {whoami && (
            <Dropdown
              trigger={['click']}
              placement="bottomRight"
              menu={{ items: [{ key: 'forget', icon: <LogoutOutlined />, label: 'Forget token' }], onClick: onForget }}
            >
              <button type="button" className="user-chip" aria-haspopup="menu" aria-label={`Signed in as ${whoami.token.name}, ${whoami.owner.login}. Open menu`}>
                <Av name={whoami.token.name} size={32} />
                <span className="user-text">
                  <span className="user-name clamp1">{whoami.token.name}</span>
                  <span className="user-sub clamp1">{whoami.owner.login}</span>
                </span>
                <DownOutlined className="user-caret" />
              </button>
            </Dropdown>
          )}
        </div>
      </div>
    </header>
  )
}
