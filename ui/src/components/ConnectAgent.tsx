import { CopyOutlined } from '@ant-design/icons'
import { Alert, Button, Collapse, Modal, Select, Space, Steps, Typography } from 'antd'
import { useState } from 'react'
import { agentSetupPrompt, clientSetupCommand, safeHubOrigin, type ClientAgent } from '../connectAgent.ts'

const { Paragraph, Text } = Typography

export function ConnectAgent({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [copyState, setCopyState] = useState<'ready' | 'copied' | 'failed'>('ready')
  const [selectedAgent, setSelectedAgent] = useState<ClientAgent | undefined>()
  const origin = safeHubOrigin(window.location.origin)
  const command = selectedAgent ? clientSetupCommand(window.location.origin, selectedAgent) : null
  const prompt = agentSetupPrompt(window.location.origin)
  const close = () => {
    setCopyState('ready')
    setSelectedAgent(undefined)
    onClose()
  }
  const copyInstructions = async () => {
    if (!prompt) return
    try {
      await navigator.clipboard.writeText(prompt)
      setCopyState('copied')
    } catch {
      setCopyState('failed')
    }
  }

  return (
    <Modal
      open={open}
      title="Connect your agent"
      onCancel={close}
      width={600}
      footer={<Button onClick={close}>Done</Button>}
    >
      <Space direction="vertical" size={12} style={{ width: '100%' }}>
        <Paragraph style={{ marginBottom: 0 }}>
          Paste setup instructions into your coding agent from your repository. Your agent will connect your account and install AgentSCM.
        </Paragraph>
        {origin && prompt ? (
          <>
            <Text type="secondary">Hub: <Text code>{origin}</Text></Text>
            <Button type="primary" icon={<CopyOutlined />} onClick={() => void copyInstructions()} block>
              {copyState === 'copied' ? 'Setup instructions copied' : copyState === 'failed' ? 'Copy failed — open preview' : 'Copy setup instructions'}
            </Button>
            <Collapse
              size="small"
              items={[
                {
                  key: 'preview',
                  label: 'Preview setup instructions',
                  children: <Text copyable={{ text: prompt }}>{prompt}</Text>,
                },
                {
                  key: 'command',
                  label: 'Use command line',
                  children: (
                    <Space direction="vertical" size={8} style={{ width: '100%' }}>
                      <Select<ClientAgent>
                        value={selectedAgent}
                        placeholder="Choose your coding agent"
                        onChange={setSelectedAgent}
                        options={[
                          { value: 'claude', label: 'Claude Code' },
                          { value: 'codex', label: 'Codex' },
                          { value: 'cursor', label: 'Cursor' },
                          { value: 'copilot', label: 'Copilot' },
                          { value: 'gemini', label: 'Gemini' },
                        ]}
                        style={{ width: '100%' }}
                      />
                      {command ? <Text code copyable={{ text: command }}>{command}</Text> : <Text type="secondary">Choose an agent to get its command.</Text>}
                    </Space>
                  ),
                },
              ]}
            />
          </>
        ) : (
          <Alert
            type="warning"
            showIcon
            message="This page is not using a supported Hub origin. Ask your operator for the Hub's HTTPS URL."
          />
        )}
        <Steps
          direction="vertical"
          size="small"
          items={[
            { title: 'Paste into your agent', description: 'Open your repository and paste the instructions.' },
            { title: 'Sign in with your forge', description: 'Complete the browser sign-in when asked.' },
            { title: 'Review AgentSCM access', description: 'Approve AgentSCM hooks and MCP in your agent.' },
            { title: 'Verify a native edit', description: 'Your agent checks MCP and its edit hook in the same session. Login, approval and quota blockers include the next step.' },
          ]}
        />
        <Text type="secondary">macOS and Linux. Your agent guides setup; you approve sign-in and trust. Configuration alone is unverified.</Text>
      </Space>
    </Modal>
  )
}
