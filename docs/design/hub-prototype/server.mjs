// Local preview server: serves public/ and streams a synthetic Hub story over SSE.
// It opens with the whole story already played, then keeps sending new, random but valid events so the Hub stays live.
// One shared clock, so every open tab shows the same moment.
// Usage: node server.mjs [port]
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { scenarios } from './scenarios.mjs';
import * as hub from './public/core.js';
import { invent, due, resetLive } from './live.mjs';

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), 'public');
const port = Number(process.argv[2] || process.env.PORT || 5210);
for (const s of Object.values(scenarios)) s.all = [...s.seed, ...s.events, ...s.chats].sort((a, b) => a.t - b.t);

// Story minutes per real second once the scripted day is over: slow enough that "2 min ago" reads true.
const LIVE_RATE = 0.25;
const clock = { scn: 'team', t: 0, playing: true, speed: 1 };
const clients = new Set();
const scn = () => scenarios[clock.scn];
const meta = () => { const { seed, events, chats, all, ...m } = scn(); return m; };
const clockMsg = () => ({ ...clock, end: scn().end });

function send(res, event, data) { res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`); }
function reset(c) {
  const evs = scn().all.filter(e => e.t <= clock.t);
  c.sent = evs.length;
  send(c.res, 'reset', { meta: meta(), events: evs, clock: clockMsg() });
}

// The server keeps its own copy of the Hub state, built with the page's reducer, so every random event
// it invents points at a task, pull request, agent or chat that really exists at that moment.
let fed = 0;
function catchUp() { const all = scn().all; while (fed < all.length && all[fed].t <= clock.t) hub.apply(all[fed++]); }
function start(name) {
  clock.scn = name; clock.t = scn().end; clock.playing = true;
  scn().all = scn().all.filter(e => !e.live);
  hub.init(meta()); fed = 0; catchUp(); hub.S.t = clock.t;
}

function emit(e) { if (!e) return; scn().all.push(e); catchUp(); }
let nextAt = Date.now() + 4000;

let last = Date.now();
setInterval(() => {
  const now = Date.now();
  if (clock.playing) clock.t += ((now - last) / 1000) * (clock.t < scn().end ? scn().rate : LIVE_RATE) * clock.speed;
  last = now;
  hub.S.t = clock.t; catchUp();
  if (clock.t >= scn().end) {
    // A bad invented event must not take the preview down; log it and keep streaming.
    try { due(clock.t, now).forEach(emit); if (now >= nextAt) { emit(invent(hub, clock.t, now)); nextAt = now + 4000 + Math.random() * 4000; } }
    catch (err) { console.error('live event skipped:', err.message); nextAt = now + 4000; }
  }
  const all = scn().all;
  for (const c of clients) {
    const fresh = [];
    while (c.sent < all.length && all[c.sent].t <= clock.t) fresh.push(all[c.sent++]);
    if (fresh.length) send(c.res, 'events', fresh);
    send(c.res, 'clock', clockMsg());
  }
}, 250);

function control(body) {
  const { action, value } = body;
  if (action === 'play') clock.playing = true;
  else if (action === 'pause') clock.playing = false;
  else if (action === 'speed') clock.speed = Math.max(0.25, Math.min(16, Number(value) || 1));
  else if (action === 'seek') { clock.t = Math.max(0, Math.min(scn().end, Number(value) || 0)); scn().all = scn().all.filter(e => !e.live); resetLive(); hub.init(meta()); fed = 0; catchUp(); }
  else if (action === 'scenario' && scenarios[value]) { resetLive(); start(value); }
  else return false;
  if (action === 'seek' || action === 'scenario') for (const c of clients) reset(c);
  return true;
}

start('team');

const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.woff2': 'font/woff2', '.woff': 'font/woff', '.svg': 'image/svg+xml' };

http.createServer((req, res) => {
  const url = new URL(req.url, 'http://x');
  if (url.pathname === '/api/stream') {
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' });
    const c = { res, sent: 0 };
    clients.add(c);
    reset(c);
    req.on('close', () => clients.delete(c));
    return;
  }
  if (url.pathname === '/api/control' && req.method === 'POST') {
    let raw = '';
    req.on('data', d => { raw += d; if (raw.length > 1e4) req.destroy(); });
    req.on('end', () => {
      let ok = false;
      try { ok = control(JSON.parse(raw)); } catch { /* bad JSON falls through to 400 */ }
      res.writeHead(ok ? 200 : 400, { 'content-type': 'application/json' });
      res.end(JSON.stringify(ok ? clockMsg() : { error: 'Unknown control action' }));
    });
    return;
  }
  if (url.pathname === '/api/state') {
    res.writeHead(200, { 'content-type': 'application/json' });
    return res.end(JSON.stringify({ clock: clockMsg(), meta: meta(), events: scn().all.filter(e => e.t <= clock.t) }));
  }
  const file = path.normalize(path.join(root, url.pathname === '/' ? 'index.html' : decodeURIComponent(url.pathname)));
  if (!file.startsWith(root)) { res.writeHead(403); return res.end(); }
  fs.readFile(file, (err, buf) => {
    if (err) { res.writeHead(404); return res.end('Not found'); }
    res.writeHead(200, { 'content-type': types[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
    res.end(buf);
  });
}).listen(port, '127.0.0.1', () => console.log(`AgentSCM Hub story preview: http://127.0.0.1:${port}/`));
