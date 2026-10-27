// Walkthrough copy, one typed map. Edit words here; the overlay in Tour.tsx only renders it.
// Plain language: short sentences, say what the reader sees and what to do next.
import type { Section } from '../App.tsx'
import type { Derived } from '../derive.ts'
import type { WorkOverview } from '../standup.ts'
import type { HubData } from '../useHub.ts'

/** What a live tip can read: the hub data already loaded, plus a look at the page itself. */
export interface TourContext {
  data: HubData | null
  d: Derived | null
  overview: WorkOverview | null
  /** How many visible elements match a selector on the page right now. */
  count: (selector: string) => number
  /** The trimmed text of the first visible match, so a tip can quote what the page already says. */
  text: (selector: string) => string | null
}

export interface TourStep {
  /** CSS selectors for the element to spotlight; the first visible one wins. None: a centred card. */
  target?: string[]
  title: string
  /** What the element shows. */
  what: string
  /** Why it matters. */
  why: string
  /** How to use it best. */
  how: string
  /** One sentence about the data on screen now, or null when there is nothing to point at. */
  tip?: (c: TourContext) => string | null
  /** Shown instead of the tip when it is null or the target is missing: what would appear here. */
  empty?: string
}

export type TourKey = 'welcome' | Section

export interface TourDef {
  title: string
  steps: TourStep[]
}

export const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

// Targets are stable data-tour anchors, never classes or DOM shape, so restyling a page keeps its tour.
const T = (...anchors: string[]) => anchors.map((a) => `[data-tour="${a}"]`)
// On phones the rail lives in a drawer, so nav steps fall back to the menu button.
const NAV = (item: string) => T(`nav-${item}`, 'menu')

export const WELCOME: TourDef = {
  title: 'Welcome to the AgentSCM Hub',
  steps: [
    {
      title: 'This page shows what your agents are doing',
      what: 'The hub watches every coding agent connected to this repository: which files each one holds, which task it owns, and what checks said about its work.',
      why: 'Two agents editing the same file is how work gets lost. The hub spots it first and tells you who to talk to.',
      how: 'Most pages only read, so look around freely. Two places can change things, and both say so: Setup › Access, after you sign in with GitHub or GitLab, and the inline agent views in Setup › Preferences.',
      tip: (c) => (c.d ? `Right now ${plural(c.d.agents.length, 'agent')} ${c.d.agents.length === 1 ? 'is' : 'are'} connected to this repository.` : null),
      empty: 'Once an agent connects, the count of live agents shows here.',
    },
    {
      target: T('repo'),
      title: 'Your repository',
      what: 'The repository this page is about. Your sign-in token decides which one you see.',
      why: 'Every number on every page belongs to this one repository, so check it before you act on what you read.',
      how: 'To look at another repository, open this menu and use a token made for it.',
      tip: (c) => (c.data?.whoami ? `You are looking at ${c.data.whoami.token.repository}.` : null),
    },
    {
      target: T('search'),
      title: 'Search everything with ⌘K',
      what: 'One box that finds pages, agents, held files and work items.',
      why: 'It is the fastest way to answer “who has this file?” without clicking through pages.',
      how: 'Press ⌘K (Ctrl K on Windows and Linux) or / from anywhere. Type a file name, an agent or a task. Try “walk me through” to replay a page tour.',
    },
    {
      target: T('live'),
      title: 'Live means the numbers are fresh',
      what: 'A green dot says the hub answered the last refresh. Pages refresh every few seconds while this tab is open.',
      why: 'If the hub stops answering, the page keeps the last data it had and says so. You will never act on stale numbers without a warning.',
      how: 'When it turns coral or says Reconnecting, open it to see the hub’s health.',
      tip: (c) => (!c.data?.health ? null : c.data.failing ? 'The hub did not answer the last refresh, so you are seeing older data.' : c.data.health.status === 'ok' ? 'The hub is answering right now.' : 'The hub reports a problem. Open it for details.'),
    },
    {
      target: NAV('home'),
      title: 'Dashboard: what is happening',
      what: 'One sentence on top, then four tabs. Now lists what needs you and every task as a lane. Where maps the repository. Catch up shows what happened while you were away. Team shows who is working.',
      why: 'This is where you catch two agents about to collide, before either one wastes an afternoon.',
      how: 'Start your day here. A coral number in the sentence or on the rail counts the things that need a person.',
      tip: (c) => (!c.d ? null : c.d.contested.length > 0 ? `${plural(c.d.contested.length, 'file is', 'files are')} contested right now. Open Where to see who holds each one.` : 'No contested files right now: every held path has exactly one agent.'),
    },
    {
      target: NAV('inbox'),
      title: 'Inbox: the conversations to have',
      what: 'Tasks that declared overlapping files, and the requests agents sent each other to split, join or hand over.',
      why: 'An overlap is a signal to talk, not proof of a conflict. Reading the Inbox never answers a request for anyone.',
      how: 'Open a request to see who asked whom, and whether they received and replied.',
      tip: (c) => (c.overview ? (c.overview.overlaps.length > 0 ? `${plural(c.overview.overlaps.length, 'overlap')} waiting in the Inbox.` : 'No declared scopes overlap right now.') : null),
    },
    {
      target: NAV('history'),
      title: 'History: what happened before',
      what: 'What agents reported finished, every change to Work page by page, change records ready to hand off, and the files behind them.',
      why: 'An agent saying “done” is not proof. History keeps the record so you can check before you trust it.',
      how: 'Open a finished task, then its change record, before you build on it.',
    },
    {
      target: NAV('skills'),
      title: 'Skills: did the agreed steps run',
      what: 'The workflows agents here must follow, the checks that ran on each exact revision, what CI and deploys did after a merge, and reusable recipes.',
      why: 'A passing check only proves those exact files passed. These pages keep that honest.',
      how: 'Before you trust a finished task, look for its check run or its landed change.',
      tip: (c) => {
        const done = c.overview?.recent.length ?? 0
        return c.overview ? (done > 0 ? `${plural(done, 'task was', 'tasks were')} reported finished recently. Skills tells you whether checks agree.` : 'No tasks were reported finished in the recent window yet.') : null
      },
    },
    {
      target: NAV('setup'),
      title: 'Setup: connect and configure',
      what: 'Connect agents, check the hub, manage workspace access and set your own preferences.',
      why: 'An agent only shows up once it is connected. Agents tells you which ones are ready.',
      how: 'New here? Connect one agent first. It is Ready once it proves it can reach the hub and its edit hook fires.',
      tip: (c) => (c.d ? (c.d.agents.length > 0 ? `${plural(c.d.agents.length, 'agent is', 'agents are')} already reporting here.` : 'No agents are reporting yet, so Setup is the place to start.') : null),
    },
    {
      target: T('view', 'menu'),
      title: 'Light or dark, spacious or condensed',
      what: 'This menu switches the sheet between light and dark, or follows your device, and between Spacious and Condensed.',
      why: 'Condensed fits more on screen without moving anything: every page keeps the same parts in the same places.',
      how: 'Both choices stay in this browser only. Setup › Preferences has them too.',
    },
    {
      target: T('tour-trigger'),
      title: 'Lost? Each page has its own tour',
      what: 'The question mark walks you through the page you are on, pointing at the real numbers on screen.',
      why: 'Each page answers one question. A short tour shows how to read the answer.',
      how: 'Use it whenever a page is unclear, or press ⌘K and choose “Walk me through this page”. Setup › Preferences turns tours on or off.',
    },
  ],
}

export const SECTION_TOURS: Partial<Record<Section, TourDef>> = {
  home: {
    title: 'Dashboard › Now',
    steps: [
      {
        target: T('answer'),
        title: 'The answer comes first',
        what: 'One sentence: how many agents are working on how many tasks.',
        why: 'You should know in two seconds how busy the repository is. The tabs below count what needs you and where agents cross.',
        how: 'The sentence stays on top of every dashboard tab.',
        tip: (c) => (c.d && c.overview ? `${plural(c.d.agents.length, 'agent')} on ${plural(c.overview.current.length, 'task')} right now.` : null),
      },
      {
        target: T('tabs'),
        title: 'Four tabs, one question each',
        what: 'Now, Where, Catch up and Team. Under each name is its live status: a coral dot means something there needs you.',
        why: 'Each fact lives in exactly one tab, so you never read the same thing twice.',
        how: 'Each tab has its own address, so you can bookmark or share the one you need.',
      },
      {
        target: T('needs'),
        title: 'Needs you',
        what: 'Blocked tasks, agents that went quiet but still hold their task, handovers, files two agents both hold, tasks planning the same files, and pull requests that touch files an agent holds.',
        why: 'Each one stays stuck until someone decides.',
        how: 'Each row ends with what to do next. Follow its link to the task, the file or the Inbox.',
        empty: 'When nothing needs you, this box says so in green.',
      },
      {
        target: T('work-lanes'),
        title: 'The track',
        what: 'Every unfinished task as one lane: claimed, where it is now with the face of the agent holding it, its next step, then finished.',
        why: 'A coral segment is blocked and an amber one went quiet. A quiet task is still assigned, so ask its lead before taking it over.',
        how: 'Click any segment to open the task. When a task moves, its new segment sweeps in with the agent riding it.',
        tip: (c) => {
          const cur = c.overview?.current
          if (!cur) return null
          const blocked = cur.filter((i) => i.state === 'blocked').length
          return blocked > 0 ? `${plural(blocked, 'task is', 'tasks are')} blocked right now. Open one to read the blocker the agent wrote.` : cur.length > 0 ? `${plural(cur.length, 'task')} assigned and none blocked.` : null
        },
        empty: 'When agents announce tasks, each appears here as a lane.',
      },
    ],
  },
  team: {
    title: 'Dashboard › Team',
    steps: [
      {
        target: T('now-agents'),
        title: 'Who is working',
        what: 'Live agents grouped by the person who owns them. A green dot means the agent reported in the last five minutes; amber means it is quieter but still holds its files.',
        why: 'Before you start something, check nobody already holds those files.',
        how: 'Each chip is a file or folder the agent holds. A coral chip is contested.',
        tip: (c) => (c.d && c.d.agents.length > 0 ? `${c.d.agents[0]!.name} (${c.d.agents[0]!.owner}) holds ${plural(c.d.agents[0]!.holdings.length, 'path')}.` : null),
        empty: 'No agents are active. When a connected agent starts editing, it appears here with the files it holds.',
      },
    ],
  },
  inbox: {
    title: 'Inbox',
    steps: [
      {
        target: T('answer'),
        title: 'What needs a conversation',
        what: 'How many declared-scope overlaps and open requests there are, with a bar showing where each one stands.',
        why: 'Overlaps are a signal to talk before two agents write the same code. Detecting one sends nothing.',
        how: 'Coral is an overlap nobody has raised yet, amber a request waiting for an answer, violet one that settled.',
        tip: (c) => (c.overview ? (c.overview.overlaps.length > 0 ? `${plural(c.overview.overlaps.length, 'overlap')} right now.` : 'No overlaps right now.') : null),
      },
    ],
  },
  now: {
    title: 'Dashboard › Where',
    steps: [
      {
        target: T('now-map'),
        title: 'The repository as a map',
        what: 'One tile per top-level folder an agent holds files in or a task plans to touch. Faces are the agents there; a deeper colour means more people.',
        why: 'A coral ring and pill mark a crossing: two agents holding one file, or two tasks planning the same one.',
        how: 'Pick a tile to see who is editing there, which tasks plan to, and why it crosses.',
        tip: (c) => (c.d ? (c.d.contested.length > 0 ? `${plural(c.d.contested.length, 'file')} contested. Look for the coral tile.` : 'No contested files right now.') : null),
        empty: 'Tiles appear when an agent starts editing or a task declares its scope.',
      },
      {
        target: T('now-contested'),
        title: 'Contested files',
        what: 'Files that two agents hold at the same time.',
        why: 'The hub refuses the second agent’s first edit to one of these. If nobody steps in, one agent’s work gets thrown away.',
        how: 'Each row says who holds it and whether a handoff was asked for. Then tell one agent to wait or hand it over.',
        tip: (c) => (!c.d ? null : c.d.contested.length > 0 ? `${plural(c.d.contested.length, 'file')} two agents are editing at once.` : null),
        empty: 'Nothing is contested now. When two agents claim the same path, the file appears here with both holders.',
      },
      {
        target: T('now-prs'),
        title: 'Pull requests touching held files',
        what: 'Open pull requests whose changes overlap files a live agent is editing.',
        why: 'A merge that lands under an agent’s feet makes its work conflict.',
        how: 'If an overlap shows up, merge the pull request first or ask the agent to rebase after it.',
        tip: (c) => (!c.d || !c.data?.work ? null : !c.data.work.connected ? null : c.d.prOverlaps.length > 0 ? `${plural(c.d.prOverlaps.length, 'pull request overlaps', 'pull requests overlap')} with a live agent.` : null),
        empty: 'Pull requests appear here once GitHub or GitLab is connected to the hub and one touches a held file.',
      },
    ],
  },

  work: {
    title: 'History',
    steps: [
      {
        target: T('answer'),
        title: 'What was reported finished',
        what: 'Tasks agents reported finished in the recent window, with who finished them and when.',
        why: 'Finished is the agent’s own report. It does not prove review, verification or a merge.',
        how: 'Open a task, then its change record, before you build on it. Changes lists every start, join, takeover and checkpoint.',
        tip: (c) => {
          const done = c.overview?.recent.filter((o) => o.kind === 'completion').length
          return done === undefined ? null : done > 0 ? `${plural(done, 'task')} reported finished recently.` : 'Nothing was reported finished recently.'
        },
      },
      {
        target: T('tabs'),
        title: 'Four views of what happened',
        what: 'Finished, Changes, Change records and Files.',
        why: 'Each answers a different question, so you do not have to hunt through one long list.',
        how: 'Files reads from your own checkout on this computer; the rest come from the hub.',
      },
    ],
  },

  activity: {
    title: 'Dashboard › Catch up',
    steps: [
      {
        target: ['#since-last-checked'],
        title: 'Since you last checked',
        what: 'A sentence about the time you were away, then three lanes of dots: merges, work updates and messages between agents. A coral dot needs a look.',
        why: 'You can see at a glance whether anything happened, and when.',
        how: 'Pick a lane name to narrow the list below. Mark reviewed moves your own review position; opening or paging never does.',
        empty: 'When records arrive after your last review, they appear here as dots.',
      },
      {
        target: T('activity-filter'),
        title: 'The full log',
        what: 'Every hub event, newest first. Four buttons filter it: Everything, Refusals & handoffs, Announcements and Sessions.',
        why: 'On a busy repository the signal is easy to lose. Refusals are the events that need a person.',
        how: 'Pick Refusals & handoffs to see only collisions and who gave a file to whom.',
        tip: (c) => {
          const n = c.count('[data-tour="activity-feed"] .event.tone-bad')
          return n > 0 ? `${plural(n, 'event')} below ${n === 1 ? 'is a' : 'are'} refusal${n === 1 ? '' : 's'}: an agent tried to edit a file another agent holds.` : null
        },
        empty: 'Refusals appear when an agent is refused an edit because another agent holds the file.',
      },
      {
        target: T('activity-load-older'),
        title: 'Go further back',
        what: 'Load older fetches the events before the oldest one on screen. When there are none left it says That’s everything.',
        why: 'A conflict that matters now may have started earlier than the latest events.',
        how: 'Press Load older until you reach the refusal or handoff you are looking for.',
        empty: 'Load older appears under the log once events have loaded.',
      },
    ],
  },

  workflows: {
    title: 'Skills › Checks',
    steps: [
      {
        target: T('answer'),
        title: 'Checks are evidence, not approval',
        what: 'Tests and reviews that ran on a specific revision and were recorded on the hub.',
        why: 'A passing run only proves those exact files passed. If the code moved on, the proof no longer applies.',
        how: 'Match the revision shown on a run to the one you plan to land.',
      },
      {
        target: T('workflows-runs'),
        title: 'Workflow runs',
        what: 'Each row is one run: its name, ID, the revision it tested and its result.',
        why: 'A Failed or Blocked pill is the fastest way to see which work should not land yet.',
        how: 'Open a run to see each step, its findings and the files it read.',
        tip: (c) => {
          const bad = c.count('[data-tour="workflows-runs"] .run-row .pill.tone-bad')
          const n = c.count('[data-tour="workflows-runs"] .run-row')
          return n === 0 ? null : bad > 0 ? `${plural(bad, 'run')} on this page ${bad === 1 ? 'failed or is blocked' : 'failed or are blocked'}. Open one to see which step.` : `${plural(n, 'run')} on this page and none failed.`
        },
        empty: 'Runs appear here after a skill workflow records its result on the hub. The hub may also say it cannot look them up.',
      },
      {
        target: T('workflow-run-picker'),
        title: 'Find a run',
        what: 'A search box that finds a run by its name, branch or ID, across every run the hub keeps.',
        why: 'You rarely know where a run sits in the list. Your agent tells you the branch or prints the run ID.',
        how: 'Type a few letters of the branch or name and pick the run, or paste the run ID your agent printed.',
        empty: 'The search box sits in the Workflow runs header once the hub can list runs.',
      },
      {
        target: T('pager'),
        title: 'Older and newer runs',
        what: 'Buttons that move one page back or forward through the run list.',
        why: 'The filter above the list only looks at the page you see, not every run.',
        how: 'If the filter finds nothing, page back with Older, or use the search box, which looks at every run.',
        empty: 'Page buttons appear when there are more runs than fit on one page.',
      },
    ],
  },

  'change-record': {
    title: 'Change records',
    steps: [
      {
        target: T('answer'),
        title: 'One page per piece of work',
        what: 'A change record gathers what is known about a work item: its source, the last recorded check, the decision and what to do next.',
        why: 'It is the handoff page. Someone taking over should not have to ask the last agent what happened.',
        how: 'Opening a record only reads. It does not accept or transfer the work.',
      },
      {
        target: T('change-record-picker'),
        title: 'Find a work item',
        what: 'A search box for work items by title, agent or branch. It also takes a work ID, which starts with wrk_.',
        why: 'You can reach any record from what you remember about the task, without first finding its ID.',
        how: 'Type part of the task title or the agent’s name and pick the item. A work ID from an agent works too.',
        empty: 'Once a record is open, this page shows its details instead of the lookup box.',
      },
      {
        target: T('change-latest'),
        title: 'The latest check, in plain labels',
        what: 'Chips for what ran, how it ended, whether it applies to the current source and how much evidence is kept.',
        why: 'Ran and verified are different. “Verification not established” means a command ran but nothing proved the change correct.',
        how: 'Read the chips left to right. A grey or yellow chip is a gap, not a pass.',
        tip: (c) => {
          const n = c.count('[data-tour="change-latest"] .pill')
          return n > 0 ? `${plural(n, 'chip')} describe the latest action. Any yellow or grey one means something is unproven.` : null
        },
        empty: 'Open a record from the lookup to see the chips for its latest recorded action.',
      },
      {
        target: T('change-gaps'),
        title: 'Known gaps',
        what: 'Facts the record could not establish, such as an unknown source or sessions that were not captured.',
        why: 'Gaps stay visible instead of looking like success.',
        how: 'Fix the gaps before handing off: capture the missing action, then reload the record.',
        tip: (c) => (c.count('[data-tour="change-gaps"] li') > 0 ? `The count in this box’s header is how many gaps this record has.` : null),
        empty: 'Open a record to see its gaps. A record with none says so.',
      },
    ],
  },

  landed: {
    title: 'Skills › After merge',
    steps: [
      {
        target: T('answer'),
        title: 'Did it land, and did main stay green',
        what: 'How many recent published changes were merged, and how many broke something after landing.',
        why: 'A merge can go in and still break the main branch. This is where you hear about it.',
        how: 'If the sentence mentions a failure, open the change it points at.',
        tip: (c) => {
          const n = c.count('[data-tour="landed-list"] .landed-row .pill.tone-bad')
          return n > 0 ? `${plural(n, 'change')} failed after landing. Open one to see which check.` : null
        },
        empty: 'When agents publish changes through the hub, this sentence counts how many landed and how many failed after.',
      },
      {
        target: T('landed-picker'),
        title: 'Find a change',
        what: 'A search box for published changes by branch, title or pull request number. It also takes a change ID, which starts with pub_.',
        why: 'You usually know the branch or the pull request, not the ID the publishing agent printed.',
        how: 'Type the branch name or a PR number such as 42 and pick the change to open it.',
        empty: 'The search box appears once this hub can list the changes agents published.',
      },
      {
        target: T('landed-list'),
        title: 'Recent changes',
        what: 'One row per published change, newest first. The five dots show how far it got: branch, pull request, merge, CI and deploys.',
        why: '“Landed, CI failed after” means the merge went in but the base branch broke. “Waiting to merge” means nothing has landed yet.',
        how: 'Open a row to see the CI runs and deployments for the merge commit.',
        tip: (c) => {
          const n = c.count('[data-tour="landed-list"] .landed-row')
          const bad = c.count('[data-tour="landed-list"] .landed-row .pill.tone-bad')
          return n === 0 ? null : bad > 0 ? `${plural(bad, 'row')} here ${bad === 1 ? 'is' : 'are'} red. Landed, CI failed after means the merge went in but main broke; open it to see which check.` : `${plural(n, 'change')} listed and none red.`
        },
        empty: 'Changes appear here once an agent publishes one through the hub. The hub may also be too old to show them.',
      },
    ],
  },

  skills: {
    title: 'Skills',
    steps: [
      {
        target: T('answer'),
        title: 'Which workflows agents must follow',
        what: 'The skills this repository asks agents to follow, such as the pull request gate, and where that choice was read from.',
        why: 'A skill turns “run the tests and get a review” from a habit into steps the hub can track for each branch.',
        how: 'Check the adoption line first. A skill only counts once the lock file is committed.',
        tip: (c) => c.text('[data-tour="answer"] .answer-text'),
      },
      {
        target: T('skills-adopted'),
        title: 'Adopted skills',
        what: 'Skills pinned in this repository’s lock file, with how many runs are open, waiting at a stop gate or done.',
        why: 'These are the workflows agents here are expected to follow on every pull request.',
        how: 'Open View runs to see where each pull request is in its workflow.',
        tip: (c) => {
          const n = c.count('[data-tour="skills-adopted"] [data-tour="skills-item"]')
          return n > 0 ? `${plural(n, 'skill is', 'skills are')} adopted here.` : null
        },
        empty: 'Nothing is adopted yet. Pick a skill under Available and copy its command.',
      },
      {
        target: T('skills-head'),
        title: 'What is enforced and what is planned',
        what: 'Each skill says which gates are enforced now and which are only planned. “Enforced: Stop hook” means an opt-in Stop hook can keep the author’s session from finishing at tests, review and fix. “Planned: merge check” is not built yet.',
        why: 'So you know which steps the hub checks at all, and which rely only on the agent following along.',
        how: 'Treat the hook as a reminder, not a lock. It is off unless installed, lets the agent finish when the hub cannot be reached or the checkout is detached, and accepts evidence the agents report themselves.',
        tip: (c) => {
          const t = c.text('[data-tour="skills-head"]')
          return t ? `The first skill on screen reads: ${t}.` : null
        },
        empty: 'Each skill shows its version, its hash and its Enforced and Planned labels here.',
      },
      {
        target: T('skills-how'),
        title: 'How it works, in plain English',
        what: 'One step per gate, written by the hub: who does it, what it needs, and whether the Stop hook blocks on it. Full procedure opens the exact text agents follow.',
        why: 'You can check what an agent is supposed to do before you adopt it, without reading the workflow file.',
        how: 'Read the steps top to bottom. They match the gates you will see on each run.',
        tip: (c) => {
          const t = c.text('[data-tour="skills-how"] .gate-plain')
          return t ? `Step 1 says: “${t}”` : null
        },
        empty: 'The plain-English steps appear here for each skill the hub ships.',
      },
      {
        target: T('skills-command'),
        title: 'Adopt or remove with a command',
        what: 'The exact command to run in the repository, with a copy button.',
        why: 'This page never writes. Adoption is a change to agentscm-skills.lock.json that you commit and review like any other.',
        how: 'Copy the command, run it in your checkout, then commit the lock file.',
        tip: (c) => {
          const t = c.text('[data-tour="skills-command"] code')
          return t ? `Copy this: ${t}` : null
        },
        empty: 'A command to adopt or remove a skill appears under each one.',
      },
    ],
  },
  templates: {
    title: 'Skills › Library',
    steps: [
      {
        target: T('answer'),
        title: 'A template is a saved recipe of checks',
        what: 'A recipe says which files to check, which commands to run and whether Claude should review. Any change can reuse it.',
        why: 'Agents then run the same checks every time, so a pass means the same thing each time.',
        how: 'This page only looks. It never starts or approves a run.',
      },
      {
        target: T('templates-change-picker'),
        title: 'See a change’s recipe',
        what: 'Pick a change from the search box, then give the path of the template file inside it.',
        why: 'It shows the steps, what each waits for and which are required, before anyone runs them.',
        how: 'Search the changes on this computer or paste a change ID. The example path in the second box is a good starting point.',
        tip: (c) => (c.count('.recipe-step') > 0 ? `${plural(c.count('.recipe-step'), 'step')} in the recipe below. Required means the run fails if that step fails.` : null),
        empty: 'After you open a recipe, its steps list here with Required or Optional on each.',
      },
      {
        target: T('templates-run-picker'),
        title: 'Where did a run get to',
        what: 'A search box that finds a run by its name, state or ID. It shows each step’s result and what to do next.',
        why: 'It turns a run your agent started into a clear pass, fail or blocked answer.',
        how: 'Check the “What to do next” note under the steps. It is written for you to act on.',
        tip: (c) => {
          const bad = c.count('.run-row .pill.tone-bad')
          return c.count('.run-row') > 0 ? (bad > 0 ? `${plural(bad, 'step')} failed or ${bad === 1 ? 'is' : 'are'} blocked in this run.` : 'No step is failed or blocked in this run.') : null
        },
        empty: 'After you open a run, its steps list here.',
      },
      {
        target: T('templates-source'),
        title: 'Preview data or your own computer',
        what: 'A banner says when the page shows sample data. This page reads from agentscm serve on your computer, not from the hub.',
        why: 'Sample data looks real. The banner is how you tell the two apart.',
        how: 'To see your own data, start agentscm serve and set its address in Setup › Preferences.',
        tip: (c) => (c.count('.notice.tone-brand') > 0 ? 'You are looking at sample data right now.' : null),
        empty: 'No preview banner is showing, so this page is reading from your computer.',
      },
    ],
  },

  timeline: {
    title: 'Timeline',
    steps: [
      {
        target: T('answer'),
        title: 'Saved pictures of your files',
        what: 'A snapshot is a saved copy of your workspace at one moment, taken on your computer.',
        why: 'When an agent changes the wrong thing, you can see what the files looked like before.',
        how: 'This page only reads. Saving and starting again from a snapshot happen in the CLI.',
      },
      {
        target: T('timeline-list'),
        title: 'Saved moments',
        what: 'Newest first. Each row says why it was saved, when, and how many files changed.',
        why: 'A row that says a check tested this snapshot ties a test result to the exact files it ran on.',
        how: 'Open a row for its details and the command to start again from it. Use Older to go further back.',
        tip: (c) => {
          const n = c.count('[data-tour="timeline-list"] .snap')
          const checks = c.count('[data-tour="timeline-list"] .snap.kind-check_link')
          return n === 0 ? null : `${plural(n, 'saved moment')} on this page, ${plural(checks, 'of them a check link', 'of them check links')}.`
        },
        empty: 'Agents save a snapshot at the end of each turn once the timeline is on. Nothing is saved yet.',
      },
      {
        target: T('timeline-compare', 'timeline-list'),
        title: 'Compare two moments',
        what: 'Tick Compare on two rows, then press the button that appears.',
        why: 'It lists the files added, changed or removed between them, so you see exactly what an agent did in between.',
        how: 'Pick an older and a newer row. Only files both of them cover are compared.',
        tip: (c) => (c.count('[data-tour="timeline-list"] .snap-pick') >= 2 ? 'There are enough rows here to compare two. Tick one on each side of the work you care about.' : null),
        empty: 'You need at least two saved moments to compare.',
      },
    ],
  },

  workspace: {
    title: 'Workspace',
    steps: [
      {
        target: T('answer'),
        title: 'The one page where you can change things',
        what: 'The workspace is the shared place for repositories, members and workflow runs. It uses your GitHub or GitLab account, not your hub token.',
        why: 'Starting runs and managing members need a person who is allowed to. This is how the hub knows who you are.',
        how: 'Sign in with your forge account. The other pages stay read-only.',
      },
      {
        target: T('workspace-you', 'workspace-signin'),
        title: 'What you are allowed to do',
        what: 'Your roles and five abilities: read, run workflows, approve, integrate and manage the workspace.',
        why: 'Buttons stay disabled when you lack the ability. This list says why.',
        how: 'If an ability you need is dimmed, ask a workspace admin to change your role.',
        tip: (c) => {
          const on = c.count('[data-tour="workspace-you"] .caps li.is-on')
          return c.count('[data-tour="workspace-you"] .caps li') > 0 ? `You can do ${on} of ${c.count('[data-tour="workspace-you"] .caps li')} things here.` : null
        },
        empty: 'You are not signed in. Use Continue with GitHub or GitLab. If neither shows, ask your operator to turn on sign-in.',
      },
      {
        target: T('workspace-tabs'),
        title: 'Repositories, members, settings',
        what: 'Repositories & runs shows recent workflow runs and starts new ones. Members lists who has access. Settings holds the shared name.',
        why: 'Each tab is a separate permission. Many people only need the first.',
        how: 'Open Repositories & runs to see run results. Run history is kept on the hub, not in this browser.',
        empty: 'These tabs appear after you sign in.',
      },
      {
        target: T('workspace-runs'),
        title: 'Recent runs',
        what: 'Workflow runs on the hub, newest first, 20 per page. Each row has a result.',
        why: 'Unknown and stale runs need an operator to look before anyone tries again.',
        how: 'Choose the repository at the top of the box. Open a run to see each step.',
        tip: (c) => (c.count('[data-tour="workspace-runs"] .ws-run') > 0 ? `${plural(c.count('[data-tour="workspace-runs"] .ws-run'), 'run')} on this page. Unknown or stale ones need operator review.` : null),
        empty: 'Runs appear here after someone starts a workflow on a configured repository.',
      },
      {
        target: T('workspace-change-picker'),
        title: 'Pick the change to run',
        what: 'A search box for current native changes. It also takes the ID of any existing change.',
        why: 'Loading a change shows the exact revision and review context before anything runs.',
        how: 'Search for the change, check the revision it shows, then start the run. Starting a run is a real action.',
        empty: 'This box appears when you can start runs on a repository with a workflow profile.',
      },
    ],
  },

  setup: {
    title: 'Setup › Agents',
    steps: [
      {
        target: T('answer'),
        title: 'Ready needs proof, not settings',
        what: 'An agent is Ready after it proves, in one session, that it reaches the hub and that its edit hook fires on a real edit.',
        why: 'A config file can look right and still do nothing. Proof is the only thing that shows the hub will see edits.',
        how: 'Do the setup, then ask the agent to make one small edit.',
      },
      {
        target: T('setup-sessions'),
        title: 'Verified sessions',
        what: 'One row per agent session with its state and two checks: MCP and Edit hook.',
        why: 'Both must be verified. Proof expires, so a Ready agent can go back to Unverified.',
        how: 'Read the line under each row. It says what to ask the agent to do next.',
        tip: (c) => {
          const n = c.count('[data-tour="setup-sessions"] .setup-row')
          const blocked = c.count('[data-tour="setup-sessions"] .setup-row .pill.tone-warn')
          return n === 0 ? null : blocked > 0 ? `${plural(blocked, 'session')} ${blocked === 1 ? 'is' : 'are'} Blocked. The line under each says what stops it.` : `${plural(n, 'session')} listed and none Blocked.`
        },
        empty: 'Sessions appear here once an agent set up with agentscm client setup or agentscm quickstart verifies itself.',
      },
      {
        target: T('setup-steps', 'setup-connect'),
        title: 'Four steps to connect',
        what: 'Paste the instructions into your agent, sign in, approve access yourself, then verify a real edit.',
        why: 'You approve the sign-in and the trust prompts, because the agent should not approve its own access.',
        how: 'Do the steps in order. Nothing runs from this page.',
        empty: 'This page may not be served from a supported address. The box then says which address to open.',
      },
      {
        target: T('setup-actions', 'setup-connect'),
        title: 'Copy the instructions',
        what: 'A button that copies setup instructions for your agent, and a command-line option for each supported agent.',
        why: 'The instructions already contain this hub’s address, so you do not type it.',
        how: 'Use Preview the instructions to read what you are about to paste.',
        empty: 'The copy button appears when the page is served from an HTTPS or loopback address.',
      },
    ],
  },

  health: {
    title: 'Setup › Hub',
    steps: [
      {
        target: T('answer'),
        title: 'Is the hub answering',
        what: 'A yes or no, and which database holds the data.',
        why: 'If the hub is down, agents let every edit through. Nothing is protected until it is back.',
        how: 'This page keeps trying. You do not need to reload.',
        tip: (c) => (c.data?.health ? (c.data.health.status === 'ok' ? 'The hub is answering right now.' : 'The hub is not answering. Agents are letting edits through.') : null),
        empty: 'The sentence appears as soon as the hub answers its first health check.',
      },
      {
        target: T('health-counters'),
        title: 'How often the hook stepped in',
        what: 'Counts since the hub started: refused first edits, repeated edits, edits let through when unsure and rate-limited refusals.',
        why: 'Many “let through when unsure” means the hub could not decide. Protection was weaker than it looked.',
        how: 'Hover the (i) beside each number. The counts reset when the hub restarts.',
        tip: (c) => {
          const n = c.count('[data-tour="health-counters"] .counter:not(.is-zero)')
          return c.count('[data-tour="health-counters"] .counter') > 0 ? (n > 0 ? `${plural(n, 'counter')} above zero. Zero counters are dimmed.` : 'All counters are zero, so the hook has not stepped in.') : null
        },
        empty: 'Counters appear when the hub is healthy and reports coordination numbers.',
      },
      {
        target: T('health-speed'),
        title: 'Hook speed',
        what: 'How long 95 of 100 edit checks took, against a 250 ms budget.',
        why: 'A slow hook slows every edit an agent makes. Past the budget, the hub lets edits through instead of waiting.',
        how: 'Green is fine, yellow is getting slow and red is near the budget. Tell your operator if it stays red.',
        tip: (c) => {
          const p = c.data?.health?.coordination?.hook_p95_ms
          return p ? `The slowest typical hook call took about ${Math.round(p) || '<1'} ms.` : null
        },
        empty: 'No hook calls have been measured yet, so there is no speed to show.',
      },
    ],
  },

  settings: {
    title: 'Setup › Preferences',
    steps: [
      {
        target: T('answer'),
        title: 'Mostly yours, saved in this browser',
        what: 'Theme, sidebar, tours and your token are saved in this browser only. Inline agent views are saved on the hub.',
        why: 'Browser choices affect only you. The inline views default changes what everyone on this hub sees.',
        how: 'Change browser settings freely. Changing your own inline views needs a workspace sign-in, and the hub default needs an administrator.',
      },
      {
        target: T('settings-theme'),
        title: 'Theme',
        what: 'Light, dark, or follow your device.',
        why: 'System keeps the page readable day and night without you changing it.',
        how: 'Press [ anywhere to switch the sidebar between full and icons only.',
      },
      {
        target: T('settings-walkthrough'),
        title: 'Feature walkthrough',
        what: 'A switch and a Run now button for the welcome tour and for each page tour.',
        why: 'Switch a tour on to see it again the next time you open that page.',
        how: 'Run now plays it straight away. Turn tours off here once you know the pages.',
        empty: 'Tours are not available in this build.',
      },
      {
        target: T('settings-local'),
        title: 'This computer',
        what: 'The address of agentscm serve on your machine, with a status pill next to it.',
        why: 'Templates and Timeline read from there. Without it they show sample data.',
        how: 'Leave it empty to use this page’s own address. Save and check tells you whether it answered.',
      },
      {
        target: T('settings-token'),
        title: 'Your token',
        what: 'Who you are signed in as, the repository, and when the token expires. Forget this token removes it from this browser.',
        why: 'The token decides which repository you see. It is read-only here.',
        how: 'If pages say you lack access, check the repository and expiry first.',
        tip: (c) => (c.data?.whoami ? `Your token ${c.data.whoami.token.name} is for ${c.data.whoami.token.repository}.` : null),
        empty: 'Your token details appear as soon as the hub answers.',
      },
    ],
  },
}

export function tourFor(key: TourKey): TourDef | null {
  return key === 'welcome' ? WELCOME : (SECTION_TOURS[key] ?? null)
}
