#!/usr/bin/env node
// Two browsers, one room, cameras on — the video half of quic-call.
//
//   cargo run -p call-relay -- 127.0.0.1:4433
//   RUSTFLAGS=--cfg=web_sys_unstable_apis idealyst dev --web --local --port 8093 crates/quic-call
//   RELAY=https://127.0.0.1:4433/ HASH=<printed by the relay> node tools/quic-call-video-flows.js
//
// Chromium's fake camera draws a moving test pattern, so both pages encode
// real VP8 the whole time. Passing means each page DECODED the other's frames
// and is playing them in a <video> — not merely that it sent some.
const { chromium } = require('./node_modules/playwright-core');

// A control by its name: an idea-ui Button is named by its label; the call
// bar's IconButtons by the group wrapped around each.
const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8093';
const RELAY = process.env.RELAY || 'https://127.0.0.1:4433/';
const HASH = process.env.HASH || '';
const SECONDS = Number(process.env.SECONDS || 6);

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
// An idea-ui Switch: the label is a span, the toggle its sibling button.
const toggle = (p, label) => p.getByText(label, { exact: true })
  .locator('xpath=following-sibling::*[@role="button"]').click();
// The control bar's buttons, by what they say now.
const press = (p, label) => btn(p, label).click();
// Counters live in the settings modal; read them with it open, then close it
// so the control bar can be pressed again.
const withSettings = async (p, f) => {
  await press(p, 'Settings');
  await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
  const out = await f();
  await press(p, 'Done');
  return out;
};

// The loudest sample each page has played in the last half second, from a
// tap on every node connected to its speakers — one level per output, so a
// silent stream (a share that ended) cannot hide a loud one.
const outputTap = () => {
  window.__levels = [];
  Object.defineProperty(window, '__outPeak', { get: () => Math.max(0, ...window.__levels) });
  const connect = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (dest, ...rest) {
    const result = connect.call(this, dest, ...rest);
    if (dest instanceof AudioDestinationNode && this instanceof AudioWorkletNode && !this.__tapped) {
      this.__tapped = true;
      const sp = this.context.createScriptProcessor(1024, 1, 1);
      const mute = this.context.createGain(); mute.gain.value = 0;
      const slot = window.__levels.push(0) - 1;
      let peak = 0, since = performance.now();
      sp.onaudioprocess = (e) => {
        for (const v of e.inputBuffer.getChannelData(0)) peak = Math.max(peak, Math.abs(v));
        if (performance.now() - since > 500) { window.__levels[slot] = peak; peak = 0; since = performance.now(); }
      };
      connect.call(this, sp); connect.call(sp, mute); connect.call(mute, this.context.destination);
    }
    return result;
  };
};
// The loudest the page played over `ms`.
const loudest = async (p, ms) => {
  let m = 0;
  for (const end = Date.now() + ms; Date.now() < end; await sleep(250)) m = Math.max(m, await p.evaluate(() => window.__outPeak));
  return m;
};
// Every <video>'s on-screen width, widest first.
const widths = (p) => p.evaluate(() => [...document.querySelectorAll('video')]
  .map(v => Math.round(v.getBoundingClientRect().width)).sort((x, y) => y - x));

// A screen share the test controls: a canvas that keeps changing, and a tone
// as the screen's audio. Headless Chromium has no screen to pick.
const fakeScreen = () => {
  navigator.mediaDevices.getDisplayMedia = async () => {
    const c = document.createElement('canvas');
    c.width = 1280; c.height = 720;
    const x = c.getContext('2d');
    let n = 0;
    setInterval(() => { x.fillStyle = `hsl(${(n++ * 7) % 360} 70% 50%)`; x.fillRect(0, 0, 1280, 720); }, 50);
    const ctx = new AudioContext();
    ctx.resume();
    const osc = ctx.createOscillator();
    const dst = ctx.createMediaStreamDestination();
    osc.connect(dst); osc.start();
    return new MediaStream([...c.captureStream(15).getVideoTracks(), ...dst.stream.getAudioTracks()]);
  };
};

// The video counters line, parsed.
const video = async (p) => {
  const text = await p.evaluate(() => document.body.innerText);
  const line = text.split('\n').find(l => /^camera /.test(l)) || '';
  const n = (k) => Number((line.match(new RegExp(`${k} (\\d+)`)) || [])[1] || 0);
  return { line, sent: n('frames sent'), keys: n('keys'), dropped: n('dropped'),
           shown: n('frames shown'), asked: n('keyframes asked for') };
};

// Every <video> on the page: whether it is playing something with a size.
const players = (p) => p.evaluate(() => [...document.querySelectorAll('video')]
  .map(v => ({ w: v.videoWidth, h: v.videoHeight, t: v.currentTime, paused: v.paused })));

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const url = `${APP}/?url=${encodeURIComponent(RELAY)}&hash=${HASH}`;
  const open = async (name) => {
    const p = await browser.newPage({ permissions: ['microphone', 'camera'] });
    await p.addInitScript(fakeScreen);
    await p.addInitScript(outputTap);
    p.on('console', m => { if (/\[(quic-call|call-video|call-audio)\].*(video|camera|screen)/.test(m.text())) console.log(`        ${name}: ${m.text()}`); });
    p.on('pageerror', e => console.log(`        ${name} page error: ${e.message}`));
    await p.goto(url);
    await btn(p, 'Join').waitFor();
    return p;
  };
  const a = await open('a'), b = await open('b');
  // Suppression off: this is about video, and a debug bundle cannot run
  // DeepFilterNet in real time anyway.
  for (const p of [a, b]) await withSettings(p, () => p.getByText('Off', { exact: true }).first().click());
  // Off — the audio-only default — so this also checks the switch turns it on.
  await btn(a, 'Join').click();
  await btn(b, 'Join').click();
  await sleep(1500);
  await press(a, 'Turn camera on');
  await press(b, 'Turn camera on');
  await sleep(SECONDS * 1000);

  for (const [name, p] of [['a', a], ['b', b]]) {
    const v = await withSettings(p, () => video(p));
    console.log(`        ${name}: ${v.line}`);
    check(`${name} encodes its camera`, v.sent > SECONDS * 10, v.line);
    check(`${name} sent a keyframe`, v.keys >= 1, v.line);
    check(`${name} decodes the other's video`, v.shown > SECONDS * 10, v.line);
    const vs = await players(p);
    check(`${name} shows two videos (its own and the other's)`, vs.length === 2, JSON.stringify(vs));
    check(`${name}'s videos have pictures in them`, vs.every(x => x.w > 0 && x.h > 0 && !x.paused), JSON.stringify(vs));
  }

  // The connection indicator: a word in the bar, and on hover the numbers.
  check('the bar says the connection is good', (await a.evaluate(() => document.body.innerText)).includes('Connected'));
  await a.getByLabel('Connection details').hover();
  await a.getByText('Round trip to the relay', { exact: true }).waitFor({ timeout: 5000 });
  const card = await a.evaluate(() => document.body.innerText);
  const rtt = Number((card.match(/Round trip to the relay\s+(\d+) ms/) || [])[1]);
  check('hovering it shows the round trip', rtt >= 0 && rtt < 200, card.slice(card.indexOf('Round trip'), card.indexOf('Round trip') + 300));
  const rates = card.match(/Video\s+↑ ([\d.]+) (kb|Mb)\/s\s+↓ ([\d.]+) (kb|Mb)\/s/);
  check('and video bitrates both ways', rates && Number(rates[1]) > 0 && Number(rates[3]) > 0, (rates || [''])[0]);
  check('and an estimated latency', /Estimated latency\s+≈ \d+ ms mouth to ear/.test(card), card.match(/Estimated latency[^\n]*\n?[^\n]*/)?.[0]);
  await a.mouse.move(5, 5);
  await sleep(300);
  check('and it goes when the pointer leaves', !(await a.getByText('Round trip to the relay', { exact: true }).isVisible()));

  // The remote tile is a canvas capture: its clock should be moving.
  const before = await players(a);
  await sleep(1000);
  const after = await players(a);
  check('a\'s remote video is live, not a still', after[1] && after[1].t > before[1].t,
        `${JSON.stringify(before[1])} -> ${JSON.stringify(after[1])}`);

  // Muting changes one mark on one tile: the videos must be the same
  // elements afterwards, not rebuilt ones (which flicker as they restart).
  await a.evaluate(() => document.querySelectorAll('video').forEach(v => { v.__kept = true; }));
  await press(a, 'Mute');
  await sleep(600);
  const kept = await a.evaluate(() => [...document.querySelectorAll('video')].map(v => !!v.__kept));
  check('muting does not rebuild the videos', kept.length === 2 && kept.every(Boolean), JSON.stringify(kept));
  check('a\'s own tile shows it is muted, and the button says Unmute',
        await btn(a, 'Unmute').isVisible() && await a.getByLabel('Mic off').count() === 1);
  await sleep(600);
  check('b sees that a is muted', await b.getByLabel('Mic off').count() === 1);
  await press(a, 'Unmute');
  await sleep(600);
  check('and that a is not, once a unmutes', await b.getByLabel('Mic off').count() === 0 && await a.getByLabel('Mic off').count() === 0);

  // A screen share from a: its own track, so b gets a third video — a's
  // camera, a's screen, b's own camera — and the screen's audio goes too.
  const sharing = [];
  a.on('console', m => { if (m.text().includes("sending the screen share's audio")) sharing.push(m.text()); });
  await press(a, 'Share screen');
  await sleep(3000);
  const bWithScreen = await players(b);
  check('b sees a\'s screen as a tile of its own', bWithScreen.length === 3, JSON.stringify(bWithScreen));
  check('the screen tile is labelled as a screen', await b.getByText("'s screen", { exact: false }).isVisible());
  check('a sends the screen\'s audio as a track of its own', sharing.length === 1, JSON.stringify(sharing));
  const shared = await withSettings(a, () => a.evaluate(() => document.body.innerText.split('\n').find(l => /^camera /.test(l)) || ''));
  check('a encodes its screen', /screen \d+×\d+/.test(shared), shared);
  // Click a's screen tile on b: it fills the main area, the rest go aside.
  const viewport = b.viewportSize().width;
  await b.getByText("'s screen", { exact: false }).click();
  await sleep(800);
  const focused = await widths(b);
  check('clicking a tile focuses it: it fills the main area', focused[0] > viewport * 0.6, JSON.stringify(focused));
  check('and the other tiles move to the side column', focused.slice(1).every(w => w <= 240), JSON.stringify(focused));
  await b.getByText("'s screen", { exact: false }).click();
  await sleep(800);
  const unfocused = await widths(b);
  check('clicking it again goes back to the grid', unfocused[0] < viewport * 0.6, JSON.stringify(unfocused));

  await press(a, 'Stop sharing');
  await sleep(1500);
  await sleep(1500);
  check('b drops the screen tile when a stops sharing', (await players(b)).length === 2, JSON.stringify(await players(b)));

  // The tile menu: b mutes a for itself, unmutes, then turns a down to
  // nothing with the slider. a's fake mic beeps, so b's output goes from the
  // beep to silence and back.
  const loudBefore = await loudest(b, 2500);
  const caption = b.getByText(/^#\d+$/).first();
  const tile = caption.locator('xpath=ancestor::*[count(.//*[@role="button"])>=2][1]');
  const corner = tile.locator('[role="button"]').last();
  const openMenu = async (label) => {
    await corner.click();
    await b.getByText(label, { exact: true }).waitFor({ timeout: 5000 });
  };
  await openMenu('Volume for you');
  // Anchored to the button it came from, not somewhere off the tile.
  const [btnBox, menuBox] = [await corner.boundingBox(), await b.getByText('Mute for me', { exact: true }).boundingBox()];
  check('the tile menu opens at its button', Math.abs(menuBox.y - btnBox.y) < 260 && Math.abs((menuBox.x + menuBox.width) - (btnBox.x + btnBox.width)) < 260,
        `button ${JSON.stringify(btnBox)} menu item ${JSON.stringify(menuBox)}`);
  await b.getByText('Mute for me', { exact: true }).click();
  await sleep(1200);
  const loudMuted = await loudest(b, 2000);
  check('"Mute for me" silences a on b', loudBefore > 0.05 && loudMuted < loudBefore / 10, `${loudBefore} -> ${loudMuted}`);
  check('and the menu stays open, saying so', await b.getByText('Unmute for me', { exact: true }).isVisible()
        && await b.getByText('Muted for you — volume kept', { exact: true }).isVisible());
  await b.getByText('Unmute for me', { exact: true }).click();
  await sleep(1200);
  const loudBack = await loudest(b, 2000);
  check('"Unmute for me" brings a back at the volume it had', loudBack > loudBefore / 2, `${loudBefore} -> ${loudBack}`);

  check('unmuting leaves it open too', await b.getByText('Mute for me', { exact: true }).isVisible());
  await b.keyboard.press('Escape');
  await sleep(300);
  await openMenu('Volume for you');
  // The slider's track (its root also holds the speaker icons either side).
  const track = b.getByText('Volume for you', { exact: true })
    .locator('xpath=following-sibling::*[1]').locator('[style*="touch-action"]').first();
  // Dragged off the left end, so it lands on exactly 0%.
  const box = await track.boundingBox();
  await b.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await b.mouse.down();
  await b.mouse.move(box.x - 40, box.y + box.height / 2, { steps: 5 });
  await b.mouse.up();
  await sleep(500);
  const readout = await b.getByText('Volume for you', { exact: true }).locator('xpath=following-sibling::*[1]').innerText();
  check('the volume menu shows 0%', readout.trim().endsWith('0%') && !/[1-9]\d*%/.test(readout), JSON.stringify(readout));
  await b.keyboard.press('Escape');
  // The tap reports half-second windows: let the ones from before the change
  // pass before measuring after it.
  await sleep(1200);
  const loudAfter = await loudest(b, 2500);
  console.log(`        b's output from a: peak ${loudBefore.toFixed(3)} before, ${loudMuted.toFixed(3)} muted, ${loudBack.toFixed(3)} unmuted, ${loudAfter.toFixed(3)} at 0%`);
  check('turning a down to 0% silences a on b', loudBefore > 0.05 && loudAfter < loudBefore / 10, `${loudBefore} -> ${loudAfter}`);

  // Camera off on b: a's view of b goes away... once b leaves, at least. While
  // b stays, its decoder simply stops getting frames.
  await press(b, 'Turn camera off');
  await sleep(2800);
  check('b\'s preview goes when its camera is off', (await players(b)).length === 1, JSON.stringify(await players(b)));
  check('a shows b as a card once b\'s camera stops', (await players(a)).length === 1, JSON.stringify(await players(a)));
  // Someone joining after a muted learns it from a.
  await press(a, 'Mute');
  await sleep(500);
  const c = await open('c');
  await withSettings(c, () => c.getByText('Off', { exact: true }).first().click());
  await btn(c, 'Join').click();
  await btn(c, 'Leave').waitFor({ timeout: 30000 });
  let marks = 0;
  for (const end = Date.now() + 8000; Date.now() < end && marks !== 1; await sleep(250)) marks = await c.getByLabel('Mic off').count();
  check('a newcomer sees who is already muted', marks === 1, `${marks} marks`);
  await btn(c, 'Leave').click();
  await press(a, 'Unmute');
  await btn(b, 'Leave').click();
  await sleep(1500);
  const cards = await a.evaluate(() => document.body.innerText);
  // A person's tile is captioned with their id alone, on a line of its own.
  check('a drops b from the room when b leaves', !/^#\d+$/m.test(cards), cards.slice(0, 300));

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
