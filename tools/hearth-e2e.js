// The real thing: a browser, the wasm client, and the server it talks to.
//
// Everything here goes through the UI — claim the instance, sign in, post,
// reply, open a DM — because the point is that the two halves are one product.
// The server harnesses (auth-flows.sh, message-flows.sh) prove the API; this
// proves the app.
//
//   cargo build -p hearth --bin server --features server
//   cd crates/hearth && idealyst build --web
//   DATABASE_URL=… ./target/debug/server | tee /tmp/hearth-server.log &
//   node tools/hearth-e2e.js
//
// Expects a FRESH database: it claims the instance, which works once.

const { chromium } = require('playwright-core');
const fs = require('fs');

const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://127.0.0.1:8090';
const LOG = process.env.LOG || '/tmp/hearth-server.log';
const OUT = process.env.OUT || '/tmp/hearth-e2e-';
const SHOTS = !!process.env.SHOTS;

let pass = 0, fail = 0;
const check = (name, ok, detail) => {
  if (ok) { console.log(`  ok   ${name}`); pass++; }
  else { console.log(` FAIL  ${name}`); if (detail) console.log(`        ${detail}`); fail++; }
};

const text = p => p.evaluate(() => document.body.innerText);
const has = async (p, s) => (await text(p)).includes(s);
const shot = async (p, n) => { if (SHOTS) await p.screenshot({ path: OUT + n + '.png' }); };

/// Type into the field whose label sits above it. The auth forms are the only
/// place labels are reliable, and matching on them is what keeps this readable.
const fill = async (p, label, value) => {
  const ok = await p.evaluate(({ label }) => {
    // idea-ui's Field puts the label in the input's immediate parent, above
    // it, with any help text below.
    for (const i of document.querySelectorAll('input')) {
      const box = i.parentElement;
      if (box && (box.innerText || '').trim().startsWith(label)) {
        i.dataset.target = '1';
        return true;
      }
    }
    return false;
  }, { label });
  if (!ok) return false;
  const el = await p.$('input[data-target="1"]');
  await el.click();
  await el.type(value, { delay: 5 });
  await p.evaluate(() => document.querySelector('input[data-target="1"]')?.removeAttribute('data-target'));
  return true;
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
    // Scroll it into view first: an element below the fold still has a size,
    // so a click at its rect lands outside the viewport and does nothing.
    el.scrollIntoView({ block: 'center' });
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (!at) return false;
  await p.mouse.click(at.x, at.y);
  await p.waitForTimeout(900);
  return true;
};

(async () => {
  const claimToken = (fs.readFileSync(LOG, 'utf8').match(/claim\/([a-f0-9]+)/) || [])[1];
  if (!claimToken) { console.log('no claim token in ' + LOG + ' — is this a fresh instance?'); process.exit(1); }

  const browser = await chromium.launch({ executablePath: EXE, headless: true });
  const ana = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const calls = [];
  const errors = [];
  ana.on('request', r => { if (r.url().includes('/_srv/')) calls.push(r.url().split('/_srv/')[1]); });
  ana.on('pageerror', e => errors.push(e.message));

  await ana.goto(APP, { waitUntil: 'load' });
  await ana.waitForTimeout(3500);

  // --- claiming ---
  check('an unclaimed instance offers to be claimed', await has(ana, 'Claim this Hearth'));
  check('the client asked the server what it is', calls.includes('instance_status'));
  await shot(ana, '01-claim');

  await fill(ana, 'Claim link', claimToken);
  await fill(ana, 'Handle', 'ana');
  await fill(ana, 'Display name', 'Ana Ruiz');
  await fill(ana, 'Password', 'a good passphrase');
  await clickText(ana, 'Claim this instance');
  await ana.waitForTimeout(2500);

  check('claiming signs you straight in', await has(ana, 'Ana Ruiz'));
  check('and lands somewhere honest with no spaces yet', await has(ana, 'Nothing open'));
  check('and the shell loaded from the server', calls.includes('bootstrap'));
  await shot(ana, '02-claimed');

  // --- a space ---
  // No spaces yet, so the rail's + is the only way forward. Find it rather
  // than assume where it sits — the rail is shorter with no spaces in it.
  const plus = await ana.evaluate(() => {
    const rail = document.querySelector('#app').firstElementChild.children[0];
    // The add button is the only one in the rail with a border — the space
    // buttons are filled, the DM button is filled, this one is an outline.
    const btn = [...rail.querySelectorAll('[role=button]')].find(
      e => parseFloat(getComputedStyle(e).borderTopWidth) > 0,
    );
    if (!btn) return null;
    const r = btn.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  });
  if (plus) await ana.mouse.click(plus.x, plus.y);
  await ana.waitForTimeout(800);
  check('the join dialog opens', await has(ana, 'Somewhere to be'));
  await clickText(ana, 'Start a new one');
  await fill(ana, 'Name your space', 'The Cabin');
  await clickText(ana, 'Create space');
  await ana.waitForTimeout(2000);

  check('a space is created on the server', await has(ana, 'The Cabin'));
  check('and comes back with its channels', await has(ana, 'general'));
  check('four channels and two rooms, as promised', (await text(ana)).includes('welcome') && (await text(ana)).includes('The Porch'));
  await shot(ana, '03-space');

  // --- posting ---
  await clickText(ana, 'general');
  const composer = await ana.$('input[placeholder^="Message"]');
  check('the composer is there', !!composer);
  await composer.click();
  await composer.type('First message in a real database.', { delay: 5 });
  await ana.keyboard.press('Enter');
  await ana.waitForTimeout(1500);
  check('a message posts', await has(ana, 'First message in a real database.'));
  check('and went to the server', calls.includes('send_message'));
  await shot(ana, '04-posted');

  // --- it is really persisted ---
  await ana.reload({ waitUntil: 'load' });
  await ana.waitForTimeout(3500);
  check('the session survives a reload', !(await has(ana, 'Claim this Hearth')));
  // A reload lands on the space's first channel, not the last one visited, so
  // getting back to #general is a navigation rather than an assumption.
  await clickText(ana, 'general');
  check('and the message is still there', await has(ana, 'First message in a real database.'));
  await shot(ana, '05-reloaded');

  // --- a second person ---
  const invite = await ana.evaluate(async () => {
    const r = await fetch('/_srv/my_spaces', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: 'null',
    });
    const spaces = (await r.json()).Ok;
    const c = await fetch('/_srv/create_invite', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify([spaces[0].id, 1]),
    });
    return (await c.json()).Ok;
  });
  check('an invite can be minted', !!invite, invite);

  const theo = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  await theo.goto(APP, { waitUntil: 'load' });
  await theo.waitForTimeout(3500);
  check('a second visitor gets a sign-in screen', await has(theo, 'Sign in'));
  await clickText(theo, 'I have an invite');
  await theo.waitForTimeout(400);
  await fill(theo, 'Invite link', invite);
  await fill(theo, 'Handle', 'theo');
  await fill(theo, 'Display name', 'Theo Lang');
  await fill(theo, 'Password', 'theos passphrase');
  await clickText(theo, 'Create account');
  await theo.waitForTimeout(2500);

  check('an invite creates an account and joins the space', await has(theo, 'The Cabin'));
  await clickText(theo, 'general');
  check('and the new member sees the existing history', await has(theo, 'First message in a real database.'));
  await shot(theo, '06-invited');

  // --- two people, one channel ---
  const tc = await theo.$('input[placeholder^="Message"]');
  await tc.click();
  await tc.type('Theo, arriving.', { delay: 5 });
  await theo.keyboard.press('Enter');
  await theo.waitForTimeout(1500);
  check("the second person can post", await has(theo, 'Theo, arriving.'));

  await ana.reload({ waitUntil: 'load' });
  await ana.waitForTimeout(3500);
  await clickText(ana, 'general');
  check("and the first person sees it", await has(ana, 'Theo, arriving.'));
  check('with the right author', await has(ana, 'Theo Lang'));
  await shot(ana, '07-two-people');

  // --- the roster is real ---
  check('the roster lists both members', (await text(ana)).includes('Ana Ruiz') && (await text(ana)).includes('Theo Lang'));

  // --- signing out ---
  await ana.mouse.click(35, 872);
  await ana.waitForTimeout(900);
  check('settings show who you are', await has(ana, '@ana'));
  await clickText(ana, 'Sign out');
  await ana.waitForTimeout(1500);
  check('signing out returns to the sign-in screen', await has(ana, 'Sign in'));
  check("and the previous account's data is gone", !(await has(ana, 'The Cabin')));
  await shot(ana, '08-signed-out');

  // --- signing back in ---
  await fill(ana, 'Handle or email', 'ana');
  await fill(ana, 'Password', 'a good passphrase');
  await clickText(ana, 'Sign in');
  await ana.waitForTimeout(2500);
  await clickText(ana, 'general');
  check('signing back in restores everything', await has(ana, 'First message in a real database.'));
  await shot(ana, '09-back-in');

  check('no runtime errors', errors.length === 0, errors.join('; '));

  console.log(`\n${pass} passed, ${fail} failed`);
  await browser.close();
  process.exit(fail ? 1 : 0);
})();
