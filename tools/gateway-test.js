// Everything through ONE origin, the way a tunnelled visitor sees it.
// Nothing here touches the director or the nodes directly.
const { chromium } = require('playwright-core');
const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const ORIGIN = process.env.ORIGIN || 'http://localhost:8080';
const sleep = ms => new Promise(r => setTimeout(r, ms));
// ngrok's free tier serves an interstitial to browsers unless this header is
// present; it does not affect a direct localhost run.
const HEADERS = { 'ngrok-skip-browser-warning': '1' };
const api = async p => { const r = await fetch(ORIGIN + p, { headers: HEADERS }); return r.ok ? r.json() : { status: r.status }; };

(async () => {
  const browser = await chromium.launch({ executablePath: EXE, args: [
    '--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream']});
  const ctx = await browser.newContext({ permissions: ['microphone'], extraHTTPHeaders: HEADERS });
  const pages = [];
  let bad = 0;
  const check = (n, ok) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${n}`); if (!ok) bad++; };

  for (let i = 0; i < 3; i++) {
    const page = await ctx.newPage();
    await page.goto(ORIGIN, { waitUntil: 'load' });
    await sleep(4000);
    const inputs = await page.$$('input[type="text"], input:not([type])');
    if (i === 0) {
      // The default must already be same-origin — that is what makes a remote
      // visitor able to just press Join.
      const dflt = await inputs[0].inputValue();
      check(`director defaults to this origin (${dflt})`, dflt === `${ORIGIN}/_voice`);
    }
    // Only the room is set; the director field is left at its default.
    await inputs[1].click();
    await page.keyboard.press('ControlOrMeta+A');
    await page.keyboard.press('Backspace');
    await page.keyboard.type('tunnel', { delay: 15 });
    await sleep(150);
    await page.evaluate(() => [...document.querySelectorAll('button')]
      .find(b => /join/i.test(b.textContent)).click());
    pages.push(page);
    await sleep(700);
  }

  const peaks = pages.map(() => 0);
  let last = pages.map(() => null);
  for (let t = 0; t < 28; t++) {
    await sleep(250);
    for (let i = 0; i < pages.length; i++) {
      await pages[i].bringToFront();  // rAF is throttled in background tabs
      const s = await pages[i].evaluate(() => {
        const txt = document.body.innerText;
        const m = txt.match(/sent (\d+) . recv (\d+) . dropped (\d+) . peers (\d+)/);
        const rem = txt.match(/remote \[[^\]]*\]\s*(\d+)%/);
        return { sent:m?+m[1]:0, recv:m?+m[2]:0, dropped:m?+m[3]:0, peers:m?+m[4]:0, remote:rem?+rem[1]:0 };
      });
      peaks[i] = Math.max(peaks[i], s.remote);
      last[i] = s;
    }
  }

  const room = await api('/_voice/rooms/tunnel');
  const fleet = await api('/_voice/fleet');
  console.log(`--- room: ${JSON.stringify(room)}`);
  last.forEach((s, i) => console.log(
    `  tab${i+1}: sent=${s.sent} recv=${s.recv} dropped=${s.dropped} peers=${s.peers} remotePeak=${peaks[i]}%`));

  check('room active with 3 participants via gateway', room.participants === 3);
  check('exactly one node owns it', fleet.nodes.filter(n => n.rooms > 0).length === 1);
  pages.forEach((_, i) => {
    check(`tab${i+1} sees both peers`, last[i].peers === 2);
    check(`tab${i+1} heard a real signal`, peaks[i] > 0);
    check(`tab${i+1} dropped nothing`, last[i].dropped === 0);
  });

  // Redirect rewriting: dial a node that does NOT own the room and confirm the
  // frame we get back points at the GATEWAY, not at an internal ws://127.0.0.1
  // address a remote visitor could never reach.
  const owner = room.node_id;
  const other = fleet.nodes.map(n => n.id).find(id => id !== owner);
  const redirect = await pages[0].evaluate(async ({ other }) => {
    const url = `${location.origin.replace(/^http/, 'ws')}/_voice/node/${other}/ws?room=tunnel&id=999999`;
    const ws = new WebSocket(url);
    return await new Promise(res => {
      const done = v => { try { ws.close(); } catch {} res(v); };
      ws.onmessage = e => typeof e.data === 'string' && done(e.data);
      ws.onerror = () => done('ERROR');
      setTimeout(() => done('TIMEOUT'), 4000);
    });
  }, { other });
  console.log(`--- redirect from ${other} (owner is ${owner}): ${redirect}`);
  check('redirect points back at the gateway origin',
        redirect.startsWith('redirect:') && redirect.includes('/_voice/node/'));
  check('redirect leaks no internal address', !redirect.includes('127.0.0.1:87'));

  await browser.close();
  console.log(bad ? `\n${bad} check(s) failed` : '\nall checks passed');
  process.exit(bad ? 1 : 0);
})();
