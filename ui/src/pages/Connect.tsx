import { useState } from 'react'
import { agentSetupPrompt, CLIENT_AGENTS, clientSetupCommand, safeHubOrigin, type ClientAgent } from '../connectAgent.ts'
import { CopyButton, Icon, Notice } from '../ui.tsx'

const AGENT_NAMES: Record<ClientAgent, string> = { claude: 'Claude Code', codex: 'Codex', cursor: 'Cursor', copilot: 'Copilot', gemini: 'Gemini' }

const STEPS = [
  { icon: 'clipboard-check', title: 'Paste into your agent', body: 'Open your repository in your coding agent and paste the instructions.' },
  { icon: 'person-check', title: 'Sign in with your forge', body: 'Complete the GitHub or GitLab browser sign-in when asked.' },
  { icon: 'shield-check', title: 'Approve AgentSCM access', body: 'Approve the AgentSCM hooks and MCP server inside your agent yourself.' },
  { icon: 'patch-check', title: 'Verify a real edit', body: 'Your agent checks MCP and its edit hook in the same session. Blockers come with the next step.' },
]

/** Instructions for connecting a coding agent to this hub. Copies text; never runs anything. */
export function ConnectAgent() {
  const [agent, setAgent] = useState<ClientAgent | ''>('')
  const origin = safeHubOrigin(window.location.origin)
  const prompt = agentSetupPrompt(window.location.origin)
  const command = agent ? clientSetupCommand(window.location.origin, agent) : null
  if (!origin || !prompt) {
    const { protocol, hostname, port } = window.location
    const local = protocol === 'http:' && (hostname === 'localhost' || hostname.endsWith('.localhost'))
    return (
      <Notice title="This page is not served from a supported Hub address">
        {local ? `Open this page at http://127.0.0.1${port ? `:${port}` : ''}/ui/ instead of ${hostname}. ` : 'Ask your operator for the Hub’s HTTPS URL. '}Setup commands are only offered for HTTPS or a loopback address.
      </Notice>
    )
  }
  return (
    <div className="connect">
      <ol className="steps">
        {STEPS.map((s, i) => (
          <li key={s.title}>
            <span className="step-mark" aria-hidden="true">
              <Icon name={s.icon} />
            </span>
            <span>
              <strong>
                {i + 1}. {s.title}
              </strong>
              <span className="muted">{s.body}</span>
            </span>
          </li>
        ))}
      </ol>
      <div className="connect-actions">
        <div className="muted small">
          Hub address <code>{origin}</code>
        </div>
        <CopyButton text={prompt} label="Copy setup instructions" className="btn btn-primary" />
      </div>
      <details className="disclosure">
        <summary>
          <Icon name="eye" /> Preview the instructions
        </summary>
        <p className="prompt-preview">{prompt}</p>
      </details>
      <details className="disclosure">
        <summary>
          <Icon name="terminal" /> Use the command line instead
        </summary>
        <label className="form-label small mt-2" htmlFor="agent-pick">
          Your coding agent
        </label>
        <select id="agent-pick" className="form-select" value={agent} onChange={(e) => setAgent(e.target.value as ClientAgent | '')}>
          <option value="">Choose an agent…</option>
          {CLIENT_AGENTS.map((a) => (
            <option key={a} value={a}>
              {AGENT_NAMES[a]}
            </option>
          ))}
        </select>
        {command && (
          <div className="command">
            <code>{command}</code>
            <CopyButton text={command} />
          </div>
        )}
        {command && <p className="muted small mt-2 mb-0">Then run <code>npx agentscm client check</code> in the checkout (Hub binding only). Verify hooks and MCP from your agent.</p>}
      </details>
      <p className="muted small mb-0">macOS and Linux. Your agent guides setup; you approve sign-in and trust. Configuration alone is not verification. If this checkout already has hooks from <code>agentscm quickstart</code>, <code>client setup</code> can stop on them; run <code>agentscm uninstall</code> first. <code>agentscm init-repo</code> only commits shared instructions; each developer still runs this setup.</p>
    </div>
  )
}
