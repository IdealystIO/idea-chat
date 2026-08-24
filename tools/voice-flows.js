#!/usr/bin/env node
// Two people, one voice room, real audio — through Hearth.
//
// This is the loop-test's claim (a non-zero peak on the FAR side is the only
// proof the whole path carried audio) applied to the chat app, plus the thing
// the standalone demo never had: the relay refusing anyone who was not let in
// by the chat server.
//
//   bash tools/reset-dev.sh          # hearth on :8090, voice enabled
//   node tools/voice-flows.js
//
// Needs the local fleet running with the same VOICE_SHARED_SECRET.
const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  '/home/nicho/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = 'http://localhost:8090';
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
  // An unauthenticated call is refused by the session guard before the handler
  // runs, and that refusal is plain text rather than the `{Ok}`/`{Err}` wire
  // shape — so parsing has to survive it.
  const raw = await r.text();
  let body;
  try { body = JSON.parse(raw); } catch { body = { Err: { Failed: raw.trim() } }; }
  return { body, status: r.status, cookie: setCookie ? setCookie.split(';')[0] : null };
};

const fill = async (p, label, value) => {
  await p.evaluate(({ label }) => {
    for (const i of document.querySelectorAll('input')) {
      const box = i.parentElement;
      if (box && (box.innerText || '').trim().startsWith(label)) { i.dataset.target = '1'; return; }
    }
  }, { label });
  const el = await p.$('input[data-target="1"]');
  if (!el) throw new Error(`no field labelled ${label}`);
  await el.click();
  await el.type(value, { delay: 4 });
  await p.evaluate(() => document.querySelectorAll('input[data-target]').forEach(i => delete i.dataset.target));
};

const clickText = async (p, label) => {
  const at = await p.evaluate(({ label }) => {
    const els = [...document.querySelectorAll('div,span,button')].filter(e => {
      if ((e.innerText || '').trim() !== label) return false;
      const r = e.getBoundingClientRect();
      return r.width > 0 && r.height > 0;
    });
    const el = els[els.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (at) await p.mouse.click(at.x, at.y);
  return !!at;
};

/// Click a room in the LIST, not wherever its name happens to appear.
///
/// Once you are in a room its name is also in the tray at the bottom, and a
/// plain text match picks that up — clicking a label that does nothing, which
/// reads in the results as "the room did not respond".
const clickRoom = async (p, label) => {
  const at = await p.evaluate(({ label }) => {
    const els = [...document.querySelectorAll('div,span')].filter(e => {
      if ((e.innerText || '').trim() !== label) return false;
      const r = e.getBoundingClientRect();
      return r.width > 0 && r.height > 0 && r.y < 700;
    });
    const el = els[els.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (at) await p.mouse.click(at.x, at.y);
  return !!at;
};

const text = p => p.evaluate(() => document.body.innerText);
const has = async (p, s) => (await text(p)).includes(s);
const stats = p => p.evaluate(() => {
  try { return JSON.parse(window.__hearthVoiceStats()); }
  catch (e) { return { error: String(e) }; }
});

// The fake mic beeps periodically, so a single reading is zero most of the
// time. Poll and keep the maximum — the same lesson the loop test learned.
const peakOver = async (pages, ms) => {
  const best = pages.map(() => ({ mic: 0, remote: 0, recv: 0, peers: 0 }));
  const until = Date.now() + ms;
  while (Date.now() < until) {
    for (let i = 0; i < pages.length; i++) {
      const s = await stats(pages[i]);
      if (s && !s.error) {
        best[i].mic = Math.max(best[i].mic, s.micPeak);
        best[i].remote = Math.max(best[i].remote, s.remotePeak);
        best[i].recv = Math.max(best[i].recv, s.recv);
        best[i].peers = Math.max(best[i].peers, s.peers);
      }
    }
    await pages[0].waitForTimeout(250);
  }
  return best;
};

(async () => {
  const fs = require('fs');
  const claim = (fs.readFileSync(LOG, 'utf8').match(/claim\/([a-f0-9]+)/) || [])[1];
  if (!claim) { console.log('no claim token — run tools/reset-dev.sh first'); process.exit(1); }

  // --- accounts and a space, over the API ---
  const claimed = await call('claim_instance',
    [{ token: claim, handle: 'ana', display_name: 'Ana Ruiz', email: null, password: 'a good passphrase' }]);
  const anaJar = claimed.cookie;
  const space = (await call('create_space', ['cabin', 'The Cabin'], anaJar)).body.Ok;
  const porch = space.channels.find(c => c.name === 'The Porch');
  const general = space.channels.find(c => c.name === 'general');
  check('a space has a voice room', !!porch, JSON.stringify(space.channels.map(c => c.name)));

  const invite = (await call('create_invite', [space.id, 5], anaJar)).body.Ok;
  const theo = await call('register',
    [{ invite_code: invite, handle: 'theo', display_name: 'Theo Lang', email: null, password: 'another passphrase' }]);
  const theoJar = theo.cookie;
  check('a second person joins the space', !!theoJar);

  // --- the ticket is an authorization decision, not a formality ---
  const outsider = await call('claim_instance', [{ token: 'nope', handle: 'x', display_name: 'x', email: null, password: 'x' }]);
  const anon = await call('voice_ticket', [porch.id], null);
  check('a signed-out caller gets no ticket', !!anon.body.Err, JSON.stringify(anon.body));

  const mine = await call('voice_ticket', [porch.id], anaJar);
  check('a member gets a ticket for their room', !!mine.body.Ok, JSON.stringify(mine.body));
  check('and it names this room', mine.body.Ok && mine.body.Ok.room === porch.id);
  check('and points at this origin', mine.body.Ok && mine.body.Ok.relay === '/_voice');

  const strangerSpace = (await call('create_space', ['vault', 'The Vault'], theoJar)).body.Ok;
  const secretRoom = strangerSpace.channels.find(c => c.name === 'The Porch');
  const trespass = await call('voice_ticket', [secretRoom.id], anaJar);
  check('a room in a space you are not in is refused', !!trespass.body.Err,
    JSON.stringify(trespass.body));

  // --- two browsers, one room, real audio ---
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const open = async (handle, password) => {
    const p = await browser.newPage({ viewport: { width: 1440, height: 900 }, permissions: ['microphone'] });
    await p.goto(APP + '/', { waitUntil: 'load' });
    await p.waitForTimeout(3000);
    await fill(p, 'Handle or email', handle);
    await fill(p, 'Password', password);
    await clickText(p, 'Sign in');
    await p.waitForTimeout(3000);
    return p;
  };

  const a = await open('ana', 'a good passphrase');
  const t = await open('theo', 'another passphrase');
  check('both are signed in', (await has(a, 'The Cabin')) && (await has(t, 'The Cabin')));

  // Ana reads #general first, so we can prove joining does not move her.
  await clickText(a, 'general');
  await a.waitForTimeout(1200);

  await clickRoom(a, 'The Porch');
  await clickRoom(t, 'The Porch');
  await a.waitForTimeout(4000);

  check('ana is connected', await has(a, 'Voice connected'), (await text(a)).slice(0, 200));
  check('theo is connected', await has(t, 'Voice connected'), (await text(t)).slice(0, 200));
  check('and joining did not move ana off #general', await has(a, 'Welcome to #general'));
  check('the tray names the room she is in', await has(a, 'The Porch'));

  // The call view is a second, deliberate act — and it is where camera and
  // screen share live, because a voice room IS the video room.
  await clickRoom(a, 'The Porch');
  await a.waitForTimeout(800);
  check('clicking the room again opens the call view', await has(a, 'Back to chat'));
  check('the call view carries camera and screen share',
    (await has(a, 'Camera off')) && (await has(a, 'Share screen')));
  await clickText(a, 'Back to chat');
  await a.waitForTimeout(800);
  check('closing the call view returns her to the channel',
    await has(a, 'Welcome to #general'));
  check('and she is still in the call', await has(a, 'Voice connected'));

  const [sa, st] = await peakOver([a, t], 9000);
  console.log(`        ana : ${JSON.stringify(sa)}`);
  console.log(`        theo: ${JSON.stringify(st)}`);

  check('ana captured audio', sa.mic > 0, JSON.stringify(sa));
  check('theo captured audio', st.mic > 0, JSON.stringify(st));
  check('ana received packets from theo', sa.recv > 0, JSON.stringify(sa));
  check('theo received packets from ana', st.recv > 0, JSON.stringify(st));
  check('ana sees exactly one peer', sa.peers === 1, JSON.stringify(sa));
  check('theo sees exactly one peer', st.peers === 1, JSON.stringify(st));
  // The claim that matters: a real signal came out the far end, not just bytes.
  check('ana HEARD theo', sa.remote > 0, JSON.stringify(sa));
  check('theo HEARD ana', st.remote > 0, JSON.stringify(st));

  // --- leaving actually hangs up ---
  // The hang-up is the last control in the tray strip — an icon, so it is
  // found by where it lives rather than by its label.
  const hangUpAt = await a.evaluate(() => {
    const label = [...document.querySelectorAll('div,span')]
      .find(e => (e.innerText || '').trim() === 'Voice connected');
    if (!label) return null;
    // idea-ui renders an IconButton as a styled div, not a <button>, so the
    // controls are found by shape: the small square children of the strip.
    let strip = label;
    for (let i = 0; i < 4 && strip.parentElement; i++) {
      strip = strip.parentElement;
      const squares = [...strip.children].filter(c => {
        const r = c.getBoundingClientRect();
        return r.width > 16 && r.width < 40 && Math.abs(r.width - r.height) < 6;
      });
      if (squares.length >= 2) {
        const r = squares[squares.length - 1].getBoundingClientRect();
        return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
      }
    }
    return null;
  });
  check('the tray offers a way to hang up', !!hangUpAt);
  if (hangUpAt) await a.mouse.click(hangUpAt.x, hangUpAt.y);
  await a.waitForTimeout(900);
  const hangup = await a.evaluate(() => {
    const els = [...document.querySelectorAll('div,span,button')];
    return !!els.find(e => (e.innerText || '').includes('Voice connected'));
  });
  check('leaving clears the connected state', !hangup);

  // And it really hung up — the transport is torn down, not merely hidden.
  const after = await stats(a);
  check('and the session is actually torn down', after.peers === 0,
    JSON.stringify(after));
  // Theo's peer count does not decrement — it counts senders heard since he
  // joined, not who is present. What must stop is packets arriving.
  const before = (await stats(t)).recv;
  await t.waitForTimeout(2500);
  const later = (await stats(t)).recv;
  check('theo stops receiving once ana hangs up', later === before,
    `recv ${before} -> ${later}`);

  console.log(`\n${pass} passed, ${fail} failed`);
  await browser.close();
  process.exit(fail ? 1 : 0);
})();
