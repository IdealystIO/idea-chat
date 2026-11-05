#!/usr/bin/env node
// Two browsers, one room, over the bare `datagram` crate — no chat app.
//
//   cargo run -p call-relay -- 127.0.0.1:4433
//   idealyst dev --web --local --port 8093 crates/quic-call   # APP=http://localhost:8093
//   RELAY=https://127.0.0.1:4433/ HASH=<printed by the relay> node tools/quic-call-flows.js
//
// Chromium's fake microphone beeps, so both pages send real Opus the whole
// time. Passing means each page RECEIVED the other's audio and fed it through
// its jitter buffer to an output — not merely that it connected.
const { chromium } = require('./node_modules/playwright-core');

// A control by its name: an idea-ui Button is named by its label; the call
// bar's IconButtons by the group wrapped around each.
const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8093';
const RELAY = process.env.RELAY || 'https://127.0.0.1:4433/';
const HASH = process.env.HASH || '';  // empty: the relay has a CA certificate
const SECONDS = Number(process.env.SECONDS || 8);

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};

const counters = async (p) => {
  const text = await p.evaluate(() => document.body.innerText);
  const line = text.split('\n').find(l => /^sent \d/.test(l)) || '';
  const n = (k) => Number((line.match(new RegExp(`${k} (\\d+)`)) || [])[1] || 0);
  return { line, sent: n('sent'), received: n('received'), late: n('late'), dsp: n('dsp'),
           concealed: n('concealed'), underruns: n('underruns'), peers: n('peers'), text };
};

// The loudest the two meters read over `ms`, in bars out of 24. The meters show
// a per-frame peak and the fake mic beeps, so one reading proves nothing.
const meters = async (p, ms) => {
  let mic = 0, sent = 0;
  for (const end = Date.now() + ms; Date.now() < end; ) {
    const t = await p.evaluate(() => document.body.innerText);
    const bars = (name) => ((t.match(new RegExp(`${name}\\s*\\[(#*)`)) || [])[1] || '').length;
    mic = Math.max(mic, bars('mic')); sent = Math.max(sent, bars('sent'));
    await new Promise(r => setTimeout(r, 50));
  }
  return { mic, sent };
};

// Click a segment of the noise-suppression control.
const suppression = (p, label) => p.getByText(label, { exact: true }).click();

// Settings, counters and meters are in the settings modal: open it, and keep
// it open while reading them. Closed again before anything in the room (the
// control bar) is pressed, since its backdrop covers the room.
const openSettings = async (p) => {
  await btn(p, 'Settings').click();
  await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
};
const closeSettings = (p) => btn(p, 'Done').click();

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const url = `${APP}/?url=${encodeURIComponent(RELAY)}&hash=${HASH}`;
  const open = async (name) => {
    const p = await browser.newPage({ permissions: ['microphone'] });
    p.on('console', m => { if (m.text().includes('[quic-call]')) console.log(`        ${name}: ${m.text()}`); });
    p.on('pageerror', e => console.log(`        ${name} page error: ${e.message}`));
    await p.goto(url);
    await btn(p, 'Join').waitFor({ timeout: 60000 });
    return p;
  };
  const a = await open('a');
  const b = await open('b');
  await btn(a, 'Join').click();
  await btn(b, 'Join').click();
  await btn(a, 'Leave').waitFor({ timeout: 30000 });
  await btn(b, 'Leave').waitFor({ timeout: 30000 });
  check('both pages show the room with its control bar',
        await btn(a, 'Share screen').isVisible()
          && await btn(b, 'Mute').isVisible());
  await openSettings(a);
  await openSettings(b);

  await new Promise(r => setTimeout(r, SECONDS * 1000));
  const sa = await counters(a), sb = await counters(b);
  console.log(`        a: ${sa.line}`);
  console.log(`        b: ${sb.line}`);
  const expected = SECONDS * 50 * 0.6;

  check('a is live', sa.text.includes('Live'), sa.text);
  check('b is live', sb.text.includes('Live'), sb.text);
  check('a sends ~50 datagrams/s', sa.sent > expected, `sent ${sa.sent}`);
  check('b sends ~50 datagrams/s', sb.sent > expected, `sent ${sb.sent}`);
  check('a hears b', sa.received > expected && sa.peers === 1, sa.line);
  check('b hears a', sb.received > expected && sb.peers === 1, sb.line);

  // DeepFilterNet is the default. The fake mic is a beep — a tone, not
  // speech — so the model should remove most of it: what a SENDS is far
  // quieter than what its mic hears.
  const neural = await meters(a, 3000);
  console.log(`        a with DeepFilterNet: mic ${neural.mic}/24, sent ${neural.sent}/24`);
  check('a\'s mic hears the fake beep', neural.mic >= 3, JSON.stringify(neural));
  check('DeepFilterNet removes most of it before the wire',
        neural.sent * 3 <= neural.mic, JSON.stringify(neural));
  const before = await counters(a);
  check('a keeps up ~50 datagrams/s with the model running', before.sent > SECONDS * 50 * 0.6, before.line);

  // Off, mid-call: the stage swaps, nothing is reopened, the call carries on,
  // and now what is sent is what the mic heard.
  await suppression(a, 'Off');
  await new Promise(r => setTimeout(r, 1000));
  const off = await meters(a, 3000);
  console.log(`        a with suppression off: mic ${off.mic}/24, sent ${off.sent}/24`);
  // Not "as heard": the leveller is on by default and brings a beep this loud
  // down toward its target. What matters is that it gets through at all.
  check('with suppression off, the beep is sent rather than removed',
        off.sent >= 8 && off.sent >= neural.sent + 6, JSON.stringify(off));
  const ta = await counters(a), tb = await counters(b);
  check('a\'s status says suppression is off', ta.text.includes('noise suppression off'),
        ta.text.split('\n').find(l => l.startsWith('Live')));
  check('a keeps sending across the swap', ta.sent > before.sent + 150, `${before.sent} -> ${ta.sent}`);
  check('b keeps hearing a across the swap', tb.received > sa.received + 150 && tb.late === sb.late,
        `${sb.received} -> ${tb.received}, late ${sb.late} -> ${tb.late}`);

  // And back on, which builds the model again mid-call.
  await suppression(a, 'DeepFilterNet');
  await new Promise(r => setTimeout(r, 1500));
  const again = await meters(a, 3000);
  console.log(`        a with DeepFilterNet again: mic ${again.mic}/24, sent ${again.sent}/24`);
  check('switching DeepFilterNet back on mid-call takes effect', again.sent * 3 <= again.mic, JSON.stringify(again));

  // Manual bitrate down to 12 kb/s mid-call, which is the 8 kHz band: the
  // encoder is rebuilt at the next block — CELT to SILK — and the call
  // carries on. The slider is clicked at its left end, like a person would.
  const pre = await counters(a), preB = await counters(b);
  await a.getByText('Manual', { exact: true }).click();
  const slider = a.getByText('Manual audio:', { exact: false }).locator('xpath=following-sibling::*[1]');
  const box = await slider.boundingBox();
  await a.mouse.click(box.x + 2, box.y + box.height / 2);
  await new Promise(r => setTimeout(r, 2500));
  const post = await counters(a), postB = await counters(b);
  check('a keeps sending after dropping to the 8 kHz band', post.sent > pre.sent + 80, `${pre.sent} -> ${post.sent}`);
  check('b keeps hearing a after the band switch', postB.received > preB.received + 80 && postB.late === preB.late,
        `${preB.received} -> ${postB.received}, late ${preB.late} -> ${postB.late}`);

  // DeepFilterNet retuned mid-call: back to it, then a limit and a lower
  // speech gate. Each change rebuilds the model; the call carries on, and
  // with a 12 dB limit the beep it removed outright now leaks through.
  await suppression(a, 'DeepFilterNet');
  await new Promise(r => setTimeout(r, 2500));
  const preT = await counters(a), preTB = await counters(b);
  // idea-ui's Slider is pointer-driven, not an <input type=range>: press it
  // at the fraction of its track the value sits at. Each is the element
  // after the label that shows its value.
  const setSlider = async (label, fraction) => {
    const track = a.getByText(label, { exact: false }).locator('xpath=following-sibling::*[1]');
    const box = await track.boundingBox();
    if (!box) return false;
    await a.mouse.click(box.x + box.width * fraction, box.y + box.height / 2);
    return true;
  };
  const found = (await setSlider('Max noise reduction', (12 - 6) / 94))
    && (await setSlider('Speech gate', (-20 + 30) / 30));
  check('the page offers the DeepFilterNet sliders', found);
  if (found) {
    await new Promise(r => setTimeout(r, 3000));
    const tuned = await meters(a, 3000);
    const postT = await counters(a), postTB = await counters(b);
    console.log(`        a with DeepFilterNet, 12 dB limit, gate -20: mic ${tuned.mic}/24, sent ${tuned.sent}/24`);
    check('the page shows the new limit and gate', /Max noise reduction: 1[1-3] dB/.test(postT.text) && /frames under -(19|20|21) dB/.test(postT.text),
          postT.text.split('\n').find(l => l.startsWith('Max noise')));
    check('a 12 dB limit lets some of the beep through', tuned.sent > neural.sent, `${neural.sent} -> ${tuned.sent}`);
    check('a keeps sending across the retune', postT.sent > preT.sent + 250, `${preT.sent} -> ${postT.sent}`);
    check('b keeps hearing a across the retune', postTB.received > preTB.received + 250 && postTB.late === preTB.late,
          `${preTB.received} -> ${postTB.received}, late ${preTB.late} -> ${postTB.late}`);
  }

  // Hang up b; a should drop the peer when the relay says so.
  await closeSettings(b);
  await btn(b, 'Leave').click();
  await new Promise(r => setTimeout(r, 1500));
  const after = await counters(a);
  check('a forgets b after b leaves', after.peers === 0, after.line);

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
