// Shared store, reducer and building blocks for the three Hub story options.
// The server streams synthetic events; everything on screen is derived from them here.

export const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const h = esc;

// ---------- state ----------
export let S = null;

export function init(meta) {
  S = {
    meta, t: 0, playing: false, speed: 1, end: meta.end,
    people: Object.fromEntries(meta.people.map(p => [p.id, p])),
    you: meta.you || meta.people[0].id,
    agents: {}, work: {}, prs: {}, overlaps: {}, requests: {}, loops: {}, convs: {},
    follow: {}, chats: {}, races: [], skillRuns: [], passes: [], searches: [], usage: null,
    hub: { up: true, since: 0 }, log: [], coverage: { forge_pull: { state: 'complete', at: 0 } },
  };
}

const uniqPush = (arr, v) => { for (const x of [].concat(v)) if (!arr.includes(x)) arr.push(x); };

export function apply(e) {
  const w = e.id && S.work[e.id];
  switch (e.kind) {
    case 'hub.up': S.hub = { up: true, since: e.t }; break;
    case 'hub.down': S.hub = { up: false, since: e.t }; break;
    case 'session.start': {
      const a = { ...e.agent, start: e.t, end: e.agent.ended ? e.t + 50 : null, lastSeen: e.t, stale: false };
      S.agents[a.id] = a;
      if (a.parent) S.passes.push({ t: e.t, kind: 'subagent', from: a.parent, to: a.id, note: `started by ${a.evidence}` });
      break;
    }
    case 'session.end': for (const id of [].concat(e.agent)) if (S.agents[id]) S.agents[id].end = e.t; break;
    case 'session.stale': S.agents[e.agent].stale = true; break;
    case 'work.start':
      S.work[e.work.id] = { ...e.work, state: 'active', participants: [e.work.lead], started: e.t, updated: e.t, checkins: [], gen: 1 };
      break;
    case 'work.update':
      if (e.patch?.waitsFor) w.waitStart = e.t;
      if (e.patch && e.patch.waitsFor === null && w.waitStart != null) { (w.waits ||= []).push({ from: w.waitStart, to: e.t, on: w.waitsFor }); w.waitStart = null; }
      Object.assign(w, e.patch || {});
      for (const [k, v] of Object.entries(e.add || {})) uniqPush(w[k], v);
      w.updated = e.t;
      break;
    case 'work.checkin': w.checkins.push({ t: e.t, agent: e.agent, note: e.note }); w.updated = e.t; S.agents[e.agent].lastSeen = e.t; break;
    case 'overlap.detect': S.overlaps[e.overlap.id] = { ...e.overlap, t: e.t, state: 'open', response: null }; break;
    case 'overlap.respond': {
      const o = S.overlaps[e.id];
      o.response = { ...e.response, t: e.t };
      S.passes.push({ t: e.t, kind: 'scope', from: e.response.by, to: null, note: `${e.response.kind}: ${e.response.note}` });
      break;
    }
    case 'overlap.resolve': Object.assign(S.overlaps[e.id], { state: 'resolved', resolvedAt: e.t }); break;
    case 'request.send': S.requests[e.request.id] = { ...e.request, t: e.t, receipt: null, reply: null }; break;
    case 'request.receipt': S.requests[e.id].receipt = e.t; break;
    case 'request.reply': S.requests[e.id].reply = { t: e.t, text: e.reply }; break;
    case 'handover.offer':
      Object.assign(w, { state: 'handover', handover: { from: e.from, note: e.note, t: e.t }, next: 'Needs a new owner' });
      break;
    case 'handover.accept': {
      const from = w.handover.from;
      Object.assign(w, { lead: e.to, person: e.person, state: 'active', next: 'Finish JPY rounding' });
      w.handover.acceptedAt = e.t; w.handover.to = e.to; w.gen += 1;
      uniqPush(w.participants, e.to);
      S.passes.push({ t: e.t, kind: 'handover', from, to: e.to, note: `waited ${dur(e.t - w.handover.t)}`, work: w.id });
      break;
    }
    case 'pr.open': S.prs[e.pr.num] = { ...e.pr, t: e.t, commits: [{ t: e.t, rev: e.pr.head, type: 'feat', msg: 'Initial change' }], reviews: [], ci: [], merged: null }; S.work[e.pr.work].pr = e.pr.num; break;
    case 'pr.commit': { const p = S.prs[e.num]; p.commits.push({ t: e.t, rev: e.rev, type: e.type, msg: e.msg }); p.head = e.rev; break; }
    case 'pr.review': S.prs[e.num].reviews.push({ t: e.t, by: e.by, high: e.high, med: e.med, human: !!e.human }); break;
    case 'ci': { const p = S.prs[e.num]; p.ci.push({ t: e.t, rev: e.rev, status: e.status, test: e.test, attempt: p.ci.length + 1 }); break; }
    case 'pr.merge': S.prs[e.num].merged = { rev: e.rev, by: e.by, t: e.t }; break;
    case 'loop.detect': S.loops[e.loop.id] = { ...e.loop, t: e.t, response: null, outcome: null }; break;
    case 'loop.respond': S.loops[e.id].response = { ...e.response, t: e.t }; break;
    case 'loop.outcome': S.loops[e.id].outcome = { text: e.outcome, t: e.t }; break;
    case 'stage': {
      // Gate stages are derived from PR data; any other key is a repo-defined workflow step after merge.
      if (['build', 'review', 'checks', 'merge'].includes(e.key)) break;
      const b = S.follow[e.work] || (S.follow[e.work] = { skill: e.key });
      Object.assign(b, { status: e.status, t: e.t });
      if (e.status === 'ready') b.readyAt = e.t;
      if (e.claim) { b.claim = e.claim; b.claimedAt = e.t; S.passes.push({ t: e.t, kind: 'claim', from: null, to: e.claim, note: `claimed ${skillName(e.key)}`, work: e.work }); }
      if (e.artifact) b.artifact = e.artifact;
      break;
    }
    case 'stage.race': S.races.push({ t: e.t, work: e.work, key: e.key, loser: e.loser }); break;
    case 'skill.run': S.skillRuns.push({ t: e.t, skill: e.skill, agent: e.agent, work: e.work, result: e.result }); break;
    case 'conv.capture': S.convs[e.conv.id] = { ...e.conv, t: e.t }; break;
    case 'chat': (S.chats[e.conv] ||= []).push(e); break;
    case 'history.search': S.searches.push({ t: e.t, query: e.query }); break;
    case 'usage': S.usage = { t: e.t, list: e.usage }; break;
    case 'coverage': S.coverage[e.source] = { state: e.state, unread: e.unread || 0, deferred: e.deferred || 0, at: e.t }; break;
  }
  if (e.agent && typeof e.agent === 'string' && S.agents[e.agent] && e.kind !== 'session.end') S.agents[e.agent].lastSeen = e.t;
  if (e.say) S.log.push(e);
}

// ---------- time ----------
const base = () => Date.parse(S.meta.start);
export const clock = (t, utc = S.meta.zone.utc) => new Date(base() + (t + utc * 60) * 60000).toISOString().slice(11, 16);
const dayIndex = (t, utc = S.meta.zone.utc) => Math.floor((base() + (t + utc * 60) * 60000) / 86400000);
export function dayLabel(t) {
  const d = dayIndex(S.t) - dayIndex(t);
  if (d <= 0) return 'Today';
  if (d === 1) return 'Yesterday';
  return new Date(base() + (t + S.meta.zone.utc * 60) * 60000).toLocaleDateString('en', { weekday: 'long', timeZone: 'UTC' });
}
export const weekday = t => new Date(base() + (t + S.meta.zone.utc * 60) * 60000).toLocaleDateString('en', { weekday: 'short', timeZone: 'UTC' });
export function dur(m) {
  m = Math.max(0, Math.round(m));
  if (m < 60) return `${m} min`;
  const hrs = Math.floor(m / 60), mm = m % 60;
  return mm ? `${hrs} h ${mm} min` : `${hrs} h`;
}
export function ago(t) {
  const d = Math.floor(S.t - t);
  if (d < 1) return 'just now';
  if (d < 60) return `${d} min ago`;
  if (d < 1440) return `${Math.floor(d / 60)} h ago`;
  return dayLabel(t).toLowerCase();
}
export const when = t => `<time title="${h(weekday(t))} ${clock(t)} ${h(S.meta.zone.label)}">${h(ago(t))}</time>`;

// ---------- derived ----------
export const agents = () => Object.values(S.agents);
export const live = a => a && !a.end;
export const works = () => Object.values(S.work);
const stateOrder = { blocked: 0, handover: 1, active: 2, paused: 3, finished: 4 };
export const workList = () => works().sort((a, b) => stateOrder[a.state] - stateOrder[b.state] || b.updated - a.updated);
export const prOf = w => w && w.pr && S.prs[w.pr];
export const team = () => S.meta.people.length > 1;

export function agentName(id, { short = false } = {}) {
  const a = S.agents[id];
  if (!a) return id;
  if (!team()) return a.role === 'subagent' ? `${a.name}` : a.name;
  const p = S.people[a.person];
  return short ? p.short : `${p.short}’s ${a.role === 'subagent' ? a.name : a.name}`;
}
export const personName = id => S.people[id]?.short || id;

export function agentStatus(a) {
  if (!live(a)) return { tone: 'muted', text: 'Ended' };
  if (a.stale) return { tone: 'warn', text: 'No check-in for a while · still assigned' };
  return { tone: 'ok', text: 'Working' };
}

export function scopeHits(scope, path) {
  return scope.some(s => {
    const base = s.replace(/\/\*\*$/, '');
    return path.startsWith(base) || base.startsWith(path);
  });
}

export const STAGES = [
  { key: 'build', label: 'Build', plain: 'Agent writes the change' },
  { key: 'review', label: 'Review', plain: 'Someone reads it' },
  { key: 'checks', label: 'Checks', plain: 'Tests run on the exact commit' },
  { key: 'merge', label: 'Merge', plain: 'GitHub merges it' },
  { key: 'live', label: 'Live', plain: 'Users have it' },
];

export function stages(w) {
  const p = prOf(w);
  const out = {};
  out.build = p ? { s: 'done', note: `${p.commits.length} commit${p.commits.length > 1 ? 's' : ''}` }
    : w.state === 'paused' ? { s: 'paused', note: 'Paused' } : w.state === 'handover' ? { s: 'wait', note: 'Needs owner' } : { s: 'active', note: 'In progress' };
  if (!p) out.review = { s: 'idle' };
  else {
    const r = p.reviews, lastR = r[r.length - 1];
    if (lastR && lastR.high + lastR.med === 0) out.review = { s: 'done', n: r.length, note: lastR.human ? `Approved by ${personName(lastR.by)}` : 'No problems found' };
    else out.review = { s: r.length ? 'retry' : 'active', n: r.length, note: r.length ? `${r.length} round${r.length > 1 ? 's' : ''}` : 'Waiting' };
  }
  if (!p) out.checks = { s: 'idle' };
  else {
    const c = p.ci, lastC = c[c.length - 1];
    if (!lastC) out.checks = { s: 'wait', note: 'Not run yet' };
    else if (lastC.status === 'pass') out.checks = { s: lastC.rev === p.head ? 'done' : 'stale', n: c.length, note: lastC.rev === p.head ? `Passed on ${lastC.rev.slice(0, 7)}` : 'Passed on an older commit' };
    else if (lastC.status === 'running') out.checks = { s: 'active', n: c.length, note: 'Running' };
    else out.checks = { s: 'fail', n: c.length, note: `Failed ${c.filter(x => x.status === 'fail').length}×` };
  }
  out.merge = p?.merged ? { s: 'done', note: `Merged ${p.merged.rev.slice(0, 7)}` } : { s: p ? 'wait' : 'idle', note: p ? 'Open' : '' };
  out.live = { s: 'unknown', note: 'Not tracked' };
  return out;
}
export function currentStage(w) {
  const st = stages(w);
  return STAGES.find(x => !['done', 'none', 'unknown'].includes(st[x.key].s)) || STAGES[STAGES.length - 1];
}
export const skillName = id => S.meta.skills.find(s => s.id === id)?.name || id;
export const workflowSkill = () => S.meta.skills.find(s => s.lifecycle);

// The repo's own after-merge workflow step for this task, if the repo defines one.
export function followUp(w) {
  const sk = w.label === 'feature' && workflowSkill();
  if (!sk) return null;
  const b = S.follow[w.id];
  const st = !b ? { s: 'idle', note: 'Starts after merge' }
    : b.status === 'ready' ? { s: 'ready', note: 'Waiting for a session to check in' }
    : b.status === 'claimed' ? { s: 'active', note: `Running in ${agentName(b.claim)}` }
    : { s: 'done', note: `Saved ${b.artifact}` };
  return { skill: sk, b, ...st };
}
export function flowChip(w, { compact = false } = {}) {
  const f = followUp(w);
  if (!f) return '';
  return `<div class="flow-after flow-${f.s}"><i class="bi bi-arrow-return-right" aria-hidden="true"></i><span>${compact ? 'Then' : 'After merge, the repo’s workflow runs'}</span>${mono(f.skill.name)}<span class="flow-state">${stageIcon(f.s)}${h(f.note)}</span>${compact ? '' : proposed()}</div>`;
}
export function loopOf(w) { return Object.values(S.loops).find(l => l.work === w.id); }

// Everything that needs a person, worded for a first-time reader.
export function attention() {
  const out = [];
  for (const o of Object.values(S.overlaps)) {
    if (o.state !== 'open') continue;
    const a = S.work[o.a], b = S.work[o.b];
    const rq = Object.values(S.requests).find(r => r.overlap === o.id);
    out.push({
      id: o.id, tone: 'warn', icon: 'bi-sign-intersection', t: o.t, paths: o.paths, works: [o.a, o.b],
      title: 'Two agents plan to edit the same file',
      plain: `${agentName(a.lead)} and ${agentName(b.lead)} both plan to change ${o.paths.join(', ')}. Nothing has broken. This is an early heads-up.`,
      status: rq ? (rq.reply ? 'Replied' : rq.receipt ? 'Request received, waiting for a reply' : 'Request sent, not received yet') : 'Waiting for one agent to change its plan',
      copy: `AgentSCM says ${agentName(b.lead)} is also planning to edit ${o.paths.join(', ')} for “${b.title}”. Narrow your scope, hand over, or ask to join before editing.`,
    });
  }
  for (const l of Object.values(S.loops)) {
    if (l.outcome) continue;
    out.push({
      id: l.id, tone: 'bad', icon: 'bi-arrow-repeat', t: l.t, works: [l.work], proposed: true, paths: [l.file],
      title: `PR #${l.num} is stuck in a loop`,
      plain: `It merged main ${l.merges}× and failed the same test ${l.fails}×. That test belongs to “${S.work[l.blockedBy].title}”, which is still open.`,
      status: l.response ? `Sam chose: ${l.response.note}` : `Suggestion: ${l.suggest}`,
      copy: `Stop merging main into #${l.num}. Wait for “${S.work[l.blockedBy].title}” to land, then merge main once and rerun checks.`,
    });
  }
  for (const w of works()) {
    if (w.state === 'handover') out.push({
      id: `ho-${w.id}`, tone: 'brand', icon: 'bi-box-arrow-in-right', t: w.handover.t, works: [w.id], paths: w.scope,
      title: `“${w.title}” needs a new owner`,
      plain: `${agentName(w.handover.from)} handed it over: “${w.handover.note}”`,
      status: 'Still assigned to the last owner until someone accepts it',
      copy: `Pick up AgentSCM work ${w.id} (“${w.title}”). Read the handover note first.`,
    });
    const f = followUp(w);
    if (f?.b?.status === 'ready') out.push({
      id: `fl-${w.id}`, tone: 'brand', icon: 'bi-arrow-return-right', t: f.b.readyAt, works: [w.id], proposed: true,
      title: `${f.skill.name} is ready for PR #${w.pr}`,
      plain: 'Your repo’s workflow runs this after a merge. It starts the next time an agent session checks in. Nothing wakes your agents, so this can wait.',
      status: agents().some(live) ? 'An agent is open. It will see this at its next step.' : 'No agent is open right now',
      copy: `${f.skill.name} #${w.pr}`,
    });
    const p = prOf(w);
    const lastC = p?.ci[p.ci.length - 1];
    if (p && !p.merged && lastC?.status === 'fail' && !loopOf(w)) out.push({
      id: `ci-${p.num}`, tone: 'bad', icon: 'bi-x-octagon', t: lastC.t, works: [w.id], paths: w.scope,
      title: `Checks failed on PR #${p.num}`,
      plain: `${lastC.test} failed on ${lastC.rev.slice(0, 7)}.`,
      status: `${agentName(w.lead)} owns the fix`,
      copy: `Checks failed on #${p.num} (${lastC.test}). Fix it and push.`,
    });
  }
  for (const a of agents()) {
    if (!a.stale || !live(a)) continue;
    const w = works().find(x => x.lead === a.id && x.state !== 'finished');
    if (w) out.push({
      id: `st-${a.id}`, tone: 'warn', icon: 'bi-hourglass-split', t: a.lastSeen, works: [w.id], paths: w.scope,
      title: `${agentName(a.id)} has gone quiet`,
      plain: `No check-in since ${clock(a.lastSeen)}. “${w.title}” is still assigned, so it isn’t free to take.`,
      status: `Ask ${personName(a.person)} before picking it up`,
      copy: `Ask ${personName(a.person)} whether “${w.title}” is still being worked on.`,
    });
  }
  return out.sort((a, b) => ({ bad: 0, warn: 1, brand: 2 }[a.tone] - { bad: 0, warn: 1, brand: 2 }[b.tone]) || b.t - a.t);
}

// Interventions that already have a response or outcome: the "it helped" record.
export function settled() {
  const out = [];
  for (const o of Object.values(S.overlaps)) if (o.response) out.push({ t: o.response.t, icon: 'bi-sign-intersection', tone: 'ok', title: `Heads-up on ${o.paths.join(', ')}`, chain: [['Spotted', o.t], [o.response.kind, o.response.t], [o.state === 'resolved' ? 'Settled' : 'Open', o.resolvedAt ?? null]], note: o.response.note, works: [o.a, o.b] });
  for (const l of Object.values(S.loops)) if (l.response) out.push({ t: l.response.t, icon: 'bi-arrow-repeat', tone: l.outcome ? 'ok' : 'warn', title: `Loop on PR #${l.num}`, chain: [['Spotted', l.t], [l.response.kind, l.response.t], [l.outcome ? l.outcome.text : 'Waiting', l.outcome?.t ?? null]], note: l.response.note, works: [l.work], proposed: true });
  for (const w of works()) if (w.handover?.acceptedAt) out.push({ t: w.handover.acceptedAt, icon: 'bi-box-arrow-in-right', tone: 'ok', title: `Handover of “${w.title}”`, chain: [['Offered', w.handover.t], ['Accepted', w.handover.acceptedAt]], note: `${agentName(w.handover.from)} → ${agentName(w.handover.to)}, waited ${dur(w.handover.acceptedAt - w.handover.t)}`, works: [w.id] });
  return out.sort((a, b) => b.t - a.t);
}

// ---------- small components ----------
const CLIENT = { claude: 'Claude Code', codex: 'Codex', cursor: 'Cursor' };
export const clientName = c => CLIENT[c] || c;

export function avatar(id, size = '') {
  const a = S.agents[id];
  if (!a) return '';
  const p = S.people[a.person];
  const txt = a.role === 'subagent' ? '<i class="bi bi-diagram-2" aria-hidden="true"></i>' : h(team() ? p.short.slice(0, 1) : { claude: 'Cl', codex: 'Cx', cursor: 'Cu' }[a.client]);
  return `<span class="av av-${h(a.client)} ${a.role === 'subagent' ? 'av-sub' : ''} ${live(a) ? (a.stale ? 'is-stale' : 'is-live') : 'is-off'} ${size}" title="${h(agentName(id))} · ${h(clientName(a.client))}" aria-hidden="true">${txt}</span>`;
}
export const agentChip = id => `<span class="agent-chip">${avatar(id)}<span>${h(agentName(id))}</span></span>`;
export const pill = (tone, text, icon) => `<span class="pill pill-${tone}">${icon ? `<i class="bi ${icon}" aria-hidden="true"></i>` : ''}${h(text)}</span>`;
export const proposed = () => `<span class="pill pill-proposed" title="Not on main yet. Shown so you can judge the idea.">Proposed</span>`;
export const inReview = () => `<span class="pill pill-proposed pill-review" title="Built on the feat/durable-work-catchup branch, not merged yet">In review</span>`;
export const mono = s => `<code class="mono">${h(s)}</code>`;
export const taskHref = id => `#/task/${encodeURIComponent(id)}`;
export const workLink = id => S.work[id] ? `<a href="${taskHref(id)}" class="work-link" data-work="${h(id)}">${h(S.work[id].title)}</a>` : '';
export const copyBtn = (text, label = 'Copy for your agent') => `<button type="button" class="btn btn-sm btn-outline-brand copy" data-copy="${h(text)}"><i class="bi bi-clipboard" aria-hidden="true"></i><span>${h(label)}</span></button>`;

const STATE = {
  active: ['brand', 'Working', 'bi-play-circle'], paused: ['warn', 'Paused', 'bi-pause-circle'], blocked: ['bad', 'Blocked', 'bi-slash-circle'],
  handover: ['brand', 'Needs a new owner', 'bi-box-arrow-in-right'], finished: ['ok', 'Agent says done', 'bi-check2-circle'],
};
export const stateText = s => STATE[s][1];
export const statePill = s => pill(STATE[s][0], STATE[s][1], STATE[s][2]);

const STAGE_ICON = { done: 'bi-check-circle-fill', active: 'bi-circle-half', retry: 'bi-arrow-repeat', fail: 'bi-x-circle-fill', ready: 'bi-stars', wait: 'bi-circle', idle: 'bi-circle', paused: 'bi-pause-circle', stale: 'bi-exclamation-circle', none: 'bi-dash-circle', unknown: 'bi-question-circle' };
export const stageIcon = s => `<i class="bi ${STAGE_ICON[s] || 'bi-circle'}" aria-hidden="true"></i>`;

// The dashboard splits by question: what's happening now, where people are working, what did I miss, who's around.
// Each tab carries its live status so you know whether it's worth opening; each has its own link.
export const homeTab = ui => (['now', 'where', 'catchup', 'team'].includes(ui.tab) ? ui.tab : 'now');
export function homeTabs(ui) {
  const tab = homeTab(ui);
  const att = attention().length;
  const since = lastReviewed();
  const fresh = sinceItems(since).length;
  const crossings = Object.values(S.overlaps).filter(o => o.state === 'open').length;
  const open = team() ? S.meta.people.filter(p => isAwake(p)).length : agents().filter(a => a.role !== 'subagent' && live(a)).length;
  const tabs = [
    ['now', 'Now', att ? `${att} need${att === 1 ? 's' : ''} you` : 'Nothing needs you', att ? 'warn' : ''],
    ['where', 'Where', crossings ? `${crossings} crossing${crossings === 1 ? '' : 's'}` : 'No crossings', crossings ? 'warn' : ''],
    ['catchup', 'Catch up', fresh ? `${fresh} new since ${since ? clock(since) : 'the start'}` : 'You’re up to date', fresh ? 'new' : ''],
    ['team', team() ? 'Team' : 'Crew', team() ? `${open} of ${S.meta.people.length} awake` : `${open} agent${open === 1 ? '' : 's'} open`, ''],
  ];
  return `<nav class="htabs" aria-label="Parts of this view">${tabs.map(([k, label, sub, tone]) => `<a href="#/home/${k}" class="htab ${tab === k ? 'on' : ''} ${tone ? `tone-${tone}` : ''}" ${tab === k ? 'aria-current="page"' : ''}><b>${label}</b><span>${tone ? '<i aria-hidden="true"></i>' : ''}${h(sub)}</span></a>`).join('')}</nav>`;
}

// Sub-tabs for a page: [key, label, status, tone]. The first one is the default.
export function pageTabs(base, tabs, cur) {
  const tab = tabs.some(t => t[0] === cur) ? cur : tabs[0][0];
  return `<nav class="htabs" style="--n:${tabs.length}" aria-label="Parts of this page">${tabs.map(([k, label, sub, tone]) => `<a href="#/${base}/${k}" class="htab ${tab === k ? 'on' : ''} ${tone ? `tone-${tone}` : ''}" ${tab === k ? 'aria-current="page"' : ''}><b>${h(label)}</b><span>${tone ? '<i aria-hidden="true"></i>' : ''}${h(sub)}</span></a>`).join('')}</nav>`;
}

export function masthead(icon, q, answer, extra = '') {
  return `<header class="masthead"><h1 class="q"><i class="bi ${icon}" aria-hidden="true"></i>${h(q)}</h1><p class="answer" aria-live="polite">${answer}</p>${extra}</header>`;
}
export function box({ title, icon, tone = 'brand', count, explain, body, cls = '', id = '' }) {
  return `<section class="box tone-${tone} ${cls}" ${id ? `id="${h(id)}"` : ''}><div class="box-head"><i class="bi ${icon}" aria-hidden="true"></i><h2>${h(title)}</h2>${count != null ? `<span class="count">${h(count)}</span>` : ''}${explain ? `<p class="explain">${h(explain)}</p>` : ''}</div><div class="box-body">${body}</div></section>`;
}

// Lane bar: a task's five stages as one segmented capsule. Finished stages fill, the current one widens and
// says where the task is in words with its holder's face, and stages not reached yet stay quiet.
// mode 'fit' weights the current stage; 'grid' keeps equal columns so rows line up under one header (the track).
export function laneBar(w, { mode = 'fit', link = false } = {}) {
  const st = stages(w);
  const cur = currentStage(w);
  const loop = loopOf(w);
  const holder = w.state === 'handover' ? null : cur.key === 'merge' ? 'forge' : w.lead;
  const isHere = x => x.key === cur.key && !['done', 'none', 'unknown'].includes(st[x.key].s);
  const fit = STAGES.map(x => (isHere(x) ? 'minmax(0, 3fr)' : 'minmax(0, 1fr)')).join(' ');
  const segs = STAGES.map(x => {
    const v = st[x.key];
    const here = isHere(x);
    const note = x.key === 'checks' && loop && !loop.outcome ? `Going in circles${v.note ? ` · ${v.note}` : ''}` : v.note || '';
    const mark = !here ? stageIcon(v.s) : holder === 'forge' ? `<span class="baton baton-forge" title="GitHub decides"><i class="bi bi-github" aria-hidden="true"></i></span>` : holder ? `<span class="baton">${avatar(holder)}</span>` : `<span class="baton baton-empty" title="Nobody holds it"><i class="bi bi-question-lg" aria-hidden="true"></i></span>`;
    const inner = `${mark}<span class="lb-label" aria-hidden="true">${h(x.label)}</span>${note ? `<span class="lb-note" aria-hidden="true">${h(note)}</span>` : ''}${v.n > 1 ? `<b class="lb-laps" aria-hidden="true">${v.n}×</b>` : ''}`;
    const said = `${x.label}: ${note || { done: 'done', idle: 'not yet', wait: 'not yet', none: 'not tracked', unknown: 'unknown' }[v.s] || v.s}${here ? ' (now)' : ''}`;
    return `<li data-k="lb-${h(w.id)}-${x.key}-${v.s}" class="lb-seg lb-${v.s} ${here ? 'is-here' : ''}" title="${h(x.plain)}: ${h(note || v.s)}">${link ? `<button type="button" class="lb-hit" data-work="${h(w.id)}" data-focus="stage" aria-label="${h(w.title)}, ${h(said)}">${inner}</button>` : `<span class="lb-hit">${inner}<span class="visually-hidden">${h(said)}</span></span>`}</li>`;
  }).join('');
  return `<div class="lb-wrap"><ol class="lb lb-${mode}" style="--lb-fit:${fit}" aria-label="Stages of ${h(w.title)}">${segs}</ol></div>`;
}

export function crew({ group = true } = {}) {
  const roots = agents().filter(a => a.role !== 'subagent' && (live(a) || a.end > S.t - 120));
  if (!roots.length) return `<p class="empty-line">No agents are open. When one starts, it shows up here by itself.</p>`;
  const row = a => {
    const st = agentStatus(a);
    const w = works().find(x => x.state !== 'finished' && (x.lead === a.id || x.participants.includes(a.id)));
    const kids = agents().filter(k => k.parent === a.id);
    return `<li data-k="ag-${h(a.id)}" class="crew-row ${live(a) ? '' : 'ended'}">${avatar(a.id)}<div class="crew-main"><div class="crew-name">${h(agentName(a.id))}${agentName(a.id).endsWith(clientName(a.client)) ? '' : ` <span class="crew-client">${h(clientName(a.client))}</span>`}</div><div class="crew-doing">${w ? workLink(w.id) : `<span class="muted">${live(a) ? 'No task yet' : `Ended ${ago(a.end)}`}</span>`}</div></div>${pill(st.tone, st.text)}${kids.length ? `<ul class="crew-kids" aria-label="Subagents of ${h(agentName(a.id))}">${kids.map(k => `<li data-k="ag-${h(k.id)}" class="crew-row sub ${live(k) ? '' : 'ended'}">${avatar(k.id)}<div class="crew-main"><div class="crew-name">${h(k.name)}</div><div class="crew-ev">Subagent · linked by ${h(k.evidence)}</div></div>${pill(agentStatus(k).tone, live(k) ? 'Working' : 'Stopped')}</li>`).join('')}</ul>` : ''}</li>`;
  };
  if (!group || !team()) return `<ul class="crew">${roots.map(row).join('')}</ul>`;
  return S.meta.people.map(p => {
    const mine = roots.filter(a => a.person === p.id);
    return `<div class="crew-person"><div class="crew-person-head">${h(p.short)} <span>${h(p.city)} · ${clock(S.t, p.utc)}</span></div>${mine.length ? `<ul class="crew">${mine.map(row).join('')}</ul>` : `<p class="empty-line">${isAwake(p) ? 'No agent open' : 'Off for the day'}</p>`}</div>`;
  }).join('');
}
export function isAwake(p, t = S.t) {
  const hr = ((base() / 60000 + t + p.utc * 60) / 60) % 24;
  return hr >= p.hours[0] && hr < p.hours[1];
}

export function attentionList(items, { limit = 99, compact = false } = {}) {
  if (!items.length) return `<div class="all-clear"><i class="bi bi-check2-circle" aria-hidden="true"></i><div><strong>Nothing needs you.</strong><span>Agents are working on separate files. You’ll see a heads-up here before two agents edit the same file.</span></div></div>`;
  return `<ul class="attn ${compact ? 'attn-compact' : ''}">${items.slice(0, limit).map(a => `<li data-k="at-${h(a.id)}" class="attn-item tone-${a.tone}"><i class="bi ${a.icon} attn-icon" aria-hidden="true"></i><div class="attn-main"><div class="attn-title"><a href="${taskHref(a.works[0])}" data-work="${h(a.works[0])}">${h(a.title)}</a>${a.proposed ? proposed() : ''}</div>${compact ? '' : `<p>${h(a.plain)}</p>`}<div class="attn-status">${h(a.status)} · ${when(a.t)}</div></div>${compact ? '' : copyBtn(a.copy)}</li>`).join('')}</ul>`;
}

export function settledList(items, limit = 4) {
  if (!items.length) return `<p class="empty-line">When a heads-up gets a response, the whole chain shows here: what was spotted, what someone did, and how it ended.</p>`;
  return `<ul class="settled">${items.slice(0, limit).map(s => { const done = s.chain.filter(([, t]) => t != null); const took = done.length > 1 ? done[done.length - 1][1] - done[0][1] : null; return `<li data-k="se-${h(s.title)}">
    <i class="bi ${s.icon} settled-icon" aria-hidden="true"></i>
    <div class="settled-main"><div class="settled-title">${h(s.title)}${s.proposed ? proposed() : ''}</div><p>${h(s.note)}</p></div>
    <div class="settled-side"><ol class="steps">${s.chain.map(([l, t]) => `<li class="${t == null ? 'pending' : ''}"><b>${h(l)}</b><small>${t != null ? clock(t) : 'not yet'}</small></li>`).join('')}</ol>${took != null ? `<span class="settled-took">${took < 1 ? 'Under a minute' : `Took ${dur(took)}`}</span>` : ''}</div>
  </li>`; }).join('')}</ul>`;
}

export const taskOf = e => (e.id && S.work[e.id] ? e.id : e.work?.id || e.work || (e.num && S.prs[e.num]?.work) || e.loop?.work);

export function storyLine(e, { withWork = true } = {}) {
  const icon = EVENT_ICON[e.kind] || 'bi-dot';
  const tone = eventTone(e);
  const wid = taskOf(e);
  return `<li data-k="ev-${e.t}-${h(e.kind)}" class="story-line ${tone ? 'tone-' + tone : ''}"><i class="bi ${icon}" aria-hidden="true"></i><span class="story-say">${h(e.say)}${withWork && wid && typeof wid === 'string' && S.work[wid] ? ` <span class="story-work">${workLink(wid)}</span>` : ''}</span><time>${clock(e.t)}</time></li>`;
}

// ---------- task page ----------
// Does this story event belong to task `id`? Overlaps and requests count for both sides.
export function eventTouches(e, id) {
  const ov = e.overlap || S.overlaps[e.id] || (S.requests[e.id || e.request?.id] && S.overlaps[S.requests[e.id || e.request?.id].overlap]);
  if (ov && (ov.a === id || ov.b === id)) return true;
  if (e.conv?.work === id) return true;
  return taskOf(e) === id;
}

const EVENT_ICON = {
  'session.start': 'bi-box-arrow-in-right', 'session.end': 'bi-box-arrow-left', 'work.start': 'bi-flag', 'work.update': 'bi-pencil', 'work.checkin': 'bi-chat-left-text',
  'overlap.detect': 'bi-sign-intersection', 'overlap.respond': 'bi-reply', 'overlap.resolve': 'bi-check2', 'pr.open': 'bi-git', 'pr.commit': 'bi-dot', 'pr.review': 'bi-eyeglasses',
  ci: 'bi-activity', 'pr.merge': 'bi-sign-merge-left', 'loop.detect': 'bi-arrow-repeat', 'loop.respond': 'bi-signpost-split', 'loop.outcome': 'bi-check2-all', stage: 'bi-arrow-return-right', 'stage.race': 'bi-lock',
  'skill.run': 'bi-lightning', 'conv.capture': 'bi-journal-text', 'hub.down': 'bi-wifi-off', 'hub.up': 'bi-wifi', 'request.send': 'bi-send', 'request.receipt': 'bi-envelope-open', 'request.reply': 'bi-reply',
  'handover.offer': 'bi-box-arrow-right', 'handover.accept': 'bi-box-arrow-in-right', 'history.search': 'bi-search', usage: 'bi-speedometer', 'session.stale': 'bi-hourglass-split',
};
const eventTone = e => (e.kind === 'ci' ? (e.status === 'pass' ? 'ok' : e.status === 'fail' ? 'bad' : '') : ['overlap.detect', 'session.stale'].includes(e.kind) ? 'warn'
  : ['loop.detect', 'hub.down'].includes(e.kind) ? 'bad' : ['pr.merge', 'overlap.resolve', 'loop.outcome'].includes(e.kind) || (e.kind === 'stage' && e.status === 'done') ? 'ok' : '');
const CODE = new Set(['pr.open', 'pr.commit', 'pr.review', 'ci', 'pr.merge']);
const MILESTONE = new Set(['work.start', 'pr.open', 'pr.merge', 'loop.detect', 'overlap.detect', 'handover.offer', 'handover.accept']);

export const canRead = c => S.agents[c.agent]?.person === S.you && c.access !== 'none';
// Chat text is escaped first; redaction markers become visible "hidden" chips, search words get highlighted.
export function chatText(t, q = '') {
  let s = h(t);
  for (const w of q.trim().toLowerCase().split(/\s+/).filter(x => x.length > 1)) s = s.replace(new RegExp(`(${w.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')})`, 'gi'), '<mark>$1</mark>');
  return s.replace(/\[REDACTED:([a-z_]+)\]/g, (_, k) => `<span class="redacted" title="Removed on this computer before saving">hidden ${k.replace(/_/g, ' ')}</span>`);
}
const laneClass = id => (id === 'hub' ? 'lc-hub' : `lc-${S.agents[id].client}${S.agents[id].role === 'subagent' ? ' lc-sub' : ''}`);

// Which rail(s) a thread item sits on: an agent, or the shared Hub & GitHub rail.
function laneOf(it) {
  if (it.kind === 'chat') return [it.m.role === 'sub' ? it.m.agent : it.conv.agent];
  if (it.kind === 'note') return [it.agent];
  const e = it.e;
  const lead = n => S.work[S.prs[n].work].lead;
  const by = {
    'session.start': () => e.agent.id, 'session.end': () => e.agent, 'session.stale': () => e.agent, 'work.checkin': () => e.agent, 'skill.run': () => e.agent,
    'work.start': () => e.work.lead, 'overlap.respond': () => e.response?.by, 'handover.offer': () => e.from, 'handover.accept': () => e.to,
    'pr.commit': () => lead(e.num), 'pr.review': () => e.by, stage: () => e.claim, 'stage.race': () => e.loser,
    'request.send': () => e.request.from, 'request.receipt': () => S.requests[e.id].to, 'request.reply': () => S.requests[e.id].to,
  }[e.kind];
  const ids = [].concat(by ? by() : []).filter(id => S.agents[id]);
  return ids.length ? ids : ['hub'];
}

function threadOf(w, filter) {
  const crew = new Set(w.participants);
  for (const a of agents()) if (a.parent && crew.has(a.parent)) crew.add(a.id);
  const items = [];
  for (const e of S.log) {
    if (e.kind === 'conv.capture' || e.kind === 'history.search' || e.kind === 'usage') continue;
    const sess = e.kind.startsWith('session.') && [].concat(e.agent?.id || e.agent).some(a => crew.has(a));
    if (sess || eventTouches(e, w.id)) items.push({ t: e.t, kind: 'event', e });
  }
  for (const c of Object.values(S.convs).filter(c => c.work === w.id)) {
    if (c.access === 'none' && S.agents[c.agent]?.person === S.you) items.push({ t: c.t, kind: 'note', agent: c.agent, text: `${clientName(S.agents[c.agent].client)} shares what it does, not its chat text.` });
    if (canRead(c)) for (const m of S.chats[c.id] || []) items.push({ t: m.t, kind: 'chat', m, conv: c });
  }
  items.sort((a, b) => a.t - b.t || (a.kind === 'event' ? -1 : 1));
  const lanes = ['hub', ...new Set(items.flatMap(laneOf).filter(x => x !== 'hub'))].sort((a, b) => (a === 'hub' ? -1 : b === 'hub' ? 1 : S.agents[a].start - S.agents[b].start));
  // An agent from another task only gets a rail between its first and last appearance here.
  const span = {};
  for (const L of lanes) if (L !== 'hub' && !crew.has(L)) { const ts = items.filter(it => laneOf(it).includes(L)).map(it => it.t); span[L] = [Math.min(...ts), Math.max(...ts)]; }
  lanes.span = span;
  const cat = it => (it.kind !== 'event' ? 'chat' : CODE.has(it.e.kind) ? 'code' : 'coord');
  return { lanes, items: filter && filter !== 'all' ? items.filter(it => cat(it) === filter) : items, all: items };
}

// Each rail has its own colour, used on the line, its sticky head and every message on it.
const laneVar = (lanes, L) => { const i = lanes.indexOf(L); return i <= 0 ? 'var(--lane-hub)' : `var(--lane-${(i - 1) % 6})`; };
// Rail x positions scale with --g (rail spacing), so the phone layout can tighten them in CSS.
const X = i => `calc(var(--g) * ${i} + var(--g) / 2)`;
const laneName = L => (L === 'hub' ? 'Hub and GitHub' : S.agents[L].role === 'subagent' ? `${S.agents[L].name} (subagent)` : agentName(L));
const laneInitial = L => {
  const a = S.agents[L];
  if (a.role === 'subagent') { const w = a.name.split(/\s+/); return (w.length > 1 ? w[0][0] + w[w.length - 1][0] : a.name.slice(0, 2)).replace(/^./, c => c.toUpperCase()); }
  return team() ? S.people[a.person].short.slice(0, 1) : { claude: 'Cl', codex: 'Cx', cursor: 'Cu' }[a.client];
};

function railFor(lanes, it, { now = null, focus = '' } = {}) {
  const nodes = now || it.kind === 'sep' ? [] : laneOf(it);
  const startOf = it.kind === 'event' && it.e.kind === 'session.start' ? it.e.agent.id : null;
  const endOf = it.kind === 'event' && it.e.kind === 'session.end' ? [].concat(it.e.agent) : [];
  const fx = L => (focus ? (L === focus ? ' is-focus' : ' is-dim') : '');
  let out = '';
  lanes.forEach((L, i) => {
    let seg = null;
    if (L === 'hub') seg = 'full';
    else {
      const a = S.agents[L], sp = lanes.span?.[L];
      const start = sp ? sp[0] : a.start, end = sp ? sp[1] : a.end ?? Infinity;
      if (sp && it.t === start && nodes.includes(L)) seg = it.t === end ? null : 'start';
      else if (sp && it.t === end && nodes.includes(L)) seg = 'end';
      else if (startOf === L) seg = 'start';
      else if (endOf.includes(L)) seg = 'end';
      else if (it.t >= start && it.t < end) seg = 'full';
      if (seg && !nodes.includes(L) && (seg === 'start' || seg === 'end')) seg = null;
      const p = a.parent ? lanes.indexOf(a.parent) : -1;
      if (p >= 0 && (startOf === L || endOf.includes(L))) out += `<i class="br ${startOf === L ? 'br-out' : 'br-in'}${fx(L)}" style="--lc:${laneVar(lanes, L)};left:${X(p)};width:calc(var(--g) * ${i - p})"></i>`;
    }
    if (seg) out += `<i class="ln ln-${seg}${L === 'hub' ? ' ln-hub' : ''}${fx(L)}" style="--lc:${laneVar(lanes, L)};left:${X(i)}"></i>`;
  });
  nodes.forEach((L, k) => {
    const i = lanes.indexOf(L);
    if (i < 0) return;
    const kind = it.kind === 'event' ? 'nd-' + (eventTone(it.e) || 'plain') : it.m?.role === 'you' ? 'nd-you' : 'nd-chat';
    if (k === 0) out += `<i class="cn${fx(L)}" style="--lc:${laneVar(lanes, L)};left:${X(i)}"></i>`;
    out += `<b class="nd ${kind}${fx(L)}" style="--lc:${laneVar(lanes, L)};left:${X(i)}"></b>`;
  });
  if (now) { const i = lanes.indexOf(now); if (i >= 0) out += `<b class="nd nd-now" style="--lc:${laneVar(lanes, now)};left:${X(i)}"></b>`; }
  return `<span class="rail-cell" aria-hidden="true">${out}</span>`;
}

function threadRow(it, lanes, q, focus) {
  const rail = railFor(lanes, it, { focus });
  const time = `<time>${clock(it.t)}</time>`;
  const L = laneOf(it)[0];
  const dim = focus && !laneOf(it).includes(focus) ? ' is-dim' : '';
  const dot = `<i class="who-dot" style="--lc:${laneVar(lanes, L)}" aria-hidden="true"></i>`;
  if (it.kind === 'chat') {
    const m = it.m;
    if (m.role === 'tool') return `<li data-k="ch-${it.t}-${h(m.text.slice(0, 20))}" class="tr tr-tool${dim}">${rail}<div class="tr-body"><span class="tool-chip">${dot}<i class="bi ${m.feature ? 'bi-clock-history' : 'bi-terminal'}" aria-hidden="true"></i><span>${chatText(m.text, q)}${m.feature ? ' ' + inReview() : ''}</span></span></div>${time}</li>`;
    const who = m.role === 'you' ? `${team() ? personName(S.agents[it.conv.agent].person) : 'You'} → ${agentName(it.conv.agent)}` : m.role === 'sub' ? `${S.agents[m.agent].name} · subagent` : agentName(it.conv.agent);
    return `<li data-k="ch-${it.t}-${h(m.text.slice(0, 20))}" class="tr tr-chat${dim}">${rail}<div class="tr-body"><div class="bubble bubble-${m.role}"><span class="bubble-who">${dot}${m.role === 'you' ? '<i class="bi bi-person-fill" aria-hidden="true"></i>' : ''}${h(who)}</span><p>${chatText(m.text, q)}</p></div></div>${time}</li>`;
  }
  if (it.kind === 'note') return `<li class="tr tr-note${dim}">${rail}<div class="tr-body"><span class="muted small">${dot}<i class="bi bi-eye-slash" aria-hidden="true"></i> ${h(it.text)}</span></div>${time}</li>`;
  const e = it.e, tone = eventTone(e);
  const big = MILESTONE.has(e.kind) || (e.kind === 'stage' && e.say);
  return `<li data-k="ev-${e.t}-${h(e.kind)}" class="tr tr-event ${big ? 'tr-mile' : ''} ${tone ? 'tone-' + tone : ''}${dim}">${rail}<div class="tr-body"><span class="ev"><i class="bi ${EVENT_ICON[e.kind] || 'bi-dot'}" aria-hidden="true"></i><span>${h(e.say)}</span>${e.kind === 'loop.detect' ? proposed() : ''}</span></div>${time}</li>`;
}

// The separate truths about "done", each from its own source. Landing has two more: CI after it, and users having it.
export function truthLadder(w) {
  const p = prOf(w);
  const lastC = p?.ci[p.ci.length - 1];
  const steps = [
    { label: 'Agent says done', src: 'Reported by the agent', on: w.state === 'finished' },
    { label: 'Checks passed on the exact commit', src: lastC ? `${lastC.rev.slice(0, 7)} · attempt ${lastC.attempt}` : 'No checks yet', on: lastC?.status === 'pass' && lastC.rev === p.head },
    { label: 'GitHub merged it', src: p?.merged ? `Seen on GitHub · ${p.merged.rev.slice(0, 7)}` : 'Not yet', on: !!p?.merged },
    { label: 'CI after landing', src: p?.merged ? 'Not reported for this repo yet' : 'Runs after the merge', on: p?.merged ? null : false },
    { label: 'Users have it', src: 'Deployments aren’t tracked here', on: null },
  ];
  return `<ol class="donefacts" aria-label="Is it really done? Five separate facts">${steps.map(s => `<li class="${s.on === true ? 'on' : s.on === null ? 'unknown' : ''}"><i class="bi ${s.on === true ? 'bi-check-circle-fill' : s.on === null ? 'bi-question-circle' : 'bi-circle'}" aria-hidden="true"></i><span><b>${h(s.label)}</b><small>${h(s.on === null ? 'Unknown' : s.on ? 'Yes' : 'Not yet')} · ${h(s.src)}</small></span></li>`).join('')}</ol>`;
}

// A change record: what this task set out to do, what it decided, and which references back it up.
// Each reference says whether the Hub verified it or only has the agent's word for it.
function changeRecord(w) {
  const p = prOf(w);
  const said = w.checkins[w.checkins.length - 1];
  const refs = [
    ...(p ? p.commits.slice(-3).reverse().map(c => ({ what: `Commit ${c.rev.slice(0, 7)}`, note: c.msg, ok: p.merged ? c.t <= p.merged.t : c.rev === p.head, how: p.merged ? 'Seen on GitHub in the merged branch' : c.rev === p.head ? 'The pull request’s current head' : 'An earlier commit, since replaced' })) : []),
    ...w.scope.slice(0, 2).map(f => ({ what: f, note: 'Declared file plan', ok: false, how: 'The agent’s word. Not checked against the code.' })),
    ...(w.handover ? [{ what: 'Handover note', note: w.handover.note, ok: false, how: 'Written by the agent that handed over' }] : []),
  ];
  const ready = w.state === 'finished' || w.state === 'handover';
  return `<div class="cr">
    <p class="cr-verdict tone-${ready ? 'ok' : 'muted'}"><i class="bi ${ready ? 'bi-box-arrow-right' : 'bi-hourglass-split'}" aria-hidden="true"></i>${ready ? 'Ready to hand off: it has an intent, a decision and a next action.' : 'Not ready yet: the agent is still working on it.'}</p>
    <dl class="cr-facts">
      <div><dt>Intent</dt><dd>${h(w.title)}<span class="cr-scope">${w.scope.map(mono).join(' ')}</span></dd></div>
      <div><dt>Decision</dt><dd>${said ? `“${h(said.note)}” <small>${h(agentName(said.agent))} · ${when(said.t)}</small>` : '<span class="muted">No decision recorded yet</span>'}</dd></div>
      <div><dt>Next action</dt><dd>${h(w.next || (w.state === 'finished' ? 'None. Reported finished.' : 'Not reported'))}</dd></div>
    </dl>
    <ul class="cr-refs">${refs.map(x => `<li class="${x.ok ? 'is-ok' : ''}"><i class="bi ${x.ok ? 'bi-patch-check-fill' : 'bi-patch-question'}" aria-hidden="true"></i><span><b>${h(x.what)}</b> ${h(x.note)}<small>${x.ok ? 'Verified' : 'Not verified'} · ${h(x.how)}</small></span></li>`).join('')}</ul>
    ${copyBtn(`agentscm work show ${w.id} --record`, 'Copy the change record command')}
  </div>`;
}

export function taskPage(id, { back = '#/home', backLabel = 'Back', tf = 'all', q = '', lane = '' } = {}) {
  const crumb = `<nav class="crumb" aria-label="Breadcrumb"><a href="${h(back)}" class="crumb-back"><i class="bi bi-arrow-left" aria-hidden="true"></i>${h(backLabel)}</a><span class="crumb-sep" aria-hidden="true">/</span><span>Task ${mono(id)}</span></nav>`;
  const w = S.work[id];
  if (!w) return `${crumb}<div class="empty"><i class="bi bi-question-circle" aria-hidden="true"></i><p><strong>We can’t find task ${h(id)}.</strong> It may not have started yet, or it may belong to a repository you can’t see.</p><a class="btn btn-sm btn-outline-brand" href="${h(back)}">${h(backLabel)}</a></div>`;
  const p = prOf(w), loop = loopOf(w);
  const att = attention().filter(a => a.works.includes(id));
  const { lanes, items, all } = threadOf(w, tf);
  const focus = lanes.includes(lane) ? lane : '';
  const overl = Object.values(S.overlaps).filter(o => o.a === id || o.b === id);
  const recs = historyRecords().filter(r => r.w?.id === id);
  const related = new Map();
  if (w.waitsFor) related.set(w.waitsFor, 'This task is waiting for it');
  for (const x of w.waits || []) if (!related.has(x.on)) related.set(x.on, `This task waited ${dur(x.to - x.from)} for it`);
  if (loop && !related.has(loop.blockedBy)) related.set(loop.blockedBy, 'Owns the test that kept failing here');
  for (const o of overl) { const other = o.a === id ? o.b : o.a; if (!related.has(other)) related.set(other, `Planned the same file: ${o.paths.join(', ')}`); }
  for (const x of works()) if ((x.waitsFor === id || (x.waits || []).some(y => y.on === id)) && !related.has(x.id)) related.set(x.id, 'Was waiting for this task');
  const next = w.next || (w.state === 'finished' ? 'Nothing. The agent reported this finished.' : 'No next step reported');
  const ongoing = !(p?.merged && (!followUp(w) || followUp(w).s === 'done'));
  const counts = { all: all.length, chat: all.filter(i => i.kind !== 'event').length, code: all.filter(i => i.kind === 'event' && CODE.has(i.e.kind)).length };
  counts.coord = all.length - counts.chat - counts.code;
  const filters = [['all', 'Everything'], ['chat', 'Chat'], ['code', 'Code and checks'], ['coord', 'Coordination']];
  let lastDay = null, newMarked = false;
  const since = lastReviewed();
  const fresh = all.filter(it => it.t > since).length;
  const rows = items.map(it => {
    const mark = !newMarked && since > 0 && it.t > since ? (newMarked = true, `<li class="tr tr-new">${railFor(lanes, { ...it, kind: 'sep' }, { focus })}<div class="tr-body"><span><i class="bi bi-clock-history" aria-hidden="true"></i>New since you last checked</span></div></li>`) : '';
    const d = dayLabel(it.t);
    const sep = d !== lastDay && (lastDay !== null || d !== 'Today') ? `<li class="tr tr-day">${railFor(lanes, { ...it, kind: 'sep' }, { focus })}<div class="tr-body"><span>${h(d)}</span></div></li>` : '';
    lastDay = d;
    return sep + mark + threadRow(it, lanes, q, focus);
  }).join('');

  const legend = `<ul class="lanes">${lanes.map(L => {
    if (L === 'hub') return `<li><button type="button" class="lane-pick ${focus === L ? 'on' : ''}" data-set="lane" data-val="${focus === L ? '' : L}" aria-pressed="${focus === L}"><i class="swatch swatch-hub" aria-hidden="true"></i><div><b>Hub and GitHub</b><span>Heads-ups, checks, merges</span></div></button></li>`;
    const a = S.agents[L], ppl = S.people[a.person];
    const role = L === w.lead ? 'Leads this task' : a.role === 'subagent' ? `Subagent of ${agentName(a.parent)}` : w.handover?.from === L ? 'Handed it over' : w.participants.includes(L) ? 'Helping' : 'Answered a heads-up';
    return `<li data-k="ln-${h(L)}"><button type="button" class="lane-pick ${focus === L ? 'on' : ''}" data-set="lane" data-val="${focus === L ? '' : L}" aria-pressed="${focus === L}"><i class="swatch" style="--lc:${laneVar(lanes, L)}" aria-hidden="true"></i><div><b>${h(laneName(L))}</b><span>${h(role)}${team() ? ` · ${h(ppl.city)} ${clock(S.t, ppl.utc)}` : ''}</span></div>${pill(agentStatus(a).tone, live(a) ? (a.stale ? 'Quiet' : 'Working') : 'Ended')}</button></li>`;
  }).join('')}</ul>`;

  return `
    ${crumb}
    <header class="task-hero">
      <div class="d-meta">${statePill(w.state)} ${mono(w.id)} ${p ? `<span class="pill pill-muted"><i class="bi bi-git" aria-hidden="true"></i>PR #${p.num}${p.merged ? ' · merged' : ''}</span>` : ''} <span class="muted small">Started ${when(w.started)}</span>${fresh ? `<a class="pill pill-brand since-chip" href="#thread"><i class="bi bi-clock-history" aria-hidden="true"></i>${fresh} new since you last checked</a>` : ''}</div>
      <h1 class="task-title">${h(w.title)}</h1>
      <div class="task-next"><span>Next</span><p>${h(next)}</p></div>
      ${laneBar(w)}${flowChip(w)}
      ${truthLadder(w)}
      <div class="task-actions">${copyBtn(`Continue AgentSCM work ${id} (“${w.title}”). Next: ${next}`)} ${copyBtn(`agentscm work show ${id}`, 'Copy CLI command')}<span class="muted small">Nothing runs from this page. Your agents and GitHub stay in charge.</span></div>
    </header>
    ${att.length ? `<div class="task-alerts" id="alerts">${attentionList(att)}</div>` : ''}
    <div class="task-grid">
      <section class="thread-wrap" id="thread" aria-label="Thread">
        <div class="thread-head">
          <h2><i class="bi bi-git" aria-hidden="true"></i>The thread</h2>
          <p>Every agent has its own coloured rail. Subagents branch off and come back. Tap a face to follow one agent.</p>
        </div>
        <div class="thread-sticky" style="--n:${lanes.length}">
          <div class="rail-heads" role="group" aria-label="Follow one rail">${lanes.map((L, i) => `<button type="button" class="rh ${focus === L ? 'on' : ''} ${focus && focus !== L ? 'is-dim' : ''}" style="--lc:${laneVar(lanes, L)};left:${X(i)}" data-set="lane" data-val="${focus === L ? '' : L}" aria-pressed="${focus === L}" title="${focus === L ? 'Stop following' : 'Follow'} ${h(laneName(L))}"><span aria-hidden="true">${L === 'hub' ? '<i class="bi bi-broadcast"></i>' : h(laneInitial(L))}</span><span class="visually-hidden">${focus === L ? 'Stop following' : 'Follow'} ${h(laneName(L))}</span></button>`).join('')}</div>
          <div class="sticky-right">
            <p class="follow-label">${focus ? `<i class="who-dot" style="--lc:${laneVar(lanes, focus)}" aria-hidden="true"></i>Following <b>${h(laneName(focus))}</b> <button type="button" class="btn btn-sm btn-link p-0" data-set="lane" data-val="">Show everyone</button>` : 'Showing everyone'}</p>
            <div class="filters" role="group" aria-label="Show in thread">${filters.map(([k, l]) => `<button type="button" class="btn btn-sm ${tf === k ? 'active' : ''}" data-set="tf" data-val="${k}" aria-pressed="${tf === k}">${h(l)} <span class="fcount">${counts[k]}</span></button>`).join('')}</div>
          </div>
        </div>
        <ol class="thread" id="progress" style="--n:${lanes.length}">${rows || '<li class="tr"><div class="tr-body muted">Nothing of this kind yet.</div></li>'}
          ${ongoing ? `<li class="tr tr-now">${railFor(lanes, { t: S.t, kind: 'now' }, { now: w.lead, focus })}<div class="tr-body"><span><b>Now</b> · The thread continues here as agents report.</span></div><time>${clock(S.t)}</time></li>` : ''}
        </ol>
      </section>
      <aside class="task-side" aria-label="Task facts">
        ${box({ title: 'Ready to hand off?', id: 'record', icon: 'bi-file-earmark-check', explain: 'The change record: what it meant to do, what it decided, and what backs that up.', body: changeRecord(w) })}
        ${box({ title: 'On the rails', id: 'who', icon: 'bi-people', count: lanes.length, explain: 'Pick one to follow it through the thread.', body: `${legend}${w.handover ? `<div class="handover-note"><i class="bi bi-sticky" aria-hidden="true"></i><div><strong>Handover note from ${h(agentName(w.handover.from))}</strong><p>${h(w.handover.note)}</p><small>${w.handover.acceptedAt ? `Picked up by ${h(agentName(w.handover.to))} after ${dur(w.handover.acceptedAt - w.handover.t)}` : 'Not picked up yet. It stays assigned until someone accepts it.'}</small></div></div>` : ''}` })}
        ${box({ title: 'Files', id: 'files', icon: 'bi-folder2-open', tone: overl.some(o => o.state === 'open') ? 'warn' : 'brand', explain: 'What it plans to touch, and who else planned the same.', body: `<ul class="paths">${w.scope.map(s => `<li>${mono(s)}</li>`).join('')}</ul>${overl.length ? `<ul class="mini">${overl.map(o => `<li data-k="ov-${h(o.id)}">${pill(o.state === 'open' ? 'warn' : 'ok', o.state === 'open' ? 'Heads-up open' : 'Sorted out', 'bi-sign-intersection')} ${mono(o.paths.join(', '))}<div class="muted small">Also planned by ${workLink(o.a === id ? o.b : o.a)}${o.response ? ` · ${h(o.response.kind)}: ${h(o.response.note)}` : ''}</div></li>`).join('')}</ul>` : '<p class="muted small mb-0">No other task has planned these files.</p>'}` })}
        ${box({ title: 'Sessions', id: 'sessions', icon: 'bi-journal-text', count: recs.length, explain: 'Open one to read the whole chat, if it’s yours.', body: recs.length ? `<ul class="mini">${recs.map(r => `<li data-k="ses-${h(r.id)}"><a class="ses-link" href="#/session/${h(r.id)}">${avatar(r.a.id)}<span><b>${h(r.title)}</b><small>${h(dayLabel(r.t))} ${clock(r.t)} · ${h(agentName(r.a.id))}</small></span><i class="bi bi-chevron-right" aria-hidden="true"></i></a>${pill(r.access.tone, r.access.text, r.access.icon)}</li>`).join('')}</ul>` : '<p class="muted small mb-0">No sessions linked yet.</p>' })}
        ${related.size ? box({ title: 'Related tasks', id: 'related', icon: 'bi-link-45deg', count: related.size, body: `<ul class="mini">${[...related].map(([rid, why]) => `<li>${workLink(rid)} ${statePill(S.work[rid].state)}<div class="muted small">${h(why)}</div></li>`).join('')}</ul>` }) : ''}
      </aside>
    </div>`;
}

// The latest thing an agent said on this task, from chats you're allowed to read.
export function lastWords(w) {
  return Object.values(S.convs).filter(c => c.work === w.id && canRead(c))
    .flatMap(c => (S.chats[c.id] || []).filter(m => m.role === 'agent' || m.role === 'sub').map(m => ({ ...m, by: m.role === 'sub' ? m.agent : c.agent })))
    .sort((x, y) => y.t - x.t)[0];
}

// ---------- session page ----------
export function sessionPage(aid, { back = '#/history', backLabel = 'Back to History', q = '' } = {}) {
  const crumb = `<nav class="crumb" aria-label="Breadcrumb"><a href="${h(back)}" class="crumb-back"><i class="bi bi-arrow-left" aria-hidden="true"></i>${h(backLabel)}</a><span class="crumb-sep" aria-hidden="true">/</span><span>Session</span></nav>`;
  const a = S.agents[aid];
  const rec = historyRecords().find(r => r.id === aid);
  if (!a || !rec) return `${crumb}<div class="empty"><i class="bi bi-question-circle" aria-hidden="true"></i><p><strong>We can’t find that session.</strong> It may not have started yet, or it isn’t one you can see.</p><a class="btn btn-sm btn-outline-brand" href="${h(back)}">${h(backLabel)}</a></div>`;
  const { c, w, mine } = rec;
  const readable = c && canRead(c);
  const msgs = readable ? S.chats[c.id] || [] : [];
  const kids = agents().filter(k => k.parent === aid);
  const did = S.log.filter(e => laneOf({ kind: 'event', e }).some(x => x === aid || kids.some(k => k.id === x)) && e.kind !== 'conv.capture');
  const why = !c ? 'No chat was saved for this session.' : c.access === 'none' ? `${clientName(a.client)} shares what it does, not its chat text. Here’s what it did.` : !mine ? 'You can see who, when and which files. Chat text stays with the person who ran it.' : '';
  const chips = [pill(rec.access.tone, rec.access.text, rec.access.icon)];
  if (readable) chips.push(pill('muted', 'Secrets removed before saving. Best effort.', 'bi-shield-check'));
  const typing = readable && live(a) && msgs.length ? `<li class="msg msg-agent typing"><span class="dots" aria-label="${h(agentName(aid))} is working"><i></i><i></i><i></i></span></li>` : '';
  return `
    ${crumb}
    <header class="ses-hero">
      ${avatar(aid, 'av-xl')}
      <div>
        <h1 class="task-title">${h(rec.title)}</h1>
        <p class="ses-meta">${h(agentName(aid))}${agentName(aid) === clientName(a.client) ? '' : ` · ${h(clientName(a.client))}`} · ${h(dayLabel(a.start))} ${clock(a.start)}–${a.end != null ? clock(a.end) : 'now'} · ${mono(S.meta.repo)}</p>
        <div class="chips">${w ? `<a class="chip-link" href="${taskHref(w.id)}" data-work="${h(w.id)}"><i class="bi bi-flag" aria-hidden="true"></i>${h(w.title)}</a>` : '<span class="pill pill-muted">Not linked to a task</span>'} ${chips.join(' ')}</div>
      </div>
      ${readable ? `<div class="ses-act">${copyBtn(`${a.client === 'claude' ? 'claude --resume' : 'codex resume'} ${c.id}`, 'Copy resume command')}</div>` : ''}
    </header>
    <div class="task-grid">
      <section class="convo-wrap" aria-label="Chat">
        ${readable ? `<ol class="convo">${msgs.map(m => m.role === 'tool'
          ? `<li data-k="m-${m.t}-${h(m.text.slice(0, 16))}" class="msg msg-tool ${m.t > lastReviewed() ? 'is-unread' : ''}"><span class="tool-chip"><i class="bi ${m.feature ? 'bi-clock-history' : 'bi-terminal'}" aria-hidden="true"></i><span>${chatText(m.text, q)}${m.feature ? ' ' + inReview() : ''}</span></span><time>${clock(m.t)}</time></li>`
          : `<li data-k="m-${m.t}-${h(m.text.slice(0, 16))}" class="msg msg-${m.role === 'you' ? 'you' : m.role === 'sub' ? 'sub' : 'agent'}">${m.role === 'you' ? '' : avatar(m.role === 'sub' ? m.agent : aid)}<div class="msg-bubble"><span class="bubble-who">${h(m.role === 'you' ? 'You' : m.role === 'sub' ? `${S.agents[m.agent].name} · subagent` : agentName(aid))}<time>${clock(m.t)}</time></span><p>${chatText(m.text, q)}</p></div></li>`).join('')}${typing}</ol>${!live(a) ? `<p class="convo-end"><i class="bi bi-box-arrow-left" aria-hidden="true"></i>Session ended ${when(a.end)}</p>` : ''}`
          : `<div class="convo-locked"><i class="bi bi-chat-square-dots" aria-hidden="true"></i><p>${h(why)}</p></div><h2 class="convo-did">What it did <span>${did.length}</span></h2><ul class="story">${did.slice().reverse().map(e => storyLine(e)).join('')}</ul>`}
      </section>
      <aside class="task-side" aria-label="Session facts">
        ${box({ title: 'Files in this session', icon: 'bi-folder2-open', body: rec.files.length ? `<ul class="paths">${rec.files.map(f => `<li>${mono(f)}</li>`).join('')}</ul>` : '<p class="muted small mb-0">No files recorded.</p>' })}
        ${kids.length ? box({ title: 'Subagents it started', icon: 'bi-diagram-2', count: kids.length, body: `<ul class="lanes">${kids.map(k => `<li><i class="swatch ${laneClass(k.id)}" aria-hidden="true"></i><div><b>${h(k.name)}</b><span>Linked by ${h(k.evidence)}</span></div>${pill(live(k) ? 'ok' : 'muted', live(k) ? 'Working' : 'Stopped')}</li>`).join('')}</ul>` }) : ''}
        ${readable ? box({ title: 'What it did', icon: 'bi-list-check', count: did.length, body: did.length ? `<ul class="story">${did.slice(-6).reverse().map(e => storyLine(e, { withWork: false })).join('')}</ul>` : '<p class="muted small mb-0">Nothing recorded yet.</p>' }) : ''}
      </aside>
    </div>`;
}

// ---------- History ----------
export function convTitle(c) {
  const a = S.agents[c.agent];
  return canRead(c) || !c.work ? c.title : `${S.work[c.work].title} session`;
}

export function historyRecords() {
  const recs = [];
  for (const a of agents()) {
    const c = Object.values(S.convs).find(x => x.agent === a.id);
    const w = c?.work ? S.work[c.work] : works().find(x => x.lead === a.id) || works().find(x => x.participants.includes(a.id));
    const mine = a.person === S.you;
    if (a.role === 'subagent') continue;
    if (!mine && !w) continue;
    const files = c?.files?.length && (mine || !c) ? c.files : w ? w.scope : [];
    const notes = w ? [w.title, w.next, w.handover?.note, ...w.checkins.map(x => x.note)].filter(Boolean) : [];
    let access;
    if (!mine) access = { tone: 'muted', icon: 'bi-person', text: 'Team session record. Who, when and which files only.' };
    else if (!c) access = { tone: 'muted', icon: 'bi-dash-circle', text: 'No chat was saved' };
    else if (c.access === 'none') access = { tone: 'muted', icon: 'bi-eye-slash', text: `${clientName(a.client)} shares activity, not chat text` };
    else if (c.access === 'shared') access = { tone: 'brand', icon: 'bi-cloud-check', text: 'Chat saved on the Hub · only you and admins can read it · kept 30 days' };
    else access = { tone: 'ok', icon: 'bi-laptop', text: 'Chat saved on this computer' };
    const chat = c && canRead(c) ? S.chats[c.id] || [] : [];
    recs.push({
      id: a.id, a, c, w, mine, files, access, t: a.start, chat,
      title: mine && c ? c.title : w ? `${w.title}` : `${clientName(a.client)} session`,
      words: mine && c ? c.words : 0,
      hay: [mine && c ? c.title : '', ...files, agentName(a.id), clientName(a.client), personName(a.person), ...notes, ...chat.map(m => m.text)].join(' ').toLowerCase(),
    });
  }
  return recs.sort((x, y) => y.t - x.t);
}

// Where a session sits within its own day (0–100), in the same time zone as clock(). It stops at that day's end.
export function ribbonOf(start, end) {
  const left = ((((base() / 60000 + start + S.meta.zone.utc * 60) % 1440) + 1440) % 1440) / 14.4;
  return { left, width: Math.max(1.5, Math.min((end - start) / 14.4, 100 - left)) };
}
// Every file AgentSCM knows about: declared plans and files saved with sessions. Globs stay as folders.
function knownFiles() {
  const set = new Set();
  for (const w of works()) w.scope.forEach(f => set.add(f));
  for (const c of Object.values(S.convs)) (c.files || []).forEach(f => set.add(f));
  return [...set].sort();
}
function filesTab(ui) {
  const files = knownFiles();
  const sel = files.includes(ui.file) ? ui.file : files[0];
  const hit = f => scopeHits([f.replace(/\/\*\*$/, '')], sel.replace(/\/\*\*$/, '')) || scopeHits([sel.replace(/\/\*\*$/, '')], f.replace(/\/\*\*$/, ''));
  const tasks = works().filter(w => w.scope.some(hit));
  const sess = historyRecords().filter(x => x.files.some(hit));
  const heads = Object.values(S.overlaps).filter(o => o.paths.some(hit));
  const byArea = S.meta.areas.map(a => ({ a, list: files.filter(f => f.startsWith(a.path)) })).filter(x => x.list.length);
  const other = files.filter(f => !S.meta.areas.some(a => f.startsWith(a.path)));
  const fileBtn = f => `<li><button type="button" class="fx-file ${f === sel ? 'on' : ''}" data-set="file" data-val="${h(f)}" aria-pressed="${f === sel}"><i class="bi ${f.endsWith('**') ? 'bi-folder2' : 'bi-file-earmark-code'}" aria-hidden="true"></i>${h(f.split('/').slice(-2).join('/'))}</button></li>`;
  const story = [
    ...tasks.map(w => ({ t: w.started, html: `${avatar(w.lead)}<span>${workLink(w.id)} planned it <small>${h(agentName(w.lead))} · ${statePill(w.state)}</small></span>` })),
    ...sess.map(x => ({ t: x.t, html: `${avatar(x.a.id)}<span><a href="#/session/${h(x.id)}">${h(x.title)}</a> touched it <small>${h(agentName(x.a.id))}</small></span>` })),
    ...heads.map(o => ({ t: o.t, html: `<span class="av av-system" aria-hidden="true"><i class="bi bi-sign-intersection"></i></span><span>Two agents planned it at once <small>${workLink(o.a)} and ${workLink(o.b)} · ${o.state === 'open' ? 'open' : 'sorted out'}</small></span>` })),
  ].sort((x, y) => y.t - x.t);
  return `<div class="fx">
    <nav class="fx-tree" aria-label="Files">${byArea.map(x => `<h3>${h(x.a.label)} <code>${h(x.a.path)}/</code></h3><ul>${x.list.map(fileBtn).join('')}</ul>`).join('')}${other.length ? `<h3>Elsewhere</h3><ul>${other.map(fileBtn).join('')}</ul>` : ''}</nav>
    <section class="fx-read" aria-live="polite">
      <h2 class="fx-path">${mono(sel)}</h2>
      <p class="fx-sum">${tasks.length} task${tasks.length === 1 ? '' : 's'}, ${sess.length} session${sess.length === 1 ? '' : 's'} and ${heads.length} heads-up${heads.length === 1 ? '' : 's'} touched this.</p>
      <ol class="fx-story">${story.map(x => `<li><time>${h(dayLabel(x.t))} ${clock(x.t)}</time><div>${x.html}</div></li>`).join('') || '<li><div class="muted">Nothing recorded for this file.</div></li>'}</ol>
      <p class="since-note tone-plain"><i class="bi bi-info-circle" aria-hidden="true"></i>Shallow history: only what AgentSCM saved. The full history lives in Git.</p>
      ${copyBtn(`git log --follow -- ${sel.replace(/\/\*\*$/, '')}`, 'Copy the Git history command')}
    </section>
  </div>`;
}

export function historyPage(ui) {
  const recs = historyRecords();
  const tabs = pageTabs('history', [['sessions', 'Sessions', `${recs.length} you can see`], ['files', 'Files', `${knownFiles().length} files with a story`]], ui.tab);
  if (ui.tab === 'files') return `${masthead('bi-clock-history', 'What happened before?', 'Pick a file to see every task, session and heads-up that touched it.')}${tabs}${filesTab(ui)}`;
  const q = (ui.q || '').trim().toLowerCase();
  const f = ui.hf || 'all';
  const shown = recs.filter(r => (!q || q.split(/\s+/).every(w => r.hay.includes(w)))
    && (f === 'all' || (f === 'mine' && r.mine) || (f === 'unlinked' && !r.w) || f === r.a.client
      || (f === 'today' && dayLabel(r.t) === 'Today') || (f === 'earlier' && dayLabel(r.t) !== 'Today')));
  const filters = [['all', 'Everything'], ['today', 'Today'], ['earlier', 'Earlier'], ['claude', 'Claude Code'], ['codex', 'Codex'], ...(team() ? [['cursor', 'Cursor'], ['mine', 'Only mine']] : []), ['unlinked', 'Not linked to a task']];
  const suggestion = team() ? 'JPY rounding' : 'redaction';
  return `
    ${masthead('bi-clock-history', 'What happened before?', `${recs.length} sessions you can see. Search by tool, day, file or words. You don’t need a task ID.`)}
    ${tabs}
    <div class="history-tools">
      <label class="visually-hidden" for="hq">Search history</label>
      <div class="search"><i class="bi bi-search" aria-hidden="true"></i><input id="hq" data-ui="q" class="form-control" type="search" placeholder="Try “${h(suggestion)}”, a file name, or an agent" value="${h(ui.q || '')}" autocomplete="off"></div>
      <div class="filters" role="group" aria-label="Filter history">${filters.map(([k, l]) => `<button type="button" class="btn btn-sm ${f === k ? 'active' : ''}" data-hf="${k}" aria-pressed="${f === k}">${h(l)}</button>`).join('')}</div>
    </div>
    ${shown.length ? [...new Set(shown.map(r => dayLabel(r.t)))].map(day => { const list = shown.filter(r => dayLabel(r.t) === day); return `<section class="hist-day"><h2 class="hist-day-head">${h(day)}<span>${list.length} session${list.length === 1 ? '' : 's'}</span></h2><ul class="history">${list.map(r => `
      <li data-k="hi-${h(r.id)}" class="hist-row">
        <div class="hist-when"><strong>${clock(r.t)}</strong><span>${r.a.end != null ? `to ${clock(r.a.end)}` : 'still open'}</span><span class="hist-ribbon" aria-hidden="true">${(b => `<i style="left:${b.left.toFixed(2)}%;width:${b.width.toFixed(2)}%"></i>`)(ribbonOf(r.t, r.a.end ?? S.t))}</span></div>
        <div class="hist-main">
          <a class="hist-title" href="#/session/${h(r.id)}">${avatar(r.a.id)}<span>${h(r.title)}</span><i class="bi bi-chevron-right" aria-hidden="true"></i></a>
          ${(() => { const qq = (ui.q || '').trim().toLowerCase(); const hit = qq && r.chat.find(m => qq.split(/\s+/).every(x => m.text.toLowerCase().includes(x))); const m = hit || r.chat.find(x => x.role === 'you'); return m ? `<blockquote class="hist-quote"><span>${h(m.role === 'you' ? 'You' : m.role === 'tool' ? 'Tool' : agentName(r.a.id))}:</span> ${chatText(m.text.length > 160 ? m.text.slice(0, 157) + '…' : m.text, ui.q || '')}</blockquote>` : ''; })()}
          <div class="hist-meta">${h(agentName(r.a.id))}${agentName(r.a.id) === clientName(r.a.client) ? '' : ` · ${h(clientName(r.a.client))}`} ${r.w ? `· task ${workLink(r.w.id)}` : '· <span class="pill pill-muted">Not linked to a task</span>'}</div>
          ${r.files.length ? `<div class="hist-files">${r.files.slice(0, 3).map(mono).join(' ')}</div>` : ''}
          <div class="hist-access">${pill(r.access.tone, r.access.text, r.access.icon)}</div>
        </div>
        <div class="hist-act">${r.mine && r.c && r.c.access !== 'none' ? copyBtn(`${r.a.client === 'claude' ? 'claude --resume' : 'codex resume'} ${r.c.id}`, 'Copy resume command') : ''}</div>
      </li>`).join('')}</ul></section>`; }).join('')
      : `<div class="empty"><i class="bi bi-search" aria-hidden="true"></i><p><strong>Nothing matches “${h(ui.q)}”.</strong> Try a file name or a tool name, or clear the filters.</p><button class="btn btn-sm btn-outline-brand" data-hf="all" data-clear="1">Clear search</button></div>`}
    <p class="fineprint"><i class="bi bi-shield-lock" aria-hidden="true"></i> History shows only sessions you’re allowed to see. Chat text from other people is never listed or counted here. Turning chat sharing off doesn’t delete copies already kept. <span class="pill pill-proposed">Proposed</span> Search looks inside your own chat text, which needs a new index.</p>`;
}

// ---------- Timeline: saved moments of the workspace ----------
// Each checkpoint, commit and handover is a moment you can look back at, compare, and start again from.
function moments() {
  const out = [];
  for (const w of works()) {
    w.checkins.forEach(c => out.push({ t: c.t, kind: 'Checkpoint', icon: 'bi-chat-left-text', what: c.note, by: c.agent, w }));
    if (w.handover) out.push({ t: w.handover.t, kind: 'Handover', icon: 'bi-box-arrow-right', what: w.handover.note, by: w.handover.from, w });
    (prOf(w)?.commits || []).forEach(c => out.push({ t: c.t, kind: c.type === 'merge-main' ? 'Merged main' : 'Commit', icon: 'bi-git', what: `${c.rev.slice(0, 7)} · ${c.msg}`, by: w.lead, w }));
  }
  return out.filter(m => m.t >= 0).sort((a, b) => a.t - b.t);
}
export function timelinePage(ui) {
  const ms = moments();
  if (!ms.length) return `${masthead('bi-camera', 'What did the work look like earlier?', 'No saved moments yet. Each checkpoint and commit becomes one.')}`;
  const i = Math.min(ms.length - 1, Math.max(0, ui.tm === undefined || ui.tm === '' ? ms.length - 1 : Number(ui.tm)));
  const m = ms[i];
  const vs = ui.tc === 'prev' && i > 0 ? ms[i - 1] : null;
  const from = vs ? vs.t : m.t, to = vs ? m.t : S.t;
  const between = S.log.filter(e => e.t > from && e.t <= to);
  const files = [...new Set(works().filter(w => between.some(e => typeof taskOf(e) === 'string' && taskOf(e) === w.id)).flatMap(w => w.scope))];
  const span = Math.max(1, S.t - ms[0].t);
  return `
    ${masthead('bi-camera', 'What did the work look like earlier?', `${ms.length} saved moments. Pick one to see it, compare it, or start again from it.`)}
    ${box({ title: 'Saved moments', icon: 'bi-camera', count: ms.length, explain: 'Every checkpoint, commit and handover, on one line. Tap one.', cls: 'box-wide', body: `
      <div class="tl-strip">${ms.map((x, k) => `<button type="button" class="tl-dot tl-${x.kind === 'Checkpoint' ? 'cp' : x.kind === 'Handover' ? 'ho' : 'cm'} ${k === i ? 'on' : ''}" style="left:${(((x.t - ms[0].t) / span) * 100).toFixed(2)}%" data-set="tm" data-val="${k}" aria-pressed="${k === i}" aria-label="${h(x.kind)} at ${clock(x.t)}: ${h(x.what)}"></button>`).join('')}</div>
      <div class="sl-axis tl-axis" aria-hidden="true"><span>${h(dayLabel(ms[0].t))} ${clock(ms[0].t)}</span><span>now</span></div>
      <p class="tl-legend"><span><i class="tl-key tl-cp"></i>Checkpoint</span><span><i class="tl-key tl-cm"></i>Commit</span><span><i class="tl-key tl-ho"></i>Handover</span></p>` })}
    <div class="today-grid">
      <div class="today-story">
        ${box({ title: `${m.kind} · ${dayLabel(m.t)} ${clock(m.t)}`, icon: m.icon, explain: `On ${m.w.title}`, body: `
          <div class="tl-moment">${avatar(m.by)}<div><p>${h(m.what)}</p><small>${h(agentName(m.by))} · ${workLink(m.w.id)}</small></div></div>
          <div class="tl-nav"><button type="button" class="btn btn-sm btn-outline-brand" data-set="tm" data-val="${Math.max(0, i - 1)}" ${i === 0 ? 'disabled' : ''}><i class="bi bi-chevron-left" aria-hidden="true"></i>Earlier</button><button type="button" class="btn btn-sm btn-outline-brand" data-set="tm" data-val="${Math.min(ms.length - 1, i + 1)}" ${i === ms.length - 1 ? 'disabled' : ''}>Later<i class="bi bi-chevron-right" aria-hidden="true"></i></button></div>
          <h3 class="tl-sub">Start again from here</h3>
          <p class="muted small">Your agent restores this moment in a new worktree. The browser never changes files.</p>
          ${copyBtn(`agentscm workspace restore --work ${m.w.id} --at ${clock(m.t)}`, 'Copy the restore command')}` })}
      </div>
      <aside class="today-side" aria-label="Compare">
        ${box({ title: 'Compare', icon: 'bi-arrow-left-right', explain: vs ? `From the moment before (${clock(vs.t)}) to this one.` : `From this moment to now.`, body: `
          <div class="lens seg" role="group" aria-label="Compare with"><button type="button" class="btn btn-sm ${vs ? '' : 'active'}" data-set="tc" data-val="now" aria-pressed="${!vs}">With now</button><button type="button" class="btn btn-sm ${vs ? 'active' : ''}" data-set="tc" data-val="prev" aria-pressed="${!!vs}" ${i === 0 ? 'disabled' : ''}>With the one before</button></div>
          <p class="tl-diff"><b>${between.length}</b> thing${between.length === 1 ? '' : 's'} happened, touching <b>${files.length}</b> declared file plan${files.length === 1 ? '' : 's'}.</p>
          ${files.length ? `<ul class="paths">${files.map(f => `<li>${mono(f)}</li>`).join('')}</ul>` : '<p class="muted small">No differences in declared files.</p>'}
          <p class="since-note tone-plain"><i class="bi bi-info-circle" aria-hidden="true"></i>Compares what AgentSCM saved, not file contents. Your agent can diff the code.</p>` })}
      </aside>
    </div>`;
}

// ---------- Skills ----------
// Reusable recipes: named sequences of steps agents follow. Each one lists the tasks that followed it.
function recipes() {
  const flow = workflowSkill();
  return [
    { name: 'Ship a feature', steps: ['Declare files', 'Build', '/review', 'Checks on the exact commit', 'Merge', flow.name], used: works().filter(w => w.pr) },
    { name: 'Hand over at the end of the day', steps: ['Write a checkpoint', S.meta.skills.find(x => x.id === 'handover')?.name || 'Handover note', 'Pause the work', 'Next owner accepts'], used: works().filter(w => w.handover) },
    { name: 'Get out of a merge loop', steps: ['Spot the loop', 'Stop merging main', 'Fix or quarantine the test', 'One final merge'], used: works().filter(w => loopOf(w)) },
  ];
}
function checksTab() {
  const prs = Object.values(S.prs).sort((a, b) => b.t - a.t);
  if (!prs.length) return '<p class="empty-line">No pull requests yet. Each one shows up here with every check attempt.</p>';
  return `${box({ title: 'Where is each pull request?', icon: 'bi-signpost-split', count: prs.length, explain: 'The repo’s gate, left to right, for every open or merged pull request.', cls: 'box-wide', body: `<ol class="rl">${prs.map(p => { const w = S.work[p.work]; return `<li class="rl-row" data-k="pg-${p.num}"><div class="rl-task"><div class="lane-title">#${p.num} ${workLink(w.id)}</div><div class="lane-meta">${statePill(w.state)}</div></div>${laneBar(w, { mode: 'grid', link: true })}</li>`; }).join('')}</ol>` })}
    ${box({ title: 'Did the checks pass?', icon: 'bi-check2-square', explain: 'Every attempt on every pull request, oldest first. A repeat of the same failing test is the loop warning sign.', cls: 'box-wide', body: `<ul class="ck">${prs.map(p => { const fails = p.ci.filter(c => c.status === 'fail'); const same = fails.length > 1 && fails.every(c => c.test === fails[0].test); const found = p.reviews.reduce((n, x) => ({ high: n.high + x.high, med: n.med + x.med }), { high: 0, med: 0 }); return `<li data-k="ck-${p.num}"><div class="ck-head"><b>#${p.num}</b> ${workLink(p.work)}${p.merged ? pill('ok', 'Merged', 'bi-sign-merge-left') : ''}</div>
      <ol class="ck-runs">${p.ci.map(c => `<li class="ck-run is-${c.status}" title="Attempt ${c.attempt} · ${h(c.rev.slice(0, 7))}${c.test ? ` · ${h(c.test)}` : ''}"><i class="bi ${c.status === 'pass' ? 'bi-check-lg' : c.status === 'fail' ? 'bi-x-lg' : 'bi-hourglass-split'}" aria-hidden="true"></i><span>${c.attempt}</span></li>`).join('') || '<li class="ck-none">No checks yet</li>'}</ol>
      <p class="ck-sum">${p.ci.length ? `${p.ci.length} attempt${p.ci.length === 1 ? '' : 's'}, ${fails.length} failed${same ? `, all on ${mono(fails[0].test)}` : ''}.` : 'Waiting for the first run.'} ${p.reviews.length ? `Review found ${found.high} serious and ${found.med} medium problem${found.high + found.med === 1 ? '' : 's'} over ${p.reviews.length} round${p.reviews.length === 1 ? '' : 's'}.` : ''}</p></li>`; }).join('')}</ul>` })}`;
}
function recipesTab() {
  return `<ul class="rcp">${recipes().map(x => `<li data-k="rc-${h(x.name)}" class="rcp-row">
      <div class="rcp-head"><h3>${h(x.name)}</h3><span class="muted small">${x.used.length ? `Followed by ${x.used.map(w => workLink(w.id)).join(', ')}` : 'Not followed yet'}</span>${copyBtn(`agentscm template show "${x.name}"`, 'Copy command')}</div>
      <ol class="flowbar" style="--n:${x.steps.length}">${x.steps.map(st => `<li><span class="fb-step">${h(st)}</span></li>`).join('')}</ol>
    </li>`).join('')}</ul>
    <p class="fineprint">${proposed()} Recipes are the Hub’s workflow templates. Your agents follow them; nothing runs from this page.</p>`;
}

export function skillsPage(ui = {}) {
  const runsOf = id => S.skillRuns.filter(r => r.skill === id);
  const state = sk => (sk.attention ? 'attention' : sk.proposed ? 'available' : 'adopted');
  const counts = { adopted: S.meta.skills.filter(x => state(x) === 'adopted').length, available: S.meta.skills.filter(x => state(x) === 'available').length, attention: S.meta.skills.filter(x => state(x) === 'attention').length };
  const fails = Object.values(S.prs).flatMap(p => p.ci).filter(c => c.status === 'fail').length;
  const tabs = pageTabs('skills', [
    ['skills', 'Skills', counts.attention ? `${counts.attention} need${counts.attention === 1 ? 's' : ''} attention` : `${counts.adopted} adopted`, counts.attention ? 'warn' : ''],
    ['checks', 'Checks', fails ? `${fails} failed run${fails === 1 ? '' : 's'}` : 'All passing so far', fails ? 'warn' : ''],
    ['recipes', 'Recipes', `${recipes().length} to reuse`, ''],
  ], ui.tab);
  const mast = masthead('bi-lightning', 'What can my agents do again and again?', `${S.meta.skills.length} skills live in this repo. They’ve run ${S.skillRuns.filter(r => r.result === 'done').length}× so far.`);
  if (ui.tab === 'checks') return mast + tabs + checksTab();
  if (ui.tab === 'recipes') return mast + tabs + recipesTab();
  const flow = workflowSkill();
  const flowWorks = works().filter(w => followUp(w));
  const steps = [
    ['bi-sign-merge-left', 'Merged', 'GitHub merges the PR. AgentSCM sees the exact merged commit.'],
    ['bi-arrow-return-right', 'Ready', `${flow.name} waits. Nothing wakes your agents.`],
    ['bi-box-arrow-in-right', 'Checked in', 'An agent session reaches its next step, or a new one starts.'],
    ['bi-lock', 'Claimed', 'One session gets it. A second one is told “already claimed”.'],
    ['bi-file-earmark-check', 'Saved', 'It runs in that agent. The result is linked to the task.'],
  ];
  // Which step a task's workflow has reached: 0 before the merge, 4 once its result is saved.
  const at = w => { const f = followUp(w); return { idle: 0, wait: 0, ready: 1, active: 3, done: 5 }[f.s] ?? 0; };
  const bar = (w, i) => `<li class="fb-seg ${i < at(w) ? 'is-done' : ''} ${i === at(w) - 1 && at(w) < 5 ? 'is-here' : ''}"></li>`;
  return `
    ${mast}${tabs}
    <p class="adopt">${[['adopted', 'Adopted', 'ok'], ['available', 'Available', 'brand'], ['attention', 'Needs attention', 'warn']].map(([k, l, t]) => `<span class="adopt-${k}">${pill(t, `${counts[k]} ${l.toLowerCase()}`)}</span>`).join(' ')} <span class="muted small">Adopted skills are rules agents in this repo must follow.</span></p>
    ${box({ title: 'After a merge, the repo’s workflow runs', icon: 'bi-diagram-3', count: null, explain: `Five steps, left to right. Each task below is lit up to the step it has reached. This repo’s example is ${flow.name}; any skill can be a step.`, cls: 'box-wide', body: `
      <ol class="flowbar">${steps.map(([i, t, p]) => `<li><span class="fb-step"><i class="bi ${i}" aria-hidden="true"></i>${h(t)}</span><small>${h(p)}</small></li>`).join('')}</ol>
      ${flowWorks.length ? `<ul class="fb-tasks">${flowWorks.map(w => `<li data-k="fb-${h(w.id)}"><span class="fb-task">${workLink(w.id)}<small>${h(followUp(w).note)}</small></span><ol class="fb-track" aria-label="${h(w.title)}: ${h(followUp(w).note)}">${steps.map((_, i) => bar(w, i)).join('')}</ol></li>`).join('')}</ul>` : '<p class="muted small">No task has reached this step yet.</p>'}
      <p class="fineprint">${proposed()} Chaining a skill after a merge is part of the beta lifecycle proof. Today’s PR gate stages are shown exactly as configured.</p>` })}
    <ul class="skill-list">${S.meta.skills.map(s => `
      <li data-k="sk-${h(s.id)}" class="skill-row">
        <div class="skill-name">${mono(s.name)}${state(s) === 'adopted' ? pill('ok', 'Adopted') : state(s) === 'available' ? pill('brand', 'Available') : pill('warn', 'Needs attention')}${s.proposed ? proposed() : ''}</div>
        <div class="skill-main"><p class="skill-does">${h(s.does)}</p>${s.attention ? `<p class="skill-attn"><i class="bi bi-exclamation-circle" aria-hidden="true"></i>${h(s.attention)}</p>` : ''}
          <dl class="skill-facts"><div><dt>When</dt><dd>${h(s.when)}</dd></div><div><dt>Runs in</dt><dd>${h(s.runsIn)}</dd></div><div><dt>Defined in</dt><dd>${mono(s.file)} <span class="muted">pinned ${h(s.digest)}</span></dd></div></dl>
          ${runsOf(s.id).length ? `<ul class="runs">${runsOf(s.id).map(r => `<li>${pill(r.result === 'done' ? 'ok' : 'brand', r.result === 'done' ? 'Ran' : 'Started', r.result === 'done' ? 'bi-check2' : 'bi-play')} ${h(agentName(r.agent))} on ${workLink(r.work)} · ${when(r.t)}</li>`).join('')}</ul>` : '<p class="muted small">Not used yet today.</p>'}
        </div>
        <div class="skill-act">${copyBtn(s.name, 'Copy command')}</div>
      </li>`).join('')}</ul>
    <p class="fineprint"><i class="bi bi-info-circle" aria-hidden="true"></i> Skills never run in the browser. Copy the command into your agent, CLI or forge.</p>`;
}

// ---------- Insights ----------
export function insightsRows() {
  const ov = Object.values(S.overlaps), rq = Object.values(S.requests), loops = Object.values(S.loops);
  const prs = Object.values(S.prs);
  const responded = ov.filter(o => o.response);
  const kinds = [...new Set(responded.map(o => o.response.kind))];
  const ci = prs.flatMap(p => p.ci), fails = ci.filter(c => c.status === 'fail');
  const merges = prs.reduce((n, p) => n + p.commits.filter(c => c.type === 'merge-main').length, 0);
  const rounds = prs.reduce((n, p) => n + p.reviews.length, 0);
  const waits = works().filter(w => w.handover?.acceptedAt).map(w => `${w.title}: ${dur(w.handover.acceptedAt - w.handover.t)} for a new owner`);
  for (const w of works()) {
    for (const x of w.waits || []) waits.push(`#${w.pr} waited ${dur(x.to - x.from)} for “${S.work[x.on].title}”`);
    if (w.waitStart != null) waits.push(`#${w.pr} waiting ${dur(S.t - w.waitStart)} so far for “${S.work[w.waitsFor].title}”`);
  }
  for (const [wid, b] of Object.entries(S.follow)) if (b.claimedAt != null) waits.push(`${skillName(b.skill)} for #${S.work[wid].pr}: ${dur(b.claimedAt - b.readyAt)} until a session checked in`);
  const usage = S.usage ? S.usage.list.map(u => `${clientName(u.client)}: ${u.tokens != null ? Math.round(u.tokens / 1000) + 'k tokens reported' : u.none ? 'nothing reported' : 'finishes only, no token counts'}`).join(' · ') : 'Nothing reported yet';
  const covered = S.usage ? S.usage.list.filter(u => u.tokens != null).length : 0;
  return [
    { m: 'Early heads-ups', ts: ov.map(o => o.t), v: ov.length, saw: ov.length ? `${ov.length} distinct, each counted once` : 'None yet', how: 'Declared file plans that overlap', not: 'Not the number of conflicts avoided' },
    { m: 'What agents did about them', ts: responded.map(o => o.response.t), v: responded.length, saw: responded.length ? kinds.join(', ') : '—', how: 'Linked responses with who and when', not: 'Not proof the change would have collided' },
    ...(team() ? [{ m: 'Requests between agents', ts: rq.map(x => x.t), v: rq.length, saw: rq.length ? `${rq.filter(r => r.receipt).length} received, ${rq.filter(r => r.reply).length} replied` : '—', how: 'Directed requests with receipts', not: 'Reading here never answers one' }] : []),
    { m: 'Loops spotted', ts: loops.map(l => l.t), v: loops.length, saw: loops.map(l => `#${l.num}: ${l.outcome ? l.outcome.text : l.response ? 'responded' : 'open'}`).join(' · ') || '—', how: 'Repeated main merges and the same failing test', not: 'Not a measure of developer productivity', proposed: true },
    { m: 'Check runs', ts: ci.map(c => c.t), bad: fails.map(c => c.t), v: ci.length, saw: `${fails.length} failed · ${rounds} review rounds · ${merges} merges from main`, how: 'Recorded attempts on each PR', not: 'Rework recorded, not effort wasted' },
    { m: 'Skill runs', ts: S.skillRuns.filter(r => r.result === 'done').map(r => r.t), v: S.skillRuns.filter(r => r.result === 'done').length, saw: `${S.skillRuns.filter(r => r.skill === workflowSkill().id && r.result === 'done').length} workflow (${workflowSkill().name}) · ${S.races.length} duplicate claim${S.races.length === 1 ? '' : 's'} refused`, how: 'Claims and saved artifacts', not: 'Use on these tasks, not coverage of the repo' },
    { m: 'Known waits', v: waits.length, saw: waits.join(' · ') || '—', how: 'Recorded state changes', not: 'Not idle time of any person' },
    { m: 'Tokens', v: S.usage ? `${covered} of ${S.usage.list.length} tools` : '—', saw: usage, how: 'Reports from each tool', not: 'Not a bill. List price isn’t what you pay' },
  ];
}

export function insightsPage() {
  const rows = insightsRows();
  const from = Math.min(0, ...rows.flatMap(r => r.ts || []));
  return `
    ${masthead('bi-graph-up', 'Is this actually helping?', `Here’s what we saw and how we know it. Where we can’t tell yet, we say so.`)}
    ${box({ title: 'What we saw', icon: 'bi-eye', explain: 'Each number says where it comes from and what it doesn’t mean. Dots show when each one happened, from the first record to now.', cls: 'box-wide', body: `
      <ul class="ins">${rows.map(r => `<li data-k="in-${h(r.m)}" class="ins-row">
        <b class="ins-num">${h(r.v)}</b>
        <div class="ins-main"><h3>${h(r.m)} ${r.proposed ? proposed() : ''}</h3><p class="ins-saw">${h(r.saw)}</p><p class="ins-truth"><span><i class="bi bi-check2" aria-hidden="true"></i>${h(r.how)}</span><span><i class="bi bi-x" aria-hidden="true"></i>${h(r.not)}</span></p></div>
        ${r.ts ? `<span class="ins-strip" aria-hidden="true">${r.ts.map(t => `<i class="${(r.bad || []).includes(t) ? 'is-bad' : ''}" style="left:${(((t - from) / Math.max(1, S.t - from)) * 100).toFixed(2)}%"></i>`).join('')}</span>` : '<span class="ins-strip is-none">No times recorded</span>'}
      </li>`).join('')}</ul>
      <div class="ins-axis" aria-hidden="true"><span>${dayLabel(from)} ${clock(from)}</span><span>now</span></div>` })}
    ${box({ title: 'Possible savings', icon: 'bi-piggy-bank', tone: 'muted', explain: 'Kept visible so the question doesn’t hide. Nothing here is estimated yet.', cls: 'box-wide', body: `
      <ul class="savings">
        <li><strong>Conflicts avoided</strong><span>Not estimated. Needs a report from the people involved, or a documented comparison.</span></li>
        <li><strong>Tokens saved</strong><span>Unavailable until there’s a baseline of similar tasks.</span></li>
        <li><strong>Hosted CI runs reduced</strong><span>Unavailable until a replacement check is qualified as equivalent.</span></li>
        <li><strong>Money saved</strong><span>Only a measured drop on a real invoice counts. None yet.</span></li>
      </ul>` })}`;
}

// ---------- Setup ----------
export function setupPage(ui = {}) {
  const clients = [...new Set(agents().map(a => a.client))];
  const cap = { claude: [1, 1, 1, 'Opt-in'], codex: [1, 1, 0, 'No'], cursor: [1, 0, 0, 'No'] };
  const yes = v => v === 1 ? '<i class="bi bi-check-lg text-ok" aria-label="Yes"></i>' : v === 0 ? '<i class="bi bi-dash-lg muted" aria-label="No"></i>' : h(v);
  const ig = S.meta.integrations || {};
  const hk = S.meta.hooks || { p50: 0, p95: 0, calls: 0 };
  const verified = clients.map(c => ({ c, last: agents().filter(a => a.client === c && a.role !== 'subagent' && a.start >= 0).sort((x, y) => y.start - x.start)[0] }));
  const slow = hk.p95 > 250;
  const tabs = pageTabs('setup', [
    ['connection', 'Connection', S.hub.up ? 'Hub answering' : 'Hub not answering', S.hub.up ? '' : 'warn'],
    ['agents', 'Agents', `${verified.filter(v => v.last).length} of ${clients.length} tools verified`, ''],
    ['access', team() ? 'Team & access' : 'Access', team() ? `${S.meta.people.length} members` : 'Your token', ''],
  ], ui.tab);
  const tab = ['agents', 'access'].includes(ui.tab) ? ui.tab : 'connection';
  const chain = `<ol class="cc" aria-label="How this computer reaches the Hub">
      <li class="cc-node is-ok"><i class="bi bi-laptop" aria-hidden="true"></i><b>${h(S.meta.computer)}</b><small>${clients.map(clientName).join(', ')}</small></li>
      <li class="cc-link ${S.hub.up ? 'is-ok' : 'is-bad'}" aria-hidden="true"><span>${S.hub.up ? 'hooks report' : 'not answering'}</span></li>
      <li class="cc-node ${S.hub.up ? 'is-ok' : 'is-bad'}"><i class="bi bi-hdd-stack" aria-hidden="true"></i><b>Hub</b><small>${S.hub.up ? 'Connected' : `Last update ${ago(S.hub.since)}`}</small></li>
      <li class="cc-link ${S.hub.up ? 'is-ok' : 'is-off'}" aria-hidden="true"><span>${S.hub.up ? 'reads every 5 min' : 'paused'}</span></li>
      <li class="cc-node ${S.hub.up ? 'is-ok' : 'is-off'}"><i class="bi bi-github" aria-hidden="true"></i><b>GitHub</b><small>${h(S.meta.repo)}</small></li>
    </ol>`;
  const conns = [
    ['bi-slack', 'Slack direct messages', ig.slack, 'A DM with handoff buttons when two agents plan the same file or work is handed over.', 'agentscm slack connect'],
    ['bi-menu-app', 'macOS menu bar app', ig.tray, 'Shows connection health, lists checkouts to connect, and opens this page.', 'agentscm tray install'],
    ['bi-hdd-network', 'Native workspace', ig.workspace, 'Its own sign-in and roles. Contributors can start and cancel server runs. No server runs yet.', 'agentscm workspace sign-in'],
  ];
  const parts = {
    connection: `${chain}
      ${box({ title: 'Is the Hub OK?', icon: 'bi-heart-pulse', tone: S.hub.up ? (slow ? 'warn' : 'ok') : 'bad', explain: 'Measured by the Hub itself.', body: `<dl class="facts">
        <div><dt>Status</dt><dd>${S.hub.up ? pill('ok', `Answering since ${clock(S.hub.since)}`, 'bi-wifi') : pill('bad', `Not answering since ${clock(S.hub.since)}`, 'bi-wifi-off')}</dd></div>
        <div><dt>Hook speed</dt><dd>Usually ${hk.p50} ms, ${hk.p95} ms at the slow end, over ${hk.calls.toLocaleString('en')} calls. ${slow ? 'That’s slow enough for agents to notice.' : 'Agents won’t notice it.'}</dd></div>
        <div><dt>Since it started</dt><dd>${Object.keys(S.overlaps).length} heads-ups, ${Object.keys(S.requests).length} requests between agents, ${works().filter(w => w.handover).length} handover${works().filter(w => w.handover).length === 1 ? '' : 's'}</dd></div>
        <div><dt>GitHub reading</dt><dd>${(c => pill(c.gh.tone, c.gh.text))(coverage())}</dd></div>
      </dl>` })}
      <ul class="conns">${conns.map(([i, t, on, d, cmd]) => `<li class="${on ? 'is-on' : ''}"><i class="bi ${i}" aria-hidden="true"></i><div><b>${h(t)}</b> ${on ? pill('ok', 'On') : pill('muted', 'Not set up')}<p>${h(d)}</p></div>${on ? '' : copyBtn(cmd, 'Copy set-up command')}</li>`).join('')}</ul>`,
    agents: `<div class="setup-grid">
      ${box({ title: 'Verified sessions', icon: 'bi-patch-check', explain: 'A tool counts as set up once one of its sessions reported a start through the hooks.', body: `<ul class="vs">${verified.map(v => `<li>${v.last ? avatar(v.last.id) : '<span class="av av-system" aria-hidden="true"><i class="bi bi-question-lg"></i></span>'}<div><b>${h(clientName(v.c))}</b><small>${v.last ? `Last verified ${when(v.last.start)} · ${h(agentName(v.last.id))}` : 'No verified session yet'}</small></div>${v.last ? pill('ok', 'Verified', 'bi-patch-check') : copyBtn(`agentscm client setup ${v.c}`, 'Copy set-up command')}</li>`).join('')}</ul>${copyBtn('agentscm client setup', 'Connect another agent')}` })}
      ${box({ title: 'What each agent tool shares', icon: 'bi-robot', explain: 'Coverage differs by tool, so it’s listed per tool.', body: `<div class="table-responsive"><table class="table caps"><thead><tr><th scope="col">Tool</th><th scope="col">Tasks</th><th scope="col">File plans</th><th scope="col">Subagents</th><th scope="col">Chat text</th></tr></thead><tbody>${clients.map(c => `<tr><th scope="row">${h(clientName(c))}</th>${cap[c].map(v => `<td>${yes(v)}</td>`).join('')}</tr>`).join('')}</tbody></table></div>` })}
      ${box({ title: 'Chat sharing', icon: 'bi-chat-square-text', tone: 'muted', explain: 'Off unless you turn it on for this checkout.', body: `<ul class="plain-list"><li>Secrets are removed before anything leaves your computer. This is best effort.</li><li>Only the person who shared a chat, and admins, can read it.</li><li>Shared chats are kept for 30 days. Turning sharing off doesn’t delete copies already kept.</li><li>Sending usage numbers is a separate choice.</li></ul>${copyBtn('agentscm change-capture set-mode shared-conversation', 'Copy turn-on command')}` })}
      ${box({ title: 'This computer', icon: 'bi-laptop', body: `<dl class="facts"><div><dt>Repository</dt><dd>${mono(S.meta.repo)}</dd></div><div><dt>Computer</dt><dd>${h(S.meta.computer)}</dd></div><div><dt>Hooks</dt><dd>11 groups, including session start and end, subagent start and stop, compaction</dd></div></dl>${copyBtn('agentscm open', 'Copy “agentscm open”')}` })}
    </div>`,
    access: `<div class="setup-grid">
      ${box({ title: 'Your token', icon: 'bi-key', explain: 'What your agents use to talk to the Hub.', body: `<dl class="facts"><div><dt>Kind</dt><dd>${S.meta.token?.kind === 'writable' ? 'Writable: agents can claim work and send replies' : 'Read-only: inspect only'}</dd></div><div><dt>Made on</dt><dd>${h(S.meta.token?.made || S.meta.computer)}</dd></div><div><dt>Expires in</dt><dd>${h(S.meta.token?.expires || 'unknown')}</dd></div></dl>${copyBtn('agentscm token rotate', 'Copy rotate command')}` })}
      ${team() ? box({ title: 'Members', icon: 'bi-people', explain: 'Roles decide who can read, contribute or administer.', body: `<ul class="team-list">${S.meta.people.map(p => `<li><span class="av av-person" aria-hidden="true">${h(p.short[0])}</span><span>${h(p.name)}</span><span class="muted">${h(p.city)} · ${clock(S.t, p.utc)}</span>${pill(S.meta.roles?.[p.id] === 'Admin' ? 'brand' : 'muted', S.meta.roles?.[p.id] || 'Member')}</li>`).join('')}</ul>` }) : ''}
      ${box({ title: 'Native workspace', icon: 'bi-hdd-network', tone: 'muted', explain: 'Separate sign-in and permissions. You don’t need it to start.', body: `<p class="muted">${ig.workspace ? 'Signed in. You can inspect its repositories; starting or cancelling runs needs the contributor role. No server runs yet.' : 'Not set up. It has its own sign-in and its own permissions, separate from Git.'}</p>${copyBtn('agentscm workspace sign-in', 'Copy sign-in command')}` })}
    </div>`,
  };
  return `${masthead('bi-plug', 'Is everything connected?', S.hub.up ? `Yes. ${h(S.meta.repo)} on ${h(S.meta.computer)} is talking to the Hub.` : `Not right now. The Hub stopped answering ${ago(S.hub.since)}. You’re seeing the last good update.`)}${tabs}${parts[tab]}`;
}

// What each page is for, in three short steps. Opened from the top bar's help button.
const WALK = {
  home: ['The sentence on top says what’s happening and links to what needs you.', 'Four tabs split the rest by question: Now, Where, Catch up and Team. Each shows its status.', 'Every task, face and link opens the task page with the full story.'],
  inbox: ['Decisions that wait on you, grouped by how urgent they are.', 'Pick one to see why it matters and the recommended choice.', 'Answers are queued for the agent’s next check-in. Nothing wakes it up.'],
  history: ['Every session you can see, grouped by day, with when it ran.', 'Search by tool, day, file or words. Files shows each file’s story.', 'Chat text from other people is never shown.'],
  timeline: ['Each dot is a saved moment: a checkpoint, a commit or a handover.', 'Pick one, then compare it with now or the moment before.', 'Copy the restore command to start again from it in your agent.'],
  skills: ['Skills are repo steps agents can repeat. Adopted ones are rules.', 'Checks shows every pull request’s gate and every check attempt.', 'Recipes are reusable sequences of steps.'],
  insights: ['Each number says where it comes from and what it doesn’t mean.', 'Dots show when it happened.', 'Savings stay “not estimated” until there’s real evidence.'],
  setup: ['Connection shows the path from this computer to the Hub and GitHub.', 'Agents shows which tools are verified and what each shares.', 'Access shows your token, members and the native workspace.'],
  task: ['The bar shows where the task is; the facts say what “done” really means.', 'The thread holds every chat, commit and heads-up, one rail per agent.', 'Ready to hand off? shows the change record and what backs it.'],
  session: ['The chat, if it’s yours. Otherwise who, when and which files.', 'Files and subagents are on the right.', 'Copy the resume command to pick it up in your agent.'],
};
export const walkthrough = route => WALK[route] || WALK.home;

// ---------- first run ----------
export function firstRun() {
  return `<div class="first-run">
    <h2>Your agents will show up here by themselves.</h2>
    <p>${h(S.meta.repo)} is connected. Ask any connected agent to start a task. When it says what it plans to change, you’ll see it here, with the files it plans to touch.</p>
    ${copyBtn('Start an AgentSCM task for what you’re about to do, and declare the files you plan to change.', 'Copy a first prompt')}
    <ol class="steps-plain"><li><strong>Coordinate.</strong> See every agent and get a heads-up before two of them edit the same file.</li><li><strong>Keep.</strong> Find any session again by file, tool or day.</li><li><strong>Repeat.</strong> Turn steps you do every time into repo skills.</li><li><strong>Learn.</strong> See what helped, and what we can’t measure yet.</li></ol>
  </div>`;
}

// ---------- since you last checked (mirrors the feat/durable-work-catchup branch) ----------
// Per person and per story. shortcut: stored per browser; a real Hub keeps one review cursor per user.
const local = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* storage unavailable */ } },
};
const reviewKey = () => `agentscm.hub.lastReviewed.${S.meta.id}`;
export function lastReviewed() { const v = local.get(reviewKey()); return v == null ? 0 : Math.min(Number(v), S.t); }
// Keep the window you just reviewed so you can look back at it. Looking back never moves the review position.
const prevKey = () => `agentscm.hub.prevReviewed.${S.meta.id}`;
export let peekReviewed = false;
export const togglePeek = () => { peekReviewed = !peekReviewed; };
export let movedElsewhere = false;
export const noteMovedElsewhere = () => { movedElsewhere = true; };
export function markReviewed() {
  const to = Math.max(Number(local.get(reviewKey()) || 0), Math.floor(S.t));
  local.set(prevKey(), `${lastReviewed()}-${to}`); local.set(reviewKey(), String(to)); peekReviewed = false; movedElsewhere = false;
}
// What the catch-up could and couldn't read, per source. GitHub reading can fall behind; the Hub can stop answering.
export function coverage() {
  const f = S.coverage.forge_pull;
  const gh = !S.hub.up ? { tone: 'bad', text: `GitHub merges: unavailable. The Hub isn’t answering (last update ${ago(S.hub.since)}).` }
    : f.state === 'incomplete' ? { tone: 'warn', text: `GitHub merges: incomplete. ${f.unread ? `${f.unread} page${f.unread === 1 ? '' : 's'} unread` : ''}${f.unread && f.deferred ? ', ' : ''}${f.deferred ? `${f.deferred} merge${f.deferred === 1 ? '' : 's'} deferred` : ''}.` }
    : { tone: 'ok', text: `GitHub merges: complete (checked ${ago(Math.max(0, S.t - 2))}).` };
  const gap = S.meta.retainedFrom != null && lastReviewed() < S.meta.retainedFrom;
  return { gh, gap, complete: gh.tone === 'ok' && !gap };
}
// Before an agent starts work, the Hub checks its declared files. Three plain answers, matching the Hub's preflight.
export function startCheck(w) {
  if (w.state === 'finished') return null;
  if (Object.values(S.overlaps).some(o => o.state === 'open' && (o.a === w.id || o.b === w.id))) return { k: 'review_required', tone: 'warn', text: 'Start check: review first', why: 'Another task plans the same file.' };
  // Only live GitHub reading matters here; old history no longer being kept doesn't affect today's file check.
  if (coverage().gh.tone !== 'ok') return { k: 'unknown', tone: 'muted', text: 'Start check: can’t tell', why: 'GitHub reading is behind, so this isn’t a clear.' };
  return { k: 'clear_within_checked_window', tone: 'ok', text: 'Start check: clear so far', why: 'No overlap in what the Hub could check.' };
}
function prevReviewed() {
  const [from, to] = String(local.get(prevKey()) || '').split('-').map(Number);
  if (Number.isFinite(to) && to <= S.t && to === lastReviewed()) return { from, to };
  return lastReviewed() ? { from: 0, to: lastReviewed() } : null;
}

// What changed for this person since a moment, as plain sentences that each link to their proof.
export function sinceItems(since) {
  const ev = S.log.filter(e => e.t > Math.max(since, S.meta.retainedFrom ?? -Infinity));
  const out = [];
  const GROUP = { 'pr.merge': 'merge', stage: 'merge', 'overlap.detect': 'coord', 'overlap.resolve': 'coord', 'request.send': 'coord', 'request.reply': 'coord' };
  const push = (e, icon, tone, text, href) => out.push({ t: e.t, icon, tone, text, href, group: GROUP[e.kind] || 'work' });
  for (const e of ev) {
    const wid = taskOf(e), href = wid && S.work[wid] ? taskHref(wid) : null;
    if (e.kind === 'pr.merge') push(e, 'bi-sign-merge-left', 'ok', `#${e.num} merged: “${S.work[S.prs[e.num].work].title}” · ${e.rev.slice(0, 7)} · changed ${S.work[S.prs[e.num].work].scope.slice(0, 2).join(', ')}`, href);
    else if (e.kind === 'work.update' && e.patch?.state === 'finished') push(e, 'bi-check2-circle', 'ok', `${agentName(S.work[e.id].lead)} says “${S.work[e.id].title}” is done`, href);
    else if (e.kind === 'overlap.detect') push(e, 'bi-sign-intersection', 'warn', `New heads-up: two agents plan to edit ${e.overlap.paths.join(', ')}`, taskHref(e.overlap.a));
    else if (e.kind === 'overlap.resolve' && e.say) push(e, 'bi-check2', 'ok', e.say, null);
    else if (e.kind === 'loop.detect') push(e, 'bi-arrow-repeat', 'bad', `PR #${e.loop.num} got stuck in a loop`, href);
    else if (e.kind === 'loop.outcome') push(e, 'bi-check2-all', 'ok', `The loop on #${S.loops[e.id].num} ended: ${e.outcome}`, taskHref(S.loops[e.id].work));
    else if (e.kind === 'handover.offer') push(e, 'bi-box-arrow-right', 'brand', `“${S.work[e.id].title}” was handed over`, href);
    else if (e.kind === 'handover.accept') push(e, 'bi-box-arrow-in-right', 'ok', `${agentName(e.to)} picked up “${S.work[e.id].title}”`, href);
    else if (e.kind === 'stage' && e.status === 'done') push(e, 'bi-arrow-return-right', 'ok', `Workflow step ${skillName(e.key)} finished for #${S.work[e.work].pr}`, taskHref(e.work));
    else if (e.kind === 'work.start') push(e, 'bi-flag', '', `${agentName(e.work.lead)} started “${e.work.title}”`, taskHref(e.work.id));
    else if (e.kind === 'ci' && e.status === 'fail') push(e, 'bi-x-octagon', 'bad', `Checks failed on #${e.num}`, href);
    else if (e.kind === 'work.checkin') push(e, 'bi-chat-left-text', '', `Checkpoint reported on “${S.work[e.id].title}”: ${e.note}`, href);
    else if (e.kind === 'request.send') push(e, 'bi-send', 'warn', `Coordination requested: ${agentName(e.request.from)} asked ${agentName(e.request.to)} “${e.request.ask}”`, taskHref(S.overlaps[e.request.overlap].a));
    else if (e.kind === 'request.reply') push(e, 'bi-reply', '', `Reply to a heads-up: “${e.reply}”`, taskHref(S.overlaps[S.requests[e.id].overlap].a));
  }
  const replies = Object.values(S.convs).filter(canRead).map(c => ({ c, n: (S.chats[c.id] || []).filter(m => m.t > since && m.role !== 'you' && m.role !== 'tool').length })).filter(x => x.n);
  for (const r of replies) out.push({ t: S.chats[r.c.id].at(-1).t, icon: 'bi-chat-left-text', tone: '', text: `${r.n} new ${r.n === 1 ? 'reply' : 'replies'} in “${r.c.title}”`, href: `#/session/${r.c.agent}`, group: 'work' });
  return out.sort((a, b) => b.t - a.t);
}

// The catch-up summary: a sentence, then a picture of the time you were away. Three lanes (what merged, how work
// moved, what agents said to each other) with one dot per saved record at the moment it happened. Lane names filter the list.
const SINCE_GROUPS = [['merge', 'Merged'], ['work', 'Work'], ['coord', 'Between agents']];
let sinceTab = '';
export const setSinceTab = k => { sinceTab = sinceTab === k ? '' : k; };
export function sinceCard({ compact = false } = {}) {
  const since = lastReviewed();
  const cov = coverage();
  const prev = prevReviewed();
  const peek = peekReviewed && prev;
  const from = peek ? prev.from : since, to = peek ? prev.to : S.t;
  const items = peek ? sinceItems(prev.from).filter(i => i.t <= prev.to) : sinceItems(since);
  const by = Object.fromEntries(SINCE_GROUPS.map(([k]) => [k, items.filter(i => i.group === k)]));
  const prs = [...new Set(by.merge.map(i => i.text.match(/#\d+/)?.[0]).filter(Boolean))];
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
  const bad = items.find(i => i.tone === 'bad');
  const summary = [
    prs.length ? `<b>${prs.join(' and ')}</b> merged` : 'Nothing merged',
    by.work.length && `<b>${plural(by.work.length, 'update', 'updates')}</b> on the work`,
    by.coord.length && `<b>${plural(by.coord.length, 'message', 'messages')}</b> between agents`,
  ].filter(Boolean).join(', ').replace(/, ([^,]*)$/, ' and $1');
  const x = t => (to > from ? ((t - from) / (to - from)) * 100 : 100).toFixed(2);
  const tab = by[sinceTab]?.length ? sinceTab : '';
  const shown = tab ? by[tab] : items;
  const limit = compact ? 3 : 5;
  const back = prev ? `<button type="button" class="btn btn-sm btn-link since-peek" data-peek="1">${peek ? 'Back to what’s new' : `See what you reviewed at ${clock(prev.to)}`}</button>` : '';
  const lanes = SINCE_GROUPS.map(([k, label]) => `<li class="sl sl-${k} ${tab === k ? 'is-on' : ''} ${tab && tab !== k ? 'is-dim' : ''}">
      <button type="button" class="sl-name" data-since-tab="${k}" aria-pressed="${tab === k}" ${by[k].length ? '' : 'disabled'}>${label}<span>${by[k].length}</span></button>
      <span class="sl-rail" aria-hidden="true">${by[k].map(i => `<i class="sl-dot tone-${i.tone || 'plain'}" style="left:${x(i.t)}%" title="${clock(i.t)} · ${h(i.text)}"></i>`).join('')}</span></li>`).join('');
  const li = i => `<li data-k="sn-${i.t}-${h(i.text.slice(0, 18))}" class="tone-${i.tone || 'plain'}"><i class="bi ${i.icon}" aria-hidden="true"></i>${i.href ? `<a href="${h(i.href)}">${h(i.text)}</a>` : `<span>${h(i.text)}</span>`}<time>${clock(i.t)}</time></li>`;
  return `<section class="since ${items.length ? '' : 'since-quiet'} ${compact ? 'since-compact' : ''} ${peek ? 'is-peek' : ''}" aria-label="${peek ? 'Already reviewed' : 'Since you last checked'}">
    <div class="since-head">
      <div class="since-title"><b>${peek ? 'Already reviewed' : 'Since you last checked'}</b>${inReview()}<span class="since-range">${from ? clock(from) : `${weekday(0)} ${clock(0)}`} <i class="bi bi-arrow-right" aria-hidden="true"></i> ${peek ? clock(to) : 'now'}${peek ? '' : from ? ` · marked ${h(ago(from))}` : ' · nothing marked yet'}</span></div>
      ${peek ? '' : `<button type="button" class="btn btn-sm since-done" data-reviewed="1"><i class="bi bi-check2" aria-hidden="true"></i><span>${cov.gap ? 'Mark available records reviewed' : 'Mark reviewed'}</span></button>`}
    </div>
    ${movedElsewhere && !peek ? `<p class="since-note tone-brand"><i class="bi bi-arrow-repeat" aria-hidden="true"></i>Your review mark moved on another tab or device. Showing from there. It never moves backwards.</p>` : ''}
    ${cov.gap && !peek ? `<p class="since-note tone-warn"><i class="bi bi-hourglass-bottom" aria-hidden="true"></i>Records before ${clock(S.meta.retainedFrom)} are no longer kept, so this can’t be the full story since you last checked.</p>` : ''}
    ${items.length ? `<p class="since-headline">${summary}.${bad ? ` <span class="since-bad">Needs a look: ${h(bad.text)}.</span>` : ''}</p>
    <div class="since-strip"><ul class="sl-lanes">${lanes}</ul><div class="sl-axis" aria-hidden="true"><span>${from ? clock(from) : clock(0)}</span><span>${peek ? clock(to) : 'now'}</span></div></div>
    <div class="since-detail"><p class="since-showing">${tab ? `Showing ${SINCE_GROUPS.find(g => g[0] === tab)[1].toLowerCase()} only · <button type="button" class="btn btn-sm btn-link" data-since-tab="${tab}">Show everything</button>` : 'Newest first. Pick a lane to narrow it.'}</p>
    <ul class="since-list">${shown.slice(0, limit).map(li).join('')}</ul>${shown.length > limit ? `<a class="small since-more" href="#/inbox">${shown.length - limit} more in your inbox</a>` : ''}</div>`
    : `<p class="since-headline since-none">${cov.complete ? `Nothing new since ${clock(from)}.` : `Nothing new that AgentSCM could read since ${clock(from)}. It can’t say this is everything.`} AgentSCM only knows what it saved.</p>`}
    <div class="since-foot"><ul class="since-cov"><li class="tone-ok"><i aria-hidden="true"></i>Saved work and team messages: complete.</li><li class="tone-${cov.gh.tone}"><i aria-hidden="true"></i>${h(cov.gh.text)}</li><li class="tone-plain"><i aria-hidden="true"></i>Looking never marks anything as read.</li></ul>${back}</div>
  </section>`;
}

// ---------- inbox: decisions that wait on you ----------
const answersKey = () => `agentscm.hub.answers.${S.meta.id}`;
export function answers() { try { return JSON.parse(local.get(answersKey()) || '{}'); } catch { return {}; } }
export function saveAnswer(id, choice) { const a = answers(); a[id] = { choice, t: Math.floor(S.t) }; local.set(answersKey(), JSON.stringify(a)); }

const mineWork = wid => { const w = S.work[wid]; return w && (w.person === S.you || w.participants.some(a => S.agents[a]?.person === S.you)); };
export const isMine = a => !team() || a.id.startsWith('ho-') || a.works.some(mineWork);

// Turns an alert into a question with plain choices, one recommended.
export function decisionFor(a) {
  const w = S.work[a.works[0]];
  if (a.id.startsWith('O-')) {
    const o = S.overlaps[a.id];
    const mine = mineWork(o.a) ? S.work[o.a] : S.work[o.b];
    const other = mine.id === o.a ? S.work[o.b] : S.work[o.a];
    const later = mine.started > other.started;
    return { asker: mine.lead, w: mine, question: `Two agents plan to edit ${o.paths.join(', ')}. What should ${agentName(mine.lead)} do?`, choices: [
      { id: 'narrow', label: 'Step back from that file', detail: `Keep working, but leave ${o.paths[0].split('/').pop()} to “${other.title}”.`, rec: later },
      { id: 'handover', label: 'Take the file, ask the other agent to step back', detail: `Sends a request to ${agentName(other.lead)}.`, rec: !later },
      { id: 'join', label: 'Work on it together', detail: `Both tasks change it, and say so in their plans.` },
      { id: 'ignore', label: 'Go ahead anyway', detail: 'Only if you know the edits won’t touch the same lines.' },
    ] };
  }
  if (a.id.startsWith('L-')) {
    const l = S.loops[a.id];
    return { asker: S.work[l.work].lead, w: S.work[l.work], question: `PR #${l.num} keeps merging main and failing the same test. What next?`, choices: [
      { id: 'wait', label: `Wait for “${S.work[l.blockedBy].title}”, then merge main once`, detail: 'The failing test belongs to that task. Stop the merge-and-retry loop.', rec: true },
      { id: 'quarantine', label: 'Skip the flaky test for now and keep going', detail: 'Faster, but the test stays flaky for everyone.' },
      { id: 'retry', label: 'Keep merging main and retrying', detail: 'This is what has failed three times.' },
    ] };
  }
  if (a.id.startsWith('ho-')) return { asker: w.handover.from, w, question: `“${w.title}” needs a new owner. Pick it up?`, choices: [
    { id: 'take', label: 'Pick it up with my agent', detail: `Your agent reads the handover note first: “${w.handover.note}”`, rec: true },
    { id: 'leave', label: 'Leave it for someone else', detail: 'It stays assigned to the last owner until someone accepts it.' },
  ] };
  if (a.id.startsWith('fl-')) { const f = followUp(w); return { asker: w.lead, w, question: `${f.skill.name} is ready for PR #${w.pr}. Run it?`, choices: [
    { id: 'run', label: 'Run it at my agent’s next check-in', detail: f.skill.does, rec: true },
    { id: 'skip', label: 'Skip it this time', detail: 'The step is marked skipped on this task.' },
  ] }; }
  if (a.id.startsWith('ci-')) return { asker: w.lead, w, question: `Checks failed on PR #${w.pr}. What should happen?`, choices: [
    { id: 'fix', label: 'Ask my agent to fix it', detail: a.plain, rec: true },
    { id: 'rerun', label: 'Run the checks again', detail: 'Only if you think the failure is flaky.' },
    { id: 'self', label: 'I’ll look at it myself', detail: 'Your agent leaves it alone.' },
  ] };
  if (a.id.startsWith('st-')) return { asker: null, w, question: a.title + '. What now?', choices: [
    { id: 'ask', label: `Ask ${personName(w.person)} if it’s still being worked on`, detail: 'The task stays assigned until they answer.', rec: true },
    { id: 'wait', label: 'Wait another hour', detail: 'Nothing changes.' },
  ] };
  return { asker: w?.lead, w, question: a.title, choices: [] };
}

export function inboxModel() {
  const ans = answers();
  const all = attention();
  const open = all.filter(a => !ans[a.id]);
  return {
    stopped: open.filter(a => isMine(a) && a.tone === 'bad'),
    waiting: open.filter(a => isMine(a) && a.tone !== 'bad'),
    team: open.filter(a => !isMine(a)),
    done: all.filter(a => ans[a.id]).map(a => ({ ...a, answer: ans[a.id] })),
    fyi: sinceItems(lastReviewed()),
  };
}
export const inboxCount = () => { const m = inboxModel(); return m.stopped.length + m.waiting.length + m.team.length; };

export function inboxPage(ui, selId = '') {
  const m = inboxModel();
  const actionable = [...m.stopped, ...m.waiting, ...m.team];
  const sel = actionable.find(a => a.id === selId) || m.done.find(a => a.id === selId) || (selId ? null : actionable[0]);
  const row = a => `<li><a href="#/inbox/${h(a.id)}" class="ib-row tone-${a.tone} ${sel?.id === a.id ? 'on' : ''}" ${sel?.id === a.id ? 'aria-current="true"' : ''}><span class="ib-task">${h(S.work[a.works[0]]?.title || '')}</span><b>${h(decisionFor(a).question)}</b><span class="ib-meta">${a.answer ? `Answered: ${h(decisionFor(a).choices.find(c => c.id === a.answer.choice)?.label || a.answer.choice)}` : h(a.status)}<time>${h(ago(a.answer?.t ?? a.t))}</time></span></a></li>`;
  const group = (title, icon, list, cls = '') => list.length ? `<li class="ib-group ${cls}"><h3><i class="bi ${icon}" aria-hidden="true"></i>${h(title)}<span>${list.length}</span></h3><ul>${list.map(row).join('')}</ul></li>` : '';
  const empty = !actionable.length && !m.done.length;
  return `
    ${masthead('bi-inbox', 'What needs my decision?', m.stopped.length + m.waiting.length ? `<b>${m.stopped.length + m.waiting.length} decision${m.stopped.length + m.waiting.length === 1 ? '' : 's'}</b> ${m.stopped.length + m.waiting.length === 1 ? 'waits' : 'wait'} on you${m.stopped.length ? `, and <b>${m.stopped.length}</b> ${m.stopped.length === 1 ? 'is' : 'are'} holding work up` : ''}.` : `Nothing waits on you.${m.team.length ? ` ${m.team.length} ${m.team.length === 1 ? 'is' : 'are'} for the team.` : ' Your agents can carry on.'}`)}
    <div class="ib-progress">
      <ol class="ib-bar" aria-hidden="true">${[...m.stopped.map(() => 'bad'), ...m.waiting.map(() => 'wait'), ...m.team.map(() => 'team'), ...m.done.map(() => 'done')].map(k => `<li class="ib-seg ib-${k}"></li>`).join('')}</ol>
      <p>${m.done.length ? `<b>${m.done.length} decided</b>` : 'Nothing decided yet'}${m.stopped.length + m.waiting.length ? ` · <b>${m.stopped.length + m.waiting.length} on you</b>` : ''}${m.team.length ? ` · ${m.team.length} for the team` : ''}. What happened while you were away is on the <a href="#/home/catchup">dashboard’s Catch up</a>.</p>
    </div>
    <div class="inbox ${selId ? 'has-sel' : ''}">
      <nav class="ib-list" aria-label="Decisions">
        <ul>${group('Stopped on you', 'bi-sign-stop', m.stopped, 'g-bad')}${group('Waiting on you', 'bi-hourglass-split', m.waiting, 'g-wait')}${group(team() ? 'For the team' : 'Other', 'bi-people', m.team, 'g-team')}${group('Done', 'bi-check2-all', m.done, 'g-done')}</ul>
        ${empty ? '<p class="empty-line p-3">Nothing has needed you yet. Decisions land here as soon as an agent needs one.</p>' : ''}
      </nav>
      <section class="ib-read" aria-live="polite">${sel ? decisionPane(sel, ui) : `<div class="ib-none"><i class="bi bi-check2-circle" aria-hidden="true"></i><p>${selId ? 'This one has already been settled. Nothing to decide.' : 'Pick a decision on the left.'}</p>${selId ? '<a class="ib-back d-inline-flex" href="#/inbox">All decisions</a>' : ''}</div>`}</section>
    </div>`;
}

function decisionPane(a, ui) {
  const d = decisionFor(a);
  const ans = answers()[a.id];
  const picked = ui.pick?.[a.id] || ans?.choice || d.choices.find(c => c.rec)?.id;
  const next = inboxModel(); const queue = [...next.stopped, ...next.waiting].filter(x => x.id !== a.id);
  const chosen = d.choices.find(c => c.id === picked);
  return `
    <a class="ib-back" href="#/inbox"><i class="bi bi-arrow-left" aria-hidden="true"></i>All decisions</a>
    <div class="ib-from">${d.asker ? avatar(d.asker) : '<span class="av av-system" aria-hidden="true"><i class="bi bi-broadcast"></i></span>'}<div><b>${h(d.asker ? agentName(d.asker) : 'AgentSCM')}</b><span>${d.w ? `on <a href="${taskHref(d.w.id)}">${h(d.w.title)}</a>` : ''} · ${when(a.t)}</span></div>${a.proposed ? proposed() : ''}</div>
    <h2 class="ib-q">${h(d.question)}</h2>
    <p class="ib-why"><i class="bi ${a.icon}" aria-hidden="true"></i>${h(a.plain)}</p>
    ${ans ? `<div class="ib-answered"><i class="bi bi-send-check" aria-hidden="true"></i><div><b>You answered: ${h(chosen?.label || ans.choice)}</b><span>Queued ${h(ago(ans.t))}. ${d.asker ? h(agentName(d.asker)) : 'The agent'} sees it at its next check-in. Nothing wakes it up.</span></div></div>` : ''}
    <fieldset class="ib-choices" ${ans ? 'disabled' : ''}><legend class="visually-hidden">Choices</legend>
      ${d.choices.map(c => `<label class="ib-choice ${picked === c.id ? 'on' : ''}"><input type="radio" name="pick-${h(a.id)}" value="${h(c.id)}" data-pick="${h(a.id)}" ${picked === c.id ? 'checked' : ''}><span><b>${h(c.label)}${c.rec ? ' <em class="rec">Recommended</em>' : ''}</b><small>${h(c.detail)}</small></span></label>`).join('')}
    </fieldset>
    ${ans ? `<div class="ib-actions">${queue.length ? `<a class="btn btn-sm btn-outline-brand" href="#/inbox/${h(queue[0].id)}">Next decision <i class="bi bi-arrow-right" aria-hidden="true"></i></a>` : ''}<button type="button" class="btn btn-sm btn-link" data-unanswer="${h(a.id)}">Undo</button></div>`
      : `<div class="ib-actions">
          <button type="button" class="btn btn-primary ib-send" data-answer="${h(a.id)}" data-choice="${h(picked)}"><i class="bi bi-send" aria-hidden="true"></i>${chosen?.rec ? 'Accept recommended' : 'Send my choice'}</button>
          ${copyBtn(`${d.question} Decision: ${chosen?.label || ''}. ${chosen?.detail || ''}`)}
          <a class="btn btn-sm btn-link" href="${d.w ? taskHref(d.w.id) : '#/home'}">Open the task</a>
        </div>
        <p class="fineprint"><span class="pill pill-proposed">Proposed</span> Sending an answer from the Hub is new: it’s queued and delivered when the agent next checks in. Until that ships, copy it into your agent.</p>`}`;
}

// ---------- app shell ----------
const VIEW = { name: 'Dashboard' };
const SKINS = [
  { k: '', name: 'Hub', line: 'Today’s look: calm violet and system type.' },
  { k: 'dusk', name: 'Dusk', line: 'Dark violet-grey, lavender and coral, roomy. Graphite meets Glance.' },
  { k: 'mist', name: 'Mist', line: 'Light and airy, soft violet, quiet metro lines.' },
  { k: 'twilight', name: 'Twilight', line: 'Deep-violet frame around Mist’s content (light) or Dusk’s (dark).' },
  { k: 'graphite', name: 'Graphite (B)', line: 'Professional, dark. Violet-grey, status dots.' },
  { k: 'glance', name: 'Glance (C)', line: 'Electric blue and coral, metro-line progress, live strip.' },
];
const POPUP_KEY = 'agentscm.hub.alertPopups';
// shortcut: per-browser like the shipped density setting; move to a per-user Hub preference when accounts store settings.
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* storage unavailable */ } },
};

export const loadView = async () => import('./dashboard.js');

export function startApp(option) {
  const embed = new URLSearchParams(location.search).has('embed');
  const view = VIEW;
  const ui = { q: '', hf: 'all', lens: 'who', back: null, anchor: null, tf: 'all' };
  let known = null, prevAlerts = null, openNew = null, menuSig = '', lastHash = '';
  let popups = store.get(POPUP_KEY) !== 'off';
  document.body.classList.toggle('embed', embed);
  const setTitle = () => { document.title = `AgentSCM Hub · ${view.name}`; };
  setTitle();

  const top = document.getElementById('top');
  const rail = document.getElementById('rail');
  const main = document.getElementById('main');
  const page = document.getElementById('page');
  const toasts = document.getElementById('toasts');
  const path = () => (location.hash.replace(/^#\/?/, '') || 'home').split('?')[0].split('/');
  const route = () => path()[0];
  const routeArg = () => decodeURIComponent(path()[1] || '');
  const ANCHOR = { stage: 'thread', area: 'files' };
  const backLabel = h2 => {
    const [r, arg] = (h2.replace(/^#\/?/, '') || 'home').split('/');
    if (r === 'home') return `Back to ${view.name}`;
    if (r === 'task') return `Back to ${S.work[decodeURIComponent(arg)]?.title || 'the last task'}`;
    if (r === 'session') return 'Back to the session';
    return `Back to ${r[0].toUpperCase()}${r.slice(1)}`;
  };
  // A task link remembers where it came from, so the page can lead back there.
  function openTask(id, focus) {
    ui.anchor = ANCHOR[focus] || null;
    if (location.hash === taskHref(id)) { document.getElementById(ui.anchor || 'top')?.scrollIntoView({ block: 'start' }); ui.anchor = null; return; }
    location.hash = taskHref(id);
  }
  const qs = () => (embed ? `?embed${skin() ? `&skin=${encodeURIComponent(skin())}` : ''}` : '');
  // Only a known skin key ever leaves this function; the attribute itself can come from the URL.
  const skin = () => { const k = document.documentElement.getAttribute('data-skin') || ''; return SKINS.some(x => x.k === k) ? k : ''; };
  const density = () => document.documentElement.getAttribute('data-density') || 'spacious';

  // ----- alerts: unread is per user (per browser here) and per story
  const seenKey = () => `agentscm.hub.seenAlerts.${S.meta.id}`;
  const seen = () => { try { return new Set(JSON.parse(store.get(seenKey()) || '[]')); } catch { return new Set(); } };
  function markSeen(ids) { const s = seen(); ids.forEach(i => s.add(i)); store.set(seenKey(), JSON.stringify([...s])); }

  function notifMenu(items, sn) {
    const isNew = a => (openNew ? openNew.has(a.id) : !sn.has(a.id));
    const fresh = sinceItems(lastReviewed()).length;
    return `<div class="notif-head"><strong>Needs you</strong><span>${items.length ? `${items.length} open` : 'All clear'}${fresh ? ` · ${fresh} update${fresh === 1 ? '' : 's'} since you last checked` : ''}</span></div>
      ${items.length ? `<ul class="notif-list">${items.map(a => `<li><a href="#/inbox/${h(a.id)}" class="notif-item tone-${a.tone}" data-alert="${h(a.id)}"><i class="bi ${a.icon}" aria-hidden="true"></i><span><b>${h(a.title)}</b><small>${h(a.status)} · ${h(ago(a.t))}</small></span>${isNew(a) ? '<span class="new-dot" title="New"><span class="visually-hidden">New</span></span>' : ''}</a></li>`).join('')}</ul>`
        : '<p class="notif-empty">Nothing needs you. When something does, it shows up here and pops up on screen.</p>'}
      <div class="notif-foot"><a href="${qs()}#/inbox">Open your inbox</a><div class="form-check form-switch m-0"><input class="form-check-input" type="checkbox" role="switch" id="popups" ${popups ? 'checked' : ''}><label class="form-check-label" for="popups">Pop-up alerts</label></div></div>`;
  }
  function patchBell() {
    const bell = document.getElementById('bell');
    if (!bell) return;
    const items = attention(), sn = seen();
    const unseen = items.filter(a => !sn.has(a.id)).length;
    const bc = document.getElementById('bell-count');
    bc.hidden = !unseen; bc.textContent = unseen;
    bell.setAttribute('aria-label', unseen ? `Alerts, ${unseen} new` : 'Alerts');
    bell.classList.toggle('has-new', !!unseen);
    const sig = `${items.map(a => a.id + a.status).join('|')}#${[...sn].length}#${openNew ? 1 : 0}#${popups}#${Math.floor(S.t / 5)}`;
    if (sig !== menuSig) { menuSig = sig; document.getElementById('notif-menu').innerHTML = notifMenu(items, sn); }
  }
  function toast(a) {
    const el = document.createElement('div');
    el.className = `toast hub-toast tone-${a.tone}`;
    el.setAttribute('role', a.tone === 'bad' ? 'alert' : 'status');
    el.innerHTML = `<div class="toast-body"><i class="bi ${a.icon} toast-icon" aria-hidden="true"></i><div class="toast-main"><strong>${h(a.title)}</strong><p>${h(a.plain)}</p><div class="toast-actions"><a class="btn btn-sm btn-outline-brand" href="#/inbox/${h(a.id)}" data-alert="${h(a.id)}" data-bs-dismiss="toast">Decide</a><button type="button" class="btn btn-sm btn-link" data-bs-dismiss="toast">Dismiss</button></div></div><button type="button" class="btn-close" data-bs-dismiss="toast" aria-label="Close"></button></div>`;
    toasts.prepend(el);
    while (toasts.children.length > 3) toasts.lastElementChild.remove();
    el.addEventListener('hidden.bs.toast', () => el.remove());
    bootstrap.Toast.getOrCreateInstance(el, { delay: 9000 }).show();
  }
  // Pop up only items that are new since the last update; a reset or seek sets a fresh baseline silently.
  function alertNew() {
    const items = attention();
    if (prevAlerts && popups && !embed) {
      const sn = seen();
      for (const a of items) if (!prevAlerts.has(a.id) && !sn.has(a.id)) toast(a);
    }
    prevAlerts = new Set(items.map(a => a.id));
  }

  function renderTop() {
    top.innerHTML = `
      <div class="top-left">
        <button class="btn btn-icon d-lg-none" type="button" data-bs-toggle="offcanvas" data-bs-target="#rail" aria-controls="rail" aria-label="Open navigation"><i class="bi bi-list" aria-hidden="true"></i></button>
        <a class="brand" href="./${qs()}#/home" aria-label="AgentSCM Hub home"><img src="logo-full.png" alt="AgentSCM" class="logo logo-full" width="119" height="28"><img src="logo-mark.png" alt="AgentSCM" class="logo logo-mark" width="23" height="28"></a>
        <div class="dropdown repo-menu">
          <button class="btn repo" type="button" data-bs-toggle="dropdown" aria-expanded="false" aria-label="Repositories"><i class="bi bi-git" aria-hidden="true"></i>${mono(S.meta.repo)}<i class="bi bi-chevron-down" aria-hidden="true"></i></button>
          <ul class="dropdown-menu view-menu">
            <li><h6 class="dropdown-header">Repositories</h6></li>
            ${(S.meta.repos || [{ name: S.meta.repo, connected: true }]).filter(x => x.connected).map(x => `<li><span class="dropdown-item view-item ${x.name === S.meta.repo ? 'active' : ''}"><i class="bi bi-git" aria-hidden="true"></i><span><b>${h(x.name)}</b><small>${x.name === S.meta.repo ? 'Open now' : 'Connected'}</small></span>${x.name === S.meta.repo ? '<i class="bi bi-check2 view-check" aria-hidden="true"></i>' : ''}</span></li>`).join('')}
            ${(S.meta.repos || []).some(x => !x.connected) ? `<li><hr class="dropdown-divider"></li><li><h6 class="dropdown-header">On ${h(S.meta.computer)}, not connected</h6></li>${S.meta.repos.filter(x => !x.connected).map(x => `<li><span class="dropdown-item view-item"><i class="bi bi-folder2" aria-hidden="true"></i><span><b>${h(x.name)}</b><small>${h(x.path || '')}</small></span></span><div class="px-3 pb-2">${copyBtn(`agentscm connect ${x.path || x.name}`, 'Copy connect command')}</div></li>`).join('')}` : ''}
          </ul>
        </div>
      </div>
      <div class="top-right">
        <span class="conn" id="conn"></span>
        <div class="dropdown walk">
          <button class="btn btn-icon" type="button" data-bs-toggle="dropdown" data-bs-auto-close="outside" aria-expanded="false" aria-label="Walk me through this page"><i class="bi bi-question-circle" aria-hidden="true"></i></button>
          <div class="dropdown-menu dropdown-menu-end view-menu walk-menu" id="walk-menu"></div>
        </div>
        <div class="dropdown notif">
          <button class="btn btn-icon bell" id="bell" type="button" data-bs-toggle="dropdown" data-bs-auto-close="outside" aria-expanded="false" aria-label="Alerts"><i class="bi bi-bell" aria-hidden="true"></i><span class="bell-count" id="bell-count" hidden></span></button>
          <div class="dropdown-menu dropdown-menu-end notif-menu" id="notif-menu"></div>
        </div>
        <div class="dropdown">
          <button class="btn btn-icon" type="button" data-bs-toggle="dropdown" aria-expanded="false" aria-label="Choose a theme"><i class="bi bi-palette" aria-hidden="true"></i></button>
          <ul class="dropdown-menu dropdown-menu-end view-menu skin-menu">
            <li><h6 class="dropdown-header">Theme</h6></li>
            ${SKINS.map(k => `<li><button type="button" class="dropdown-item view-item ${skin() === k.k ? 'active' : ''}" data-pick-skin="${k.k}" role="menuitemradio" aria-checked="${skin() === k.k}"><i class="skin-swatch sw-${k.k || 'hub'}" aria-hidden="true"></i><span><b>${h(k.name)}</b><small>${h(k.line)}</small></span>${skin() === k.k ? '<i class="bi bi-check2 view-check" aria-hidden="true"></i>' : ''}</button></li>`).join('')}
            <li><hr class="dropdown-divider"></li>
            <li><h6 class="dropdown-header">Density</h6></li>
            <li><div class="density" role="group" aria-label="Density">${['spacious', 'condensed'].map(d => `<button type="button" class="${density() === d ? 'on' : ''}" data-pick-density="${d}" aria-pressed="${density() === d}">${d === 'spacious' ? 'Spacious' : 'Condensed'}</button>`).join('')}</div></li>
            <li><hr class="dropdown-divider"></li>
            <li><a class="dropdown-item compare-link" href="./themes.html"><i class="bi bi-layout-three-columns" aria-hidden="true"></i>Compare the themes side by side</a></li>
          </ul>
        </div>
        <button class="btn btn-icon" type="button" id="theme" aria-label="Switch light or dark mode"><i class="bi bi-circle-half" aria-hidden="true"></i></button>
      </div>`;
    const dd = top.querySelector('.notif');
    dd.addEventListener('show.bs.dropdown', () => { const sn = seen(); openNew = new Set(attention().filter(a => !sn.has(a.id)).map(a => a.id)); markSeen([...openNew]); patchBell(); });
    dd.addEventListener('hidden.bs.dropdown', () => { openNew = null; patchBell(); });
    menuSig = '';
    patchTop();
  }
  function patchTop() {
    const c = document.getElementById('conn');
    if (!c) return;
    c.innerHTML = `<span class="conn-item"><i class="bi bi-laptop" aria-hidden="true"></i>${h(S.meta.computer)}</span><span class="conn-item ${S.hub.up ? 'ok' : 'bad'}"><span class="dot" aria-hidden="true"></span>${S.hub.up ? 'Hub live' : `Hub not answering · last update ${ago(S.hub.since)}`}</span>`;
    patchBell();
  }

  function renderRail() {
    const nav = [option.nav[0], { route: 'inbox', label: 'Inbox', icon: 'bi-inbox', q: 'What needs my decision?' }, ...option.nav.slice(1)];
    const items = nav.map(n => ({ ...n, active: route() === n.route }));
    const att = inboxCount();
    rail.innerHTML = `
      <div class="offcanvas-header d-lg-none"><span class="fw-semibold">AgentSCM Hub</span><button type="button" class="btn-close" data-bs-dismiss="offcanvas" data-bs-target="#rail" aria-label="Close"></button></div>
      <div class="offcanvas-body"><nav aria-label="Hub sections"><ul class="rail-list">${items.map(n => `<li><a href="${qs()}#/${n.route}" class="${n.active ? 'active' : ''}" ${n.active ? 'aria-current="page"' : ''}><i class="bi ${n.icon}" aria-hidden="true"></i><span class="rail-label">${h(n.label)}${n.route === 'inbox' && att ? `<span class="badge-count" aria-label="${att} open decision${att === 1 ? '' : 's'}">${att}</span>` : ''}</span>${n.active ? `<small>${h(n.q)}</small>` : ''}</a></li>`).join('')}</ul></nav>
      <div class="rail-foot"><div class="rail-story"><strong>${h(S.meta.title)}</strong><span>${h(S.meta.blurb)}</span></div><p class="rail-note"><i class="bi bi-info-circle" aria-hidden="true"></i> Made-up demo data, streamed live from a local server.</p></div></div>`;
  }

  function renderMain() {
    const r = route();
    const k = document.activeElement?.dataset?.ui;
    const sel = k && document.activeElement.selectionStart;
    let html;
    if (r === 'history') html = historyPage({ ...ui, tab: routeArg() });
    else if (r === 'inbox') html = inboxPage(ui, routeArg());
    else if (r === 'skills') html = (option.skills || skillsPage)({ ...ui, tab: routeArg() });
    else if (r === 'insights') html = (option.insights || insightsPage)(ui);
    else if (r === 'setup') html = setupPage({ tab: routeArg() });
    else if (r === 'timeline') html = timelinePage(ui);
    else if (r === 'task') html = taskPage(routeArg(), { back: ui.back || '#/home', backLabel: backLabel(ui.back || '#/home'), tf: ui.tf, q: ui.q, lane: ui.lane });
    else if (r === 'session') html = sessionPage(routeArg(), { back: ui.back || '#/history', backLabel: backLabel(ui.back || '#/history'), q: ui.q });
    else html = works().length ? option.home({ ...ui, skin: skin(), tab: r === 'home' ? routeArg() : '' }) : `${option.homeMast ? option.homeMast() : ''}${firstRun()}`;
    const banner = S.hub.up ? '' : `<div class="notice notice-bad" role="status"><i class="bi bi-wifi-off" aria-hidden="true"></i><div><strong>The Hub stopped answering ${ago(S.hub.since)}.</strong> You’re seeing the last good update. Tasks keep their owners, and nothing is released.</div></div>`;
    // Keep the reader's place: the row at the top of the view stays put when content above it changes.
    const sameView = lastHash === location.hash && window.scrollY > 0;
    const anchorEl = sameView ? [...page.querySelectorAll('[data-k], .box-head h2')].find(el => el.getBoundingClientRect().top >= 0) : null;
    const anchorKey = anchorEl && (anchorEl.dataset.k ? `[data-k="${CSS.escape(anchorEl.dataset.k)}"]` : null);
    const anchorText = anchorEl && !anchorEl.dataset.k ? anchorEl.textContent : null;
    const anchorTop = anchorEl?.getBoundingClientRect().top;
    page.innerHTML = banner + html;
    lastHash = location.hash;
    if (anchorEl) {
      const again = anchorKey ? page.querySelector(anchorKey) : [...page.querySelectorAll('.box-head h2')].find(el => el.textContent === anchorText);
      if (again) window.scrollBy(0, again.getBoundingClientRect().top - anchorTop);
    }
    if (k) { const el = page.querySelector(`[data-ui="${k}"]`); if (el) { el.focus(); try { el.setSelectionRange(sel, sel); } catch { /* search inputs may not support it */ } } }
    const now = new Set([...page.querySelectorAll('[data-k]')].map(e => e.dataset.k));
    if (known) for (const el of page.querySelectorAll('[data-k]')) if (!known.has(el.dataset.k)) el.classList.add('is-new');
    known = new Set([...(known || []), ...now]);
    option.after?.(page, ui);
    const wm = document.getElementById('walk-menu');
    if (wm) wm.innerHTML = `<h6 class="dropdown-header">This page in three steps</h6><ol class="walk-steps">${walkthrough(r).map(x => `<li>${h(x)}</li>`).join('')}</ol>`;
    document.title = r === 'task' && S.work[routeArg()] ? `${S.work[routeArg()].title} · AgentSCM Hub` : `${option.nav.find(n => n.route === r)?.label || (r === 'inbox' ? 'Inbox' : view.name)} · AgentSCM Hub`;
  }
  function renderAll() { renderRail(); renderMain(); }

  document.addEventListener('click', ev => {
    // On phones the rail is a slide-out menu: close it after a pick, and let the link navigate.
    if (ev.target.closest('.rail-list a') && window.innerWidth < 992) window.bootstrap?.Offcanvas.getInstance(rail)?.hide();
    const dn = ev.target.closest('[data-pick-density]');
    if (dn) { const d = dn.dataset.pickDensity; if (d === 'spacious') document.documentElement.removeAttribute('data-density'); else document.documentElement.setAttribute('data-density', d); store.set('agentscm.hub.density', d); renderTop(); return; }
    const sk = ev.target.closest('[data-pick-skin]');
    if (sk) { const k = sk.dataset.pickSkin; if (k) document.documentElement.setAttribute('data-skin', k); else document.documentElement.removeAttribute('data-skin'); store.set('agentscm.hub.skin', k); renderTop(); return; }
    const al = ev.target.closest('[data-alert]');
    if (al) { markSeen([al.dataset.alert]); patchBell(); bootstrap.Dropdown.getInstance(document.getElementById('bell'))?.hide(); }
    const wl = ev.target.closest('[data-work]');
    if (wl) {
      if (ev.metaKey || ev.ctrlKey || ev.shiftKey) return;
      ev.preventDefault();
      openTask(wl.dataset.work, wl.dataset.focus || option.focus);
      return;
    }
    const cp = ev.target.closest('[data-copy]');
    if (cp) {
      navigator.clipboard?.writeText(cp.dataset.copy).catch(() => {});
      const span = cp.querySelector('span');
      const was = span.textContent;
      span.textContent = 'Copied';
      cp.classList.add('copied');
      setTimeout(() => { span.textContent = was; cp.classList.remove('copied'); }, 1400);
      return;
    }
    const an = ev.target.closest('[data-answer]');
    if (an) { saveAnswer(an.dataset.answer, an.dataset.choice); markSeen([an.dataset.answer]); renderAll(); patchBell(); return; }
    const un = ev.target.closest('[data-unanswer]');
    if (un) { const all = answers(); delete all[un.dataset.unanswer]; store.set(`agentscm.hub.answers.${S.meta.id}`, JSON.stringify(all)); renderAll(); return; }
    if (ev.target.closest('[data-reviewed]')) { markReviewed(); menuSig = ''; renderAll(); patchBell(); return; }
    if (ev.target.closest('[data-peek]')) { togglePeek(); renderMain(); return; }
    const st = ev.target.closest('[data-since-tab]');
    if (st) { setSinceTab(st.dataset.sinceTab); renderMain(); return; }
    const hf = ev.target.closest('[data-hf]');
    if (hf) { ui.hf = hf.dataset.hf; if (hf.dataset.clear) ui.q = ''; renderMain(); return; }
    const ui2 = ev.target.closest('[data-set]');
    if (ui2) { ui[ui2.dataset.set] = ui2.dataset.val; renderMain(); return; }
    if (ev.target.closest('#theme')) {
      const cur = document.documentElement.getAttribute('data-bs-theme') === 'dark' ? 'light' : 'dark';
      document.documentElement.setAttribute('data-bs-theme', cur);
      store.set('hubstory.theme', cur);
    }
  });
  document.addEventListener('change', ev => {
    if (ev.target.dataset.pick) { (ui.pick ||= {})[ev.target.dataset.pick] = ev.target.value; renderMain(); return; }
    if (ev.target.id === 'popups') { popups = ev.target.checked; store.set(POPUP_KEY, popups ? 'on' : 'off'); }
  });
  page.addEventListener('input', ev => {
    const k = ev.target.dataset.ui;
    if (k) { ui[k] = ev.target.value; renderMain(); }
  });
  window.addEventListener('storage', ev => { if (ev.key && ev.key.startsWith('agentscm.hub.lastReviewed.')) { noteMovedElsewhere(); renderAll(); } });
  window.addEventListener('hashchange', ev => {
    const prev = new URL(ev.oldURL).hash || '#/home';
    if (['task', 'session'].includes(route()) && prev !== location.hash) { ui.back = prev; ui.tf = 'all'; ui.lane = ''; }
    known = null;
    renderAll();
    main.focus({ preventScroll: true });
    const target = route() === 'task' && ui.anchor && document.getElementById(ui.anchor);
    ui.anchor = null;
    if (target) target.scrollIntoView({ block: 'start' }); else window.scrollTo(0, 0);
  });

  let lastPaint = 0;
  connect({
    onReset() { known = null; prevAlerts = null; renderTop(); renderAll(); alertNew(); },
    onEvents() { renderAll(); patchTop(); alertNew(); lastPaint = performance.now(); },
    onClock() {
      patchTop(); option.onClock?.(main);
      // Without new events, only "5 min ago"-style times change, so a quiet page repaints once a minute.
      if (S.playing && performance.now() - lastPaint > 60000 && !main.contains(document.activeElement)) { renderMain(); lastPaint = performance.now(); }
    },
  });
}

// ---------- live stream ----------
export function connect({ onReset, onEvents, onClock }) {
  const setClock = c => Object.assign(S, { t: c.t, playing: c.playing, speed: c.speed, end: c.end, scn: c.scn });
  // Embedded previews poll instead of streaming: browsers allow only six connections per site,
  // and a comparison page with several live streams would starve every other tab.
  if (new URLSearchParams(location.search).has('embed')) {
    let scn = '', n = -1;
    const tick = async () => {
      try {
        const d = await (await fetch('/api/state', { cache: 'no-store' })).json();
        if (d.meta.id !== scn || d.events.length < n) { init(d.meta); d.events.forEach(apply); scn = d.meta.id; n = d.events.length; setClock(d.clock); onReset(); }
        else if (d.events.length > n) { d.events.slice(n).forEach(apply); n = d.events.length; setClock(d.clock); onEvents(); }
        else { setClock(d.clock); onClock(); }
        document.body.classList.remove('stream-lost');
      } catch { document.body.classList.add('stream-lost'); }
      setTimeout(tick, 1000);
    };
    tick();
    return;
  }
  const es = new EventSource('/api/stream');
  addEventListener('pagehide', () => es.close());
  es.addEventListener('reset', m => { const d = JSON.parse(m.data); init(d.meta); d.events.forEach(apply); setClock(d.clock); onReset(); });
  es.addEventListener('events', m => { JSON.parse(m.data).forEach(apply); onEvents(); });
  es.addEventListener('clock', m => { setClock(JSON.parse(m.data)); onClock(); });
  es.onerror = () => document.body.classList.add('stream-lost');
  es.onopen = () => document.body.classList.remove('stream-lost');
}
export const control = (action, value) => fetch('/api/control', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ action, value }) });

export function bootTheme() {
  let t = null;
  try { t = localStorage.getItem('hubstory.theme'); } catch { /* storage unavailable */ }
  if (!t) t = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  document.documentElement.setAttribute('data-bs-theme', t);
}
