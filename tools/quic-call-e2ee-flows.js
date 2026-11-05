#!/usr/bin/env node
// quic-call with end-to-end encryption (call-e2ee): four pages in one room —
// two sharing a key, one with another key, one with none.
//
//   cargo run --release -p call-relay -- 127.0.0.1:4472
//   idealyst serve crates/quic-call/dist/web --port 8098
//   RELAY=https://127.0.0.1:4472/ HASH=<printed> node tools/quic-call-e2ee-flows.js
//
// Passing means the two with the same key hear each other through a relay
// that cannot read them, and everyone else is shown as unreadable rather than
// played as noise.
const { chromium } = require('./node_modules/playwright-core');

const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8098';
const RELAY = process.env.RELAY || 'https://127.0.0.1:4472/';
const HASH = process.env.HASH || '';
const SECONDS = Number(process.env.SECONDS || 6);

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

const counters = async (p) => {
  const text = await p.evaluate(() => document.body.innerText);
  const line = text.split('\n').find(l => /^sent \d/.test(l)) || '';
  const n = (k) => Number((line.match(new RegExp(`${k} (\\d+)`)) || [])[1] || 0);
  return { line, sent: n('sent'), received: n('received'), peers: n('peers') };
};
const unreadableMarks = (p) => p.getByRole('img', { name: 'Cannot decrypt', exact: true }).count();
const locked = (p) => p.getByRole('img', { name: 'End-to-end encrypted', exact: true }).isVisible().catch(() => false);

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const consoleLines = [];
  const open = async (name, key) => {
    const p = await browser.newPage({ permissions: ['microphone', 'camera'] });
    p.on('console', m => { if (m.text().includes('[call-session]')) consoleLines.push(`${name}: ${m.text()}`); });
    p.on('pageerror', e => console.log(`        ${name} page error: ${e.message}`));
    const q = new URLSearchParams({ url: RELAY, hash: HASH });
    await p.goto(`${APP}/?${q}${key ? `#key=${encodeURIComponent(key)}` : ''}`);
    await btn(p, 'Join').waitFor({ timeout: 60000 });
    // Suppression off: this is about who can read whom, not the audio.
    await btn(p, 'Settings').click();
    await p.getByText('Off', { exact: true }).first().click();
    await btn(p, 'Done').click();
    return p;
  };

  // The lobby: the key comes from the address, and the dice make a new one.
  const K = 'correct horse battery staple';
  const a = await open('a', K);
  const field = a.getByLabel('End-to-end key');
  check('the key is read from the address fragment', (await field.inputValue()) === K, await field.inputValue());
  const lobby = await open('lobby', null);
  const empty = lobby.getByLabel('End-to-end key');
  await empty.locator('xpath=ancestor::*[.//*[@role="button"] or .//button][1]').locator('[role="button"], button').last().click();
  const made = await empty.inputValue();
  check('the dice make a fresh 128-bit key', /^[A-Za-z0-9_-]{22}$/.test(made), made);
  await lobby.close();

  const b = await open('b', K);
  const c = await open('c', 'some other key');
  const d = await open('d', null);
  for (const p of [a, b, c, d]) await btn(p, 'Join').click();
  for (const p of [a, b, c, d]) await btn(p, 'Leave').waitFor({ timeout: 30000 });

  check('joining puts the key in the address, for sharing',
        decodeURIComponent(new URL(a.url()).hash) === `#key=${K}`, a.url());

  // The indicator refreshes once a second.
  await sleep(SECONDS * 1000);
  check('the encrypted pages show the lock', await locked(a) && await locked(b) && await locked(c));
  check('the page with no key does not', !(await locked(d)));
  const marks = { a: await unreadableMarks(a), b: await unreadableMarks(b), c: await unreadableMarks(c), d: await unreadableMarks(d) };
  check('a and b cannot read c (another key) or d (no key)', marks.a === 2 && marks.b === 2, JSON.stringify(marks));
  check('c cannot read anyone', marks.c === 3, JSON.stringify(marks));
  check('d, with no key, sees everyone as encrypted', marks.d === 3, JSON.stringify(marks));

  for (const p of [a, b, c, d]) {
    await btn(p, 'Settings').click();
    await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
  }
  const t0 = { a: await counters(a), b: await counters(b), c: await counters(c), d: await counters(d) };
  await sleep(3000);
  const t1 = { a: await counters(a), b: await counters(b), c: await counters(c), d: await counters(d) };
  const got = (k) => t1[k].received - t0[k].received;
  console.log(`        received in 3 s: a ${got('a')}, b ${got('b')}, c ${got('c')}, d ${got('d')}`);
  check('a hears b, and only b', got('a') > 100 && t1.a.peers === 1, t1.a.line);
  check('b hears a, and only a', got('b') > 100 && t1.b.peers === 1, t1.b.line);
  check('c hears nobody', got('c') === 0 && t1.c.peers === 0, t1.c.line);
  check('d hears nobody — no noise from sealed packets', got('d') === 0 && t1.d.peers === 0, t1.d.line);
  for (const p of [a, b, c, d]) await btn(p, 'Done').click();

  // Video is sealed per fragment: a's camera reaches b, and never c.
  const videos = (p) => p.evaluate(() => [...document.querySelectorAll('video')]
    .map(v => ({ w: v.videoWidth, t: v.currentTime })));
  await btn(a, 'Turn camera on').click();
  await sleep(4000);
  const vb = await videos(b), vc = await videos(c);
  check('b sees a\'s camera through the relay', vb.some(v => v.w > 0 && v.t > 0), JSON.stringify(vb));
  check('c decodes no picture', vc.every(v => v.w === 0), JSON.stringify(vc));
  await btn(a, 'Turn camera off').click();

  // The indicator's card says so.
  await a.getByLabel('Connection details', { exact: true }).hover();
  await sleep(500);
  const card = await a.evaluate(() => document.body.innerText);
  check('the connection card says end to end', /Encryption\s+End to end/.test(card), card.slice(-400));
  check('and counts who cannot be read', /cannot decrypt\s+2 people/.test(card), card.slice(-400));

  console.log(consoleLines.filter((l, i, all) => all.indexOf(l) === i).map(l => `        ${l}`).join('\n'));
  console.log(`\n${pass} passed, ${fail} failed`);
  await browser.close();
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
