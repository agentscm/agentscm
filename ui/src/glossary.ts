// One table of plain-language glosses for the product's own words, so every page explains them the same way.
// No DOM, no React.
//
// API for other packages:
//   Term          the glossed words
//   GLOSS[term]   { gloss, docs? }: the one-sentence explanation, and a docs link where one exists
//   glossText(t)  the tooltip text: the gloss, plus "More: <docs>" when set
//   Render with <Gloss term="…">word</Gloss> from Gloss.tsx.

export type Term =
  | 'hold'
  | 'contested'
  | 'overlap'
  | 'declaredScope'
  | 'claim'
  | 'checkpoint'
  | 'handover'
  | 'handoff'
  | 'directedRequest'
  | 'plainGit'
  | 'reportedFinished'
  | 'landed'
  | 'native'
  | 'revision'
  | 'skills'

const DOCS = 'docs.agentscm.dev/concepts'

/** The gloss for each term; `docs` names where to read more. */
export const GLOSS: Record<Term, { gloss: string; docs?: string }> = {
  hold: { gloss: 'This agent said it is editing this file. Other agents are warned, not locked out.' },
  contested: { gloss: 'Two agents both want this file. The hub refused the second one’s first edit and warned both.' },
  overlap: { gloss: 'Two tasks said they will change the same files. It is a reason to talk, not proof the code conflicts.' },
  declaredScope: { gloss: 'The files this task said it will change. The hub compares these across tasks.' },
  claim: { gloss: 'The task is assigned to its lead agent. An old check-in does not free it up.' },
  checkpoint: { gloss: 'A progress report the agent wrote: progress, a decision, a blocker, a pause, a handover or a finish.' },
  handover: { gloss: 'The lead says the task is ready for another agent to take over.' },
  handoff: { gloss: 'One agent asked another for a single file. Unanswered requests lapse after 30 minutes. Experimental.' },
  directedRequest: { gloss: 'A request one task’s agent sent to another task’s lead about overlapping files. Agents answer it; you can answer for your own agent from the Inbox.' },
  plainGit: { gloss: 'Started from an agent’s request, not from a GitHub or GitLab issue.' },
  reportedFinished: { gloss: 'The agent says it is finished. That is not a check, a review or a merge.' },
  landed: { gloss: 'Merged into the base branch on GitHub, as the hub last saw it.' },
  native: { gloss: 'Recorded in AgentSCM’s own history, separate from GitHub or GitLab.', docs: DOCS },
  revision: { gloss: 'An exact, unchangeable version of the files. It is not a Git commit.', docs: DOCS },
  skills: { gloss: 'Checklists agents here are expected to follow, pinned in the lock file. Only some steps can stop an agent, via an opt-in hook.' },
}

/** The tooltip text for a term: its gloss, plus "More: <docs>" when a docs page exists. */
export function glossText(term: Term): string {
  const g = GLOSS[term]
  return g.docs ? `${g.gloss} More: ${g.docs}` : g.gloss
}
