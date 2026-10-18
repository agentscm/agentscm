import { LockOutlined, TeamOutlined } from '@ant-design/icons'
import { Alert, Button, Checkbox, Input, Typography } from 'antd'
import { useState, type CSSProperties, type FormEvent } from 'react'
import { AGENT_TOKEN_MESSAGE, ApiError, fetchWhoami } from '../api.ts'
import { Logo } from './Header.tsx'

export function SignIn({ notice, onSignedIn }: { notice: string | null; onSignedIn: (token: string, keep: boolean) => void }) {
  const [value, setValue] = useState('')
  const [keep, setKeep] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    const token = value.trim()
    if (!token) {
      setError('Paste a read-only token to continue.')
      return
    }
    setBusy(true)
    setError(null)
    try {
      const whoami = await fetchWhoami(token, new AbortController().signal)
      if (!whoami.token.read_only) {
        // Never stored, and dropped from the form state.
        setValue('')
        setError(AGENT_TOKEN_MESSAGE)
        setBusy(false)
        return
      }
      onSignedIn(token, keep)
    } catch (e) {
      setError(e instanceof ApiError && e.status === 401 ? 'That token was not accepted. Check that it is current and for this hub.' : 'Could not reach the hub. Try again in a moment.')
      setBusy(false)
    }
  }

  return (
    <main className="signin">
      <form className="signin-card rise" onSubmit={submit} style={{ '--i': 0 } as CSSProperties}>
        <Logo height={44} />
        <h1 className="signin-title">Who is touching what, right now.</h1>
        <p className="signin-lead">Paste a read-only token for the repository to open its live status. Repository status uses this token only for reading. Workspace actions use a separate forge sign-in.</p>
        {notice && <Alert type="info" showIcon message={notice} className="signin-alert" />}
        <label htmlFor="token" className="field-label">
          Read-only token
        </label>
        <Input.Password id="token" size="large" prefix={<LockOutlined />} value={value} onChange={(e) => setValue(e.target.value)} autoComplete="off" spellCheck={false} placeholder="Paste token" status={error ? 'error' : undefined} aria-describedby={error ? 'token-error' : undefined} />
        {error && (
          <p id="token-error" className="field-error" role="alert">
            {error}
          </p>
        )}
        <Checkbox checked={keep} onChange={(e) => setKeep(e.target.checked)} className="keep">
          Keep me signed in on this browser
        </Checkbox>
        <Button type="primary" size="large" htmlType="submit" loading={busy} block>
          Open status
        </Button>
        <Button size="large" icon={<TeamOutlined />} href="#workspace" block>Sign in to workspace</Button>
        <div className="signin-help">
          <p>Create a token with</p>
          <Typography.Text code copyable>
            agentscm hub token create --read-only --name status-page --repo owner/name
          </Typography.Text>
        </div>
      </form>
    </main>
  )
}
