#!/usr/bin/env node
// quic-call against a relay that checks grants (call-auth): who gets in, what
// they are called, renewal, and a revocation from the issuer.
//
//   head -c 48 /dev/urandom | base64 > secret
//   cargo run --release -p call-relay -- 127.0.0.1:4471 --secret-file secret --grace 5
//   idealyst serve crates/quic-call/dist/web --port 8098
//   SECRET=secret RELAY=https://127.0.0.1:4471/ HASH=<printed> node tools/quic-call-auth-flows.js
//
// This script is also the issuer: it mints grants with `call-relay mint`,
// serves fresh ones on a renew URL, and revokes with `call-relay revoke`.
const { chromium } = require('./node_modules/playwright-core');
const { execFileSync } = require('child_process');
const http = require('http');

// A control by its name: an idea-ui Button is named by its label; the call
// bar's IconButtons by the group wrapped around each.
const btn = (p, name) => p.getByRole('button', { name, exact: true })
  .or(p.getByRole('group', { name, exact: true }).getByRole('button'));

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8098';
const RELAY = process.env.RELAY || 'https://127.0.0.1:4471/';
const HASH = process.env.HASH || '';
const SECRET = process.env.SECRET;
const BIN = process.env.RELAY_BIN || 'target/release/call-relay';
// Short, so renewal happens inside the test.
const TTL = 8;

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
const mint = (user, ttl = TTL) => execFileSync(BIN, ['mint', '--secret-file', SECRET, '--room', 'porch', '--user', user, '--ttl', String(ttl)]).toString().trim();

(async () => {
  // The issuer's renew URL: a fresh grant for whoever asks — this is where a
  // real issuer would check the session first.
  const renewals = [];
  const issuer = http.createServer((req, res) => {
    const user = new URL(req.url, 'http://x').searchParams.get('user');
    renewals.push(user);
    res.writeHead(200, { 'Access-Control-Allow-Origin': '*', 'Content-Type': 'text/plain' });
    res.end(mint(user));
  }).listen(0);
  const renewBase = `http://localhost:${issuer.address().port}/grant`;

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--autoplay-policy=no-user-gesture-required'],
  });
  const open = async (user, grant) => {
    const p = await browser.newPage({ permissions: ['microphone', 'camera'] });
    p.on('pageerror', e => console.log(`        ${user} page error: ${e.message}`));
    const q = new URLSearchParams({ url: RELAY, hash: HASH, room: 'porch' });
    if (grant) { q.set('grant', grant); q.set('renew', `${renewBase}?user=${user}`); }
    await p.goto(`${APP}/?${q}`);
    await btn(p, 'Join').waitFor({ timeout: 60000 });
    // Suppression off: this is about who is let in, not about the audio.
    await btn(p, 'Settings').click();
    await p.getByText('Off', { exact: true }).first().click();
    await btn(p, 'Done').click();
    return p;
  };
  const text = (p) => p.evaluate(() => document.body.innerText);

  const eve = await open('eve', null);
  await btn(eve, 'Join').click();
  await sleep(2500);
  check('without a grant the relay does not let you in', !(await btn(eve, 'Leave').isVisible().catch(() => false)),
        (await text(eve)).slice(0, 200));

  const alice = await open('alice', mint('alice'));
  const bob = await open('bob', mint('bob'));
  await btn(alice, 'Join').click();
  await btn(bob, 'Join').click();
  await btn(alice, 'Leave').waitFor({ timeout: 30000 });
  await sleep(2500);
  check('tiles are named by grant, not number', (await text(bob)).includes('alice') && (await text(alice)).includes('bob'),
        (await text(bob)).slice(0, 300));

  // Past the first grant's lifetime: renewal has to have happened.
  await sleep((TTL + 3) * 1000);
  check('both renewed their grants from the renew URL', renewals.includes('alice') && renewals.includes('bob'), JSON.stringify(renewals));
  check('and are still in, past the first grant\'s lifetime', (await text(bob)).includes('alice') && await btn(alice, 'Leave').isVisible(),
        (await text(bob)).slice(0, 300));

  execFileSync(BIN, ['revoke', '--secret-file', SECRET, '--relay', RELAY, '--hash', HASH, '--room', 'porch', '--user', 'alice']);
  await sleep(2000);
  check('a revoked person is told, by the connection indicator', (await text(alice)).includes('Removed from the call'), (await text(alice)).slice(0, 300));
  check('and the others see them go', !(await text(bob)).includes('alice\n'), (await text(bob)).slice(0, 300));

  await browser.close();
  issuer.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
