// Drive Hearth through every screen in a real browser and assert what landed.
//
// The point is the assertions, not the screenshots. Hearth's whole update model
// is `switch` regions keyed on declared dependencies (see `model.rs`, "Render
// keys"), and the failure mode when a key is wrong is silent: the region simply
// stops updating and still renders something plausible. Nothing but driving the
// real app catches that — it compiles, it lints, it looks right in a static
// screenshot, and it is broken.
//
//   cd crates/hearth && idealyst build --web
//   idealyst serve dist/web --port 8099
//   node tools/hearth-ui.js
//
// Set SHOTS=1 to also write PNGs of each screen to $OUT (default /tmp/hearth-).

const { chromium } = require('playwright-core');

const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8099';
const OUT = process.env.OUT || '/tmp/hearth-';
const SHOTS = !!process.env.SHOTS;

const checks = [];
const check = (name, ok) => {
  checks.push({ name, ok });
  console.log(`${ok ? '  ok  ' : ' FAIL '} ${name}`);
};

const text = p => p.evaluate(() => document.body.innerText);
const has = async (p, s) => (await text(p)).includes(s);

/// Locate a node by its exact visible text and click it with a real mouse.
/// Synthetic `MouseEvent`s are not enough: a pressable is a `<div role=button>`
/// wired through the platform's own click channel.
const clickText = async (p, label, opts = {}) => {
  const at = await p.evaluate(({ label, xmax, xmin }) => {
    const els = [...document.querySelectorAll('div,span,button')].filter(e => {
      if ((e.innerText || '').trim().toLowerCase() !== label.toLowerCase()) return false;
      const r = e.getBoundingClientRect();
      if (!r.width || !r.height) return false;
      if (xmax != null && r.x > xmax) return false;
      if (xmin != null && r.x < xmin) return false;
      return true;
    });
    const el = els[els.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label, xmax: opts.xmax, xmin: opts.xmin });
  if (!at) return false;
  await p.mouse.click(at.x, at.y);
  await p.waitForTimeout(700);
  return true;
};

const shot = async (p, name) => {
  if (SHOTS) await p.screenshot({ path: OUT + name + '.png' });
};

(async () => {
  const browser = await chromium.launch({ executablePath: EXE, headless: true });
  const p = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const errors = [];
  p.on('pageerror', e => errors.push('pageerror: ' + e.message));
  // `idealyst build --web` emits no favicon, so the browser's automatic
  // /favicon.ico request 404s on every load. Not an app error.
  p.on('response', r => {
    if (r.status() >= 400 && !r.url().endsWith('/favicon.ico')) {
      errors.push(`${r.status()} ${r.url()}`);
    }
  });
  p.on('console', m => {
    const t = m.text();
    if (m.type() === 'error' && !t.startsWith('Failed to load resource')) errors.push(t);
  });

  await p.goto(APP, { waitUntil: 'load' });
  await p.waitForTimeout(3500);

  // --- the room it opens on --------------------------------------------------
  check('opens on #general in The Cabin', await has(p, 'Everyone in The Cabin can read this'));
  check('transcript rendered', await has(p, 'The trail gate is finally fixed'));
  check('roster rendered', await has(p, 'AROUND — 5'));
  // Theo posts twice in a row at 9:04. Grouped, the second line follows the
  // first with no name and no avatar between them.
  check(
    'consecutive messages group under one name',
    /Gate won\.\s*Anyway — anyone up for the ridge loop/.test(await text(p)),
  );
  await shot(p, '01-chat');

  // --- navigation ------------------------------------------------------------
  // The rail carries no `switch`: which space is selected is read inside each
  // button's style closure. Assert the re-tint actually happens, because the
  // failure mode when a signal is read one level too high is a frozen
  // highlight that still looks like a plausible UI.
  const railFill = t => p.evaluate(t => {
    const rail = document.querySelector('#app').firstElementChild.children[0];
    const el = [...rail.querySelectorAll('[role=button]')]
      .find(e => (e.innerText || '').trim() === t);
    return el ? getComputedStyle(el).backgroundColor : '';
  }, t);
  const cabinBefore = await railFill('TC');
  await clickText(p, 'ST', { xmax: 70 });
  check('the rail re-tints the selected space in place', await railFill('ST') === cabinBefore);
  check('the rail clears the previous space', await railFill('TC') !== cabinBefore);
  check('switching space swaps the room list', await has(p, 'ship-log'));
  await clickText(p, 'TC', { xmax: 70 });

  await clickText(p, 'recipes', { xmax: 320 });
  check('selecting a channel switches the transcript', await has(p, 'Thermos chai'));

  await clickText(p, 'The Porch', { xmax: 320 });
  check('a live room replaces the message column', !(await has(p, 'Enter sends')));
  check('room shows its occupants as tiles', await has(p, 'Invite someone'));
  await shot(p, '02-room');

  // --- joining ---------------------------------------------------------------
  await clickText(p, 'Join room');
  check('joining puts You in the room', (await text(p)).includes('Leave room'));
  check('joining shows the voice tray strip', await has(p, 'Leave'));
  await shot(p, '03-joined');

  // --- the tray survives navigating away ------------------------------------
  await clickText(p, 'general', { xmax: 320 });
  check('you stay in the room after navigating to a channel', await has(p, 'Leave'));

  // --- composing -------------------------------------------------------------
  const field = p.locator('input').first();
  await field.click();
  await field.type('A message from the harness.', { delay: 10 });
  await p.waitForTimeout(300);
  // An input's value is not `innerText`; read the field itself.
  check(
    'the composer accepts typing',
    (await p.evaluate(() => document.querySelector('input').value)) ===
      'A message from the harness.',
  );
  await p.keyboard.press('Enter');
  await p.waitForTimeout(800);
  check('Enter posts the message', await has(p, 'A message from the harness.'));
  check(
    'sending clears the draft',
    (await p.evaluate(() => document.querySelector('input').value)) === '',
  );
  await shot(p, '04-sent');

  // --- threads ---------------------------------------------------------------
  await clickText(p, '3 replies');
  check('a thread opens with its root and replies', await has(p, '7am at the lower lot?'));
  await shot(p, '05-thread');
  // Close it via the panel's own X. The y-band matters: the app header's
  // mobile-preview toggle sits at the same x, 60px higher.
  await p.evaluate(() => {
    const x = [...document.querySelectorAll('[role=button]')].find(e => {
      const r = e.getBoundingClientRect();
      return r.x > 1380 && r.y > 60 && r.y < 120;
    });
    if (x) x.click();
  });
  await p.waitForTimeout(700);
  check('the thread closes again', !(await has(p, '7am at the lower lot?')));

  // --- the person menu -------------------------------------------------------
  await clickText(p, 'Ana Ruiz', { xmax: 320 });
  check('pressing a person opens their menu', await has(p, 'View profile'));
  check('a moderator sees moderation actions', await has(p, 'Mute in rooms'));
  await shot(p, '06-menu');
  await clickText(p, 'View profile');
  check('the profile card opens', await has(p, 'In this space since'));
  await shot(p, '07-profile');
  await p.keyboard.press('Escape');
  await p.waitForTimeout(600);

  // --- search ----------------------------------------------------------------
  await clickText(p, 'Search');
  const q = await p.evaluate(() => {
    const i = [...document.querySelectorAll('input')].find(i => i.placeholder.startsWith('Search'));
    if (!i) return null;
    const r = i.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  });
  check('the search palette opens', !!q);
  if (q) {
    await p.mouse.click(q.x, q.y);
    await p.keyboard.type('ridge');
    await p.waitForTimeout(800);
    check('search finds messages across the space', await has(p, 'Ridge, last light'));
  }
  await shot(p, '08-search');
  await p.keyboard.press('Escape');
  await p.waitForTimeout(600);

  // --- direct messages -------------------------------------------------------
  await p.mouse.click(35, 36);
  await p.waitForTimeout(800);
  check('direct messages open outside every space', await has(p, 'Outside every space'));
  check('conversations show their last line', await has(p, 'On my way.'));
  await shot(p, '09-dms');

  // --- settings, theme, accent ----------------------------------------------
  await p.mouse.click(35, 872);
  await p.waitForTimeout(800);
  check('settings open', await has(p, "Seven of them. That's the whole list."));
  await shot(p, '10-settings');

  const bg = () => p.evaluate(() =>
    getComputedStyle(document.querySelector('#app').firstElementChild).backgroundColor);
  const dark = await bg();
  await clickText(p, 'Light');
  const light = await bg();
  check('the theme toggle repaints the whole shell', dark !== light);
  await clickText(p, 'Teal');
  await p.waitForTimeout(500);
  const accent = await p.evaluate(() => {
    const el = [...document.querySelectorAll('div')]
      .find(e => (e.innerText || '').trim() === 'Teal');
    return el ? getComputedStyle(el).backgroundColor : '';
  });
  check('the accent reaches component fills', /rgb\(15, 118, 110\)|rgb\(1[0-9], 1[0-9][0-9]/.test(accent));
  await shot(p, '11-light-teal');
  await clickText(p, 'Dark');
  await clickText(p, 'Indigo');

  // --- the mobile preview ----------------------------------------------------
  await p.mouse.click(1408, 28);
  await p.waitForTimeout(900);
  check('the mobile preview opens', await has(p, 'Same app, one column'));
  // It previews the last text conversation visited, which by this point in the
  // run is Ana's DM — a live room has no message column to show.
  check('the preview shows the last conversation', await has(p, 'On my way.'));
  await shot(p, '12-mobile');

  check('no runtime errors', errors.length === 0);
  if (errors.length) console.log(errors.join('\n'));

  const failed = checks.filter(c => !c.ok);
  console.log(`\n${checks.length - failed.length}/${checks.length} checks passed`);
  await browser.close();
  process.exit(failed.length ? 1 : 0);
})();
