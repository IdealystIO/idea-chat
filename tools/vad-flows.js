// Who is talking — locally, and as everyone else sees it.
//
// This has been wrong twice, in opposite directions, and neither showed up in
// any other suite:
//
//   - permanently ON, because the model's verdict was computed against an
//     input measurement that was almost always zero (the microphone delivers
//     ~12 frames a second, the model emits 100 blocks) and a zero denominator
//     reads as "the model added 100 dB";
//   - permanently OFF, whenever the fixture is Chromium's default fake device.
//     That is a pure tone, a tone is not speech, and removing what is not
//     speech is exactly what the suppressor does. The indicator was right and
//     the fixture was wrong.
//
// So this runs against a SPEECH-LIKE fixture (tools/fixtures/speech-like.wav:
// a moving pitch, a dozen harmonics, syllables at ~4 Hz, and a noise floor)
// with a known duty cycle, and asserts the indicator tracks it — neither stuck
// on nor stuck off.
//
//   bash tools/reset-dev.sh
//   node tools/vad-flows.js

const { chromium } = require('./node_modules/playwright-core');
const path = require('path');

const CHROME = process.env.CHROME ||
  '/home/nicho/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8091';
const API = APP + '/_srv';
const LOG = '/tmp/hearth-server.log';
const WAV = path.resolve(__dirname, 'fixtures/speech-like.wav');
/// The fixture talks for 1.2 s of every 2 s.
const DUTY = 0.6;

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

/// Sample the indicator fast and count, rather than reading it once.
///
/// The stats hold only the LAST block, so a once-a-second reading lands
/// between syllables and reports zero — which looks exactly like a broken
/// feature and cost an hour the first time.
const watch = (p, seconds) => p.evaluate(async (seconds) => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  let spoke = 0, ticks = 0, lit = 0, minGain = 999, maxGain = -999;
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    let s; try { s = JSON.parse(window.__hearthVoiceStats()); } catch { s = null; }
    if (s) {
      ticks++;
      if (s.speaking) spoke++;
      minGain = Math.min(minGain, s.modelGainDb);
      maxGain = Math.max(maxGain, s.modelGainDb);
    }
    const rows = [...document.querySelectorAll('div')].filter(d => {
      const r = d.getBoundingClientRect();
      return r.x < 320 && r.height > 0 && r.height < 40;
    });
    if (rows.some(d => getComputedStyle(d).borderLeftWidth === '2px')) lit++;
    await sleep(50);
  }
  return { spoke, ticks, lit, minGain, maxGain };
}, seconds);

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

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: [
      '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
      `--use-file-for-fake-audio-capture=${WAV}`,
    ],
  });
  const open = async (handle, password, mode) => {
    const p = await browser.newPage({ viewport: { width: 1440, height: 900 }, permissions: ['microphone'] });
    await p.addInitScript(m => { try { localStorage.setItem('hearth.suppress', m); } catch {} }, mode);
    await p.goto(APP + '/', { waitUntil: 'load' });
    await p.waitForTimeout(3000);
    await fill(p, 'Handle or email', handle);
    await fill(p, 'Password', password);
    await clickText(p, 'Sign in');
    await p.waitForTimeout(3000);
    return p;
  };

  // --- the level path, with no model in the chain ---
  {
    const a = await open('ana', 'password123', 'off');
    await clickRoom(a, 'The Porch');
    await a.waitForTimeout(5000);
    const r = await watch(a, 8);
    const share = r.spoke / r.ticks;
    console.log(`        off:    speaking ${r.spoke}/${r.ticks} (${(share * 100).toFixed(0)}%), lit ${r.lit}`);
    check('off — the indicator follows the fixture rather than sitting still',
      share > 0.25 && share < 0.9, `${(share * 100).toFixed(0)}% against a ${DUTY * 100}% duty cycle`);
    check('off — and the sidebar lights up with it', r.lit > 0, `${r.lit} lit samples`);
    await a.close();
  }

  // --- the model's verdict ---
  {
    const a = await open('ana', 'password123', 'client');
    await clickRoom(a, 'The Porch');
    // The chunk and the model have to load on the first join.
    await a.waitForTimeout(12000);
    const r = await watch(a, 8);
    const share = r.spoke / r.ticks;
    console.log(`        client: speaking ${r.spoke}/${r.ticks} (${(share * 100).toFixed(0)}%), gain [${r.minGain}..${r.maxGain}], lit ${r.lit}`);

    // Both failures this suite exists for, stated as bounds rather than as a
    // target: the point is that it MOVES, not that it agrees with a synthetic
    // fixture to a percentage point.
    check('client — the indicator is not stuck on', share < 0.95,
      `${(share * 100).toFixed(0)}% — a verdict computed against a zero denominator reads as permanently speaking`);
    check('client — the indicator is not stuck off', share > 0.05,
      `${(share * 100).toFixed(0)}% — the model may be removing the fixture entirely`);
    check('client — the model gain is a believable number', r.minGain > -41 && r.maxGain < 41,
      `[${r.minGain}..${r.maxGain}] dB`);
    await a.close();
  }

  // --- and the far side hears about it ---
  {
    const a = await open('ana', 'password123', 'off');
    const t = await open('theo', 'password123', 'off');
    await clickRoom(a, 'The Porch');
    await clickRoom(t, 'The Porch');
    await a.waitForTimeout(6000);

    // Theo's view of ANA. Ana's own row is driven locally, so reading theo's
    // page is the only way to prove the event actually crossed the wire.
    const seen = await t.evaluate(async () => {
      const sleep = ms => new Promise(r => setTimeout(r, ms));
      let lit = 0, ticks = 0;
      const deadline = Date.now() + 8000;
      while (Date.now() < deadline) {
        ticks++;
        const rows = [...document.querySelectorAll('div')].filter(d => {
          const r = d.getBoundingClientRect();
          return r.x < 320 && r.height > 0 && r.height < 40;
        });
        if (rows.some(d => (d.innerText || '').includes('Ana')
              && getComputedStyle(d).borderLeftWidth === '2px')) lit++;
        await sleep(50);
      }
      return { lit, ticks };
    });
    console.log(`        theo sees ana talking: ${seen.lit}/${seen.ticks}`);
    check('theo sees ana light up', seen.lit > 0, JSON.stringify(seen));
    check('and not permanently', seen.lit < seen.ticks, JSON.stringify(seen));
  }

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
