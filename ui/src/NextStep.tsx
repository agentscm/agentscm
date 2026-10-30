// API for other packages:
//   <NextStepView step={…} viewer={login} repo={…} originUrl={…} />
//     the "Decision for …" line, the human line, then one "Copy prompt for <who>" button per agent prompt. When a prompt is
//     for an agent that belongs to `viewer` and has a confirmed deep link, "Open in <Agent>" sits beside Copy.
//     A prompt that still has a blank for the person's own words asks for them first. Nothing is ever sent for the person.
//   useStepActions(step, ctx)   the same actions as a list for a compact row: `primary` (Open in the viewer's own agent when a link
//     exists, else the first Copy) and `more` (everything else), plus the blank-filling words. <StepDetail> shows the decision line
//     and the blanks; <StepAction> draws one action as a button, a link or a menu item.
import { useState, type ReactNode } from 'react'
import { LAUNCH_LABEL, LAUNCH_NOTE, launchKindOfAgent, launchLink } from './launchLink.ts'
import { clean, PLACEHOLDER, type NextStep, type Prompt } from './nextStep.ts'
import { CopyButton, Icon } from './ui.tsx'
import './plain.css'

interface StepCtx {
  viewer?: string | null
  repo?: string | null
  originUrl?: string | null
}

/** What one prompt offers: its text (blank filled in), and the app link for the viewer's own agent once any blank is filled in. */
function promptState(p: Prompt, words: string, { viewer, repo, originUrl }: StepCtx) {
  const blank = p.text.includes(PLACEHOLDER)
  const filled = blank && clean(words) !== ''
  const text = filled ? p.text.replace(PLACEHOLDER, clean(words)) : p.text
  const kind = viewer && p.login === viewer && p.agent ? launchKindOfAgent(p.agent) : null
  const url = kind && (!blank || filled) ? launchLink(kind, text, { repo, originUrl }) : null
  return { blank, filled, text, kind, url }
}

const WORDS_NOTE = 'Type your decision first. The prompt would otherwise record the blank as the decision.'

/** The words input for a prompt that has a blank. */
function WordsInput({ words, onChange }: { words: string; onChange: (v: string) => void }) {
  return (
    <label className="next-step-words">
      <span>Your decision</span>
      <input className="form-control form-control-sm" value={words} maxLength={200} onChange={(e) => onChange(e.target.value)} placeholder="Type it here, then copy or open" />
    </label>
  )
}

/** One prompt: Copy always; Open in the app only for the viewer's own agent, only once any blank is filled in. */
function PromptActions({ p, viewer, repo, originUrl }: { p: Prompt } & StepCtx) {
  const [words, setWords] = useState('')
  const { blank, filled, text, kind, url } = promptState(p, words, { viewer, repo, originUrl })
  return (
    <>
      {blank && <WordsInput words={words} onChange={setWords} />}
      <CopyButton text={text} label={`Copy prompt for ${p.for}`} className="btn btn-sm btn-outline-secondary next-step-copy" />
      {kind && (url ? (
        <a className="btn btn-sm btn-outline-secondary next-step-copy" href={url} rel="noreferrer" title={LAUNCH_NOTE}>
          <Icon name="box-arrow-up-right" /> Open in {LAUNCH_LABEL[kind]}
        </a>
      ) : blank && !filled ? (
        <button type="button" className="btn btn-sm btn-outline-secondary next-step-copy" disabled title={WORDS_NOTE}>
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

/** One action a prompt offers, ready to draw. */
export interface StepActionItem {
  key: string
  kind: 'open' | 'copy'
  label: string
  text: string
  /** The app link; null when `kind` is open but it cannot be used yet. */
  url: string | null
}

/**
 * A step's actions as a list, for a row with room for one button: `primary` is the first Open link the viewer can use,
 * else the first Copy; `more` is everything else, in order. The words typed for a blank prompt live here, so the primary
 * can change from Copy to Open as the person types.
 */
export function useStepActions(step: NextStep | null, ctx: StepCtx) {
  const [words, setWords] = useState<string[]>([])
  const prompts = step?.prompts ?? []
  const items = prompts.flatMap((p, i): StepActionItem[] => {
    const { text, kind, url } = promptState(p, words[i] ?? '', ctx)
    return [{ key: `copy${i}`, kind: 'copy', label: `Copy prompt for ${p.for}`, text, url: null }, ...(kind ? [{ key: `open${i}`, kind: 'open' as const, label: `Open in ${LAUNCH_LABEL[kind]}`, text, url }] : [])]
  })
  const primary = items.find((a) => a.kind === 'open' && a.url) ?? items[0] ?? null
  const blanks = prompts.flatMap((p, i) => (p.text.includes(PLACEHOLDER) ? [i] : []))
  return { primary, more: items.filter((a) => a !== primary), blanks, words, setWord: (i: number, v: string) => setWords((w) => Object.assign([...w], { [i]: v })) }
}

/** A button, a link or a menu item. An Open that is not ready yet is disabled and says what is missing. */
export function StepAction({ a, className, role, onDone }: { a: StepActionItem; className: string; role?: string; onDone?: () => void }) {
  if (a.kind === 'copy') return <CopyButton text={a.text} label={a.label} className={className} role={role} />
  if (a.url) {
    return (
      <a className={className} role={role} href={a.url} rel="noreferrer" title={LAUNCH_NOTE} onClick={onDone}>
        <Icon name="box-arrow-up-right" /> {a.label}
      </a>
    )
  }
  return (
    <button type="button" className={className} role={role} disabled title={WORDS_NOTE}>
      <Icon name="box-arrow-up-right" /> {a.label}
    </button>
  )
}

/** The expanded part of a compact step: who decides, the human line, and the input for each blank. */
export function StepDetail({ step, blanks, words, setWord, children }: { step: NextStep; blanks: number[]; words: string[]; setWord: (i: number, v: string) => void; children?: ReactNode }) {
  return (
    <div className="next-step is-detail">
      {step.decides && step.decides.length > 0 && <span className="next-step-decides">Decision for {step.decides.join(' and ')}</span>}
      <span className={`next-step-human${step.human.endsWith(':') ? ' is-lead-in' : ''}`}>{step.human}</span>
      {blanks.map((i) => <WordsInput key={i} words={words[i] ?? ''} onChange={(v) => setWord(i, v)} />)}
      {children}
    </div>
  )
}
