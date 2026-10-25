// Replays both stories minute by minute and renders every screen of every option; fails on any throw.
// Usage: node selfcheck.mjs
import { scenarios } from './scenarios.mjs';
import * as core from './public/core.js';
import * as dash from './public/dashboard.js';

let renders = 0;
const bad = /undefined|NaN|\[object|null</;
const chk = (html, where) => { if (bad.test(html)) { console.error(where, html.match(new RegExp('.{60}(' + bad.source + ').{60}'))?.[0]); process.exitCode = 1; } return html; };
for (const s of Object.values(scenarios)) {
  const { seed, events, chats, ...meta } = s;
  const all = [...seed, ...events, ...chats].sort((a, b) => a.t - b.t);
  const step = Math.max(1, Math.round(s.end / 120));
  for (let t = 0; t <= s.end; t += step) {
    core.init(meta);
    all.filter(e => e.t <= t).forEach(core.apply);
    core.S.t = t;
    const ui = { q: '', hf: 'all', lens: 'who', area: meta.areas[0].path, all: '1' };
    for (const tab of ['now', 'where', 'catchup', 'team']) { chk(dash.home({ ...ui, tab }), `${s.id}@${t} home ${tab}`); renders++; }
    for (const lens of ['rework', 'past']) { chk(dash.home({ ...ui, tab: 'where', lens }), `${s.id}@${t} map ${lens}`); renders++; }
    for (const [k, html] of Object.entries({ h1: core.historyPage(ui), h2: core.historyPage({ ...ui, q: 'jpy rounding' }), h3: core.historyPage({ ...ui, q: 'zzz' }), hf: core.historyPage({ ...ui, tab: 'files' }), hf2: core.historyPage({ ...ui, tab: 'files', file: 'tests/**' }), sk: core.skillsPage(), sk2: core.skillsPage({ tab: 'checks' }), sk3: core.skillsPage({ tab: 'recipes' }), in: core.insightsPage(), se: core.setupPage(), se2: core.setupPage({ tab: 'agents' }), se3: core.setupPage({ tab: 'access' }), tl: core.timelinePage(ui), tl2: core.timelinePage({ ...ui, tm: '0', tc: 'prev' }), tl3: core.timelinePage({ ...ui, tm: '1', tc: 'prev' }), fr: core.firstRun() })) chk(html, `${s.id}@${t} ${k}`);
    for (const w of core.works()) { chk(core.taskPage(w.id, { back: '#/home', backLabel: 'Back to Today' }), `${s.id}@${t} ${w.id}`); renders++; }
    chk(core.taskPage('W-999'), `${s.id}@${t} missing task`);
    chk(core.inboxPage(ui), `${s.id}@${t} inbox`);
    for (const a of core.attention()) chk(core.inboxPage(ui, a.id), `${s.id}@${t} inbox ${a.id}`);
    for (const a of core.agents()) chk(core.sessionPage(a.id, { q: 'the' }), `${s.id}@${t} session ${a.id}`);
    for (const w of core.works()) for (const tf of ['chat', 'code', 'coord']) chk(core.taskPage(w.id, { tf, lane: w.lead }), `${s.id}@${t} ${w.id} ${tf}`);
  }
  // Ribbons: a session open past midnight stays wide, and starts land where clock() says they do.
  const rb = core.ribbonOf(s.id === 'team' ? 750 : 0, s.id === 'team' ? 1490 : 60);
  if (s.id === 'team' && !(rb.width > 40)) { console.error('team ribbon too thin', rb); process.exitCode = 1; }
  if (s.id === 'solo' && Math.abs(rb.left - 37.5) > 0.01) { console.error('solo ribbon at wrong time', rb); process.exitCode = 1; }
  const html = core.insightsPage() + core.historyPage({ q: '', hf: 'all' });
  console.assert(!/undefined|NaN|\[object/.test(html), `${s.id}: leaked undefined/NaN in final render`);
  console.log(`${s.id}: ok, ${core.works().length} tasks, ${core.attention().length} open attention items at end`);
}
// The live feed: 400 invented events per story, each applied by the real reducer and rendered on every page.
{
  const live = await import('./live.mjs');
  for (const s of Object.values(scenarios)) {
    const { seed, events, chats, ...meta } = s;
    core.init(meta); [...seed, ...events, ...chats].sort((a, b) => a.t - b.t).forEach(core.apply);
    live.resetLive();
    let t = s.end, now = 0, made = 0;
    for (let i = 0; i < 400; i++) {
      t += 1; now += 6000; core.S.t = t;
      for (const e of [...live.due(t, now), live.invent(core, t, now)].filter(Boolean)) { core.apply(e); made++; }
      if (i % 50 === 0) { const ui = { q: '', hf: 'all', lens: 'who', all: '1' }; for (const tab of ['now', 'where', 'catchup', 'team']) chk(dash.home({ ...ui, tab }), `${s.id} live ${i} ${tab}`); chk(core.inboxPage(ui), `${s.id} live inbox`); renders += 5; }
    }
    if (made < 300) { console.error(`${s.id}: live feed made only ${made} events`); process.exitCode = 1; }
    console.log(`${s.id}: live feed ok, ${made} events, ${Object.keys(core.S.overlaps).length} heads-ups`);
  }
}
console.log(`${renders} renders, no errors`);
