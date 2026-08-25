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
const APP = process.env.APP || 'http://127.0.0.1:8091';
const SECONDS = Number(process.env.SECONDS || 10);


const HANDLE = process.env.HANDLE || 'ana';
const PASSWORD = process.env.PASSWORD || 'password123';

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

// idea-ui renders a Button as a styled div, so there is no <button> to find.
const pickInRow = async (p, rowName, option) => {
  const at = await p.evaluate(({ rowName, option }) => {
    const exactly = (el, s) => (el.innerText || '').trim() === s;
    const rows = [...document.querySelectorAll('div')]
      .filter(d => (d.innerText || '').trim().startsWith(rowName)
        && [...d.querySelectorAll('div,span')].some(e => exactly(e, option)))
      .sort((a, b) => a.getBoundingClientRect().height - b.getBoundingClientRect().height);
    const row = rows[0];
    if (!row) return null;
    const hits = [...row.querySelectorAll('div,span')].filter(e => {
      if (!exactly(e, option)) return false;
      const r = e.getBoundingClientRect();
      return r.width > 0 && r.height > 0;
    });
    const el = hits[hits.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { rowName, option });
  if (at) await p.mouse.click(at.x, at.y);
  return !!at;
};

(async () => {
  const browser = await chromium.launch({
    executablePath: EXE,
    args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'],
  });
  const page = await browser.newPage();
  page.on('console', m => { if (m.type() === 'error') console.log('  [page]', m.text()); });

  console.log(`app: ${APP}`);
  await page.goto(APP, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(3000);

  // The benchmark lives in the lazy chunk, alongside the model it measures —
  // installing it at boot would anchor the whole inference engine in the main
  // bundle, which is the thing the chunk exists to avoid. So the setting has to
  // be switched on first, which is what loads the chunk.
  await fill(page, 'Handle or email', HANDLE);
  await fill(page, 'Password', PASSWORD);
  await clickText(page, 'Sign in');
  await page.waitForTimeout(3000);
  await page.mouse.click(35, 872);            // settings, from the rail
  await page.waitForTimeout(1200);
  if (!await pickInRow(page, 'Noise suppression', 'This device')) {
    console.log('  FAIL  could not find the noise suppression setting — is this a signed-in app?');
    await browser.close();
    process.exit(1);
  }

  await page.waitForFunction(() => typeof globalThis.__hearthDenoiseBench === 'function', { timeout: 180000 });
  console.log('chunk      loaded on demand (the probe ships inside it)');

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
