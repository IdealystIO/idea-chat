// Drive two browser tabs through a real join and assert audio actually moves.
// Chromium's fake media device emits a repeating tone, so a non-zero remote
// peak on the far side is real evidence the whole path carried audio.
const { chromium } = require('playwright-core');

const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = 'http://localhost:8080';

const sleep = ms => new Promise(r => setTimeout(r, ms));

async function setup(ctx, id) {
  const page = await ctx.newPage();
  const errors = [];
  page.on('console', m => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('pageerror', e => errors.push('pageerror: ' + e.message));
  await page.goto(APP, { waitUntil: 'load' });
  await sleep(2500); // wasm boot + first paint

  const inputs = await page.$$('input[type="text"], input:not([type])');
  if (inputs.length < 3) throw new Error(`expected 3 text inputs, found ${inputs.length}`);
  // relay, room, id — in declaration order. Use real keystrokes: the signal is
  // driven by the widget's native input events, and a programmatic value set
  // does not necessarily produce them.
  const idField = inputs[2];
  await idField.click();
  await page.keyboard.press('ControlOrMeta+A');
  await page.keyboard.press('Backspace');
  await page.keyboard.type(String(id), { delay: 40 });
  await sleep(250);
  const got = await idField.inputValue();
  const fields = await Promise.all(inputs.map(i => i.inputValue()));
  console.log(`  tab id=${id}: fields=${JSON.stringify(fields)}`);
  if (got !== String(id)) throw new Error(`id field shows "${got}", wanted "${id}"`);

  const joined = await page.evaluate(() => {
    const btn = [...document.querySelectorAll('button')].find(b => /join/i.test(b.textContent));
    if (!btn) return false;
    btn.click();
    return true;
  });
  if (!joined) throw new Error('no Join button found');
  return { page, errors };
}

const readState = page => page.evaluate(() => {
  const t = document.body.innerText;
  const counters = t.match(/sent (\d+) . recv (\d+) . dropped (\d+) . peers (\d+) . (\d+) Hz/);
  const remote = t.match(/remote \[[^\]]*\]\s*(\d+)%/);
  const mic = t.match(/mic\s+\[[^\]]*\]\s*(\d+)%/);
  const status = (t.split('\n').find(l => /Live in|Idle|denied|microphone|ID must/.test(l)) || '').trim();
  return {
    status,
    sent: counters ? +counters[1] : 0,
    recv: counters ? +counters[2] : 0,
    dropped: counters ? +counters[3] : 0,
    peers: counters ? +counters[4] : 0,
    rate: counters ? +counters[5] : 0,
    micPeak: mic ? +mic[1] : 0,
    remotePeak: remote ? +remote[1] : 0,
  };
});

(async () => {
  const browser = await chromium.launch({
    executablePath: EXE,
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
      '--autoplay-policy=no-user-gesture-required',
    ],
  });
  const ctx = await browser.newContext({ permissions: ['microphone'] });

  const a = await setup(ctx, 1);
  const b = await setup(ctx, 2);

  // Chromium's fake mic emits a PERIODIC beep, so both the mic and remote
  // meters are zero for most of each cycle. A single sample is therefore
  // meaningless — poll across several seconds and keep the peak.
  let sa = null, sb = null;
  const peak = (acc, s) => acc === null ? s : {
    ...s,
    micPeak: Math.max(acc.micPeak, s.micPeak),
    remotePeak: Math.max(acc.remotePeak, s.remotePeak),
  };
  for (let i = 0; i < 40; i++) {
    await sleep(250);
    sa = peak(sa, await readState(a.page));
    sb = peak(sb, await readState(b.page));
  }
  console.log('tab1(id=1):', JSON.stringify(sa));
  console.log('tab2(id=2):', JSON.stringify(sb));
  const errs = [...a.errors, ...b.errors];
  if (errs.length) console.log('console errors:', JSON.stringify(errs.slice(0, 8), null, 1));

  const checks = [
    ['tab1 captured audio',       sa.sent > 0],
    ['tab2 captured audio',       sb.sent > 0],
    ['tab1 received from tab2',   sa.recv > 0],
    ['tab2 received from tab1',   sb.recv > 0],
    ['tab1 saw exactly 1 peer',   sa.peers === 1],
    ['tab2 saw exactly 1 peer',   sb.peers === 1],
    ['no packets dropped',        sa.dropped === 0 && sb.dropped === 0],
    ['capture rate reported',     sa.rate > 0 && sb.rate > 0],
    ['tab1 heard real signal',    sa.remotePeak > 0],
    ['tab1 captured real signal', sa.micPeak > 0],
    ['tab2 heard real signal',    sb.remotePeak > 0],
    ['tab2 captured real signal', sb.micPeak > 0],
  ];
  let failed = 0;
  for (const [name, ok] of checks) {
    console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}`);
    if (!ok) failed++;
  }
  await browser.close();
  console.log(failed ? `\n${failed} check(s) failed` : '\nall checks passed');
  process.exit(failed ? 1 : 0);
})().catch(e => { console.error('harness error:', e.message); process.exit(2); });
