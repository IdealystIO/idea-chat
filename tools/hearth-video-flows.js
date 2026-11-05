#!/usr/bin/env node
// Hearth's call stage with video, and a moderator removing someone — the
// parts of the call stack hearth gained beyond audio.
//
//   bash tools/reset-dev.sh          # a fresh instance, voice on
//   node tools/hearth-video-flows.js
//
// Two people in The Porch: Ana (who owns the space, so moderates it) and Theo.
// Ana turns her camera on and Theo sees it, live; Theo shares his screen and
// Ana sees that, as a tile of its own; muting does not restart anyone's
// video; and Ana removes Theo from the call, which the relay enforces — his
// call ends, and the room shows him gone.
const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
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

// Click a real BUTTON with this label, not merely something displaying the
// text. The tray shows "Muted" as a status word while the call view offers it
// as a switch, so a match-anything click lands on the label and does nothing —
// which only became possible once the tray started updating correctly.
const clickButton = async (p, label) => {
  const at = await p.evaluate(({ label }) => {
    // idea-ui does not render real <button> elements, so "is it a control" has
    // to be asked a different way: an interactive element gets a pointer
    // cursor and a status word does not.
    const els = [...document.querySelectorAll('div,span,button')].filter(e => {
      if ((e.innerText || '').trim() !== label) return false;
      const r = e.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) return false;
      return getComputedStyle(e).cursor === 'pointer';
    });
    const el = els[els.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (!at) return false;
  await p.mouse.click(at.x, at.y);
  return true;
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

/// The names listed under a voice room in the sidebar.
///
/// Read by geometry rather than by class, because the claim under test is a
/// visual one: these names appear beneath that room. Occupant rows are
/// indented past the room's icon, so they are the rows below it that start
/// further right — the next thing at the room's own indent ends the group.
const occupantsUnder = async (p, room) => p.evaluate(({ room }) => {
  // Read the room's own group, not "rows below it": occupants are indented
  // with padding, which does not move the element's x, so geometry cannot tell
  // where the group ends. The DOM can — a room and its occupants are one box.
  //
  // Rows carry more than one line: a room row is "The Porch\n2" (name plus
  // occupant count) and an occupant row is "SA\nSam" (initials plus name).
  const first = l => (l.innerText || '').trim().split('\n')[0].trim();
  const head = [...document.querySelectorAll('div')].find(d => {
    const r = d.getBoundingClientRect();
    return r.x < 320 && r.width > 0 && r.height > 0 && r.height < 40 && first(d) === room;
  });
  if (!head) return null;

  // Up from the row to the box that holds the row AND anyone under it.
  let group = head;
  for (let i = 0; i < 4 && group.parentElement; i++) {
    group = group.parentElement;
    if (group.children.length > 1 && group.children[0].contains(head)) break;
  }
  // An occupant row is exactly "initials, name" — two lines, the first of them
  // an avatar's one or two letters. Matching that shape rather than "any child
  // after the first" matters for the LAST room in the list, where climbing for
  // the group can otherwise reach far enough to pick up the tray below it.
  const out = [];
  for (let i = 1; i < group.children.length; i++) {
    const lines = (group.children[i].innerText || '').trim().split('\n')
      .map(x => x.trim()).filter(Boolean);
    if (lines.length !== 2 || lines[0].length > 2) continue;
    if (!out.includes(lines[1])) out.push(lines[1]);
  }
  return out;
}, { room });

const text = p => p.evaluate(() => document.body.innerText);
const has = async (p, s) => (await text(p)).includes(s);
const stats = p => p.evaluate(() => {
  try { return JSON.parse(window.__hearthVoiceStats()); }
  catch (e) { return { error: String(e) }; }
});
const menuAction = async (p, person, item) => {
  for (let attempt = 0; attempt < 4; attempt++) {
    await clickText(p, person);
    for (let waited = 0; waited < 2000; waited += 200) {
      await p.waitForTimeout(200);
      if (await clickText(p, item)) return true;
    }
  }
  return false;
};


/// Every <video> on the page that is playing a picture.
const playing = (p) => p.evaluate(() => [...document.querySelectorAll('video')]
  .filter(v => v.videoWidth > 0 && !v.paused).length);

/// A screen share the test controls: a canvas that keeps changing. Headless
/// Chromium has no screen to pick.
const fakeScreen = () => {
  navigator.mediaDevices.getDisplayMedia = async () => {
    const c = document.createElement('canvas');
    c.width = 1280; c.height = 720;
    const x = c.getContext('2d');
    let n = 0;
    setInterval(() => { x.fillStyle = `hsl(${(n++ * 7) % 360} 70% 50%)`; x.fillRect(0, 0, 1280, 720); }, 50);
    return c.captureStream(15);
  };
};

(async () => {
  const fs = require('fs');
  const claim = (fs.readFileSync(LOG, 'utf8').match(/claim\/([a-f0-9]+)/) || [])[1];
  if (!claim) { console.log('no claim token — run tools/reset-dev.sh first'); process.exit(1); }

  const claimed = await call('claim_instance',
    [{ token: claim, handle: 'ana', display_name: 'Ana Ruiz', email: null, password: 'password123' }]);
  const anaJar = claimed.cookie;
  const space = (await call('create_space', ['cabin', 'The Cabin'], anaJar)).body.Ok;
  const porch = space.channels.find(c => c.name === 'The Porch');
  const invite = (await call('create_invite', [space.id, 5], anaJar)).body.Ok;
  const theoReg = await call('register',
    [{ invite_code: invite, handle: 'theo', display_name: 'Theo Lang', email: null, password: 'password123' }]);
  const members = (await call('space_members', [space.id], anaJar)).body.Ok || [];
  const theoId = (members.find(m => m.handle === 'theo') || {}).id;

  // A member cannot remove anyone; the server says so.
  const refused = await call('voice_remove', [porch.id, theoId], theoReg.cookie);
  check('a member cannot remove someone from a call', !!refused.body.Err, JSON.stringify(refused.body));

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const open = async (handle) => {
    const p = await browser.newPage({ viewport: { width: 1440, height: 900 }, permissions: ['microphone', 'camera'] });
    p.on('console', m => {
      const t = m.text();
      if ((/\[(voice|call-[a-z]+)\]/.test(t) && !t.includes('capture 48000')) || m.type() === 'error') console.log(`        ${handle}: ${t}`);
    });
    p.on('pageerror', e => console.log(`        ${handle} page error: ${e.message}`));
    await p.addInitScript(() => { try { localStorage.setItem('hearth.suppress', 'off'); } catch {} });
    await p.addInitScript(fakeScreen);
    await p.goto(APP + '/', { waitUntil: 'load' });
    await p.waitForTimeout(3000);
    await fill(p, 'Handle or email', handle);
    await fill(p, 'Password', 'password123');
    await clickText(p, 'Sign in');
    await p.waitForTimeout(3000);
    return p;
  };

  const a = await open('ana');
  const t = await open('theo');
  await clickRoom(a, 'The Porch');
  await clickRoom(t, 'The Porch');
  await a.waitForTimeout(4000);
  check('both are in the call', (await has(a, 'Voice connected')) && (await has(t, 'Voice connected')));

  // The call view: a second click on the room.
  await clickRoom(a, 'The Porch');
  await clickRoom(t, 'The Porch');
  await a.waitForTimeout(1500);
  check('the stage shows both people, from presence',
    (await has(a, 'Theo Lang')) && (await has(t, 'Ana Ruiz')));

  // --- a camera ---
  check('the camera button is there', await clickButton(a, 'Camera off'));
  await a.waitForTimeout(5000);
  check('ana sees her own camera on the stage', (await playing(a)) >= 1, `${await playing(a)} playing`);
  check('theo sees ana\'s camera, live', (await playing(t)) >= 1, `${await playing(t)} playing`);
  const ts = await stats(t);
  check('and it arrived over the call\'s link', ts.videoDown > 0, JSON.stringify(ts));

  // Muting must not rebuild the stage: the videos are the same elements after.
  await a.evaluate(() => document.querySelectorAll('video').forEach(v => { v.__kept = true; }));
  await t.evaluate(() => document.querySelectorAll('video').forEach(v => { v.__kept = true; }));
  await clickButton(a, 'Mic on');
  await a.waitForTimeout(1200);
  const keptA = await a.evaluate(() => [...document.querySelectorAll('video')].map(v => !!v.__kept));
  const keptT = await t.evaluate(() => [...document.querySelectorAll('video')].map(v => !!v.__kept));
  check('muting does not restart anyone\'s video', keptA.length > 0 && keptA.every(Boolean) && keptT.every(Boolean),
    JSON.stringify({ ana: keptA, theo: keptT }));
  await clickButton(a, 'Muted');

  // --- a screen share ---
  check('the share button is there', await clickButton(t, 'Share screen'));
  await t.waitForTimeout(4500);
  check('ana sees theo\'s screen as a tile of its own', await has(a, "Theo Lang's screen"), (await text(a)).slice(0, 400));
  check('and it is playing', (await playing(a)) >= 2, `${await playing(a)} playing`);
  check('theo sees his own screen', await has(t, 'Your screen'));
  await clickButton(t, 'Stop sharing');
  await t.waitForTimeout(3500);
  check('the screen tile goes when he stops', !(await has(a, "Theo Lang's screen")));

  // --- a moderator removes someone ---
  await clickText(a, 'Back to chat');
  await a.waitForTimeout(800);
  const removed = await menuAction(a, 'Theo Lang', 'Remove from call');
  check('a moderator\'s person menu offers "Remove from call"', removed);
  await t.waitForTimeout(2500);
  check('theo\'s call ends at once', !(await has(t, 'Voice connected')), (await text(t)).slice(0, 300));
  check('and he is told why', await has(t, 'removed you from this call'), (await text(t)).slice(0, 400));
  const after = await stats(t);
  check('and his session is really gone', after.peers === 0 && after.transport === 'none', JSON.stringify(after));
  check('ana still has her call', await has(a, 'Voice connected'));
  check('and the room no longer lists him', !(await occupantsUnder(a, 'The Porch') || []).includes('Theo Lang'),
    JSON.stringify(await occupantsUnder(a, 'The Porch')));

  // A kick is not a ban: he can come back.
  await clickRoom(t, 'The Porch');
  await t.waitForTimeout(4000);
  check('a kick is not a ban — he can rejoin', await has(t, 'Voice connected'), (await text(t)).slice(0, 200));

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
