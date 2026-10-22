// Static checks plus fixture rendering of the MCP Apps views with a minimal DOM stub. Run: node tools/mcp-views-check/check.mjs
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
const dir = new URL('../../src/hub/mcp_views/', import.meta.url);
const views = { 'work-overview.html': 'agentscm_work_overview', 'coordination-inbox.html': 'agentscm_coordination_inbox' };
const TAGS = new Set(['div', 'span', 'p', 'h1', 'ul', 'li', 'h2', 'button', 'header', 'footer', 'details', 'summary']);
const now = Math.floor(Date.now() / 1000);

function mk(tag) {
  const e = { tag, children: [], className: '', _t: '', listeners: {}, attrs: {} };
  e.appendChild = (c) => (e.children.push(c), c);
  e.addEventListener = (n, f) => (e.listeners[n] = f);
  e.setAttribute = (k, v) => (e.attrs[k] = v);
  Object.defineProperty(e, 'textContent', { get: () => e._t + e.children.map((c) => c.textContent).join(''), set: (v) => { e._t = String(v); e.children = []; } });
  Object.defineProperty(e, 'scrollHeight', { get: () => 100 });
  return e;
}
function load(html) {
  const js = html.slice(html.indexOf('<script>') + 8, html.lastIndexOf('</script>'));
  const app = mk('div'), body = mk('body'), sent = [], made = [], handlers = {};
  const doc = { body, documentElement: mk('html'), getElementById: () => app, createTextNode: (t) => { const n = mk('#text'); n._t = t; return n; },
    createElement: (t) => { made.push(t); return mk(t); } };
  const win = { parent: { postMessage: (m) => sent.push(m) }, addEventListener: (n, f) => (handlers[n] = f) };
  win.window = win; win.document = doc; win.Promise = Promise; win.Date = Date;
  vm.runInNewContext(js, Object.assign(win, doc && { document: doc }), { filename: 'view.js' });
  const msg = (m) => handlers.message({ source: win.parent, data: { jsonrpc: '2.0', ...m } });
  return { app, sent, made, msg };
}
const walk = (e, f) => { f(e); e.children.forEach((c) => walk(c, f)); };

for (const [file, tool] of Object.entries(views)) {
  const html = fs.readFileSync(new URL(file, dir), 'utf8');
  assert(Buffer.byteLength(html) < 40000, file + ' >= 40 kB');
  assert(!/https?:\/\//.test(html), file + ' has external ref');
  assert(!/innerHTML|outerHTML|insertAdjacentHTML|document\.write|eval\(|fetch\(|XMLHttpRequest/.test(html), file + ' uses forbidden API');
  assert(html.includes('ui/initialize') && html.includes('ui/notifications/initialized') && html.includes('size-changed'));
  const names = [...html.matchAll(/agentscm_[a-z_]+/g)].map((m) => m[0]).filter((n) => n !== 'agentscm_view_settings');
  assert(names.every((n) => n === tool), file + ' references ' + names);

  const evil = '<img src=x onerror=alert(1)><script>boom()</script>' + 'z'.repeat(400);
  const person = (n) => ({ token: 't', agent: n, session: 's', owner: 'o', login: 'l' });
  const item = (i, state, extra = {}) => ({ id: 'w' + i, title: evil, state, claim_status: 'active', last_checkin_at: now - 120, lead_agent: 'agent-' + i, owner_login: 'x', next_action: evil, blocker: null, ...extra });
  const wo = { current: [item(1, 'active'), item(2, 'blocked', { blocker: { summary: evil } }), item(3, 'paused', { claim_status: 'stale' }), item(4, 'handover')], recent: [{ at: now, kind: 'completion', item: item(5, 'finished') }],
    overlaps: [{ left: item(1, 'active'), right: item(2, 'active'), reasons: ['a', 'b', 'c', 'd'].map((p) => ({ left_path: p, right_path: p })), reasons_complete: false }],
    current_complete: false, recent_complete: true, overlaps_complete: false, history_retention_gap: true };
  const req = (st, acks) => ({ id: 'r', requester: person('A'), proposal: { title: evil, scope: [] }, targets: [{ lead: person('B'), reasons: ['a', 'b', 'c', 'd', 'e'].map((p) => ({ your_path: p, their_path: p })), reasons_complete: true }],
    status: st, acknowledgements: acks, acknowledgements_complete: false, responses: [], responses_complete: false, updated_at: now });
  const inbox = { from: 0, through: 9, next_after: 5, complete: false, retention_gap: false, requests: ['pending', 'responded', 'resolved', 'withdrawn'].map((s) => req(s, [])) };
  const data = file.startsWith('work') ? wo : inbox;

  const v = load(html);
  assert(v.sent[0].method === 'ui/initialize' && v.sent[0].params.protocolVersion === '2026-01-26');
  assert(v.app.textContent.includes('No data returned.'));
  v.msg({ method: 'ui/notifications/tool-result', params: { content: [{ type: 'text', text: 'not json' }] } });
  assert(v.app.textContent.includes('No data returned.'));
  v.msg({ method: 'ui/notifications/tool-result', params: { content: [{ type: 'text', text: JSON.stringify(data) }] } }); // text fallback
  const text = v.app.textContent;
  assert(text.includes('<img src=x onerror=alert(1)><script>boom()</script>'), 'markup shown as text');
  assert(!text.includes('z'.repeat(301)), 'clipped');
  assert(v.made.every((t) => TAGS.has(t)), 'unexpected tags ' + [...new Set(v.made)]);
  assert(text.includes('Read-only') && text.includes('Manage these views'));
  if (file.startsWith('work')) assert(text.includes('Overlap check incomplete') && !text.includes('No overlaps') && text.includes('partial list') && text.includes('not a Git conflict'));
  else { assert(text.includes('Step 1 of 4: Awaiting receipt') && text.includes('history omitted by the hub') && text.includes('and 2 more')); }
  assert(v.sent.some((m) => m.method === 'ui/notifications/size-changed'));
  // Refresh and paging only ever call the view's own tool.
  const buttons = []; walk(v.app, (e) => e.tag === 'button' && buttons.push(e));
  for (const b of buttons) b.listeners.click();
  const calls = v.sent.filter((m) => m.method === 'tools/call');
  assert(calls.length === buttons.length && calls.every((c) => c.params.name === tool));
  if (!file.startsWith('work')) assert.equal(JSON.stringify(calls[1].params.arguments), JSON.stringify({ after: 5, through: 9 }));
  // A failed refresh keeps the last data and says so.
  v.msg({ id: calls[0].id, error: { code: -1, message: 'x' } });
  await new Promise((r) => setImmediate(r));
  assert(/Could not refresh\. Showing data from \d\d:\d\d:\d\d/.test(v.app.textContent) && v.app.textContent.includes('Read-only'));
  console.log('ok', file, Buffer.byteLength(html), 'bytes');
}
