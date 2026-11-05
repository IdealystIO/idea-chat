#!/usr/bin/env node
// Two people, one Idea Chat voice channel, real audio, end-to-end encrypted.
//
// The claim is the loop test's: a non-zero peak on the FAR side is the only
// proof the whole path carried audio — mic, codec, sealing, relay, opening,
// playback. On top of it, that the call is sealed with a room key one device
// made and the other got through the server without it reading it.
//
//   bash tools/chat-dev.sh            # server on :8090, mail to its log
//   LOG=/path/to/that.log node tools/chat-call-flows.js
//
// Reads verification codes from the server's log and channel ids from the
// dev Postgres container.
const { execFileSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8090';
const API = APP + '/_srv';
const LOG = process.env.LOG || '/tmp/chat-server.log';
const PG = process.env.PG || 'idea-chat-dev-postgres-1';
// Speech-like, not Chromium's default tone: voice detection rightly calls a
// tone "not speech". Talks for 1.2 s of every 2 s (see tools/vad-flows.js).
const WAV = path.resolve(__dirname, 'fixtures/speech-like.wav');
const DUTY = 0.6;

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};

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

const sql = q => execFileSync('docker', ['exec', PG, 'psql', '-U', 'postgres', '-d', 'chat', '-tAc', q]).toString().trim();
const sleep = ms => new Promise(r => setTimeout(r, ms));

// Sign up and verify, reading the code the log mailer printed.
const account = async (name) => {
  const tag = Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
  // Lowercase: the server stores (and mails) emails that way.
  const email = `${name}-${tag}@example.com`.toLowerCase();
  const up = await call('sign_up', [email, 'password12345', `${name}${tag}`.slice(0, 20), name]);
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
  return { name, email, jar: v.cookie, id: v.body.Ok.me.user.id };
};

const stats = p => p.evaluate(() => {
  try { return JSON.parse(globalThis.__chatCallStats()); }
  catch (e) { return { error: String(e) }; }
});

// The fake mic beeps, so one reading is mostly silence: poll and keep maxima.
const peakOver = async (pages, ms) => {
  const best = pages.map(() => ({ micPeak: 0, remotePeak: 0, sent: 0, recv: 0, unreadable: 0, peers: 0 }));
  const last = pages.map(() => ({}));
  const until = Date.now() + ms;
  while (Date.now() < until) {
    for (let i = 0; i < pages.length; i++) {
      const s = await stats(pages[i]);
      last[i] = s;
      if (s && s.live) for (const k of Object.keys(best[i])) best[i][k] = Math.max(best[i][k], s[k]);
    }
    await sleep(150);
  }
  return best.map((b, i) => ({ ...b, encrypted: last[i].encrypted, muted: last[i].muted, live: last[i].live }));
};

const clickButton = async (p, label) => {
  const b = p.getByRole('button', { name: label, exact: true }).first();
  if (await b.count()) return b.click();
  // Icon-only actions (the tray's Mute, Deafen…) carry their label as
  // aria-label on a group, not as text.
  const labelled = p.locator(`[aria-label="${label}"]`).first();
  if (await labelled.count()) return labelled.click();
  return p.getByText(label, { exact: true }).first().click();
};

(async () => {
  console.log('--- accounts and a space ---');
  const ana = await account('Ana');
  const theo = await account('Theo');
  const space = (await call('create_space', ['Call test'], ana.jar)).body.Ok;
  check('ana made a space', !!space);
  const invite = (await call('create_invite', [space, null, null], ana.jar)).body.Ok;
  const joined = await call('join_space', [invite.code], theo.jar);
  check('theo joined it by invite', !!joined.body.Ok, JSON.stringify(joined.body));
  const lounge = sql(`select id from channels where space_id='${space}' and kind='voice' limit 1`);
  check('it has a voice channel', /^[0-9a-f-]{36}$/.test(lounge), lounge);

  console.log('--- two browsers, one call ---');
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required',
           `--use-file-for-fake-audio-capture=${WAV}`],
  });
  const open = async (who) => {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, permissions: ['microphone'] });
    const [name, value] = who.jar.split(/=(.*)/s);
    await ctx.addCookies([{ name, value, url: APP }]);
    const p = await ctx.newPage();
    p.on('console', m => {
      const t = m.text();
      if (/\[(chat|call-[a-z]+)\]/.test(t) || m.type() === 'error') console.log(`        ${who.name}: ${t}`);
    });
    p.on('pageerror', e => console.log(`        ${who.name} page error: ${e.message}`));
    // The ticket, so a failed dial can be retried by hand against the relay.
    p.on('response', async r => {
      if (r.url().endsWith('/_srv/join_call')) console.log(`        ${who.name} ticket: ${(await r.text().catch(() => '')).slice(0, 400)}`);
    });
    await p.goto(`${APP}/s/${space}/${lounge}`, { waitUntil: 'load' });
    await p.getByText('Join voice').first().waitFor({ timeout: 20000 });
    return p;
  };
  const a = await open(ana);
  const t = await open(theo);

  await clickButton(a, 'Join voice');
  await a.getByText('Voice connected').first().waitFor({ timeout: 15000 }).catch(() => {});
  await clickButton(t, 'Join voice');
  await t.getByText('Voice connected').first().waitFor({ timeout: 25000 }).catch(() => {});
  const body = p => p.evaluate(() => document.body.innerText);
  check('ana is connected', (await body(a)).includes('Voice connected'), (await body(a)).slice(0, 300));
  check('theo is connected', (await body(t)).includes('Voice connected'), (await body(t)).slice(0, 300));

  const cues = p => p.evaluate(() => { try { return JSON.parse(globalThis.__chatCues()); } catch { return null; } });
  const [ca, ct] = [await cues(a), await cues(t)];
  console.log(`        cues: ana ${JSON.stringify(ca)}, theo ${JSON.stringify(ct)}`);
  check('ana heard herself join, then theo arrive', JSON.stringify(ca) === '["joined","person-joined"]', JSON.stringify(ca));
  check('theo heard himself join — and nothing for ana, who was already there', JSON.stringify(ct) === '["joined"]', JSON.stringify(ct));

  const [sa, st] = await peakOver([a, t], 8000);
  console.log(`        ana  ${JSON.stringify(sa)}`);
  console.log(`        theo ${JSON.stringify(st)}`);
  check('the call is end-to-end encrypted on both sides', sa.encrypted === true && st.encrypted === true);
  check("ana's mic is picking up", sa.micPeak > 0);
  check("theo's mic is picking up", st.micPeak > 0);
  check('ana sends voice packets', sa.sent > 0);
  check('theo hears ana', st.recv > 0 && st.remotePeak > 0);
  check('ana hears theo', sa.recv > 0 && sa.remotePeak > 0);
  check('everything received opened with the room key', sa.unreadable === 0 && st.unreadable === 0);

  console.log('--- who is talking ---');
  // Sample both sides together: how often each says it talks, how often the
  // other sees it, and how many people each page lights up at once.
  const vad = async (ms) => {
    const r = { ticks: 0, a: 0, t: 0, aSeesT: 0, tSeesA: 0, maxLitA: 0, maxLitT: 0 };
    const until = Date.now() + ms;
    while (Date.now() < until) {
      const [x, y] = [await stats(a), await stats(t)];
      if (x.live && y.live) {
        r.ticks++;
        r.a += x.speaking; r.t += y.speaking;
        r.aSeesT += x.othersSpeaking > 0; r.tSeesA += y.othersSpeaking > 0;
        r.maxLitA = Math.max(r.maxLitA, x.speakingUsers); r.maxLitT = Math.max(r.maxLitT, y.speakingUsers);
      }
      await sleep(100);
    }
    return r;
  };
  const v = await vad(8000);
  const share = n => n / Math.max(1, v.ticks);
  console.log(`        ${JSON.stringify(v)} (fixture talks ${DUTY * 100}% of the time)`);
  check('ana is detected talking, and not stuck on', share(v.a) > 0.25 && share(v.a) < 0.95, `${(share(v.a) * 100).toFixed(0)}%`);
  check('theo is detected talking, and not stuck on', share(v.t) > 0.25 && share(v.t) < 0.95, `${(share(v.t) * 100).toFixed(0)}%`);
  check('theo sees ana talking', share(v.tSeesA) > 0.25 && share(v.tSeesA) < 0.95, `${(share(v.tSeesA) * 100).toFixed(0)}%`);
  check('ana sees theo talking', share(v.aSeesT) > 0.25 && share(v.aSeesT) < 0.95, `${(share(v.aSeesT) * 100).toFixed(0)}%`);
  check('each page lights both people (itself and the other)', v.maxLitA === 2 && v.maxLitT === 2, `${v.maxLitA}, ${v.maxLitT}`);

  console.log('--- mute ---');
  await clickButton(a, 'Mute');
  await sleep(1500);
  const [ma, mt] = await peakOver([a, t], 5000);
  check('ana reads as muted', ma.muted === true, JSON.stringify(ma));
  check('theo hears nothing from ana', mt.remotePeak === 0, JSON.stringify(mt));
  check('ana still hears theo', ma.remotePeak > 0, JSON.stringify(ma));
  const mv = await vad(4000);
  check('muted, ana is never shown talking — to herself or to theo', mv.a === 0 && mv.tSeesA === 0, JSON.stringify(mv));
  check('and theo still is', mv.aSeesT > 0, JSON.stringify(mv));
  await clickButton(a, 'Mute');
  await sleep(1500);
  const [, ut] = await peakOver([a, t], 5000);
  check('unmuted, theo hears ana again', ut.remotePeak > 0, JSON.stringify(ut));
  check('muting played nothing', JSON.stringify(await cues(t)) === '[]');

  console.log('--- leaving ---');
  await cues(a);
  await clickButton(t, 'Leave call');
  await sleep(800);
  check('theo heard himself leave', JSON.stringify(await cues(t)) === '["left"]');
  check('ana hears nothing yet — it might be a reconnect', JSON.stringify(await cues(a)) === '[]');
  await sleep(2500);
  check('then hears theo go', JSON.stringify(await cues(a)) === '["person-left"]');

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(2); });
