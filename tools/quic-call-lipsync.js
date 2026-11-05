#!/usr/bin/env node
// Lip sync, measured end to end: how far apart does a receiver SEE and HEAR
// something that happened at one instant?
//
//   cargo run -p call-relay -- 127.0.0.1:4433
//   idealyst serve crates/quic-call/dist/web --port 8098
//   APP=http://localhost:8098 RELAY=https://127.0.0.1:4433/ HASH=<printed> node tools/quic-call-lipsync.js
//
// Page a's camera and microphone are replaced by synthetic ones driven off
// one clock: once a second, a 100 ms white flash on the video and a 100 ms
// 1 kHz tone on the audio, starting together. Page b records when each tone
// starts at its audio output and when each flash starts in the remote
// <video> it is showing, both on b's performance.now(). The gap between the
// two is the A/V offset a viewer gets — positive when the picture is late.
//
// Suppression is Off on both pages: a model would remove a pure tone, which is
// its job and not this test's subject.
const { chromium } = require('./node_modules/playwright-core');

// A control by its name: an idea-ui Button is named by its label; the call
// bar's IconButtons by the group wrapped around each.
const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8098';
const RELAY = process.env.RELAY || 'https://127.0.0.1:4433/';
const HASH = process.env.HASH || '';
const SECONDS = Number(process.env.SECONDS || 10);
// Lip sync people notice: audio more than ~45 ms early or ~125 ms late
// (ITU-R BT.1359). The check is tighter than that on the early side.
const EARLY_MS = Number(process.env.EARLY_MS || 45);
const LATE_MS = Number(process.env.LATE_MS || 90);

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
// Settings and counters are in the settings modal: open it, do `f`, close it.
const settings = async (p, f) => {
  await btn(p, 'Settings').click();
  await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
  const out = await f();
  await btn(p, 'Done').click();
  return out;
};

// Page a: a camera and a microphone that flash and beep together.
const synthetic = () => {
  const make = () => {
    const ctx = new AudioContext({ sampleRate: 48000 });
    ctx.resume();
    const osc = ctx.createOscillator();
    osc.frequency.value = 1000;
    const gain = ctx.createGain();
    gain.gain.value = 0;
    const dst = ctx.createMediaStreamDestination();
    osc.connect(gain); gain.connect(dst); osc.start();
    const canvas = document.createElement('canvas');
    canvas.width = 320; canvas.height = 240;
    const x = canvas.getContext('2d');
    const video = canvas.captureStream(30);
    let on = null;
    setInterval(() => {
      const want = performance.now() % 1000 < 100;
      if (want !== on) {
        on = want;
        gain.gain.setValueAtTime(want ? 0.5 : 0, ctx.currentTime);
      }
      // Redrawn every tick so the canvas keeps producing frames.
      x.fillStyle = on ? '#fff' : '#000';
      x.fillRect(0, 0, 320, 240);
    }, 2);
    return { audio: dst.stream, video };
  };
  navigator.mediaDevices.getUserMedia = async (c) => {
    window.__av = window.__av || make();
    const s = new MediaStream();
    if (c && c.audio) window.__av.audio.getAudioTracks().forEach(t => s.addTrack(t));
    if (c && c.video) window.__av.video.getVideoTracks().forEach(t => s.addTrack(t));
    return s;
  };
};

// Page b: record tone onsets at the output and flash onsets on screen.
const recorder = () => {
  window.__tones = [];
  window.__flashes = [];
  const connect = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (dest, ...rest) {
    const result = connect.call(this, dest, ...rest);
    if (dest instanceof AudioDestinationNode && !this.__tapped && this instanceof AudioWorkletNode) {
      this.__tapped = true;
      const ctx = this.context;
      const sp = ctx.createScriptProcessor(256, 1, 1);
      let quietSince = 0;
      sp.onaudioprocess = (e) => {
        const d = e.inputBuffer.getChannelData(0);
        const ts = ctx.getOutputTimestamp();
        // When this buffer is heard, on performance.now().
        const at = ts.performanceTime + (e.playbackTime - ts.contextTime) * 1000;
        for (let i = 0; i < d.length; i++) {
          const t = at + i * 1000 / ctx.sampleRate;
          if (Math.abs(d[i]) > 0.05) {
            if (t - quietSince > 400) window.__tones.push(t);
            quietSince = Infinity;
          } else if (quietSince === Infinity) {
            quietSince = t;
          }
        }
        if (quietSince === 0) quietSince = at;
      };
      const mute = ctx.createGain(); mute.gain.value = 0;
      connect.call(this, sp); connect.call(sp, mute); connect.call(mute, ctx.destination);
    }
    return result;
  };
  const c = document.createElement('canvas');
  c.width = 4; c.height = 4;
  const x = c.getContext('2d', { willReadFrequently: true });
  let dark = performance.now();
  const tick = () => {
    const now = performance.now();
    for (const v of document.querySelectorAll('video')) {
      if (!v.videoWidth || v.dataset.local) continue;
      x.drawImage(v, 0, 0, 4, 4);
      const px = x.getImageData(0, 0, 4, 4).data;
      let sum = 0;
      for (let i = 0; i < px.length; i += 4) sum += px[i];
      const bright = sum / (px.length / 4) > 128;
      if (bright && dark !== null && now - dark > 400) window.__flashes.push(now);
      dark = bright ? null : (dark === null ? now : dark);
    }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
};

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  // LIPSYNC=off measures the page with lip sync switched off, for comparison.
  const url = `${APP}/?url=${encodeURIComponent(RELAY)}&hash=${HASH}${process.env.LIPSYNC === 'off' ? '&lipsync=off' : ''}`;
  const open = async (name, init) => {
    const p = await browser.newPage({ permissions: ['microphone', 'camera'] });
    await p.addInitScript(init);
    p.on('pageerror', e => console.log(`        ${name} page error: ${e.message}`));
    await p.goto(url);
    await btn(p, 'Join').waitFor({ timeout: 60000 });
    await settings(p, () => p.getByText('Off', { exact: true }).first().click());
    return p;
  };
  const a = await open('a', synthetic);
  const b = await open('b', recorder);
  await btn(a, 'Join').click();
  await btn(b, 'Join').click();
  await sleep(2000);
  await btn(a, 'Turn camera on').click();
  // Let the camera start and the jitter buffer settle, then measure.
  await sleep(3000);
  await b.evaluate(() => { window.__tones = []; window.__flashes = []; });
  await sleep(SECONDS * 1000);
  const { tones, flashes } = await b.evaluate(() => ({ tones: window.__tones, flashes: window.__flashes }));
  const line = await settings(b, () => b.evaluate(() =>
    document.body.innerText.split('\n').find(l => l.includes('lip-sync hold')) || ''));
  await browser.close();

  // Pair each tone with the flash nearest it.
  const offsets = tones.map(t => {
    const f = flashes.reduce((best, v) => (Math.abs(v - t) < Math.abs(best - t) ? v : best), Infinity);
    return f - t;
  }).filter(d => Math.abs(d) < 500);
  const sorted = [...offsets].sort((x, y) => x - y);
  const median = sorted.length ? sorted[Math.floor(sorted.length / 2)] : NaN;
  console.log(`        tones heard ${tones.length}, flashes seen ${flashes.length}, paired ${offsets.length}`);
  console.log(`        picture minus sound, ms: ${offsets.map(d => d.toFixed(0)).join(' ')}`);
  console.log(`        median ${median.toFixed(0)} ms · ${line.split(' · ').filter(s => /lip-sync|frames shown/.test(s)).join(' · ')}`);

  check('b hears the tones and sees the flashes', offsets.length >= SECONDS * 0.6,
        `${tones.length} tones, ${flashes.length} flashes`);
  check(`picture at most ${EARLY_MS} ms behind, ${LATE_MS} ms ahead of the sound`,
        median > -LATE_MS && median < EARLY_MS, `median picture-minus-sound ${median.toFixed(0)} ms`);
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
