// One wording per caveat the UI repeats, so each page says it once and every page says it the same way.
// No DOM, no React.
//
// API for pages:
//   FINISHED_MEANS_AGENT_SAID  what "finished" does and does not establish
//   CONTESTED_FILE             what the hub did when two agents wanted one file
//   READING_CHANGES_NOTHING    opening or refreshing a page acknowledges nothing
//   CHECKPOINT_NOT_PROOF       a checkpoint or report is the agent's word, not a check
//   OVERLAP_IS_SIGNAL          an overlap is a reason to talk, not a conflict
//   STILL_ASSIGNED             a quiet task is still assigned
//   SEARCH_THIS_PAGE_ONLY      search covers only what is loaded
//   OPEN_FROM_THIS_COMPUTER    how to reach a page that only works on this computer
// Use the constant verbatim; do not reword it. Put it in a tooltip or one line, once per page.

export const FINISHED_MEANS_AGENT_SAID = 'Finished means the agent said so, not that it was checked, reviewed or merged.'
export const CONTESTED_FILE = 'The hub warned the second agent once; a repeat edit goes ahead by design. Files are never locked.'
export const READING_CHANGES_NOTHING = 'Reading changes nothing. Changes you make here are labelled.'
export const CHECKPOINT_NOT_PROOF = 'A checkpoint is the agent’s own report, not proof of review, verification or merge.'
export const OVERLAP_IS_SIGNAL = 'An overlap is a reason to talk, not proof the code conflicts.'
export const STILL_ASSIGNED = 'A quiet task is still assigned; coordinate with its lead before taking it over.'
export const SEARCH_THIS_PAGE_ONLY = 'Search covers only what is loaded on this page.'
export const OPEN_FROM_THIS_COMPUTER = 'Open it from the AgentSCM menu bar app (Open Hub), or run agentscm open in your repository.'
