// What DeepFilterNet costs in the browser — the number that decides whether
// on-device suppression is a feature or a stutter.
//
// The native benchmark (`cargo run --release -p voice-server --bin dfn-bench`)
// says 0.11 ms per 10 ms hop. That number does not transfer: `tract` on wasm
// has no SIMD and no threads, and the engine runs inline on the same thread
// that renders the UI. This measures the real bundle in a real browser.
//
//   cargo build -p hearth --bin server --features server
//   (cd crates/hearth && idealyst build --web)
//   ./tools/fetch-model.sh
//   node tools/denoise-bench.js
//
// A hop is 10 ms of audio. `perHopMs` at or above 10 means the model cannot
// keep up with the microphone feeding it, and the call breaks up.

const { chromium } = require('playwright-core');

const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://127.0.0.1:8090';
const SECONDS = Number(process.env.SECONDS || 10);

(async () => {
  const browser = await chromium.launch({
    executablePath: EXE,
    args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'],
  });
  const page = await browser.newPage();
  page.on('console', m => { if (m.type() === 'error') console.log('  [page]', m.text()); });

  console.log(`app: ${APP}`);
  await page.goto(APP, { waitUntil: 'domcontentloaded' });

  // The probe is installed with the rest of the app; wait for it rather than
  // guessing at a load time.
  await page.waitForFunction(() => typeof globalThis.__hearthDenoiseBench === 'function', { timeout: 180000 });

  // Model download, timed separately: it is a first-join cost, not a per-hop
  // one, and conflating them would make the model look slow instead of big.
  const fetched = await page.evaluate(async (path) => {
    const t0 = performance.now();
    const r = await fetch(path);
    const b = await r.arrayBuffer();
    return { ms: performance.now() - t0, bytes: b.byteLength, status: r.status };
  }, '/assets/DeepFilterNet3_onnx.tar.gz');
  console.log(`\nmodel      ${(fetched.bytes / 1e6).toFixed(1)} MB in ${fetched.ms.toFixed(0)} ms over loopback (HTTP ${fetched.status})`);
  if (fetched.status !== 200 || fetched.bytes < 1e6) {
    console.log('  FAIL  the model is not being served — run ./tools/fetch-model.sh');
    await browser.close();
    process.exit(1);
  }

  const raw = await page.evaluate(s => globalThis.__hearthDenoiseBench(s), SECONDS);
  const r = JSON.parse(raw);

  console.log(`build      ${r.buildMs.toFixed(0)} ms   (once per call, at join)`);
  console.log(`process    ${r.perHopMs.toFixed(2)} ms of main thread per 10 ms hop  (${r.hops} hops, rtf ${r.rtf.toFixed(3)})`);
  console.log(`headroom   ${(10 / r.perHopMs).toFixed(1)}x  — how much slower a machine could be and still keep up\n`);

  // The verdict, stated rather than left to the reader. A phone is roughly
  // 3-5x slower than this desktop, so the bar is deliberately not 1x.
  const ok = r.perHopMs < 2.5;
  console.log(ok
    ? `  ok    on-device suppression is viable here (${(10 / r.perHopMs).toFixed(0)}x headroom leaves room for a phone)`
    : ` FAIL  ${r.perHopMs.toFixed(2)} ms per hop leaves too little headroom — an AudioWorklet is the way out`);

  await browser.close();
  process.exit(ok ? 0 : 1);
})().catch(e => { console.error(e); process.exit(1); });
