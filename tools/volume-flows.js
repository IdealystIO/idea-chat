// How loud everyone else is — and whether one person can be turned down.
//
// The claim under test is not "a slider moves". It is that a number chosen on
// THIS machine changes what THIS machine hears, and nothing else: the sender
// is not told, the wire does not change, and the packets keep arriving at the
// same rate whether the listener has someone at 100% or at zero.
//
// That last part is what makes the assertions here worth making. A volume
// control implemented by dropping packets, muting a track, or closing a sink
// would pass "it got quieter" and fail "it is still connected" — so every
// silence check below is paired with a check that audio is still flowing.
//
// The mapping is the other risk. A listener chooses a volume against a PERSON
// and the audio path applies it against a four-byte sender number derived from
// that person's account id (voice_proto::participant_id). Get the derivation
// wrong and the slider silently turns down somebody else — which is why the
// per-person checks below assert silence for a specific speaker rather than
// just "quieter than before".
//
//   bash tools/reset-dev.sh
//   node tools/volume-flows.js

const { chromium } = require('./node_modules/playwright-core');
const path = require('path');

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8091';
const API = APP + '/_srv';
const LOG = '/tmp/hearth-server.log';
const WAV = path.resolve(__dirname, 'fixtures/speech-like.wav');

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

/// Peak over a window, plus how many packets arrived during it.
///
/// The peak is sampled rather than read once for the same reason the VAD suite
/// samples: the stats hold only the last block, the fixture has gaps between
/// syllables, and a single reading lands in one of them often enough to look
/// like silence. The packet count is the control — it is what separates "the
/// listener turned this person down" from "the call fell over".
const listen = (p, seconds) => p.evaluate(async (seconds) => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  let peak = 0, ticks = 0, first = null, last = null;
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    let s; try { s = JSON.parse(window.__hearthVoiceStats()); } catch { s = null; }
    if (s) {
      ticks++;
      peak = Math.max(peak, s.remotePeak);
      if (first === null) first = s.recv;
      last = s.recv;
    }
    await sleep(40);
  }
  return { peak, ticks, packets: (last ?? 0) - (first ?? 0) };
}, seconds);

/// Wait until packets are actually arriving, up to `seconds`.
///
/// A fixed sleep is the wrong tool for "is the call back": joining is a ticket,
/// a microphone, and a socket, and how long that takes varies with what else
/// the machine is doing. A too-short sleep reports the mute as still in force
/// on a tab that had simply not finished rejoining — the wrong bug to go
/// looking for. Returns the last stats seen either way, so a genuine failure
/// says what the client thought was happening.
const awaitCall = (p, seconds) => p.evaluate(async (seconds) => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const deadline = Date.now() + seconds * 1000;
  let first = null, last = null;
  while (Date.now() < deadline) {
    let s; try { s = JSON.parse(window.__hearthVoiceStats()); } catch { s = null; }
    if (s) {
      last = s;
      if (first === null) first = s.recv;
      if (s.recv > first) return { live: true, stats: s };
    }
    await sleep(200);
  }
  return { live: false, stats: last };
}, seconds);

/// Open a person's menu and press one of its items.
///
/// Retried rather than pressed once, because the roster is a live region: it
/// rebuilds whenever the live state moves, and a rebuild takes the open menu
/// with it. That was worth finding — a menu that cannot survive a call is a
/// menu whose slider cannot be dragged during one — but a retry here is still
/// right, since an unlucky rebuild between the two clicks is not a failure of
/// the thing under test.
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

(async () => {
  const fs = require('fs');
  if (!fs.existsSync(WAV)) { console.log(`no fixture at ${WAV}`); process.exit(1); }
  const claim = (fs.readFileSync(LOG, 'utf8').match(/claim\/([a-f0-9]+)/) || [])[1];
  if (!claim) { console.log('no claim token — run tools/reset-dev.sh first'); process.exit(1); }

  const claimed = await call('claim_instance',
    [{ token: claim, handle: 'ana', display_name: 'Ana Ruiz', email: null, password: 'password123' }]);
  const anaJar = claimed.cookie;
  const space = (await call('create_space', ['cabin', 'The Cabin'], anaJar)).body.Ok;
  const invite = (await call('create_invite', [space.id, 5], anaJar)).body.Ok;
  await call('register',
    [{ invite_code: invite, handle: 'theo', display_name: 'Theo Lang', email: null, password: 'password123' }]);

  const members = (await call('space_members', [space.id], anaJar)).body.Ok || [];
  const ana = members.find(m => m.handle === 'ana');
  if (!ana) { console.log('could not find ana in the roster'); process.exit(1); }

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: [
      '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
      `--use-file-for-fake-audio-capture=${WAV}`,
    ],
  });

  /// `prefs` is what the app reads at startup, so seeding localStorage is how
  /// this asks "was the stored preference honoured" rather than "does the
  /// slider work" — two different claims, and the stored one is the one that
  /// has to survive a refresh.
  const open = async (handle, password, prefs = {}) => {
    const p = await browser.newPage({ viewport: { width: 1440, height: 900 }, permissions: ['microphone'] });
    p.on('console', m => { const t = m.text(); if (/\[(voice|call-[a-z]+)\]/.test(t) && !t.includes('capture 48000')) console.log(`        ${handle}: ${t}`); });
    p.on('pageerror', e => console.log(`        ${handle} page error: ${e.message}`));
    await p.addInitScript(prefs => {
      try {
        localStorage.setItem('hearth.suppress', 'off');
        for (const [k, v] of Object.entries(prefs)) localStorage.setItem('hearth.' + k, v);
      } catch {}
    }, prefs);
    await p.goto(APP + '/', { waitUntil: 'load' });
    await p.waitForTimeout(3000);
    await fill(p, 'Handle or email', handle);
    await fill(p, 'Password', password);
    await clickText(p, 'Sign in');
    await p.waitForTimeout(3000);
    return p;
  };

  /// Ana talks, theo listens, and we report what theo heard.
  const room = async (theoPrefs, seconds = 6) => {
    const a = await open('ana', 'password123');
    const t = await open('theo', 'password123', theoPrefs);
    await clickRoom(a, 'The Porch');
    await clickRoom(t, 'The Porch');
    await a.waitForTimeout(5000);
    const heard = await listen(t, seconds);
    return { a, t, heard };
  };

  // --- the master, as a stored preference ------------------------------
  let loud, quiet;
  {
    const { a, t, heard } = await room({ 'voice.volume': '1' });
    loud = heard;
    console.log(`        master 100%: peak ${heard.peak}, ${heard.packets} packets`);
    check('theo hears ana at all', heard.peak > 0, JSON.stringify(heard));
    await a.close(); await t.close();
  }
  {
    const { a, t, heard } = await room({ 'voice.volume': '0.3' });
    quiet = heard;
    console.log(`        master  30%: peak ${heard.peak}, ${heard.packets} packets`);
    check('a stored master volume is honoured on join',
      heard.peak > 0 && heard.peak < loud.peak,
      `${heard.peak} against ${loud.peak} at full`);
    check('and turning it down does not slow the stream',
      heard.packets > loud.packets * 0.5,
      `${heard.packets} packets against ${loud.packets}`);
    await a.close(); await t.close();
  }

  // --- one person, by account id ---------------------------------------
  {
    const { a, t, heard } = await room({ 'voice.volume': '1', [`voice.volume.${ana.id}`]: '0' });
    console.log(`        ana at 0%:   peak ${heard.peak}, ${heard.packets} packets`);
    // The whole point of the per-person path: silence for a NAMED speaker,
    // which a wrong id-to-sender mapping cannot produce by accident.
    check('a person set to zero is silent', heard.peak === 0, JSON.stringify(heard));
    check('and their audio is still arriving — this is a mix decision, not a disconnect',
      heard.packets > 0, JSON.stringify(heard));
    await a.close(); await t.close();
  }

  // --- mute is a lease over the level, not a value written into it ------
  //
  // Ana starts at HALF volume, and that is the whole point of the section:
  // after a mute and an unmute she has to come back to half, not to full. A
  // mute implemented by writing zero into the slider passes every check that
  // only asks "is there sound again".
  {
    const { a, t, heard } = await room({ 'voice.volume': '1', [`voice.volume.${ana.id}`]: '0.5' }, 4);
    console.log(`        ana at 50%:  peak ${heard.peak}, ${heard.packets} packets`);
    check('before muting, theo hears ana', heard.peak > 0, JSON.stringify(heard));
    check('and at her set level, not at full',
      heard.peak < loud.peak * 0.6, `${heard.peak} against ${loud.peak} at full`);
    const level = heard.peak;

    const muted = await menuAction(t, 'Ana Ruiz', 'Mute for me');
    await t.waitForTimeout(1500);
    check('the person menu offers a mute', muted);

    const after = await listen(t, 4);
    console.log(`        muted:       peak ${after.peak}, ${after.packets} packets`);
    check('muting someone mid-call silences them without a rejoin',
      after.peak === 0, JSON.stringify(after));
    check('and they are still connected', after.packets > 0, JSON.stringify(after));
    check('the roster says so', (await t.innerText('body')).includes('Muted for you'));

    // Muting must survive a reload: you silence somebody because of something
    // that is still true afterwards, and un-muting makes no sound, so there
    // would be nothing to notice.
    await t.reload({ waitUntil: 'load' });
    await t.waitForTimeout(6000);
    check('and it survives a refresh', (await t.innerText('body')).includes('Muted for you'));
    check('theo can get back into the room', await clickRoom(t, 'The Porch'));
    const relive = await awaitCall(t, 25);
    // Two claims, two checks. Folded into one they report "still silenced"
    // for a tab that had simply not finished rejoining.
    check('the call comes back after the refresh', relive.live, JSON.stringify(relive.stats));
    const rejoined = await listen(t, 5);
    console.log(`        rejoined:    peak ${rejoined.peak}, ${rejoined.packets} packets`);
    check('and the mute is still in force on it', rejoined.peak === 0, JSON.stringify(rejoined));

    await menuAction(t, 'Ana Ruiz', 'Unmute for me');
    await t.waitForTimeout(1500);
    const back = await listen(t, 5);
    console.log(`        unmuted:     peak ${back.peak} (was ${level} before the mute)`);
    check('and unmuting brings them back', back.peak > 0, JSON.stringify(back));
    // The assertion this section exists for.
    check('to the level they were at, not to full',
      back.peak <= level * 1.5 && back.peak >= level * 0.5,
      `${back.peak} against ${level} before the mute, and ${loud.peak} at full`);
    await a.close(); await t.close();
  }

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
