#!/usr/bin/env node
// The server goes away and comes back: does the app notice, reconnect, and
// carry on — live updates and all — without a reload?
//
//   bash tools/chat-dev.sh            # server on :8090, mail to its log
//   LOG=/path/to/that.log node tools/chat-reconnect-flows.js
//
// Kills the running chat-server and starts it again with
// `tools/chat-dev.sh --no-web` (override with RESTART), appending to LOG.
// The restarted server keeps running after this exits.
const { execFileSync, spawn } = require('child_process');
const fs = require('fs');
const path = require('path');
const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8090';
const API = APP + '/_srv';
const LOG = process.env.LOG || '/tmp/chat-server.log';
const RESTART = process.env.RESTART || `bash ${path.resolve(__dirname, 'chat-dev.sh')} --no-web`;
const DOWN_MS = Number(process.env.DOWN_MS || 8000);
const WAV = path.resolve(__dirname, 'fixtures/speech-like.wav');

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = ms => new Promise(r => setTimeout(r, ms));

const call = async (fn, args, jar) => {
  const r = await fetch(`${API}/${fn}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Origin: APP, ...(jar ? { Cookie: jar } : {}) },
    body: JSON.stringify(args),
  });
  const setCookie = r.headers.get('set-cookie');
  const raw = await r.text();
  let body;
  try { body = JSON.parse(raw); } catch { body = { Err: raw.trim() }; }
  return { body, cookie: setCookie ? setCookie.split(';')[0] : null };
};

const account = async (name) => {
  const tag = Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
  const email = `${name}-${tag}@example.com`.toLowerCase();
  const up = await call('sign_up', [email, 'password12345', `${name}${tag}`.toLowerCase().slice(0, 20), name]);
  if (!up.body.Ok) throw new Error(`sign_up ${name}: ${JSON.stringify(up.body)}`);
  let code;
  for (let i = 0; i < 20 && !code; i++) {
    await sleep(250);
    const line = fs.readFileSync(LOG, 'utf8').split('\n').reverse().find(l => l.includes(`to ${email}`));
    code = line && (line.match(/\b(\d{6})\b/) || [])[1];
  }
  if (!code) throw new Error(`no code for ${email} in ${LOG}`);
  const v = await call('verify_email', [email, code]);
  if (!v.body.Ok) throw new Error(`verify ${name}: ${JSON.stringify(v.body)}`);
  return { name, jar: v.cookie, id: v.body.Ok.me.user.id };
};

// A UUIDv7 with a current timestamp, for a message id.
const v7 = () => {
  const b = Buffer.alloc(16);
  b.writeUIntBE(Date.now(), 0, 6);
  require('crypto').randomFillSync(b, 6);
  b[6] = 0x70 | (b[6] & 0x0f); b[8] = 0x80 | (b[8] & 0x3f);
  const h = b.toString('hex');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
};

const serverUp = async () => {
  try { return (await fetch(APP + '/')).ok; } catch { return false; }
};

(async () => {
  console.log('--- setup ---');
  const ana = await account('Ana');
  const theo = await account('Theo');
  const space = (await call('create_space', ['Reconnect test'], ana.jar)).body.Ok;
  const invite = (await call('create_invite', [space, null, null], ana.jar)).body.Ok;
  await call('join_space', [invite.code], theo.jar);
  const sql = q => execFileSync('docker', ['exec', 'idea-chat-dev-postgres-1', 'psql', '-U', 'postgres', '-d', 'chat', '-tAc', q]).toString().trim();
  const lounge = sql(`select id from channels where space_id='${space}' and kind='voice' limit 1`);
  const general = execFileSync('docker', ['exec', 'idea-chat-dev-postgres-1', 'psql', '-U', 'postgres', '-d', 'chat', '-tAc',
    `select id from channels where space_id='${space}' and name='general'`]).toString().trim();

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required', `--use-file-for-fake-audio-capture=${WAV}`],
  });
  const tab = async (who) => {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, permissions: ['microphone'] });
    const [name, value] = who.jar.split(/=(.*)/s);
    await ctx.addCookies([{ name, value, url: APP }]);
    return ctx.newPage();
  };
  const page = await tab(ana);
  const other = await tab(theo);
  const sockets = [];
  page.on('websocket', ws => {
    const s = { url: ws.url(), open: true };
    sockets.push(s);
    console.log(`        socket opened (${sockets.length})`);
    ws.on('close', () => { s.open = false; console.log(`        socket closed`); });
  });
  page.on('console', m => { if (m.type() === 'error' || /\[chat\]/.test(m.text())) console.log(`        console: ${m.text().slice(0, 200)}`); });
  await page.goto(`${APP}/s/${space}/${general}`, { waitUntil: 'load' });
  await page.getByText('general').first().waitFor({ timeout: 20000 });

  const says = async (text, ms) => {
    try { await page.getByText(text).first().waitFor({ timeout: ms }); return true; } catch { return false; }
  };
  let r = await call('send', [general, 'before the restart', null, []], theo.jar);
  if (!r.body.Ok) {
    // The send signature differs: show it rather than guess.
    console.log(`        send: ${JSON.stringify(r.body).slice(0, 300)}`);
  }
  check('a message arrives live before the restart', await says('before the restart', 5000));
  check('one live socket is open', sockets.filter(s => s.open).length === 1, JSON.stringify(sockets));

  // Both in a call, to see what a restart does to it.
  const stats = p => p.evaluate(() => { try { return JSON.parse(globalThis.__chatCallStats()); } catch (e) { return { error: String(e) }; } });
  const hearing = async (p, ms) => {
    let best = 0; const until = Date.now() + ms;
    while (Date.now() < until) { const s = await stats(p); if (s.live) best = Math.max(best, s.remotePeak); await sleep(150); }
    return best;
  };
  const join = async (p) => {
    await p.goto(`${APP}/s/${space}/${lounge}`, { waitUntil: 'load' });
    await p.getByText('Join voice').first().waitFor({ timeout: 20000 });
    await p.getByRole('button', { name: 'Join voice', exact: true }).first().click()
      .catch(() => p.getByText('Join voice', { exact: true }).first().click());
    await p.getByText('Voice connected').first().waitFor({ timeout: 25000 }).catch(() => {});
  };
  other.on('console', m => { if (/\[chat\]/.test(m.text())) console.log(`        theo console: ${m.text().slice(0, 200)}`); });
  // Ana is on #general already: into the call by the sidebar, not a reload.
  await page.getByText('lounge', { exact: true }).first().click();
  await page.getByText('Join voice').first().waitFor({ timeout: 20000 });
  await page.getByText('Join voice', { exact: true }).first().click();
  await page.getByText('Voice connected').first().waitFor({ timeout: 25000 }).catch(() => {});
  await join(other);
  check('ana and theo are in a call, hearing each other', (await hearing(other, 4000)) > 0);
  const cues = p => p.evaluate(() => { try { return JSON.parse(globalThis.__chatCues()); } catch { return null; } });
  await cues(page); await cues(other);
  page.on('console', m => { if (/\[chat\] call/.test(m.text())) console.log(`        ana console: ${m.text().slice(0, 200)}`); });

  console.log(`--- server down for ${DOWN_MS / 1000}s ---`);
  try { execFileSync('pkill', ['-x', 'chat-server']); } catch {}
  for (let i = 0; i < 20 && await serverUp(); i++) await sleep(250);
  check('the server is down', !(await serverUp()));
  await sleep(1000);
  check('the page noticed: its socket closed', sockets.every(s => !s.open), JSON.stringify(sockets));
  await sleep(2000);
  const downA = await stats(page);
  check('the dead call is let go — mic released, nobody lit', !downA.live, JSON.stringify(downA));
  check('and the tray says it is reconnecting', (await page.evaluate(() => document.body.innerText)).includes('Reconnecting'));
  await sleep(DOWN_MS);

  console.log('--- server back ---');
  const out = fs.openSync(LOG, 'a');
  spawn('bash', ['-c', RESTART], { detached: true, stdio: ['ignore', out, out], cwd: path.resolve(__dirname, '..') }).unref();
  for (let i = 0; i < 240 && !(await serverUp()); i++) await sleep(500);
  check('the server is back', await serverUp());
  const before = sockets.length;
  for (let i = 0; i < 60 && !sockets.slice(before).some(s => s.open); i++) await sleep(500);
  check('the page reconnected on its own, within 30 s', sockets.slice(before).some(s => s.open),
    `${sockets.length - before} new sockets: ${JSON.stringify(sockets.slice(before))}`);

  r = await call('send', [general, 'after the restart', null, []], theo.jar);
  // In-app navigation, not a reload: the point is that the page recovered by itself.
  await page.getByText('general', { exact: true }).first().click().catch(() => {});
  check('and live updates flow again', await says('after the restart', 8000));

  console.log('--- the call ---');
  let back = false;
  for (let i = 0; i < 30 && !back; i++) {
    const [x, y] = [await stats(page), await stats(other)];
    back = x.live && y.live && (await hearing(other, 1000)) > 0;
    if (!back) await sleep(1000);
  }
  const body = p => p.evaluate(() => document.body.innerText.replace(/\n/g, ' | ').slice(0, 400));
  await sleep(3000); // past the leave grace: anything still to play has
  const [ra, rt] = [await cues(page), await cues(other)];
  check('through the outage each heard "lost" then "back", and nothing about each other',
    JSON.stringify(ra) === '["lost","back"]' && JSON.stringify(rt) === '["lost","back"]',
    `ana ${JSON.stringify(ra)}, theo ${JSON.stringify(rt)}`);
  check('the call came back on its own, and theo hears ana again', back,
    `ana: ${JSON.stringify(await stats(page))} — ${await body(page)}\n        theo: ${JSON.stringify(await stats(other))} — ${await body(other)}`);

  console.log('--- another device takes the call over ---');
  // A second browser profile is a second device (its key lives in storage).
  // It joins ANOTHER call: the voice page counts people, not devices, so
  // from the lounge it would show Ana as already in. Joining elsewhere takes
  // her out of the lounge, and the first device is "revoked".
  const porch = (await call('create_channel', [space, 'voice', 'porch', null], ana.jar)).body.Ok;
  const laptop = await tab(ana);
  await laptop.goto(`${APP}/s/${space}/${porch.id}`, { waitUntil: 'load' });
  await laptop.getByText('Join voice').first().waitFor({ timeout: 20000 });
  await laptop.getByText('Join voice', { exact: true }).first().click();
  await laptop.getByText('Voice connected').first().waitFor({ timeout: 25000 }).catch(() => {});
  await sleep(6000);
  const [first, second] = [await stats(page), await stats(laptop)];
  const tray = (await page.evaluate(() => document.body.innerText));
  check('the new device is in the call', second.live === true, JSON.stringify(second));
  check('the old one stepped aside instead of taking it back', !first.live && !tray.includes('Reconnecting'),
    `${JSON.stringify(first)} — ${tray.replace(/\n/g, ' | ').slice(0, 300)}`);
  const stillA = await stats(page);
  await sleep(4000);
  check('and stays aside', !(await stats(page)).live, JSON.stringify(stillA));
  check('the old device said "left" once, and never "lost"', JSON.stringify(await cues(page)) === '["left"]');

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(2); });
