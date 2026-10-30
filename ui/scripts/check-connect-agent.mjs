import { spawnSync } from 'node:child_process'
import assert from 'node:assert/strict'
import { CLIENT_AGENTS, agentSetupPrompt, clientSetupCommand, installCommand, safeHubOrigin, signInHelpPrompt } from '../src/connectAgent.ts'
import { decodeSetup, setupState } from '../src/onboarding.ts'

assert.equal(safeHubOrigin('https://hub.example.com'), 'https://hub.example.com')
assert.equal(safeHubOrigin('https://hub.example.com:8443'), 'https://hub.example.com:8443')
assert.equal(safeHubOrigin('http://127.0.0.1:7332'), 'http://127.0.0.1:7332')
assert.equal(safeHubOrigin('http://[::1]:7332'), 'http://[::1]:7332')

for (const unsafe of [
  'http://hub.example.com',
  'https://user@hub.example.com',
  'https://hub.example.com/path',
  'https://hub.example.com/?token=secret',
  'javascript:alert(1)',
  "https://hub.example.com'$(touch nope)",
]) assert.equal(safeHubOrigin(unsafe), null)

assert.equal(
  clientSetupCommand('https://hub.example.com', 'claude'),
  "npx agentscm client setup --agent 'claude' --hub 'https://hub.example.com'",
)
assert.equal(clientSetupCommand('https://hub.example.com/path', 'codex'), null)
for (const agent of CLIENT_AGENTS) {
  assert.ok(clientSetupCommand('https://hub.example.com', agent)?.includes(`--agent '${agent}'`))
}

const prompt = agentSetupPrompt('https://hub.example.com')
assert.ok(prompt?.includes('Hub at https://hub.example.com'))
assert.ok(prompt?.includes("npx agentscm --yes --json --hub-url 'https://hub.example.com'"))
assert.ok(prompt?.includes('deploy a Hub'))
assert.ok(prompt?.includes('/hooks'))
assert.ok(prompt && prompt.length < 800)
assert.ok(!prompt?.toLowerCase().includes('token='))
assert.equal(agentSetupPrompt('https://hub.example.com/?token=secret'), null)

const help = signInHelpPrompt('https://hub.example.com')
assert.ok(help?.includes('Hub at https://hub.example.com has no GitHub or GitLab sign-in'))
assert.ok(help?.includes('Sign-in configuration'))
assert.ok(help?.includes('deploy a Hub'))
assert.ok(!help?.toLowerCase().includes('token='))
assert.equal(signInHelpPrompt('http://hub.example.com'), null)

console.log('connect-agent command checks passed')

const row = { agent: 'codex', key: 'opaque-key', name: 'Agent', owner: 'member', blocker: null, mcp_verified: true, native_verified: false, expires_at: 2000, next_action: 'Verify native hook', state: 'ready' }
const [decoded] = decodeSetup([row])
assert.equal(setupState(decoded, 1000000, true), 'Unverified') // Ignore a supplied ready flag.
assert.equal(setupState({ ...decoded, native_verified: true }, 1000000, true), 'Ready')
assert.equal(setupState({ ...decoded, native_verified: true }, 2000000, true), 'Unverified')
assert.equal(setupState({ ...decoded, native_verified: true }, 1000000, false), 'Unverified')
assert.equal(setupState({ ...decoded, native_verified: true, blocker: 'quota' }, 1000000, true), 'Blocked')
assert.equal(setupState(decodeSetup([{ ...row, blocker: undefined, native_verified: true }])[0], 1000000, true), 'Blocked')
assert.deepEqual(decodeSetup([{ ...row, agent: 'other' }, { ...row, expires_at: Infinity }]), [])
assert.throws(() => decodeSetup({}))

// URL accepts shell metacharacters in hostnames. Every dynamic argument must remain literal.
const unusualOrigin = "https://a'$(id)'b.com"
assert.equal(safeHubOrigin(unusualOrigin), unusualOrigin)
const unusualCommand = clientSetupCommand(unusualOrigin, 'codex')
assert.ok(unusualCommand)
const shell = spawnSync('/bin/sh', ['-c', 'npx() { printf "%s\\n" "$@"; }; ' + unusualCommand], { encoding: 'utf8' })
assert.equal(shell.status, 0, shell.stderr)
assert.deepEqual(shell.stdout.trimEnd().split('\n'), ['agentscm', 'client', 'setup', '--agent', 'codex', '--hub', unusualOrigin])
assert.ok(agentSetupPrompt(unusualOrigin)?.includes(installCommand(unusualOrigin).replace('npx agentscm ', '')))
