// API for other packages:
//   <NextStepView step={…} viewer={login} repo={…} originUrl={…} />
//     the "Decision for …" line, the human line, then one "Copy prompt for <who>" button per agent prompt. When a prompt is
//     for an agent that belongs to `viewer` and has a confirmed deep link, "Open in <Agent>" sits beside Copy.
//     A prompt that still has a blank for the person's own words asks for them first. Nothing is ever sent for the person.
import { useState } from 'react'
import { LAUNCH_LABEL, LAUNCH_NOTE, launchKindOfAgent, launchLink } from './launchLink.ts'
import { clean, PLACEHOLDER, type NextStep, type Prompt } from './nextStep.ts'
import { CopyButton, Icon } from './ui.tsx'
import './plain.css'

/** One prompt: Copy always; Open in the app only for the viewer's own agent, only once any blank is filled in. */
function PromptActions({ p, viewer, repo, originUrl }: { p: Prompt; viewer?: string | null; repo?: string | null; originUrl?: string | null }) {
  const [words, setWords] = useState('')
  const blank = p.text.includes(PLACEHOLDER)
  const filled = blank && clean(words) !== ''
  const text = filled ? p.text.replace(PLACEHOLDER, clean(words)) : p.text
  const kind = viewer && p.login === viewer && p.agent ? launchKindOfAgent(p.agent) : null
  const url = kind && (!blank || filled) ? launchLink(kind, text, { repo, originUrl }) : null
  return (
    <>
      {blank && (
        <label className="next-step-words">
          <span>Your decision</span>
          <input className="form-control form-control-sm" value={words} maxLength={200} onChange={(e) => setWords(e.target.value)} placeholder="Type it here, then copy or open" />
        </label>
      )}
      <CopyButton text={text} label={`Copy prompt for ${p.for}`} className="btn btn-sm btn-outline-secondary next-step-copy" />
      {kind && (url ? (
        <a className="btn btn-sm btn-outline-secondary next-step-copy" href={url} rel="noreferrer" title={LAUNCH_NOTE}>
          <Icon name="box-arrow-up-right" /> Open in {LAUNCH_LABEL[kind]}
        </a>
      ) : blank && !filled ? (
        <button type="button" className="btn btn-sm btn-outline-secondary next-step-copy" disabled title="Type your decision first. The prompt would otherwise record the blank as the decision.">
          <Icon name="box-arrow-up-right" /> Open in {LAUNCH_LABEL[kind]}
        </button>
      ) : null)}
    </>
  )
}

/** A next step: who decides, what a person can do, then the exact text to paste into each agent that can act. */
export function NextStepView({ step, tour, viewer, repo, originUrl }: { step: NextStep; tour?: string; viewer?: string | null; repo?: string | null; originUrl?: string | null }) {
  return (
    <div className="next-step" data-tour={tour}>
      {step.decides && step.decides.length > 0 && <span className="next-step-decides">Decision for {step.decides.join(' and ')}</span>}
      <span className={`next-step-human${step.human.endsWith(':') ? ' is-lead-in' : ''}`}>{step.human}</span>
      {step.prompts.map((p, i) => <PromptActions key={i} p={p} viewer={viewer} repo={repo} originUrl={originUrl} />)}
    </div>
  )
}
