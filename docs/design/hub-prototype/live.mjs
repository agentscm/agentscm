// Random but valid live events for the preview. Reads the Hub state built by the page's own reducer,
// so every event points at a task, pull request, agent or chat that exists at that moment.
const pick = list => list[Math.floor(Math.random() * list.length)];
const rev = () => Math.random().toString(16).slice(2, 9);
const NOTES = ['Tests written for the edge cases.', 'Split the change into two smaller commits.', 'Refactored the parser. Checks next.', 'Waiting on review comments.', 'Fixed the rounding test.', 'Docs updated to match.', 'Found one more case; handling it now.'];
const SAYS = ['Done with the first half. Starting on the tests.', 'That test was flaky; I quarantined it and opened a task.', 'I’ll keep my changes inside the files I declared.', 'Pushed a fix. Waiting for checks.', 'Review comments addressed.', 'I’d rather not touch that file; another agent planned it.'];
const MSGS = ['Tidy the error messages', 'Handle the empty case', 'Add a regression test', 'Rename for clarity', 'Fix the off-by-one in paging'];
export let pending = [];
let lastHeadsUp = 0;
export const resetLive = () => { pending = []; lastHeadsUp = 0; };
// One new event: weighted toward everyday check-ins and chat, with an occasional heads-up that resolves itself.
export function invent(hub, t, nowMs = Date.now()) {
  const S = hub.S;
  const active = hub.works().filter(w => w.state === 'active');
  const openPrs = Object.values(S.prs).filter(p => !p.merged && S.work[p.work]?.state !== 'finished');
  const convs = Object.values(S.convs).filter(c => S.chats[c.id]?.length && hub.live(S.agents[c.agent]));
  // At most one generated heads-up open at a time, and none within two minutes of the last.
  const openHeadsUp = Object.values(S.overlaps).some(o => o.state === 'open') || pending.length || nowMs - lastHeadsUp < 120000;
  const options = [];
  if (active.length) options.push([4, () => { const w = pick(active); const note = pick(NOTES); return { t, kind: 'work.checkin', id: w.id, agent: w.lead, note, say: `${hub.agentName(w.lead)} checked in on “${w.title}”: ${note}`, live: true }; }]);
  if (convs.length) options.push([3, () => { const c = pick(convs); return { t, kind: 'chat', say: '', conv: c.id, role: 'agent', text: pick(SAYS), live: true }; }]);
  if (openPrs.length) {
    options.push([2, () => { const p = pick(openPrs); const ok = Math.random() < 0.7; return { t, kind: 'ci', num: p.num, rev: p.head, status: ok ? 'pass' : 'fail', ...(ok ? {} : { test: 'billing::totals_round' }), say: `Checks ${ok ? 'passed' : 'failed'} on #${p.num}.`, live: true }; }]);
    options.push([1, () => { const p = pick(openPrs); const msg = pick(MSGS); return { t, kind: 'pr.commit', num: p.num, rev: rev(), type: 'fix', msg, say: `${hub.agentName(S.work[p.work].lead)} pushed “${msg}” to #${p.num}.`, live: true }; }]);
  }
  if (active.length > 1 && !openHeadsUp) options.push([1, () => {
    const [a, b] = [...active].sort(() => Math.random() - 0.5);
    const path = pick(b.scope).replace(/\/\*\*$/, `/${pick(['helpers.rs', 'fixtures.rs', 'mod.rs', 'config.rs', 'rounding.rs'])}`);
    lastHeadsUp = nowMs;
    const id = `O-L${Math.floor(t)}`;
    pending = [
      { at: nowMs + 25000, ev: t2 => ({ t: t2, kind: 'overlap.respond', id, response: { by: a.lead, kind: pick(['Scope change', 'Join']), note: `Leave ${path} to ${hub.agentName(b.lead)}.` }, say: `${hub.agentName(a.lead)} answered the heads-up on ${path}.`, live: true }) },
      { at: nowMs + 31000, ev: t2 => ({ t: t2, kind: 'overlap.resolve', id, say: `Settled: each agent has its own files again.`, live: true }) },
    ];
    return { t, kind: 'overlap.detect', overlap: { id, a: a.id, b: b.id, paths: [path] }, say: `Heads-up: ${hub.agentName(a.lead)} and ${hub.agentName(b.lead)} both plan to edit ${path}.`, live: true };
  }]);
  if (!options.length) return null;
  let n = Math.random() * options.reduce((x, [w]) => x + w, 0);
  for (const [w, make] of options) if ((n -= w) <= 0) return make();
  return options[0][1]();
}
// Follow-ups that are due (a heads-up's answer and resolution), oldest first.
export function due(t, nowMs = Date.now()) { const out = []; while (pending.length && pending[0].at <= nowMs) out.push(pending.shift().ev(t)); return out; }
