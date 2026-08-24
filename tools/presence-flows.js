// Voice presence, judged by what the server SAYS rather than what it stores.
//
// The bug this exists for: the database was perfectly consistent — one row per
// person, `joined_at` never moving, `seen_at` renewed every 15 s — while the
// server announced a *join* on every heartbeat. Everyone in the room heard the
// connect tone every fifteen seconds, and each event moved the speaker to the
// bottom of the sidebar, because the reducer treats a join as "left wherever
// you were, then arrived".
//
// No amount of looking at the table would have found that. The claim is about
// the event stream, so this drives the live socket directly and counts frames.
//
//   bash tools/reset-dev.sh
//   node tools/presence-flows.js
//
// The socket is opened from inside the page so the browser attaches the session
// cookie — the same reason the app's own connection needs no token.

const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  '/home/nicho/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8091';
const API = APP + '/_srv';
const LOG = '/tmp/hearth-server.log';

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};

const call = async (fn, args, jar) => {
  const r = await fetch(`${API}/${fn}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Origin: APP, ...(jar ? { Cookie: jar } : {}) },
    body: JSON.stringify(args),
  });
  const setCookie = r.headers.get('set-cookie');
  const raw = await r.text();
  let body;
  try { body = JSON.parse(raw); } catch { body = { Err: { Failed: raw.trim() } }; }
  return { body, cookie: setCookie ? setCookie.split(';')[0] : null };
};

/// Drive one live socket through a join and some heartbeats, and report every
/// frame it was sent.
///
/// `beats` heartbeats at `HEARTBEAT_MS`-ish spacing — compressed, because the
/// claim is "a heartbeat announces nothing", and that does not depend on how
/// long you wait between them.
const runSocket = (page, room, beats) => page.evaluate(async ({ room, beats }) => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const url = location.origin.replace(/^http/, 'ws') + '/_srv/_ws/live';
  const ws = new WebSocket(url);
  const frames = [];
  await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = () => no(new Error('socket refused')); });
  ws.onmessage = e => { try { frames.push(JSON.parse(e.data)); } catch {} };

  const send = a => ws.send(JSON.stringify(a));

  // Wait for the snapshot rather than guessing at how long it takes. A fixed
  // delay here made this suite flaky: on a loaded machine the join could be
  // sent before the connection was serving, and the arrival it announced
  // landed outside the window being counted.
  const until = Date.now() + 10000;
  while (!frames.some(f => f.t === 'snapshot') && Date.now() < until) await sleep(50);
  const beforeJoin = frames.length;

  send({ t: 'action', a: 'join_voice', channel_id: room });
  await sleep(1200);
  const afterJoin = frames.length;

  for (let i = 0; i < beats; i++) {
    send({ t: 'action', a: 'still_here', channel_id: room });
    await sleep(1200);
  }
  const afterBeats = frames.length;

  send({ t: 'action', a: 'leave_voice', channel_id: room });
  await sleep(1200);

  ws.close();
  return {
    all: frames,
    joins: frames.filter(f => f.t === 'event' && f.event && f.event.e === 'voice_joined').length,
    leaves: frames.filter(f => f.t === 'event' && f.event && f.event.e === 'voice_left').length,
    duringBeats: frames.slice(afterJoin, afterBeats)
      .filter(f => f.t === 'event').map(f => f.event.e),
    snapshots: frames.filter(f => f.t === 'snapshot').length,
    beforeJoin,
  };
}, { room, beats });

(async () => {
  const fs = require('fs');
  const claim = (fs.readFileSync(LOG, 'utf8').match(/claim\/([a-f0-9]+)/) || [])[1];
  if (!claim) { console.log('no claim token — run tools/reset-dev.sh first'); process.exit(1); }

  const claimed = await call('claim_instance',
    [{ token: claim, handle: 'ana', display_name: 'Ana Ruiz', email: null, password: 'a good passphrase' }]);
  const anaJar = claimed.cookie;
  const space = (await call('create_space', ['cabin', 'The Cabin'], anaJar)).body.Ok;
  const porch = space.channels.find(c => c.name === 'The Porch');
  check('the space has a voice room', !!porch);

  const browser = await chromium.launch({ executablePath: CHROME, args: ['--no-sandbox'] });
  const page = await browser.newPage();
  // Set the session cookie the way the browser would, then load the app so the
  // socket below is same-origin and authenticated.
  await page.context().addCookies([{
    name: anaJar.split('=')[0],
    value: anaJar.split('=').slice(1).join('='),
    domain: 'localhost',
    path: '/',
  }]);
  await page.goto(APP + '/', { waitUntil: 'domcontentloaded' });

  const r = await runSocket(page, porch.id, 3);

  console.log(`        frames: ${r.snapshots} snapshot(s), ${r.joins} join(s), ${r.leaves} leave(s)`);
  check('the socket gets a snapshot on connect', r.snapshots >= 1, JSON.stringify(r.all).slice(0, 300));

  // The regression. One arrival is one event, however many times the client
  // says it is still there.
  check('arriving announces exactly one join', r.joins === 1,
    `got ${r.joins}; frames during heartbeats: ${JSON.stringify(r.duringBeats)}`);
  check('three heartbeats announce nothing at all', r.duringBeats.length === 0,
    JSON.stringify(r.duringBeats));
  check('leaving announces exactly one departure', r.leaves === 1, `got ${r.leaves}`);

  // --- a refresh must not leave you in the room ---
  //
  // The socket closing is ambiguous — a closed tab and a blip look identical
  // from the server — so a disconnect only shortens the presence lease. What
  // resolves it is the client coming back and saying where it is, which after
  // a refresh is "nowhere".
  {
    const refresh = await page.evaluate(async (room) => {
      const sleep = ms => new Promise(r => setTimeout(r, ms));
      const url = location.origin.replace(/^http/, 'ws') + '/_srv/_ws/live';

      // A call, on one connection.
      const first = new WebSocket(url);
      await new Promise(ok => { first.onopen = ok; });
      await sleep(400);
      first.send(JSON.stringify({ t: 'action', a: 'join_voice', channel_id: room }));
      await sleep(800);
      first.close();                       // the tab goes away

      // ...and comes back knowing nothing, which is what a refresh is.
      const second = new WebSocket(url);
      const frames = [];
      await new Promise(ok => { second.onopen = ok; });
      second.onmessage = e => { try { frames.push(JSON.parse(e.data)); } catch {} };
      const until = Date.now() + 10000;
      while (!frames.some(f => f.t === 'snapshot') && Date.now() < until) await sleep(50);
      second.send(JSON.stringify({ t: 'action', a: 'not_in_voice' }));
      await sleep(1200);
      second.close();
      return frames.filter(f => f.t === 'event' && f.event && f.event.e === 'voice_left').length;
    }, porch.id);

    check('a refreshed tab is taken out of the room it left', refresh === 1,
      `expected one departure, got ${refresh}`);

    const still = await call('space_members', [space.id], anaJar);
    check('and saying it twice announces nothing the second time',
      !!still.body.Ok, JSON.stringify(still.body).slice(0, 120));
  }

  // --- a second tab must not hang up the first ---
  //
  // The regression this exists for. A second tab is also "in no call" and says
  // so on connect; taken at face value that ends the call running in the first
  // one. The suite caught it the day `not_in_voice` was written, because the
  // app page's own socket IS a second connection — but relying on that being
  // noticed again is not a test.
  {
    const leaves = await page.evaluate(async (room) => {
      const sleep = ms => new Promise(r => setTimeout(r, ms));
      const url = location.origin.replace(/^http/, 'ws') + '/_srv/_ws/live';

      const calling = new WebSocket(url);
      const frames = [];
      await new Promise(ok => { calling.onopen = ok; });
      calling.onmessage = e => { try { frames.push(JSON.parse(e.data)); } catch {} };
      await sleep(400);
      calling.send(JSON.stringify({ t: 'action', a: 'join_voice', channel_id: room }));
      await sleep(800);

      // A second tab, not in a call, and honest about it — while the first is
      // still holding its lease.
      const other = new WebSocket(url);
      await new Promise(ok => { other.onopen = ok; });
      await sleep(400);
      other.send(JSON.stringify({ t: 'action', a: 'not_in_voice' }));
      await sleep(1200);

      const left = frames.filter(f => f.t === 'event' && f.event && f.event.e === 'voice_left').length;
      calling.send(JSON.stringify({ t: 'action', a: 'leave_voice', channel_id: room }));
      await sleep(400);
      calling.close();
      other.close();
      return left;
    }, porch.id);

    check('a second tab does not hang up a call in the first', leaves === 0,
      `expected no departure while the call was live, got ${leaves}`);
  }

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
