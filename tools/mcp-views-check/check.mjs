// Dependency-free static and fake-host checks. Run: node tools/mcp-views-check/check.mjs
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
const html = fs.readFileSync(new URL('../../src/hub/mcp_views/work-overview.html', import.meta.url), 'utf8');
const fixture = JSON.parse(fs.readFileSync(new URL('../../src/hub/fixtures/actionable-status.json', import.meta.url), 'utf8'));
const tags = new Set(['div', 'span', 'p', 'ul', 'li', 'button', 'details', 'summary', 'svg', 'path']);
function mk(tag) {
  const e = { tag, children: [], className: '', _t: '', listeners: {}, attrs: {}, style: { setProperty() {} }, scrollTop: 0 };
  e.appendChild = (c) => (e.children.push(c), c);
  e.addEventListener = (n, f) => (e.listeners[n] = f);
  e.setAttribute = (k, v) => (e.attrs[k] = v);
  e.contains = () => false;
  e.querySelectorAll = () => [];
  Object.defineProperty(e, 'textContent', { get: () => e._t + e.children.map((c) => c.textContent).join(''), set: (v) => { e._t = String(v); e.children = []; } });
  Object.defineProperty(e, 'scrollHeight', { get: () => 100 });
  return e;
}
const walk = (e, f) => { f(e); e.children.forEach((c) => walk(c, f)); };
function load() {
  const app = mk('div'), live = mk('div'), sent = [], made = [], handlers = {};
  const create = (tag) => { made.push(tag); return mk(tag); };
  const doc = { body: mk('body'), documentElement: mk('html'), getElementById: (id) => id === 'live' ? live : app,
    createTextNode: (text) => { const n = mk('#text'); n._t = text; return n; }, createElement: create, createElementNS: (_, tag) => create(tag),
    addEventListener() {}, querySelector: () => null };
  const win = { parent: { postMessage: (m) => sent.push(m) }, addEventListener: (name, fn) => (handlers[name] = fn) };
  const js = html.slice(html.indexOf('<script>') + 8, html.lastIndexOf('</script>'));
  vm.runInNewContext(js, { window: win, document: doc, Promise, Date, setTimeout: () => 1, clearTimeout() {} }, { filename: 'view.js' });
  return { app, sent, made, msg: (m) => handlers.message({ source: win.parent, data: { jsonrpc: '2.0', ...m } }) };
}
assert(Buffer.byteLength(html) < 40000);
assert(!/https?:\/\/(?!www\.w3\.org\/2000\/svg)/.test(html), 'external origin');
assert(!/innerHTML|outerHTML|insertAdjacentHTML|document\.write|eval\(|fetch\(|XMLHttpRequest/.test(html), 'forbidden API');
assert(html.includes('ui/initialize') && html.includes('ui/notifications/initialized') && html.includes('size-changed'));
assert([...html.matchAll(/agentscm_[a-z_]+/g)].every(([name]) => ['agentscm_work_overview', 'agentscm_view_settings'].includes(name)));
const v = load();
assert.equal(v.sent[0].method, 'ui/initialize');
assert.equal(v.sent[0].params.protocolVersion, '2026-01-26');
assert(v.app.textContent.includes('Loading work overview…'));
const evil = '<img src=x onerror=alert(1)><script>boom()</script>';
v.msg({ method: 'ui/notifications/tool-result', params: { isError: true, content: [{ type: 'text', text: evil }] } });
assert(v.app.textContent.includes(evil) && v.app.textContent.includes('Unavailable'));
v.msg({ method: 'ui/notifications/tool-result', params: { content: [{ type: 'text', text: 'not json' }] } });
assert(v.app.textContent.includes('incomplete work overview'));
const status = { ...fixture, complete: false, work: fixture.work.map((w) => ({ ...w, title: evil + 'z'.repeat(400) })) };
v.msg({ method: 'ui/notifications/tool-result', params: { content: [{ type: 'text', text: JSON.stringify({ actionable_status: status }) }] } });
assert(v.app.textContent.includes(evil));
assert(!v.app.textContent.includes('z'.repeat(301)));
assert(v.app.textContent.includes('Coverage incomplete'));
assert(v.made.every((tag) => tags.has(tag)), 'unexpected tag');
const buttons = []; walk(v.app, (e) => { if (e.tag === 'button') buttons.push(e); });
buttons[0].listeners.click();
const call = v.sent.find((m) => m.method === 'tools/call');
assert.equal(call.params.name, 'agentscm_work_overview');
assert.equal(JSON.stringify(call.params.arguments), JSON.stringify({ detail: 'full' }));
v.msg({ id: call.id, error: { message: 'Refresh refused' } });
await new Promise((resolve) => setImmediate(resolve));
assert(/Showing data from \d\d:\d\d:\d\d/.test(v.app.textContent));
assert(v.app.textContent.includes('Refresh refused'));
v.msg({ method: 'ui/notifications/tool-result', params: { structuredContent: { actionable_status: { ...fixture, complete: true, work: [], actions: [] } } } });
assert(v.app.textContent.includes('No current work in this overview.'));
assert(!v.app.textContent.includes('Refresh refused'));
assert(!v.sent.some((m) => ['ui/message', 'ui/update-model-context'].includes(m.method)));
console.log('ok work-overview.html', Buffer.byteLength(html), 'bytes');
