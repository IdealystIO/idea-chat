// Three tabs, one room, through the director onto a real fleet.
// Asserts BOTH the audio property (everyone hears both peers) and the
// coordination property (one node owns the room; it is released on empty).
const { chromium } = require('playwright-core');
const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const DIRECTOR = 'http://127.0.0.1:9000';
const sleep = ms => new Promise(r => setTimeout(r, ms));

const api = async path => {
  const r = await fetch(DIRECTOR + path);
  return r.ok ? r.json() : { status: r.status };
};

(async () => {
  const browser = await chromium.launch({ executablePath: EXE, args: [
    '--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream']});
  const ctx = await browser.newContext({ permissions: ['microphone'] });
  const pages = [];

  for (let i = 0; i < 3; i++) {
    const page = await ctx.newPage();
    await page.goto('http://localhost:8080', { waitUntil: 'load' });
    await sleep(2500);
    const inputs = await page.$$('input[type="text"], input:not([type])');
    // field 0 = director URL, field 1 = room. IDs are random per tab already.
    for (const [idx, val] of [[0, DIRECTOR], [1, 'general']]) {
      await inputs[idx].click();
      await page.keyboard.press('ControlOrMeta+A');
      await page.keyboard.press('Backspace');
      await page.keyboard.type(val, { delay: 15 });
    }
    await sleep(200);
    const id = await inputs[2].inputValue();
    await page.evaluate(() => [...document.querySelectorAll('button')]
      .find(b => /join/i.test(b.textContent)).click());
    pages.push([id, page]);
    await sleep(700);
  }

  // Let audio flow; sample peaks because the fake mic beeps periodically.
  const peaks = pages.map(() => ({ mic: 0, remote: 0 }));
  let last = pages.map(() => null);
  for (let t = 0; t < 32; t++) {
    await sleep(250);
    for (let i = 0; i < pages.length; i++) {
      // The level meters are driven by `raf_loop`, and browsers throttle
      // requestAnimationFrame in BACKGROUND tabs to about 1fps. Audio keeps
      // flowing (Web Audio is not throttled) but the meter samples so rarely
      // that it lands in the fake mic's silent gap and reads 0 forever. Focus
      // each tab before reading it, or this measures tab visibility rather
      // than audio.
      await pages[i][1].bringToFront();
      const s = await pages[i][1].evaluate(() => {
        const txt = document.body.innerText;
        const m = txt.match(/sent (\d+) . recv (\d+) . dropped (\d+) . peers (\d+) . (\d+) Hz/);
        const mic = txt.match(/mic\s+\[[^\]]*\]\s*(\d+)%/);
        const rem = txt.match(/remote \[[^\]]*\]\s*(\d+)%/);
        return { sent: m?+m[1]:0, recv: m?+m[2]:0, dropped: m?+m[3]:0, peers: m?+m[4]:0,
                 mic: mic?+mic[1]:0, remote: rem?+rem[1]:0, moved: /moved (\d+)/.exec(txt)?.[1]||'0' };
      });
      peaks[i].mic = Math.max(peaks[i].mic, s.mic);
      peaks[i].remote = Math.max(peaks[i].remote, s.remote);
      last[i] = s;
    }
  }

  const room = await api('/rooms/general');
  const fleet = await api('/fleet');

  console.log('--- room (from the director) ---');
  console.log(JSON.stringify(room));
  console.log('--- fleet ---');
  for (const n of fleet.nodes) console.log(`  ${n.id}: rooms=${n.rooms} participants=${n.participants}`);
  console.log('--- tabs ---');
  pages.forEach(([id], i) => console.log(
    `  id=${id}: sent=${last[i].sent} recv=${last[i].recv} dropped=${last[i].dropped} ` +
    `peers=${last[i].peers} moved=${last[i].moved} micPeak=${peaks[i].mic}% remotePeak=${peaks[i].remote}%`));

  let bad = 0;
  const check = (name, ok) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}`); if (!ok) bad++; };

  check('room is active with 3 participants', room.participants === 3);
  check('room is owned by exactly one node', typeof room.node_id === 'string' && room.node_id.length > 0);
  check('owning node reports 3 participants',
        fleet.nodes.some(n => n.id === room.node_id && n.participants === 3));
  check('no other node claims the room',
        fleet.nodes.filter(n => n.rooms > 0).length === 1);
  pages.forEach((_, i) => {
    check(`tab${i + 1} sees both peers`, last[i].peers === 2);
    check(`tab${i + 1} received audio`, last[i].recv > 0);
    check(`tab${i + 1} heard a real signal`, peaks[i].remote > 0);
    check(`tab${i + 1} dropped nothing`, last[i].dropped === 0);
  });

  // Cleanup: everyone leaves, the assignment must be released.
  console.log('--- all three leave ---');
  for (const [, page] of pages) {
    await page.evaluate(() => [...document.querySelectorAll('button')]
      .find(b => /leave/i.test(b.textContent))?.click());
  }
  await sleep(2500);
  const after = await api('/rooms/general');
  const fleetAfter = await api('/fleet');
  console.log('room after:', JSON.stringify(after));
  check('room released when empty', after.status === 404);
  check('no node still holds a room', fleetAfter.nodes.every(n => n.rooms === 0));
  check('fleet participant count back to zero', fleetAfter.participants === 0);

  await browser.close();
  console.log(bad ? `\n${bad} check(s) failed` : '\nall checks passed');
  process.exit(bad ? 1 : 0);
})();
