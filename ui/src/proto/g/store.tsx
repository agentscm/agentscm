// Live sample state for the G prototype: decisions made on any page change the same records everywhere
// (lanes, counts, timelines, badges). Browser memory only; a reload restores the sample.
import { createContext, useContext, useMemo, useRef, useState, type ReactNode } from 'react'
import { FLOWS, NEEDS, WORK, type FlowState, type Need, type Role, type Step, type Work } from '../data.ts'
import { morph } from '../kit.tsx'

export interface Toast { id: number; icon: string; text: string }
export interface Story { icon: string; text: string; work?: string; fresh?: boolean }

interface Store {
  works: Work[]
  work: (id: string) => Work | undefined
  resolved: Map<string, string>
  openFor: (role: Role) => Need[]
  flows: Record<string, FlowState>
  extra: Record<string, Step[]>
  stories: Story[]
  toast: Toast | null
  copilotFixed: boolean
  decide: (id: string, verdict: 'approve' | 'changes', note: string) => void
  answer: (needId: string, value: string) => void
  settle: (choice: 'keep' | 'hand' | 'split') => void
  handOver: () => void
  fixCopilot: () => void
  setFlow: (id: string, s: FlowState) => void
  addStep: (flowId: string, s: Step) => void
  dismissToast: () => void
  holdToast: () => void
  releaseToast: () => void
}

const Ctx = createContext<Store | null>(null)
export const useStore = () => {
  const s = useContext(Ctx)
  if (!s) throw new Error('StoreProvider missing')
  return s
}

const clone = (): Work[] => WORK.map((w) => ({ ...w, agents: [...w.agents], checks: w.checks.map((c) => ({ ...c })), activity: [...w.activity], prs: w.prs.map((p) => ({ ...p })), runs: [...w.runs] }))

export function StoreProvider({ children }: { children: ReactNode }) {
  const [works, setWorks] = useState<Work[]>(clone)
  const [resolved, setResolved] = useState<Map<string, string>>(() => new Map())
  const [flows, setFlows] = useState<Record<string, FlowState>>(() => Object.fromEntries(FLOWS.map((f) => [f.id, f.state])))
  const [extra, setExtra] = useState<Record<string, Step[]>>({})
  const [stories, setStories] = useState<Story[]>([])
  const [toast, setToast] = useState<Toast | null>(null)
  const [copilotFixed, setCopilotFixed] = useState(false)
  const timer = useRef<number | undefined>(undefined)

  const store = useMemo<Store>(() => {
    const say = (icon: string, text: string) => {
      window.clearTimeout(timer.current)
      setToast({ id: Date.now(), icon, text })
      timer.current = window.setTimeout(() => setToast(null), 4800)
    }
    const patch = (id: string, f: (w: Work) => Work) => setWorks((ws) => ws.map((w) => (w.id === id ? f(w) : w)))
    const close = (needId: string, outcome: string) => setResolved((m) => new Map(m).set(needId, outcome))
    const story = (s: Story) => setStories((xs) => [{ ...s, fresh: true }, ...xs.map((x) => ({ ...x, fresh: false }))])
    return {
      works,
      work: (id) => works.find((w) => w.id === id),
      resolved,
      openFor: (role) => NEEDS.filter((n) => n.roles.includes(role) && !resolved.has(n.id)),
      flows,
      extra,
      stories,
      toast,
      copilotFixed,
      decide: (id, verdict, note) => morph(() => {
        patch(id, (w) => verdict === 'approve'
          ? { ...w, stage: 'merge', state: 'Approved; GitHub is checking the merge', tone: 'brand', why: `You approved ${w.source}. GitHub runs the required checks on the merge commit before it lands.`, next: 'Nothing for you', checks: w.checks.map((c) => (c.name === 'Approval' ? { ...c, proof: 'human', detail: `You approved ${w.source}${note ? `: “${note}”` : ''}` } : c)), prs: w.prs.map((p) => ({ ...p, state: 'Approved · CI on merge', tone: 'brand' })), activity: [{ at: 'just now', text: `You approved ${w.source}` }, ...w.activity], fresh: 'just now' }
          : { ...w, stage: 'review', state: 'Sent back with your note', tone: 'brand', why: note ? `Your note: “${note}”. The author’s agent picks it up at its next check-in.` : 'The author’s agent picks up your request at its next check-in.', next: 'Nothing for you until it comes back', activity: [{ at: 'just now', text: 'You requested changes' }, ...w.activity], fresh: 'just now' })
        close('n1', verdict === 'approve' ? 'Approved' : 'Sent back')
        story({ icon: 'person-check', text: verdict === 'approve' ? 'A one-way door was approved with every required check current.' : 'A change went back to its author with a note before merge.', work: id })
        say(verdict === 'approve' ? 'check2-circle' : 'arrow-return-left', verdict === 'approve' ? 'Approved. PR #42 moves to merge; the agent, Slack and GitHub see the same.' : 'Sent back. maria’s claude-code gets your note at its next check-in.')
      }),
      answer: (needId, value) => morph(() => {
        patch('w3', (w) => ({ ...w, state: 'Writing the docs', tone: 'brand', why: `You answered: retry ${value} times. maria’s codex carries on with that.`, next: 'Nothing for you', activity: [{ at: 'just now', text: `You answered: ${value} tries` }, ...w.activity], fresh: 'just now' }))
        close(needId, `Answered: ${value} tries`)
        story({ icon: 'chat-left-text', text: 'A blocked agent got its answer without anyone opening a terminal.', work: 'w3' })
        say('send-check', `Sent “${value} tries” to maria’s codex. It continues now.`)
      }),
      settle: (choice) => morph(() => {
        const text = choice === 'keep' ? 'maria keeps totals.rs; dev’s codex waits for it' : choice === 'hand' ? 'totals.rs goes to dev’s codex first' : 'Split: maria keeps tax, dev takes rounding helpers'
        patch('w2', (w) => ({ ...w, state: choice === 'keep' ? 'Waiting for the billing change' : 'Author fixing two findings', why: `Overlap settled: ${text}.`, activity: [{ at: 'just now', text: `Overlap settled: ${text}` }, ...w.activity], fresh: 'just now' }))
        patch('w1', (w) => ({ ...w, activity: [{ at: 'just now', text: `Overlap settled: ${text}` }, ...w.activity] }))
        close('n2', text)
        story({ icon: 'arrow-left-right', text: `An overlap on src/billing/totals.rs was settled before either PR merged: ${text.toLowerCase()}.`, work: 'w2' })
        say('arrow-left-right', 'Recorded. Both agents read the agreement before their next edit.')
      }),
      handOver: () => morph(() => {
        patch('w4', (w) => ({ ...w, agents: [{ who: 'you', tool: 'claude-code' }], state: 'Picked up by your claude-code', tone: 'brand', why: 'Your agent has the handover note: the responsive pass remains.', next: 'Nothing for you', activity: [{ at: 'just now', text: 'Your claude-code picked up the handover' }, ...w.activity], fresh: 'just now' }))
        close('n4', 'Picked up by your claude-code')
        say('box-arrow-in-right', 'Handed to your claude-code with the note. Open your agent to continue.')
      }),
      fixCopilot: () => morph(() => {
        setCopilotFixed(true)
        close('n5', 'Fix copied for sam')
        say('clipboard-check', 'Copied the one-line fix for sam’s computer.')
      }),
      setFlow: (id, s) => morph(() => {
        setFlows((f) => ({ ...f, [id]: s }))
        if (s === 'Active') say('lightning-charge', 'Active. Agents read it at session start; nothing ran yet.')
      }),
      addStep: (flowId, s) => morph(() => setExtra((e) => ({ ...e, [flowId]: [...(e[flowId] ?? []), s] }))),
      dismissToast: () => setToast(null),
      holdToast: () => window.clearTimeout(timer.current),
      releaseToast: () => { timer.current = window.setTimeout(() => setToast(null), 2500) },
    }
  }, [works, resolved, flows, extra, stories, toast, copilotFixed])

  return <Ctx.Provider value={store}>{children}</Ctx.Provider>
}
