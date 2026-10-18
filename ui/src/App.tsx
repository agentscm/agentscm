import { ConfigProvider, theme } from 'antd'
import { useCallback, useEffect, useState, useSyncExternalStore, type CSSProperties, type ReactNode } from 'react'
import { Dashboard } from './components/Dashboard.tsx'
import { Header } from './components/Header.tsx'
import { SignIn } from './components/SignIn.tsx'
import { clearToken, loadTheme, loadToken, saveTheme, saveToken, type ThemePref } from './storage.ts'
import { useHub } from './useHub.ts'

const FONT = '-apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter", "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif'
const MONO = 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace'

const darkQuery = '(prefers-color-scheme: dark)'
const subscribeDark = (cb: () => void) => {
  const mq = window.matchMedia(darkQuery)
  mq.addEventListener('change', cb)
  return () => mq.removeEventListener('change', cb)
}
const systemDark = () => window.matchMedia(darkQuery).matches

/** Exposes antd tokens to styles.css so plain CSS and antd components share one palette. */
function Tokens({ children }: { children: ReactNode }) {
  const { token } = theme.useToken()
  const vars = {
    '--cb-text': token.colorText,
    '--cb-text-2': token.colorTextSecondary,
    '--cb-text-3': token.colorTextTertiary,
    '--cb-surface': token.colorBgContainer,
    '--cb-elevated': token.colorBgElevated,
    '--cb-border': token.colorBorderSecondary,
    '--cb-fill': token.colorFillTertiary,
    '--cb-fill-2': token.colorFillQuaternary,
    '--cb-primary': token.colorPrimary,
    '--cb-primary-bg': token.colorPrimaryBg,
    '--cb-font': token.fontFamily,
    '--cb-mono': token.fontFamilyCode,
    '--cb-radius': `${token.borderRadius}px`,
    '--cb-radius-lg': `${token.borderRadiusLG}px`,
  } as CSSProperties
  return (
    <div className="app" style={vars}>
      {children}
    </div>
  )
}

function Authed({ token, pref, onPref, onUnauthorized, onForget }: { token: string; pref: ThemePref; onPref: (p: ThemePref) => void; onUnauthorized: (message?: string) => void; onForget: () => void }) {
  const data = useHub(token, onUnauthorized)
  return (
    <>
      <Header whoami={data.whoami} lastOk={data.lastOk} failing={data.failing} pref={pref} onPref={onPref} onForget={onForget} />
      <Dashboard data={data} token={token} />
    </>
  )
}

export default function App() {
  const [token, setToken] = useState<string | null>(loadToken)
  const [notice, setNotice] = useState<string | null>(null)
  const [pref, setPref] = useState<ThemePref>(loadTheme)
  const prefersDark = useSyncExternalStore(subscribeDark, systemDark)
  const dark = pref === 'dark' || (pref === 'system' && prefersDark)

  useEffect(() => {
    document.documentElement.dataset['theme'] = dark ? 'dark' : 'light'
  }, [dark])

  const changePref = useCallback((p: ThemePref) => {
    setPref(p)
    saveTheme(p)
  }, [])
  const signedOut = useCallback((message: string | null) => {
    clearToken()
    setToken(null)
    setNotice(message)
  }, [])
  const onUnauthorized = useCallback((message?: string) => signedOut(message ?? 'Your token was rejected or has expired. Paste a current one to continue.'), [signedOut])
  const onForget = useCallback(() => signedOut('Token forgotten on this browser. The hub still accepts it until it expires or is revoked.'), [signedOut])

  return (
    <ConfigProvider
      theme={{
        algorithm: dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
        token: {
          colorPrimary: dark ? '#7C86FF' : '#4F5BD5',
          borderRadius: 12,
          borderRadiusLG: 20,
          fontFamily: FONT,
          fontFamilyCode: MONO,
          colorBgLayout: dark ? '#0A0C14' : '#F4F5F9',
          colorBgContainer: dark ? '#121522' : '#FFFFFF',
          colorBgElevated: dark ? '#1A1E30' : '#FFFFFF',
          colorBorderSecondary: dark ? 'rgba(255,255,255,0.06)' : 'rgba(17,24,39,0.06)',
          colorSuccess: '#12B76A',
          colorWarning: '#F79009',
          colorError: '#F04438',
        },
      }}
    >
      <Tokens>
        {token ? (
          <Authed key={token} token={token} pref={pref} onPref={changePref} onUnauthorized={onUnauthorized} onForget={onForget} />
        ) : (
          <SignIn
            notice={notice}
            onSignedIn={(t, keep) => {
              saveToken(t, keep)
              setNotice(null)
              setToken(t)
            }}
          />
        )}
      </Tokens>
    </ConfigProvider>
  )
}
