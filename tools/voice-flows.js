#!/usr/bin/env node
// Two people, one voice room, real audio — through Hearth.
//
// This is the loop-test's claim (a non-zero peak on the FAR side is the only
// proof the whole path carried audio) applied to the chat app, plus the thing
// the standalone demo never had: the relay refusing anyone who was not let in
// by the chat server.
//
//   bash tools/reset-dev.sh          # hearth on :8091, voice enabled
//   node tools/voice-flows.js
//
// Needs the local fleet running with the same VOICE_SHARED_SECRET.
const { chromium } = require('./node_modules/playwright-core');

const CHROME = process.env.CHROME ||
  process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const APP = process.env.APP || 'http://localhost:8091';
const API = APP + '/_srv';
const LOG = '/tmp/hearth-server.log';

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
  // An unauthenticated call is refused by the session guard before the handler
  // runs, and that refusal is plain text rather than the `{Ok}`/`{Err}` wire
  // shape — so parsing has to survive it.
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

// Click a real BUTTON with this label, not merely something displaying the
// text. The tray shows "Muted" as a status word while the call view offers it
// as a switch, so a match-anything click lands on the label and does nothing —
// which only became possible once the tray started updating correctly.
const clickButton = async (p, label) => {
  const at = await p.evaluate(({ label }) => {
    // idea-ui does not render real <button> elements, so "is it a control" has
    // to be asked a different way: an interactive element gets a pointer
    // cursor and a status word does not.
    const els = [...document.querySelectorAll('div,span,button')].filter(e => {
      if ((e.innerText || '').trim() !== label) return false;
      const r = e.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) return false;
      return getComputedStyle(e).cursor === 'pointer';
    });
    const el = els[els.length - 1];
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (!at) return false;
  await p.mouse.click(at.x, at.y);
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
    const r = el.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  }, { label });
  if (at) await p.mouse.click(at.x, at.y);
  return !!at;
};

/// Click a room in the LIST, not wherever its name happens to appear.
///
/// Once you are in a room its name is also in the tray at the bottom, and a
/// plain text match picks that up — clicking a label that does nothing, which
/// reads in the results as "the room did not respond".
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

/// The names listed under a voice room in the sidebar.
///
/// Read by geometry rather than by class, because the claim under test is a
/// visual one: these names appear beneath that room. Occupant rows are
/// indented past the room's icon, so they are the rows below it that start
/// further right — the next thing at the room's own indent ends the group.
const occupantsUnder = async (p, room) => p.evaluate(({ room }) => {
  // Read the room's own group, not "rows below it": occupants are indented
  // with padding, which does not move the element's x, so geometry cannot tell
  // where the group ends. The DOM can — a room and its occupants are one box.
  //
  // Rows carry more than one line: a room row is "The Porch\n2" (name plus
  // occupant count) and an occupant row is "SA\nSam" (initials plus name).
  const first = l => (l.innerText || '').trim().split('\n')[0].trim();
  const head = [...document.querySelectorAll('div')].find(d => {
    const r = d.getBoundingClientRect();
    return r.x < 320 && r.width > 0 && r.height > 0 && r.height < 40 && first(d) === room;
  });
  if (!head) return null;

  // Up from the row to the box that holds the row AND anyone under it.
  let group = head;
  for (let i = 0; i < 4 && group.parentElement; i++) {
    group = group.parentElement;
    if (group.children.length > 1 && group.children[0].contains(head)) break;
  }
  // An occupant row is exactly "initials, name" — two lines, the first of them
  // an avatar's one or two letters. Matching that shape rather than "any child
  // after the first" matters for the LAST room in the list, where climbing for
  // the group can otherwise reach far enough to pick up the tray below it.
  const out = [];
  for (let i = 1; i < group.children.length; i++) {
    const lines = (group.children[i].innerText || '').trim().split('\n')
      .map(x => x.trim()).filter(Boolean);
    if (lines.length !== 2 || lines[0].length > 2) continue;
    if (!out.includes(lines[1])) out.push(lines[1]);
  }
  return out;
}, { room });

const text = p => p.evaluate(() => document.body.innerText);
const has = async (p, s) => (await text(p)).includes(s);
const stats = p => p.evaluate(() => {
  try { return JSON.parse(window.__hearthVoiceStats()); }
  catch (e) { return { error: String(e) }; }
});

// The fake mic beeps periodically, so a single reading is zero most of the
// time. Poll and keep the maximum — the same lesson the loop test learned.
const peakOver = async (pages, ms) => {
  const best = pages.map(() => (
    { mic: 0, remote: 0, recv: 0, peers: 0, sent: 0, bytes: 0, unsent: 0, concealed: 0, playoutMs: 0, underruns: 0, resyncs: 0, muted: false, offMainThread: false,
      codec: '?', transport: 'none' }));
  const until = Date.now() + ms;
  while (Date.now() < until) {
    for (let i = 0; i < pages.length; i++) {
      const s = await stats(pages[i]);
      if (s && !s.error) {
        best[i].mic = Math.max(best[i].mic, s.micPeak);
        best[i].remote = Math.max(best[i].remote, s.remotePeak);
        best[i].recv = Math.max(best[i].recv, s.recv);
        best[i].peers = Math.max(best[i].peers, s.peers);
        best[i].sent = Math.max(best[i].sent, s.sent);
        best[i].bytes = Math.max(best[i].bytes, s.bytesSent);
        best[i].unsent = Math.max(best[i].unsent, s.unsent);
        best[i].concealed = Math.max(best[i].concealed, s.concealed);
        best[i].playoutMs = Math.max(best[i].playoutMs, s.playoutMs);
        best[i].underruns = Math.max(best[i].underruns, s.underruns);
        best[i].resyncs = Math.max(best[i].resyncs, s.resyncs);
        if (s.muted) best[i].muted = true;
        if (s.offMainThread) best[i].offMainThread = true;
        best[i].codec = s.codec;
        if (s.transport && s.transport !== 'none') best[i].transport = s.transport;
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

  // --- accounts and a space, over the API ---
  const claimed = await call('claim_instance',
    [{ token: claim, handle: 'ana', display_name: 'Ana Ruiz', email: null, password: 'password123' }]);
  const anaJar = claimed.cookie;
  const space = (await call('create_space', ['cabin', 'The Cabin'], anaJar)).body.Ok;
  const porch = space.channels.find(c => c.name === 'The Porch');
  const general = space.channels.find(c => c.name === 'general');
  check('a space has a voice room', !!porch, JSON.stringify(space.channels.map(c => c.name)));

  const invite = (await call('create_invite', [space.id, 5], anaJar)).body.Ok;
  const theo = await call('register',
    [{ invite_code: invite, handle: 'theo', display_name: 'Theo Lang', email: null, password: 'password123' }]);
  const theoJar = theo.cookie;
  check('a second person joins the space', !!theoJar);

  // A third member who never joins a call — presence has to be visible to
  // someone standing outside it, which is the whole point of tracking it in
  // the chat server rather than reading it off the relay.
  const wren = await call('register',
    [{ invite_code: invite, handle: 'wren', display_name: 'Wren Idris', email: null, password: 'password123' }]);
  check('a third person joins the space', !!wren.cookie);

  // --- the ticket is an authorization decision, not a formality ---
  const outsider = await call('claim_instance', [{ token: 'nope', handle: 'x', display_name: 'x', email: null, password: 'x' }]);
  const anon = await call('voice_ticket', [porch.id], null);
  check('a signed-out caller gets no ticket', !!anon.body.Err, JSON.stringify(anon.body));

  const mine = await call('voice_ticket', [porch.id], anaJar);
  check('a member gets a ticket for their room', !!mine.body.Ok, JSON.stringify(mine.body));
  check('and it names this room', mine.body.Ok && mine.body.Ok.room === porch.id);
  // The call relay this server runs, and a call-auth grant for this room and
  // this person — the relay checks it itself.
  check('and points at a call relay', mine.body.Ok && /^https:\/\//.test(mine.body.Ok.relay), JSON.stringify(mine.body.Ok));
  check('with a grant for this room', mine.body.Ok && mine.body.Ok.token.startsWith(`g1.${porch.id}.`), mine.body.Ok && mine.body.Ok.token);

  const strangerSpace = (await call('create_space', ['vault', 'The Vault'], theoJar)).body.Ok;
  const secretRoom = strangerSpace.channels.find(c => c.name === 'The Porch');
  const trespass = await call('voice_ticket', [secretRoom.id], anaJar);
  check('a room in a space you are not in is refused', !!trespass.body.Err,
    JSON.stringify(trespass.body));

  // --- two browsers, one room, real audio ---
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
           '--autoplay-policy=no-user-gesture-required'],
  });
  const open = async (handle, password) => {
    const p = await browser.newPage({ viewport: { width: 1440, height: 900 }, permissions: ['microphone'] });
    // The transport choice is decided in the client and reported on the
    // console; without this a fallback is a one-word verdict with no cause.
    p.on('console', m => {
      const t = m.text();
      if (/\[(voice|call-[a-z]+)\]/.test(t) || m.type() === 'error') console.log(`        ${handle}: ${t}`);
    });
    p.on('pageerror', e => console.log(`        ${handle} page error: ${e.message}`));
    // Suppression OFF for this suite, pinned before the app boots.
    //
    // Not a workaround — the fixture and the feature are incompatible by
    // design. Chromium's fake microphone emits a pure tone, a tone is not
    // speech, and removing what is not speech is exactly DeepFilterNet's job.
    // With suppression on (the default) every peak here reads zero, which is
    // the suppressor working and this suite's evidence disappearing. What this
    // suite proves is that the TRANSPORT carries audio; `suppression-flows.js`
    // is where the suppressor is judged, on assertions built for it.
    await p.addInitScript(() => {
      try { localStorage.setItem('hearth.suppress', 'off'); } catch {}
    });
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

  // Ana reads #general first, so we can prove joining does not move her.
  await clickText(a, 'general');
  await a.waitForTimeout(1200);

  await clickRoom(a, 'The Porch');
  await clickRoom(t, 'The Porch');
  await a.waitForTimeout(4000);

  check('ana is connected', await has(a, 'Voice connected'), (await text(a)).slice(0, 200));
  check('theo is connected', await has(t, 'Voice connected'), (await text(t)).slice(0, 200));
  check('and joining did not move ana off #general', await has(a, 'Welcome to #general'));
  check('the tray names the room she is in', await has(a, 'The Porch'));

  // The call view is a second, deliberate act — and it is where camera and
  // screen share live, because a voice room IS the video room.
  await clickRoom(a, 'The Porch');
  await a.waitForTimeout(800);
  check('clicking the room again opens the call view', await has(a, 'Back to chat'));
  check('the call view carries camera and screen share',
    (await has(a, 'Camera off')) && (await has(a, 'Share screen')));
  await clickText(a, 'Back to chat');
  await a.waitForTimeout(800);
  check('closing the call view returns her to the channel',
    await has(a, 'Welcome to #general'));
  check('and she is still in the call', await has(a, 'Voice connected'));

  const [sa, st] = await peakOver([a, t], 9000);
  console.log(`        ana : ${JSON.stringify(sa)}`);
  console.log(`        theo: ${JSON.stringify(st)}`);

  check('ana captured audio', sa.mic > 0, JSON.stringify(sa));
  check('theo captured audio', st.mic > 0, JSON.stringify(st));
  check('ana received packets from theo', sa.recv > 0, JSON.stringify(sa));
  check('theo received packets from ana', st.recv > 0, JSON.stringify(st));
  // On a loopback link with nothing lost, each side should receive about what
  // the other sent. "> 0" is satisfied by a counter that is wrong by a factor
  // of two, which is exactly what happened when the receive path was rewritten
  // and the old increment was left in place beside the new one. The window is
  // generous because the two readings are sampled at different instants.
  const near = (a, b) => Math.abs(a - b) < Math.max(b * 0.25, 20);
  check('and about as many as were sent, not double',
    near(sa.recv, st.sent) && near(st.recv, sa.sent),
    `ana recv ${sa.recv} vs theo sent ${st.sent}; theo recv ${st.recv} vs ana sent ${sa.sent}`);
  check('ana sees exactly one peer', sa.peers === 1, JSON.stringify(sa));
  check('theo sees exactly one peer', st.peers === 1, JSON.stringify(st));
  // The claim that matters: a real signal came out the far end, not just bytes.
  check('ana HEARD theo', sa.remote > 0, JSON.stringify(sa));
  check('theo HEARD ana', st.remote > 0, JSON.stringify(st));

  // The codec, checked from outside. A silent fallback to raw PCM sounds
  // identical and costs about thirty times the bandwidth, so "it works" is
  // not evidence that it works the way we think.
  check('ana is sending Opus, not raw PCM', sa.codec === 'opus', sa.codec);
  const perPacket = sa.bytes / Math.max(sa.sent, 1);
  console.log(`        ana wire: ${sa.codec}, ${perPacket.toFixed(0)} bytes/packet`
    + ` (${(perPacket * 8 * 50 / 1000).toFixed(0)} kbps at 20 ms)`);
  // 32 bytes of that is the header. Raw would be 1952.
  check('and a packet is tens of bytes, not two thousand',
    perPacket > 32 && perPacket < 400, `${perPacket} bytes/packet`);

  // The one claim no native test can make: a BROWSER carried this call over
  // QUIC datagrams, against a self-signed certificate it had to pin by hash.
  // Every assertion above — captured, sent, received, HEARD — was measured on
  // this transport, which is what makes the one below more than a handshake.
  console.log(`        transport: ${sa.transport} / ${st.transport}`);
  check('ana is on WebTransport', sa.transport === 'webtransport', sa.transport);
  check('theo is too', st.transport === 'webtransport', st.transport);
  // The send path crossing a sync/async boundary is the new thing here, and a
  // queue that overflows is silent in exactly the way a quiet room is: the
  // packets simply stop, with the meters still moving. This is the assertion
  // that would catch it.
  check('and nothing was dropped on the way out',
    sa.unsent === 0 && st.unsent === 0, `ana ${sa.unsent}, theo ${st.unsent}`);

  // The receive path is a jitter buffer now, not a decode-on-arrival. That is
  // invisible from the audio on a loopback — which is the point of asserting on
  // the depth instead: a buffer carrying delay is a buffer that is in the path.
  // Zero would mean packets are going round it.
  console.log(`        playout: ${sa.playoutMs} ms held at peak, ${sa.concealed} frames concealed`);
  check('the jitter buffer is carrying delay, so it is in the path',
    sa.playoutMs >= 20 && st.playoutMs >= 20, `ana ${sa.playoutMs} ms, theo ${st.playoutMs} ms`);
  // On loopback there is nothing to conceal. A number here on THIS link would
  // mean the buffer is mis-sequencing packets that arrived perfectly well.
  check('and inventing nothing on a link with no loss',
    sa.concealed === 0 && st.concealed === 0, `ana ${sa.concealed}, theo ${st.concealed}`);

  // The claim the audio-io crate exists to make. A device that quietly fell
  // back to a main-thread ScriptProcessorNode sounds identical on an idle test
  // machine and drops samples the moment the page is busy — so, like the
  // transport and the codec, it has to be observable rather than assumed.
  check('audio runs on its own thread, not the main one',
    sa.offMainThread && st.offMainThread, `ana ${sa.offMainThread}, theo ${st.offMainThread}`);
  // At most one, and the one is the priming transient: the output starts
  // playing the moment it has a cushion's worth, so the very first trough can
  // touch empty before the producer settles into its rhythm. A SECOND underrun
  // means the producer genuinely cannot keep up, which is the failure worth
  // catching. Not asserted as zero, because it is not reliably zero — and an
  // assertion that fails one run in four teaches people to re-run rather than
  // to look.
  check('and the output did not repeatedly run dry',
    sa.underruns <= 1 && st.underruns <= 1, `ana ${sa.underruns}, theo ${st.underruns}`);
  // A sender whose sequence restarts used to be refused forever: every packet
  // arrived, every one was rejected as late, and that listener heard silence
  // for the rest of the call while every other counter looked healthy. Nobody
  // rejoins during this test, so a resync here means the buffer is restarting
  // for a reason it should not.
  check('and no stream had to restart',
    sa.resyncs === 0 && st.resyncs === 0, `ana ${sa.resyncs}, theo ${st.resyncs}`);

  // --- mute ---
  // The button used to toggle a signal nothing read: the label said "Muted"
  // and the microphone kept transmitting. So this asserts the thing that
  // matters — the far side stops HEARING her — and, separately, that packets
  // keep flowing. Mute is not disconnection: dropping the packets would leave
  // a hole in theo's jitter buffer, and a jitter buffer conceals a hole by
  // asking the codec to invent a continuation, so a mute would arrive as
  // muttering.
  // The labelled switch is in the CALL view; the tray's is an unlabelled icon.
  await clickRoom(a, 'The Porch');
  await a.waitForTimeout(600);
  check('the mic switch is a button, not just a label',
    await clickButton(a, 'Mic on'));
  // Wait for the SWITCH to land, then for the meters to fall, before measuring
  // anything. `peakOver` takes the maximum over its window, so starting it too
  // early folds in the last moments of an unmuted microphone and reads them as
  // a mute that did not work.
  for (let i = 0; i < 40 && !(await stats(a))?.muted; i++) await a.waitForTimeout(250);
  await a.waitForTimeout(1500);
  const [ma, mt] = await peakOver([a, t], 6000);
  console.log(`        muted  ana : ${JSON.stringify(ma)}`);
  console.log(`        muted  theo: ${JSON.stringify(mt)}`);
  check('the mic switch reaches the microphone, not just the label', ma.muted, JSON.stringify(ma));
  // And the label follows. A component body runs once, so a value read at
  // build time never changes again unless its region is keyed on it — the call
  // view was, the room-list tray was not, so muting worked while the icon sat
  // there saying otherwise.
  check('and the call view shows it', await has(a, 'Muted'));
  check('muted — her own level meter reads nothing', ma.mic === 0, `${ma.mic}`);
  check('muted — theo stops hearing her', mt.remote * 4 <= st.remote,
    `theo heard ${st.remote} before, ${mt.remote} while muted`);
  check('muted — but she is still sending, so his buffer has nothing to conceal',
    mt.recv > st.recv && mt.concealed === st.concealed,
    JSON.stringify([st, mt]));
  check('and it can be pressed again', await clickButton(a, 'Muted'));
  // Wait for the SWITCH, then check the label — not a fixed timeout. The
  // region rebuilds when the signal changes, and how long that takes is the
  // scheduler's business, not something to guess at.
  for (let i = 0; i < 40 && (await stats(a))?.muted; i++) await a.waitForTimeout(250);
  await a.waitForTimeout(400);
  check('and unmuting puts it back', await has(a, 'Mic on'));
  await clickText(a, 'Back to chat');
  await a.waitForTimeout(600);

  // --- presence: who the sidebar says is in a room ---
  // The claim is that this works for people who are NOT in the call — you
  // should be able to see three people talking before deciding to join them.
  // So the third browser watches without joining anything.
  // Opened AFTER the other two are already talking, and measured from the
  // moment the shell appears: the room's occupants have to be part of the
  // first paint, not something that arrives later. They used to come from a
  // poll whose first pass ran before the space was known, so a room with
  // people in it rendered empty and filled in up to eight seconds later.
  const w = await open('wren', 'password123');
  check('a watcher is signed in and in no call', !(await has(w, 'Voice connected')));

  const shellAt = Date.now();
  let seen = null;
  while (Date.now() - shellAt < 3000) {
    seen = await occupantsUnder(w, 'The Porch');
    if (seen && seen.length) break;
    await w.waitForTimeout(50);
  }
  const occupantsAfter = Date.now() - shellAt;
  console.log(`        occupants on screen ${occupantsAfter}ms after the shell`);
  check('the room is drawn with its people, not filled in later',
    occupantsAfter < 2000, `took ${occupantsAfter}ms`);
  console.log(`        watcher sees under The Porch: ${JSON.stringify(seen)}`);
  check('the sidebar lists the people in the room, to someone not in it',
    !!seen && seen.includes('Ana Ruiz') && seen.includes('Theo Lang'), JSON.stringify(seen));
  check('and an empty room lists nobody',
    JSON.stringify(await occupantsUnder(w, 'Movie Night')) === '[]',
    JSON.stringify(await occupantsUnder(w, 'Movie Night')));

  // Moving rooms must move you, not clone you — and the watcher must be told,
  // not discover it. This used to need an eleven second wait for the next
  // poll; over the live connection it is an event, so the budget is tight
  // enough that a poll could not pass it.
  const movedAt = Date.now();
  await clickRoom(t, 'Movie Night');
  let porchNow = null, movieNow = null;
  while (Date.now() - movedAt < 4000) {
    porchNow = await occupantsUnder(w, 'The Porch');
    movieNow = await occupantsUnder(w, 'Movie Night');
    if (movieNow && movieNow.includes('Theo Lang')) break;
    await w.waitForTimeout(50);
  }
  const propagated = Date.now() - movedAt;
  console.log(`        after Theo moves — Porch: ${JSON.stringify(porchNow)}  Movie: ${JSON.stringify(movieNow)}`);
  console.log(`        propagated to another client in ${propagated}ms`);
  check('another client is told within a moment, not on a poll', propagated < 2500,
    `took ${propagated}ms`);
  check('moving rooms leaves no ghost in the old one',
    !!porchNow && !porchNow.includes('Theo Lang'), JSON.stringify(porchNow));
  check('and shows them in the new one',
    !!movieNow && movieNow.includes('Theo Lang'), JSON.stringify(movieNow));
  await clickRoom(t, 'The Porch');
  await t.waitForTimeout(3000);

  // --- leaving actually hangs up ---
  // The hang-up is the last control in the tray strip — an icon, so it is
  // found by where it lives rather than by its label.
  const hangUpAt = await a.evaluate(() => {
    const label = [...document.querySelectorAll('div,span')]
      .find(e => (e.innerText || '').trim() === 'Voice connected');
    if (!label) return null;
    // idea-ui renders an IconButton as a styled div, not a <button>, so the
    // controls are found by shape: the small square children of the strip.
    let strip = label;
    for (let i = 0; i < 4 && strip.parentElement; i++) {
      strip = strip.parentElement;
      const squares = [...strip.children].filter(c => {
        const r = c.getBoundingClientRect();
        return r.width > 16 && r.width < 40 && Math.abs(r.width - r.height) < 6;
      });
      if (squares.length >= 2) {
        const r = squares[squares.length - 1].getBoundingClientRect();
        return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
      }
    }
    return null;
  });
  check('the tray offers a way to hang up', !!hangUpAt);
  if (hangUpAt) await a.mouse.click(hangUpAt.x, hangUpAt.y);
  await a.waitForTimeout(900);
  const hangup = await a.evaluate(() => {
    const els = [...document.querySelectorAll('div,span,button')];
    return !!els.find(e => (e.innerText || '').includes('Voice connected'));
  });
  check('leaving clears the connected state', !hangup);

  // And it really hung up — the transport is torn down, not merely hidden.
  const after = await stats(a);
  check('and the session is actually torn down', after.peers === 0,
    JSON.stringify(after));
  // Theo's peer count does not decrement — it counts senders heard since he
  // joined, not who is present. What must stop is packets arriving.
  const before = (await stats(t)).recv;
  await t.waitForTimeout(2500);
  const later = (await stats(t)).recv;
  check('theo stops receiving once ana hangs up', later === before,
    `recv ${before} -> ${later}`);

  console.log(`\n${pass} passed, ${fail} failed`);
  await browser.close();
  process.exit(fail ? 1 : 0);
})();
