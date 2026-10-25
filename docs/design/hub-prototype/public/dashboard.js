// The Hub dashboard: one sentence on what's happening, then four questions, each its own tab.
// Now: what needs you and where every task is. Where: the repo as a map of who plans to work where.
// Catch up: what happened since you last checked. Team: who's around and who ran when.
import {
  S, esc as h, masthead, box, attention, attentionList, settled, settledList, works, workList, avatar, agentName, workLink,
  scopeHits, live, prOf, historyRecords, mono, pill, dayLabel, clock, statePill, team, agents, isAwake, proposed, followUp,
  stageIcon, STAGES, laneBar, lastWords, chatText, when, storyLine, insightsRows, sinceCard, crew, homeTabs, homeTab, startCheck } from './core.js';

export const nav = [
  { route: 'home', label: 'Dashboard', icon: 'bi-activity', q: 'What’s happening?' },
  { route: 'history', label: 'History', icon: 'bi-clock-history', q: 'What happened before?' },
  { route: 'timeline', label: 'Timeline', icon: 'bi-camera', q: 'What did the work look like earlier?' },
  { route: 'skills', label: 'Skills', icon: 'bi-lightning', q: 'What can my agents repeat?' },
  { route: 'insights', label: 'Insights', icon: 'bi-graph-up', q: 'Is this helping?' },
  { route: 'setup', label: 'Setup', icon: 'bi-plug', q: 'Is everything connected?' },
];
export const focus = 'stage';

const inArea = (paths, area) => paths.some(p => p.replace(/\/\*\*$/, '').startsWith(area.path) || area.path.startsWith(p.replace(/\/\*\*$/, '')));

function areaFacts(area) {
  const ws = works().filter(w => w.state !== 'finished' && scopeHits(w.scope, area.path));
  const here = [...new Set(ws.flatMap(w => w.participants.filter(id => live(S.agents[id]))))];
  const crossings = Object.values(S.overlaps).filter(o => inArea(o.paths, area));
  const open = crossings.filter(o => o.state === 'open');
  const rework = works().filter(w => scopeHits(w.scope, area.path)).map(prOf).filter(Boolean)
    .reduce((n, p) => n + p.commits.filter(c => c.type !== 'feat').length + p.ci.filter(c => c.status === 'fail').length, 0);
  const past = historyRecords().filter(r => r.files.some(f => inArea([f], area)));
  return { ws, here, crossings, open, rework, past };
}

function answer() {
  const on = agents().filter(a => live(a));
  const active = workList().filter(w => w.state !== 'finished');
  const att = attention().length;
  const subs = on.filter(a => a.role === 'subagent').length;
  const crossing = S.meta.areas.filter(a => areaFacts(a).open.length);
  return `${on.length ? `<b>${on.length - subs} agent${on.length - subs === 1 ? '' : 's'}</b>${subs ? ` and <b>${subs} subagent${subs === 1 ? '' : 's'}</b>` : ''} ${on.length === 1 ? 'is' : 'are'} working` : 'No agents are open right now'}${active.length ? ` on <b>${active.length} task${active.length === 1 ? '' : 's'}</b>` : ''}. ${att ? `<a href="#/home/now" class="tone-link-warn">${att} thing${att === 1 ? '' : 's'} need${att === 1 ? 's' : ''} you.</a>` : 'Nothing needs you.'}${crossing.length ? ` <a href="#/home/where" class="tone-link-warn" data-set="area" data-val="${h(crossing[0].path)}">${crossing.length} crossing${crossing.length === 1 ? '' : 's'} in ${crossing.map(x => h(x.label)).join(' and ')}.</a>` : ''}`;
}
export const homeMast = () => masthead('bi-activity', 'What’s happening?', answer());

// One task = one lane. Columns line up under the stage header; the task column carries its next step,
// and the agent's last words sit under the bar.
function lane(w) {
  const f = followUp(w);
  const flow = f ? `<button type="button" class="rl-flow flow-${f.s}" data-work="${h(w.id)}" data-focus="stage"><i class="bi bi-arrow-return-right" aria-hidden="true"></i><span>Then the workflow runs</span>${mono(f.skill.name)}<span class="flow-state">${stageIcon(f.s)}${h(f.note)}</span>${f.b?.claim && f.s === 'active' ? `<span class="baton">${avatar(f.b.claim)}</span>` : ''}${proposed()}</button>` : '';
  const said = w.state !== 'finished' && lastWords(w);
  return `<li data-k="ln-${h(w.id)}" class="rl-row state-${h(w.state)}"><div class="rl-task"><div class="lane-title">${workLink(w.id)}</div><div class="lane-meta">${statePill(w.state)}${w.pr ? `<span class="muted small">PR #${w.pr}</span>` : ''}</div>${(c => (c ? `<span class="start-check tone-${c.tone}" title="${h(c.why)}"><i aria-hidden="true"></i>${h(c.text)}</span>` : ''))(startCheck(w))}${w.waitsFor ? `<div class="lane-wait"><i class="bi bi-hourglass" aria-hidden="true"></i> Waiting for ${workLink(w.waitsFor)}</div>` : ''}${w.next && w.state !== 'finished' ? `<p class="rl-next"><i class="bi bi-arrow-right-short" aria-hidden="true"></i><span>Next: ${h(w.next)}</span></p>` : ''}</div>${laneBar(w, { mode: 'grid', link: true })}${said ? `<p class="rl-said" data-k="said-${said.t}"><i class="bi bi-chat-quote" aria-hidden="true"></i><span>“${chatText(said.text)}”</span><small>${h(agentName(said.by))} · ${when(said.t)}</small></p>` : ''}${flow}</li>`;
}

function passes() {
  const ps = S.passes.slice().reverse();
  if (!ps.length) return '<p class="empty-line">Every handover, subagent and skill claim shows up here as a baton pass.</p>';
  const label = { subagent: 'Started a subagent', scope: 'Changed plan', handover: 'Handover', claim: 'Claimed a skill' };
  return `<ol class="passes">${ps.slice(0, 8).map(p => `<li data-k="ps-${p.t}-${h(p.kind)}" class="pass pass-${h(p.kind)}"><span class="pass-hands">${p.from ? avatar(p.from) : '<span class="av av-system" aria-hidden="true"><i class="bi bi-stars"></i></span>'}${p.to ? `<i class="bi bi-arrow-right" aria-hidden="true"></i>${avatar(p.to)}` : ''}</span><span class="pass-text"><b>${h(label[p.kind])}</b> ${h(p.from ? agentName(p.from) : '')}${p.to ? ` → ${h(agentName(p.to))}` : ''}<small>${h(p.note)}</small></span><time>${clock(p.t)}</time></li>`).join('')}</ol>`;
}

// Who is awake (team) or which agents ran (solo), on one shared time axis.
function sunStrip() {
  const span = team() ? [0, 1440] : [0, S.meta.end];
  const pct = t => ((t - span[0]) / (span[1] - span[0])) * 100;
  const clampBar = (a, b) => { const x = Math.max(span[0], a), y = Math.min(span[1], b); return y > x ? `left:${pct(x)}%;width:${pct(y) - pct(x)}%` : null; };
  let rows;
  if (team()) {
    rows = S.meta.people.map(p => {
      const startH = (p.hours[0] - p.utc + 24) % 24, len = (p.hours[1] - p.hours[0]) * 60;
      const hrs = [[startH * 60, startH * 60 + len], [startH * 60 - 1440, startH * 60 - 1440 + len]].map(([a, b]) => clampBar(a, b)).filter(Boolean);
      const sess = agents().filter(a => a.person === p.id && a.role !== 'subagent' && a.start >= 0).map(a => clampBar(a.start, a.end ?? S.t)).filter(Boolean);
      return `<li class="sun-row"><span class="sun-who"><b>${h(p.short)}</b><small>${h(p.city)} · ${clock(S.t, p.utc)}</small></span><span class="sun-lane">${hrs.map(s => `<i class="sun-hours" style="${s}"></i>`).join('')}${sess.map(s => `<i class="sun-sess" style="${s}"></i>`).join('')}</span>${isAwake(p) ? '<span class="sun-tag on">Awake</span>' : '<span class="sun-tag">Asleep</span>'}</li>`;
    });
  } else {
    rows = agents().filter(a => a.start >= 0).map(a => `<li class="sun-row ${a.role === 'subagent' ? 'is-sub' : ''}"><span class="sun-who"><b>${h(agentName(a.id))}</b><small>${a.role === 'subagent' ? `subagent of ${h(agentName(a.parent))}` : 'main session'}</small></span><span class="sun-lane"><i class="sun-sess" style="${clampBar(a.start, a.end ?? S.t)}"></i></span>${live(a) ? '<span class="sun-tag on">Open</span>' : '<span class="sun-tag">Closed</span>'}</li>`);
  }
  const ticks = team() ? [0, 360, 720, 1080, 1440].map(t => `<span style="left:${pct(t)}%">${String((t / 60) % 24).padStart(2, '0')}:00${t === 1440 ? ' UTC' : ''}</span>`) : [0, 60, 120, 180, 240].map(t => `<span style="left:${pct(t)}%">${clock(t)}</span>`);
  return `<div class="sun" style="--now:${Math.min(100, pct(S.t))}%"><ul class="sun-rows">${rows.join('')}</ul><div class="sun-axis">${ticks.join('')}</div></div>`;
}

const LENS = [['who', 'Who’s here', 'bi-person'], ['rework', 'Redo work', 'bi-arrow-repeat'], ['past', 'Past sessions', 'bi-clock-history']];

function tile(area, ui) {
  const f = areaFacts(area);
  const val = ui.lens === 'rework' ? f.rework : ui.lens === 'past' ? f.past.length : f.here.length;
  const heat = Math.min(1, val / (ui.lens === 'rework' ? 8 : ui.lens === 'past' ? 4 : 3));
  const lensText = ui.lens === 'rework' ? `${f.rework} redo${f.rework === 1 ? '' : 's'}` : ui.lens === 'past' ? `${f.past.length} past session${f.past.length === 1 ? '' : 's'}` : f.here.length ? `${f.here.length} here` : 'Quiet';
  const sel = ui.area === area.path;
  const marks = attention().filter(a => a.paths && inArea(a.paths, area));
  return `<button type="button" data-k="ar-${h(area.path)}" class="tile ${f.open.length ? 'is-crossing' : ''} ${sel ? 'is-selected' : ''}" style="grid-column:span ${area.w};grid-row:span ${area.h};--heat:${heat}" data-set="area" data-val="${h(sel ? '' : area.path)}" aria-pressed="${sel}" aria-label="${h(area.label)}: ${h(lensText)}${f.open.length ? ', two agents plan the same file' : ''}${marks.length ? `, ${marks.length} inbox item${marks.length > 1 ? 's' : ''}` : ''}">
    <span class="tile-head"><b>${h(area.label)}</b><code>${h(area.path)}/</code></span>
    <span class="tile-people">${f.here.map(id => avatar(id)).join('')}</span>
    ${f.open.length ? `<span class="crossing"><i class="bi bi-sign-intersection" aria-hidden="true"></i>${h(f.open[0].paths[0].replace(/\/\*\*$/, '/').split('/').filter(Boolean).pop())}</span>` : f.crossings.some(o => o.state === 'resolved' && S.t - o.resolvedAt < 90) ? '<span class="crossing settled"><i class="bi bi-check2" aria-hidden="true"></i>Sorted out</span>' : ''}
    ${marks.length ? `<span class="tile-marks">${marks.map(m => `<i class="bi ${m.icon} mark-${m.tone}" title="${h(m.title)}" aria-hidden="true"></i>`).join('')}</span>` : ''}
    <span class="tile-lens">${h(lensText)}</span>
  </button>`;
}

// The picked area opens as a drawer under the map, inside the same box. Every row: a face, a line, a quiet detail.
function areaPanel(ui) {
  const area = S.meta.areas.find(a => a.path === ui.area);
  if (!area) return `<p class="map-hint"><i class="bi bi-hand-index" aria-hidden="true"></i> Pick an area to see who’s there, any crossings and past sessions that touched it.</p>`;
  const f = areaFacts(area);
  const item = (face, main, meta) => `<li class="ad-item">${face}<div><div class="ad-main">${main}</div>${meta ? `<div class="ad-meta">${meta}</div>` : ''}</div></li>`;
  const sys = icon => `<span class="av av-system" aria-hidden="true"><i class="bi ${icon}"></i></span>`;
  const col = (title, list, empty, extra = '') => `<section><h4>${title}</h4>${list.length ? `<ul class="ad-list">${list.join('')}</ul>${extra}` : `<p class="empty-line">${empty}</p>`}</section>`;
  return `<div class="area-drawer" data-k="ad-${h(area.path)}">
    <div class="area-drawer-head"><h3>${h(area.label)} <code>${h(area.path)}/</code></h3><button type="button" class="btn btn-sm btn-outline-brand" data-set="area" data-val="">Close</button></div>
    <div class="area-cols">
      ${col('Working here', f.ws.map(w => item(avatar(w.lead), workLink(w.id), `${statePill(w.state)} ${w.scope.filter(s => scopeHits([s], area.path)).map(mono).join(' ')}`)), 'Nobody right now.')}
      ${col('Crossings', f.crossings.map(o => item(sys('bi-sign-intersection'), `${mono(o.paths.join(', '))}`, `${pill(o.state === 'open' ? 'warn' : 'ok', o.state === 'open' ? 'Open' : 'Sorted out')} ${workLink(o.a)} and ${workLink(o.b)}${o.response ? ` · ${h(o.response.kind)}: ${h(o.response.note)}` : ''}`)), 'None so far.')}
      ${col('Past sessions here', f.past.slice(0, 4).map(x => item(avatar(x.a.id), h(x.title), `${h(dayLabel(x.t))} ${clock(x.t)} · ${h(agentName(x.a.id))}`)), 'No saved sessions touched this area.', '<a href="#/history/files" class="small ad-more">See every file’s story</a>')}
    </div></div>`;
}

export function home(ui) {
  ui.lens ||= 'who';
  const att = attention();
  const ws = workList();
  const log = S.log.slice().reverse();
  const shown = ui.all === '1' ? log : log.slice(0, 12);
  const rows = insightsRows();
  const tab = homeTab(ui);
  const parts = {
    now: `
    ${att.length ? box({ title: 'Needs you', icon: 'bi-bell', tone: att[0].tone === 'bad' ? 'bad' : 'warn', count: att.length, explain: 'Only things a person should decide. Each one ends with something you can hand to your agent.', body: attentionList(att, { limit: 3 }) }) : ''}
    ${box({ title: 'The track', icon: 'bi-signpost-split', count: ws.length, explain: 'Every task, left to right. Filled means done. The wide part is where it is now, with the face of who holds it, its next step and the last thing its agent said.', cls: 'box-wide', body: `
      <div class="rl-head" aria-hidden="true"><span></span><div class="rl-cols">${STAGES.map(x => `<span><b>${h(x.label)}</b><small>${h(x.plain)}</small></span>`).join('')}</div></div>
      <ol class="rl">${ws.map(lane).join('')}</ol>` })}`,
    where: `
    ${box({ title: S.meta.repo, icon: 'bi-grid-1x2', explain: 'Areas of the repo, sized by how big they are. Faces show who plans to work there.', cls: 'box-map', body: `
      <div class="map-bar">
        <div class="lens seg" role="group" aria-label="Color the map by">${LENS.map(([k, l, i]) => `<button type="button" class="btn btn-sm ${ui.lens === k ? 'active' : ''}" data-set="lens" data-val="${k}" aria-pressed="${ui.lens === k}"><i class="bi ${i}" aria-hidden="true"></i>${h(l)}</button>`).join('')}</div>
        <p class="map-legend"><span><i class="legend-swatch" aria-hidden="true"></i>Deeper color, more ${ui.lens === 'rework' ? 'redo work' : ui.lens === 'past' ? 'past sessions' : 'agents'}</span><span><i class="bi bi-sign-intersection text-warn" aria-hidden="true"></i>Crossing: two agents plan the same file. A heads-up, not a conflict.</span></p>
      </div>
      <div class="map lens-${h(ui.lens)}">${S.meta.areas.map(a => tile(a, ui)).join('')}</div>
      ${areaPanel(ui)}` })}
    ${box({ title: 'Heads-ups that worked out', icon: 'bi-check2-all', tone: 'ok', explain: 'Two agents planned the same file. What happened next, and how it ended.', body: settledList(settled(), 4) })}`,
    catchup: `
    ${sinceCard()}
    <div class="today-grid">
      <div class="today-story">
        ${box({ title: 'What happened', icon: 'bi-book', count: S.log.length, explain: 'The day so far, newest first. Click a task to see its whole story.', body: `<ul class="story">${shown.map(e => storyLine(e)).join('')}</ul>${log.length > 12 ? `<button class="btn btn-sm btn-link" data-set="all" data-val="${ui.all === '1' ? '0' : '1'}">${ui.all === '1' ? 'Show less' : `Show all ${log.length}`}</button>` : ''}` })}
      </div>
      <aside class="today-side" aria-label="Handovers">
        ${box({ title: 'Baton passes', icon: 'bi-arrow-left-right', count: S.passes.length, explain: 'Who handed what to whom, and why.', body: passes() })}
      </aside>
    </div>`,
    team: `
    ${box({ title: team() ? 'Around the world' : 'Who ran when', icon: team() ? 'bi-globe2' : 'bi-clock', explain: team() ? 'Pale bars are working hours. Solid bars are open agent sessions. The line is now.' : 'Each agent and subagent session. The line is now.', body: sunStrip() })}
    <div class="today-grid">
      <div class="today-story">
        ${box({ title: team() ? 'The team right now' : 'Your crew', icon: 'bi-people', explain: team() ? 'Grouped by person, with their local time.' : 'Agents and the subagents they started.', body: crew() })}
      </div>
      <aside class="today-side" aria-label="Tallies">
        ${box({ title: 'So far today', icon: 'bi-graph-up', tone: 'muted', body: `<ul class="tally">${rows.slice(0, 4).map(r => `<li><span>${h(r.m)}</span><b>${h(r.v)}</b></li>`).join('')}</ul><a href="#/insights" class="small">What these numbers mean</a>` })}
      </aside>
    </div>`,
  };
  return `${homeMast()}${homeTabs(ui)}<div class="htab-body" data-k="tab-${tab}">${parts[tab]}</div>`;
}

export function onClock(main) {
  const sun = main.querySelector('.sun');
  if (!sun) return;
  const span = team() ? 1440 : S.meta.end;
  sun.style.setProperty('--now', `${Math.min(100, (S.t / span) * 100)}%`);
}
