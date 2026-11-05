#!/usr/bin/env node
// quic-call's bitrate control (call-rate): automatic on a clean link and on a
// lossy one, and manual. Starts its own relays — one clean, one dropping a
// quarter of everything (`call-relay --loss 25`) — so it needs only the page:
//
//   idealyst serve crates/quic-call/dist/web --port 8098
//   node tools/quic-call-rate-flows.js
const { chromium } = require('./node_modules/playwright-core');
const { spawn } = require('child_process');

const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8098';
const BIN = process.env.RELAY_BIN || 'target/release/call-relay';

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

// A relay on `port`, and the hash it prints.
const relay = (port, extra = []) => new Promise((resolve, reject) => {
  const child = spawn(BIN, [`127.0.0.1:${port}`, ...extra]);
  let out = '';
  child.stdout.on('data', d => {
    out += d;
    const m = out.match(/hash=([0-9a-f]{64})/);
    if (m) resolve({ child, url: `https://127.0.0.1:${port}/`, hash: m[1] });
  });
  child.on('exit', code => reject(new Error(`relay on ${port} exited ${code}: ${out}`)));
});

// "Sending audio 40 kb/s (20 kHz) · video 600 kb/s — Auto — clear link, full rate"
const rateLine = async (p) => {
  const t = await p.evaluate(() => document.body.innerText);
  const line = t.split('\n').find(l => l.startsWith('Sending ')) || '';
  const kbps = (what) => {
    const m = line.match(new RegExp(`${what} ([0-9.]+) (kb|Mb)/s`));
    return m ? Number(m[1]) * (m[2] === 'Mb' ? 1000 : 1) : 0;
  };
  return { line, audio: kbps('audio'), video: kbps('video'), fullband: line.includes('(20 kHz)') };
};

(async () => {
  const clean = await relay(4475);
  const lossy = await relay(4476, ['--loss', '25']);
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const open = async (r) => {
    const p = await browser.newPage({ permissions: ['microphone', 'camera'] });
    p.on('pageerror', e => console.log(`        page error: ${e.message}`));
    await p.goto(`${APP}/?${new URLSearchParams({ url: r.url, hash: r.hash })}`);
    await btn(p, 'Join').waitFor({ timeout: 60000 });
    await btn(p, 'Settings').click();
    await p.getByText('Off', { exact: true }).first().click();
    await btn(p, 'Done').click();
    await btn(p, 'Join').click();
    await btn(p, 'Leave').waitFor({ timeout: 30000 });
    await btn(p, 'Turn camera on').click();
    return p;
  };
  const settings = async (p) => {
    await btn(p, 'Settings').click();
    await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
  };

  // A clean link: everything, at full rate.
  const a = await open(clean), b = await open(clean);
  await sleep(8000);
  await settings(a);
  const full = await rateLine(a);
  check('a clean link sends at the ceiling', full.audio === 40 && full.fullband && full.video === 600
        && full.line.includes('clear link, full rate'), full.line);

  // Manual: exactly what is asked, whatever the link.
  await a.getByText('Manual', { exact: true }).click();
  const slider = a.getByText('Manual audio:', { exact: false }).locator('xpath=following-sibling::*[1]');
  const box = await slider.boundingBox();
  await a.mouse.click(box.x + 2, box.y + box.height / 2);
  await sleep(2500);
  const manual = await rateLine(a);
  check('manual sends what the sliders say', manual.audio === 12 && !manual.fullband && manual.line.includes('Manual'),
        manual.line);
  await a.getByText('Auto — follows the link', { exact: true }).click();
  await sleep(1500);
  const resumed = await rateLine(a);
  check('back to auto, it takes over again', resumed.line.includes('Auto'), resumed.line);
  await btn(a, 'Done').click();

  // A lossy link: a quarter of everything dropped, for everyone — so it is
  // the sender's to fix, and it backs off.
  const c = await open(lossy), d = await open(lossy);
  await sleep(12000);
  await settings(c);
  const cut = await rateLine(c);
  const card = await c.evaluate(() => document.body.innerText);
  console.log(`        under 25% loss: ${cut.line}`);
  check('25% loss backs the video off', cut.video > 0 && cut.video < 400, cut.line);
  check('to its floor before the voice is touched, or the voice is untouched',
        cut.video <= 100 || (cut.audio === 40 && cut.fullband), cut.line);
  check('and says why', /lost, backing off|holding/.test(cut.line), cut.line);
  await btn(c, 'Done').click();
  await c.getByLabel('Connection details', { exact: true }).hover();
  await sleep(500);
  const loss = ((await c.evaluate(() => document.body.innerText)).match(/others lose of you\s+([0-9.]+)%/) || [])[1];
  check('the card shows what the others lose of us', loss && Math.abs(Number(loss) - 25) < 12, `${loss}%`);

  // And the clean pair was never touched by the lossy one.
  await settings(b);
  const still = await rateLine(b);
  check('a call on a clean link stays at full rate', still.video === 600 && still.audio === 40, still.line);

  console.log(`\n${pass} passed, ${fail} failed`);
  await browser.close();
  clean.child.kill();
  lossy.child.kill();
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
