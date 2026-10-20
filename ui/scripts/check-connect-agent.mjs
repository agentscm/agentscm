import { spawnSync } from 'node:child_process'
import assert from 'node:assert/strict'
import { CLIENT_AGENTS, agentSetupPrompt, clientSetupCommand, safeHubOrigin } from '../src/connectAgent.ts'

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
assert.ok(prompt?.includes('AgentSCM at https://hub.example.com'))
assert.ok(prompt?.includes('Never configure or deploy a Hub'))
assert.ok(prompt?.includes("client setup --agent NAME --hub 'https://hub.example.com'"))
for (const agent of CLIENT_AGENTS) assert.ok(prompt?.includes(agent))
assert.ok(prompt?.includes('Run npx agentscm client check'))
assert.ok(prompt?.includes('verify a real agent session reaches the expected repository'))
assert.ok(!prompt?.toLowerCase().includes('token='))
assert.equal(agentSetupPrompt('https://hub.example.com/?token=secret'), null)

console.log('connect-agent command checks passed')

// URL accepts shell metacharacters in hostnames. Every dynamic argument must remain literal.
const unusualOrigin = "https://a'$(id)'b.com"
assert.equal(safeHubOrigin(unusualOrigin), unusualOrigin)
const unusualCommand = clientSetupCommand(unusualOrigin, 'codex')
assert.ok(unusualCommand)
const shell = spawnSync('/bin/sh', ['-c', 'npx() { printf "%s\\n" "$@"; }; ' + unusualCommand], { encoding: 'utf8' })
assert.equal(shell.status, 0, shell.stderr)
assert.deepEqual(shell.stdout.trimEnd().split('\n'), ['agentscm', 'client', 'setup', '--agent', 'codex', '--hub', unusualOrigin])
assert.ok(agentSetupPrompt(unusualOrigin)?.includes(unusualCommand.replace("--agent 'codex'", '--agent NAME')))
