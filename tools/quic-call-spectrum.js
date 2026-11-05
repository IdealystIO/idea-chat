// What does the RECEIVING page actually play? Feed page a a known WAV as its
// microphone, record every node page b connects to its speakers, and look at
// the spectrum. The source is one second of speech-like harmonics below
// 3.5 kHz, then a second with no voice, with a 60 Hz mains hum (and its 120
// and 180 Hz harmonics) under both. Anything above 4 kHz was added on the way;
// the `hum` band and the pause floor show what the voice processing removed.
//
//   node tools/quic-call-spectrum.js <app url> <relay url> <hash> [off|browser|neural] [on|off]
//
// The last argument is the voice-processing switch. Recordings and the source
// WAV land in a fresh temp directory, printed at the end.
//
// AIR=1 adds tones at 9.5–17 kHz under the voice — sibilance, roughly — to
// see the band above 8 kHz carried rather than added.
const fs = require('fs');
const path = require('path');
const { chromium } = require('./node_modules/playwright-core');

const [APP, RELAY, HASH, MODE = 'off', VOICE = 'on'] = process.argv.slice(2);
const RATE = 48000;
const OUT = fs.mkdtempSync(path.join(require('os').tmpdir(), 'quic-call-spectrum-'));

// --- source: 1 s of speech-like harmonics below 3.5 kHz, then 1 s of silence ---
function wav(file) {
  const n = RATE * 2;
  const pcm = new Int16Array(n);
  for (let i = 0; i < n; i++) {
    const t = i / RATE;
    let v = 0;
    if (i < RATE) {
      const env = 0.5 - 0.5 * Math.cos(2 * Math.PI * t); // fade in and out, no clicks
      for (let h = 1; h * 140 <= 3500; h++) v += Math.sin(2 * Math.PI * 140 * h * t) / h * 0.25 * env;
      if (process.env.AIR) for (const f of [9500, 11000, 13000, 15000, 17000]) v += Math.sin(2 * Math.PI * f * t) * 0.01 * env;
    }
    // Mains hum, all the way through: 60 Hz and two harmonics, about -30 dBFS.
    v += 0.02 * Math.sin(2 * Math.PI * 60 * t) + 0.012 * Math.sin(2 * Math.PI * 120 * t) + 0.008 * Math.sin(2 * Math.PI * 180 * t);
    pcm[i] = Math.round(v * 32767);
  }
  const buf = Buffer.alloc(44 + n * 2);
  buf.write('RIFF', 0); buf.writeUInt32LE(36 + n * 2, 4); buf.write('WAVE', 8);
  buf.write('fmt ', 12); buf.writeUInt32LE(16, 16); buf.writeUInt16LE(1, 20); buf.writeUInt16LE(1, 22);
  buf.writeUInt32LE(RATE, 24); buf.writeUInt32LE(RATE * 2, 28); buf.writeUInt16LE(2, 32); buf.writeUInt16LE(16, 34);
  buf.write('data', 36); buf.writeUInt32LE(n * 2, 40);
  Buffer.from(pcm.buffer).copy(buf, 44);
  fs.writeFileSync(file, buf);
}

// --- analysis ---
function fft(re, im) {
  const n = re.length;
  for (let i = 1, j = 0; i < n; i++) {
    let bit = n >> 1;
    for (; j & bit; bit >>= 1) j ^= bit;
    j ^= bit;
    if (i < j) { [re[i], re[j]] = [re[j], re[i]]; [im[i], im[j]] = [im[j], im[i]]; }
  }
  for (let len = 2; len <= n; len <<= 1) {
    const a = -2 * Math.PI / len;
    for (let i = 0; i < n; i += len) {
      for (let k = 0; k < len / 2; k++) {
        const wr = Math.cos(a * k), wi = Math.sin(a * k);
        const xr = re[i + k + len / 2] * wr - im[i + k + len / 2] * wi;
        const xi = re[i + k + len / 2] * wi + im[i + k + len / 2] * wr;
        re[i + k + len / 2] = re[i + k] - xr; im[i + k + len / 2] = im[i + k] - xi;
        re[i + k] += xr; im[i + k] += xi;
      }
    }
  }
}

function spectrum(x, rate) {
  const N = 4096;
  const power = new Float64Array(N / 2);
  let frames = 0;
  for (let s = 0; s + N <= x.length; s += N / 2) {
    const re = new Float64Array(N), im = new Float64Array(N);
    for (let i = 0; i < N; i++) re[i] = x[s + i] * (0.5 - 0.5 * Math.cos(2 * Math.PI * i / (N - 1)));
    fft(re, im);
    for (let k = 0; k < N / 2; k++) power[k] += re[k] * re[k] + im[k] * im[k];
    frames++;
  }
  for (let k = 0; k < N / 2; k++) power[k] /= Math.max(frames, 1);
  return { power, hz: rate / N };
}

const db = (p) => (10 * Math.log10(p + 1e-20)).toFixed(1);

function report(name, x, rate) {
  const rms = (a) => Math.sqrt(a.reduce((s, v) => s + v * v, 0) / Math.max(a.length, 1));
  // 20 ms blocks; the quietest third is the pauses.
  const blocks = [];
  const B = Math.round(rate * 0.02);
  for (let s = 0; s + B <= x.length; s += B) blocks.push({ s, r: rms(x.subarray(s, s + B)) });
  const sorted = [...blocks].sort((a, b) => a.r - b.r);
  const quiet = sorted.slice(0, Math.floor(sorted.length / 3));
  const loud = sorted.slice(Math.floor(sorted.length * 2 / 3));
  const gather = (bs) => { const out = new Float32Array(bs.length * B); bs.forEach((b, i) => out.set(x.subarray(b.s, b.s + B), i * B)); return out; };

  const bands = (sp) => {
    const sum = (lo, hi) => { let p = 0; for (let k = Math.floor(lo / sp.hz); k < Math.min(sp.power.length, hi / sp.hz); k++) p += sp.power[k]; return p; };
    return { hum: db(sum(55, 65) + sum(115, 125) + sum(175, 185)), '0-4k': db(sum(0, 4000)), '4-8k': db(sum(4000, 8000)), '8-12k': db(sum(8000, 12000)), '12-24k': db(sum(12000, 24000)) };
  };
  const peaks = (sp, from) => {
    const list = [];
    for (let k = Math.ceil(from / sp.hz) + 1; k < sp.power.length - 1; k++) {
      if (sp.power[k] > sp.power[k - 1] && sp.power[k] >= sp.power[k + 1]) list.push([k * sp.hz, sp.power[k]]);
    }
    return list.sort((a, b) => b[1] - a[1]).slice(0, 5).map(([f, p]) => `${Math.round(f)} Hz ${db(p)} dB`);
  };

  const all = spectrum(x, rate);
  const q = gather(quiet);
  const qs = spectrum(q, rate);
  console.log(`\n== ${name}: ${(x.length / rate).toFixed(1)} s at ${rate} Hz`);
  console.log(`   rms overall ${rms(x).toFixed(4)} · loud third ${rms(gather(loud)).toFixed(4)} · quiet third (pauses) ${rms(q).toFixed(5)}`);
  console.log(`   band power, whole:  ${JSON.stringify(bands(all))}`);
  console.log(`   band power, pauses: ${JSON.stringify(bands(qs))}`);
  console.log(`   strongest peaks above 4 kHz, whole:  ${peaks(all, 4000).join(' · ')}`);
  console.log(`   strongest peaks above 4 kHz, pauses: ${peaks(qs, 4000).join(' · ')}`);
}

(async () => {
  // SOURCE=path.wav to use a recording instead (48 kHz mono 16-bit) — real
  // speech in real noise is the only fair test of a suppressor.
  const src = process.env.SOURCE || path.join(OUT, 'source.wav');
  if (!process.env.SOURCE) wav(src);
  const browser = await chromium.launch({
    executablePath: process.env.CHROME || process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome',
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           `--use-file-for-fake-audio-capture=${src}`, '--autoplay-policy=no-user-gesture-required'],
  });
  const url = `${APP}/?url=${encodeURIComponent(RELAY)}&hash=${HASH}`;
  const tap = () => {
    window.__taps = [];
    const connect = AudioNode.prototype.connect;
    AudioNode.prototype.connect = function (dest, ...rest) {
      const result = connect.call(this, dest, ...rest);
      if (dest instanceof AudioDestinationNode && !this.__tapped) {
        this.__tapped = true;
        const ctx = this.context;
        const rec = { rate: ctx.sampleRate, kind: this.constructor.name, chunks: [] };
        window.__taps.push(rec);
        const sp = ctx.createScriptProcessor(4096, 1, 1);
        sp.onaudioprocess = (e) => { if (window.__recording) rec.chunks.push(Array.from(e.inputBuffer.getChannelData(0))); };
        const mute = ctx.createGain(); mute.gain.value = 0;
        connect.call(this, sp); connect.call(sp, mute); connect.call(mute, ctx.destination);
      }
      return result;
    };
  };
  const open = async (name) => {
    const p = await browser.newPage({ permissions: ['microphone'] });
    await p.addInitScript(tap);
    p.on('console', m => { if (m.text().includes('[quic-call]')) console.log(`   ${name}: ${m.text()}`); });
    await p.goto(url);
    await p.getByRole('button', { name: 'Join', exact: true }).waitFor({ timeout: 60000 });
    const label = { off: 'Off', browser: 'Browser', neural: 'DeepFilterNet' }[MODE];
    // Every setting is in the settings modal.
    await p.getByRole('button', { name: 'Settings', exact: true }).click();
    await p.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
    await p.getByText(label, { exact: true }).click();
    // The stages that default on (declick defaults off).
    if (VOICE === 'off') for (const stage of ['De-hum', 'Level', 'Expand']) {
      await p.getByText(stage, { exact: false }).first().locator('xpath=following-sibling::*[@role="button"]').click();
    }
    // Echo cancellation off: two tabs in one browser would cancel each other.
    await p.getByText('Echo cancellation', { exact: false }).locator('xpath=following-sibling::*[@role="button"]').click();
    await p.getByRole('button', { name: 'Done', exact: true }).click();
    return p;
  };
  const a = await open('a');
  const b = await open('b');
  await a.getByRole('button', { name: 'Join', exact: true }).click();
  await b.getByRole('button', { name: 'Join', exact: true }).click();
  await new Promise(r => setTimeout(r, 3000));
  await b.evaluate(() => { window.__recording = true; });
  // RECORD_S to record longer — a whole loop of a SOURCE recording, so runs
  // compare the same material whatever point of the loop they start at.
  await new Promise(r => setTimeout(r, Number(process.env.RECORD_S || 6) * 1000));
  const taps = await b.evaluate(() => window.__taps.map(t => ({ rate: t.rate, kind: t.kind, samples: t.chunks.flat() })));
  // In a call, Settings is a bar IconButton, named by the group around it.
  await b.getByRole('group', { name: 'Settings', exact: true }).getByRole('button').click();
  await b.getByText('Diagnostics', { exact: true }).waitFor({ timeout: 10000 });
  const line = await b.evaluate(() => document.body.innerText.split('\n').find(l => /^sent \d/.test(l)));
  console.log(`   b: ${line}`);
  await browser.close();

  // The source itself, through the same analysis, as the reference.
  const raw = fs.readFileSync(src);
  const ref = new Float32Array((raw.length - 44) / 2);
  for (let i = 0; i < ref.length; i++) ref[i] = raw.readInt16LE(44 + i * 2) / 32768;
  report('source (reference)', ref.length > RATE * 6 ? ref.subarray(0, RATE * 6) : ref, RATE);
  taps.forEach((t, i) => {
    const x = Float32Array.from(t.samples);
    fs.writeFileSync(path.join(OUT, `b-tap${i}-${MODE}-${VOICE}.f32`), Buffer.from(x.buffer));
    report(`b tap ${i} (${t.kind}) — suppression ${MODE}, voice processing ${VOICE}`, x, t.rate);
  });
  console.log(`\nrecordings: ${OUT}`);
})().catch(e => { console.error(e); process.exit(1); });
