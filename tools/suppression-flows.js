// Noise suppression, end to end, in both places it can run.
//
// The claim under test is not "the model works" — `cargo test -p voice-dsp`
// and the SDK's own tests cover the DSP. It is that the *choice* is real: that
// picking a mode in Settings changes what the relay is asked for, that the
// relay's answer comes back, and that audio still crosses the room in every
// mode. A suppression feature that silently does nothing sounds exactly like a
// suppression feature that is not helping.
//
//   bash tools/reset-dev.sh
//   VOICE_DENOISE=on ./target/debug/voice-node      # for the server-side case
//   node tools/suppression-flows.js
//
// Run with the node's VOICE_DENOISE unset to exercise the refusal path
// instead: server mode must then report `off`, not `server`.

const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8091';
const API = APP + '/_srv';
const LOG = '/tmp/hearth-server.log';
// What the node was started with. The refusal path is a real, supported
// outcome, so the suite asserts against whichever one is actually running
// rather than demanding the permissive one.
// The call relay forwards audio without decoding it, so it never runs
// suppression: a server request is always refused. `NODE_ALLOWS=1` was for the
// old voice-node.
const NODE_ALLOWS = process.env.NODE_ALLOWS === '1';

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
  try { body = JSON.parse(raw); } catch { body = { Err: { Failed: raw.trim() } }; }
  return { body, status: r.status, cookie: setCookie ? setCookie.split(';')[0] : null };
};

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

const clickRoom = async (p, label) => {
  const at = await p.evaluate(({ label }) => {
    const els = [...document.querySelectorAll('div,span')].filter(e => {
      if ((e.innerText || '').trim() !== label) return false;
      const r = e.getBoundingClientRect();
      return r.width > 0 && r.height > 0 && r.y < 700;
    });
    const el = els[els.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (at) await p.mouse.click(at.x, at.y);
  return !!at;
};

/// Pick an option inside ONE settings row.
///
/// A plain text click cannot do this: the settings page has several rows whose
/// options are "On" and "Off", so matching a label alone would toggle whichever
/// one happened to render last. Scoping to the row is the difference between
/// testing the control and testing the page layout.
///
/// There is no `<button>` to look for — idea-ui renders a Button as a styled
/// `div`, which is exactly the kind of thing a test that assumed HTML
/// semantics would get wrong while reporting it as a missing feature. So the
/// row is "the smallest box that contains both the row's name and something
/// whose text is exactly this option".
const pickInRow = async (p, rowName, option, { dryRun = false } = {}) => {
  const at = await p.evaluate(({ rowName, option }) => {
    const exactly = (el, s) => (el.innerText || '').trim() === s;
    const rows = [...document.querySelectorAll('div')]
      // `startsWith`, not `includes`: every ancestor up to <body> "includes"
      // the row's name, and some of them are zero-height layout wrappers that
      // sort to the front and then hand back whichever "Off" pill the PAGE
      // happens to contain. Anchoring to the start of the text is what makes
      // this the row and not one of its ancestors.
      .filter(d => (d.innerText || '').trim().startsWith(rowName)
        && [...d.querySelectorAll('div,span')].some(e => exactly(e, option)))
      .sort((a, b) => a.getBoundingClientRect().height - b.getBoundingClientRect().height);
    const row = rows[0];
    if (!row) return null;
    // Innermost match: a pill's text sits in a child of the pill, and clicking
    // the deepest one lands inside every ancestor that carries the handler.
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
  if (dryRun) return !!at;
  if (at) await p.mouse.click(at.x, at.y);
  return !!at;
};

const text = p => p.evaluate(() => document.body.innerText);
const has = async (p, s) => (await text(p)).includes(s);
const stats = p => p.evaluate(() => {
  try { return JSON.parse(window.__hearthVoiceStats()); }
  catch (e) { return { error: String(e) }; }
});

/// Which mode the settings page currently describes.
///
/// Read from the note under the row rather than from the pills: a selected
/// pill differs from an unselected one only by colour, and colour is not
/// something this harness can read reliably. The note is prose that says which
/// mode is chosen, which makes it both the user-visible answer and the
/// testable one.
const noteFor = async (p) => {
  const t = await text(p);
  if (t.includes('DeepFilterNet runs here')) return 'This device';
  if (t.includes('DeepFilterNet runs on the relay')) return 'Server';
  if (t.includes("Your browser's own suppression")) return 'Off';
  return '(no note on the page)';
};

/// Hang up, and make sure it took.
///
/// The tray's hang-up is an icon with no text, so this goes the way a person
/// would when they want to be sure: open the call view, which has a labelled
/// control, and use that. Then check — because the mode is read at join, a
/// call that fails to end quietly turns the NEXT mode's measurements into a
/// second reading of this one.
const hangUp = async (p) => {
  for (let attempt = 0; attempt < 3; attempt++) {
    if (!(await has(p, 'Voice connected'))) return;
    if (!(await has(p, 'Leave room'))) {
      await clickRoom(p, 'The Porch');
      await p.waitForTimeout(1200);
    }
    await clickText(p, 'Leave room');
    await p.waitForTimeout(1500);
    if (await has(p, 'Back to chat')) {
      await clickText(p, 'Back to chat');
      await p.waitForTimeout(800);
    }
  }
  if (await has(p, 'Voice connected')) throw new Error('could not leave the call');
};

// The fake mic beeps periodically, so one reading is zero most of the time.
// Poll and keep the maximum.
// Wait until the node actually reports the mode, rather than sleeping and
// hoping. The neural chain has to fetch and initialise a 7.8 MB model first,
// and how long that takes depends on the machine — so a fixed sleep is a race
// that only shows up on a loaded one. When it lost, the measurement window
// below still contained pre-swap audio and `peakOver` takes the MAXIMUM, so
// the tone "had not been suppressed" when really it had not been measured.
const waitForMode = async (page, mode, ms = 30000) => {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    const s = await stats(page);
    if (s && s.suppress === mode) return true;
    await page.waitForTimeout(250);
  }
  return false;
};

const peakOver = async (pages, ms) => {
  const best = pages.map(() => ({ mic: 0, remote: 0, sent: 0, recv: 0, peers: 0, suppress: '?' }));
  const until = Date.now() + ms;
  while (Date.now() < until) {
    for (let i = 0; i < pages.length; i++) {
      const s = await stats(pages[i]);
      if (s && !s.error) {
        best[i].mic = Math.max(best[i].mic, s.micPeak);
        best[i].remote = Math.max(best[i].remote, s.remotePeak);
        best[i].sent = Math.max(best[i].sent, s.sent);
        best[i].recv = Math.max(best[i].recv, s.recv);
        best[i].peers = Math.max(best[i].peers, s.peers);
        best[i].suppress = s.suppress;
      }
    }
    await pages[0].waitForTimeout(250);
  }
  return best;
};

(async () => {
  const fs = require('fs');
  const claim = (fs.readFileSync(LOG, 'utf8').match(/claim\/([a-f0-9]+)/) || [])[1];
  if (!claim) { console.log('no claim token — run tools/reset-dev.sh first'); process.exit(1); }

  const claimed = await call('claim_instance',
    [{ token: claim, handle: 'ana', display_name: 'Ana Ruiz', email: null, password: 'password123' }]);
  const anaJar = claimed.cookie;
  const space = (await call('create_space', ['cabin', 'The Cabin'], anaJar)).body.Ok;
  const invite = (await call('create_invite', [space.id, 5], anaJar)).body.Ok;
  await call('register',
    [{ invite_code: invite, handle: 'theo', display_name: 'Theo Lang', email: null, password: 'password123' }]);

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  // Which wasm chunks each tab fetched, so the split can be asserted rather
  // than assumed. A chunk that loads on every page view is a chunk that is not
  // doing its job, and nothing about the app would look different.
  const chunks = {};
  const open = async (handle, password) => {
    const p = await browser.newPage({ viewport: { width: 1440, height: 900 }, permissions: ['microphone'] });
    chunks[handle] = [];
    p.on('request', r => {
      const u = r.url();
      if (u.includes('lazy_body') && u.endsWith('.wasm')) chunks[handle].push(u.split('/').pop());
    });
    // A suite that cannot see the page's own console is blind to exactly the
    // failures that present as "the setting did nothing".
    p.on('console', m => {
      const t = m.text();
      if (t.startsWith('[suppress]')) console.log(`        ${handle}: ${t}`);
    });
    p.on('pageerror', e => console.log(`        ${handle}: PAGE ERROR ${String(e).slice(0, 160)}`));
    await p.goto(APP + '/', { waitUntil: 'load' });
    await p.waitForTimeout(3000);
    await fill(p, 'Handle or email', handle);
    await fill(p, 'Password', password);
    await clickText(p, 'Sign in');
    await p.waitForTimeout(3000);
    return p;
  };

  const a = await open('ana', 'password123');
  const t = await open('theo', 'password123');
  check('both are signed in', (await has(a, 'The Cabin')) && (await has(t, 'The Cabin')));

  // The setting exists and says what it costs. Worth asserting: the whole
  // point of three modes rather than a switch is that the user can see the
  // trade, and a row that renders without its help text is a switch again.
  // Before anything is switched on: the inference engine must not have been
  // downloaded. This is the whole point of the boundary — it is ~6.8 MB of
  // release wasm that most visitors never need.
  check('the suppression chunk is not fetched on an ordinary visit',
    chunks['ana'].length === 0, JSON.stringify(chunks));

  await a.mouse.click(35, 872);
  await a.waitForTimeout(1200);
  check('settings offer noise suppression', await has(a, 'Noise suppression'));
  check('and say what it costs', await has(a, '8 MB download'));

  /// Choose a suppression mode in one tab, and confirm the page agrees.
  const setMode = async (p, option) => {
    // Settings is a toggle on the rail; the avatar button is at the bottom.
    if (!(await has(p, 'Noise suppression'))) {
      await p.mouse.click(35, 872);
      await p.waitForTimeout(1000);
    }
    const picked = await pickInRow(p, 'Noise suppression', option);
    if (!picked) throw new Error(`no "${option}" option in the noise suppression row`);
    await p.waitForTimeout(600);
    // Verify the pick landed. The note under the row says something different
    // for each mode, which makes it the one thing on the page that can
    // distinguish "clicked the right pill" from "clicked a pill". An earlier
    // version of this suite skipped the check and spent three modes testing
    // the same call.
    const note = await noteFor(p);
    if (note !== option) {
      throw new Error(`picked "${option}" but the page describes "${note}"`);
    }
    await p.mouse.click(35, 872);   // back out of settings
    await p.waitForTimeout(1000);
  };

  /// Set both tabs to `option`, join The Porch, and report what the relay said.
  const callWith = async (option) => {
    for (const p of [a, t]) await setMode(p, option);

    await clickRoom(a, 'The Porch');
    await clickRoom(t, 'The Porch');
    // Client-side has a model to fetch and build on the first call; later
    // calls reuse the fetched bytes but still build a model per join.
    await a.waitForTimeout(option === 'This device' ? 9000 : 5000);
    const [sa, st] = await peakOver([a, t], 4000);

    // Leave, so the next mode starts from a clean call rather than inheriting
    // the socket the previous one opened. This is not tidiness: the mode is
    // read at join and the model is built there, so a call that is still up
    // would report the mode it was STARTED with — which is exactly how the
    // first run of this suite passed three modes' worth of checks against one
    // `off` call.
    await hangUp(a);
    await hangUp(t);
    return [sa, st];
  };

  // --- off: the baseline every other mode is judged against ---
  //
  // Chromium's fake microphone emits a periodic pure tone. That matters more
  // here than anywhere else in this repo: a sine is not speech, and
  // DeepFilterNet's whole job is to remove what is not speech. So the fixture
  // that proves audio moves in the OFF case is precisely the fixture the
  // neural modes are supposed to delete — which is why the checks below are
  // not the same checks.
  let baseline = 0;
  {
    const [sa, st] = await callWith('Off');
    console.log(`        off    ana: ${JSON.stringify(sa)}`);
    baseline = Math.max(sa.remote, st.remote);
    check('off — the relay reports no suppression', sa.suppress === 'off', JSON.stringify(sa));
    check('off — audio crosses the room', sa.recv > 0 && st.recv > 0, JSON.stringify([sa, st]));
    check('off — the far side hears the tone', baseline > 0, JSON.stringify([sa, st]));
  }

  /// The two claims that survive a fixture the model is designed to remove.
  ///
  /// "Packets are still flowing" separates a working suppressor from a broken
  /// pipeline: a chain that died would send nothing at all, and a far side at
  /// zero received would be indistinguishable from one at zero volume.
  /// "The tone is attenuated" is then positive evidence that the model ran —
  /// audio arrived, and it is much quieter than the same audio was without it.
  const neuralChecks = (label, sa, st) => {
    check(`${label} — the cleaned stream is still producing packets`,
      sa.sent > 0 && st.recv > 0, JSON.stringify([sa, st]));
    check(`${label} — audio still crosses the room`, sa.recv > 0 && st.recv > 0,
      JSON.stringify([sa, st]));
    const loudest = Math.max(sa.remote, st.remote);
    check(`${label} — the tone is suppressed (${baseline}% -> ${loudest}%)`,
      loudest * 4 <= baseline,
      `a sine is not speech, so the model should remove most of it; ${JSON.stringify([sa, st])}`);
  };

  // --- on this device: the model runs here, the wire carries clean audio ---
  {
    const [sa, st] = await callWith('This device');
    console.log(`        client ana: ${JSON.stringify(sa)}`);
    check('on-device — the relay is told where it runs', sa.suppress === 'client', JSON.stringify(sa));
    check('on-device — and choosing it fetched the chunk, once',
      chunks['ana'].length === 1, JSON.stringify(chunks['ana']));
    neuralChecks('on-device', sa, st);
  }

  // --- no "on the server" ---
  // The call relay forwards audio without decoding it, so there is nowhere
  // server-side for a model to run, and the setting must not offer one.
  check('settings do not offer suppression on the server',
    !(await pickInRow(a, 'Noise suppression', 'Server', { dryRun: true })), 'a "Server" option is still offered');

  // --- changing your mind without hanging up ---
  //
  // The setting used to be read only at join, so turning suppression on while
  // already in a call did nothing until you left and came back — which is not
  // what anyone expects a settings toggle to mean. The capture chain is now
  // swapped under the running socket.
  {
    for (const p of [a, t]) await setMode(p, 'Off');
    await clickRoom(a, 'The Porch');
    await clickRoom(t, 'The Porch');
    await a.waitForTimeout(9000);
    const [before] = await peakOver([a, t], 1000);
    check('mid-call — the call starts unsuppressed', before.suppress === 'off',
      JSON.stringify(before));

    await setMode(a, 'This device');
    check('mid-call — the swap completes', await waitForMode(a, 'client'));
    // Then long enough for the replacement chain's leveller to settle. It
    // starts at unity gain by design — the first frames of a call should sound
    // like the microphone, not like a gain sweep — so the first second or two
    // after a swap is a transient, not a steady state. This wait is for the
    // TRANSIENT, which is a real property of the chain; the wait for the model
    // to load is above, where it belongs, and is not a guess.
    await a.waitForTimeout(6000);
    const [after, afterT] = await peakOver([a, t], 4000);
    console.log(`        swap   before: ${JSON.stringify(before)}`);
    console.log(`        swap   after : ${JSON.stringify(after)}`);

    check('mid-call — suppression turns on without rejoining', after.suppress === 'client',
      JSON.stringify(after));
    // The discriminator. A rejoin resets every counter, so a value that is
    // still climbing from where it was is proof the socket was never dropped —
    // which is the whole claim, and the one thing "it says client now" cannot
    // establish on its own.
    check('mid-call — it is the same call, not a silent rejoin',
      after.recv > before.recv && after.sent > before.sent,
      `counters must keep climbing: ${JSON.stringify([before, after])}`);
    check('mid-call — audio keeps flowing to the far side', afterT.recv > 0,
      JSON.stringify(afterT));
    // Judged from theo's seat, not ana's. `remote` on ana is what she RECEIVES
    // from theo — the wrong end of the room — and her local `mic` meter is a
    // max over a window with two browsers and two models competing for the
    // machine, which made it read a transient often enough to be useless as
    // evidence. What the swap claims is that the far side stops hearing the
    // noise, so that is what is checked.
    check(`mid-call — theo stops hearing ana's tone (${baseline}% -> ${afterT.remote}%)`,
      afterT.remote * 4 <= baseline, JSON.stringify(afterT));

    await hangUp(a);
    await hangUp(t);
  }

  await browser.close();
  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error(e); process.exit(1); });
