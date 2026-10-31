// API for other packages:
//   <StartTask token me repo originUrl />   Dashboard › Now: describe a task, hand it to your own agent.
// Tasks start in an agent, never in this page. The card builds one prompt (read the overview, preflight, then start),
// copies it, or opens the viewer's own connected agents with it filled in. The agent shows it and waits for Enter.
import { useState } from 'react'
import { fetchSetup } from './api.ts'
import { LAUNCH_LABEL, LAUNCH_NOTE, launchKindOfClient, launchLink, type LaunchKind } from './launchLink.ts'
import { clean } from './nextStep.ts'
import { Box, CopyButton, Icon } from './ui.tsx'
import { usePoll } from './usePoll.ts'

/** The words a person hands to an agent to start a task. The task text is the person's own, flattened to one line. */
export const startTaskPrompt = (task: string): string =>
  `Use AgentSCM. Read agentscm_work_overview, run agentscm_work_preflight for the files you plan to change, then agentscm_start with a fixed merge window. Task: “${clean(task)}”`

export function StartTask({ token, me, repo, originUrl }: { token: string; me: string | null; repo: string | null; originUrl: string | null }) {
  const [task, setTask] = useState('')
  const setup = usePoll((signal) => fetchSetup(token, signal), 60_000, token)
  const mine = setup.kind === 'ready' ? setup.value.filter((r) => r.owner === me) : null
  const kinds = [...new Set((mine ?? []).flatMap((r) => launchKindOfClient(r.agent) ?? []))] as LaunchKind[]
  const empty = clean(task) === ''
  const prompt = startTaskPrompt(task)
  const btn = 'btn btn-sm btn-outline-secondary'
  return (
    <Box id="box-start" tour="start-task" icon="play-circle" title="Start a task" explain="Tasks start in your agent, not on this page. The prompt asks it to read the work overview, check for overlap, then start the task, so it shows up here. You press Enter in the agent.">
      <p className="small muted">Tasks start in your agent. Describe one and hand it over.</p>
      <div className="start-task">
        <input className="form-control form-control-sm" aria-label="What should the agent do?" placeholder="What should the agent do?" value={task} maxLength={200} onChange={(e) => setTask(e.target.value)} />
        <div className="start-task-actions">
          {kinds.map((k) =>
            empty ? (
              <button key={k} type="button" className={btn} disabled title="Describe the task first.">
                <Icon name="box-arrow-up-right" /> Open in {LAUNCH_LABEL[k]}
              </button>
            ) : (
              (() => {
                const url = launchLink(k, prompt, { repo, originUrl })
                return url ? (
                  <a key={k} className={btn} href={url} rel="noreferrer" title={LAUNCH_NOTE}>
                    <Icon name="box-arrow-up-right" /> Open in {LAUNCH_LABEL[k]}
                  </a>
                ) : (
                  <button key={k} type="button" className={btn} disabled title="This task is too long to open in an app. Copy the prompt instead.">
                    <Icon name="box-arrow-up-right" /> Open in {LAUNCH_LABEL[k]}
                  </button>
                )
              })()
            ),
          )}
          {empty ? (
            <button type="button" className={btn} disabled title="Describe the task first.">
              <Icon name="clipboard" /> Copy prompt
            </button>
          ) : (
            <CopyButton text={prompt} label="Copy prompt" className={btn} />
          )}
        </div>
      </div>
      {mine !== null && kinds.length === 0 && (
        <p className="small muted">
          {mine.length === 0 ? <>None of your agents is connected yet. <a href="#setup">Connect one in Setup › Agents</a>, or copy the prompt into any agent.</> : <>Your connected agents have no app link here. Copy the prompt into one. <a href="#setup">Setup › Agents</a></>}
        </p>
      )}
    </Box>
  )
}
