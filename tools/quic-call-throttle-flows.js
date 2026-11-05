#!/usr/bin/env node
// A relay throttling per receiver (`call-relay --throttle on`): one client's
// download capped at 300 kb/s (`--cap 300 --loss-to 3`), with a camera at
// 600 kb/s coming its way. Run twice — throttling off, then on — and compare.
// Without, the capped client loses pieces of nearly every frame, asks for
// keyframe after keyframe, and the sender sends them to everyone; with, the
// relay holds back what it cannot take and it gets what it can, intact.
// Starts its own relays; needs only the page:
//
//   idealyst serve crates/quic-call/dist/web --port 8098
//   node tools/quic-call-throttle-flows.js
const { chromium } = require('./node_modules/playwright-core');
const { spawn } = require('child_process');

const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8098';
const BIN = process.env.RELAY_BIN || 'target/release/call-relay';
const SETTLE_S = Number(process.env.SETTLE_S || 20);

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

const relay = (port, extra) => new Promise((resolve, reject) => {
  const child = spawn(BIN, [`127.0.0.1:${port}`, ...extra]);
  const r = { child, log: '' };
  child.stdout.on('data', d => {
    r.log += d;
    const m = r.log.match(/hash=([0-9a-f]{64})/);
    if (m && !r.url) { r.url = `https://127.0.0.1:${port}/`; r.hash = m[1]; resolve(r); }
  });
  child.on('exit', code => { if (!r.url) reject(new Error(`relay exited ${code}: ${r.log}`)); });
});

const card = async (p) => {
  await p.getByLabel('Connection details', { exact: true }).hover();
  await sleep(400);
  const t = await p.evaluate(() => document.body.innerText);
  await p.mouse.move(0, 0);
  return t;
};
// The video counters, from the settings' diagnostics.
const videoCounters = async (p) => {
  await btn(p, 'Settings').click();
  await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
  const t = await p.evaluate(() => document.body.innerText);
  await btn(p, 'Done').click();
  const line = t.split('\n').find(l => l.includes('frames sent')) || '';
  const n = (k) => Number((line.match(new RegExp(`${k} (\\d+)`)) || [])[1] || 0);
  return { keys: n('keys'), shown: n('frames shown'), asked: n('keyframes asked for'), line };
};
const row = (text, name) => ((text.match(new RegExp(`${name}\\s+([^\\n]+)`)) || [])[1] || '').trim();

async function run(browser, throttle) {
  const r = await relay(throttle ? 4481 : 4480, ['--throttle', throttle ? 'on' : 'off', '--cap', '300', '--loss-to', '3']);
  const open = async (camera) => {
    const p = await browser.newPage({ permissions: ['microphone', 'camera'] });
    p.on('pageerror', e => console.log(`        page error: ${e.message}`));
    await p.goto(`${APP}/?${new URLSearchParams({ url: r.url, hash: r.hash })}`);
    await btn(p, 'Join').waitFor({ timeout: 60000 });
    await btn(p, 'Settings').click();
    await p.getByText('Off', { exact: true }).first().click();
    await btn(p, 'Done').click();
    await btn(p, 'Join').click();
    await btn(p, 'Leave').waitFor({ timeout: 30000 });
    if (camera) await btn(p, 'Turn camera on').click();
    await sleep(800); // join in order: a #1, b #2, c #3
    return p;
  };
  const a = await open(true), b = await open(false), c = await open(false);
  await sleep(SETTLE_S * 1000);
  const [ca, cb, cc] = [await card(a), await card(b), await card(c)];
  const [va, vc] = [await videoCounters(a), await videoCounters(c)];
  const vb = await b.evaluate(() => [...document.querySelectorAll('video')].map(v => v.videoWidth));
  await Promise.all([a, b, c].map(p => p.close()));
  r.child.kill();
  return {
    log: r.log,
    aSending: row(ca, 'Sending'), aBitrate: row(ca, 'Bitrate'),
    bDownload: row(cb, 'Download'), bSees: vb.some(w => w > 0),
    cLost: Number((row(cc, 'Audio lost').match(/[0-9.]+/) || [NaN])[0]), cDownload: row(cc, 'Download'),
    cLabel: (cc.match(/(Connected|Limited download|Fair connection|Poor connection)/) || [])[1],
    aKeys: va.keys, cAsked: vc.asked, cShown: vc.shown,
  };
}

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });

  const say = (name, r) => console.log(`        throttling ${name}: a sent ${r.aKeys} keyframes; c asked for ${r.cAsked}, `
    + `showed ${r.cShown} frames, lost ${r.cLost}% of its audio; ${r.cDownload || 'nothing held back'}`);
  const off = await run(browser, false);
  say('off', off);
  const on = await run(browser, true);
  say('on ', on);
  const steps = (on.log.match(/#3: download losing.*/g) || []);
  console.log(steps.map(l => `        relay: ${l}`).join('\n'));

  check('without throttling, the capped client keeps asking for keyframes', off.cAsked > 10, `${off.cAsked}`);
  check('with throttling, it asks for far fewer', on.cAsked < off.cAsked / 2, `${on.cAsked} against ${off.cAsked}`);
  check('so the sender sends everyone far fewer keyframes', on.aKeys < off.aKeys / 2, `${on.aKeys} against ${off.aKeys}`);
  check('and the capped client\'s voice is no worse', on.cLost <= Math.max(off.cLost, 2), `${on.cLost}% against ${off.cLost}%`);
  check('the relay held back video from the capped client', steps.length > 0, on.log.slice(-500));
  check('and only from it', !/#[12]: download losing/.test(on.log), on.log.slice(-500));
  check('the capped client is told', on.cDownload.startsWith('limited by the relay') && on.cLabel === 'Limited download',
        `${on.cLabel} / ${on.cDownload}`);
  check('the clean client gets everything', on.bDownload === '' && on.bSees, `${on.bDownload} / sees video ${on.bSees}`);
  check('and the sender is not cut for the capped one', /full rate|rising/.test(on.aBitrate) && /video 600/.test(on.aSending),
        `${on.aSending} — ${on.aBitrate}`);

  console.log(`\n${pass} passed, ${fail} failed`);
  await browser.close();
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
