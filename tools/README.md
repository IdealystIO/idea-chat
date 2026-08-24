# tools

## `loop-test.js` — end-to-end voice loop check

Drives two real browser tabs through a join and asserts audio actually moved.
Chromium's fake media device emits a periodic tone, so a non-zero peak on the
far side is real evidence the whole path carried audio — not just that packets
were counted.

```bash
npm install playwright-core          # browsers come from ~/.cache/ms-playwright
cargo run -p voice-server &
(cd crates/voice-client && idealyst dev --web --local) &
node tools/loop-test.js
```

Two things this harness learned the hard way, both preserved in the script:

- **Type, don't `fill()`.** A programmatic value set updates the DOM but does
  not necessarily drive the widget's native input events, so the bound signal
  never moves and the join silently uses a stale value. It presents as both
  tabs joining under the same ID and the relay correctly suppressing everything
  as self-echo.
- **Sample peaks over a window.** The fake mic beeps periodically, so a single
  reading of either meter is zero most of the time. Poll and keep the max.

## `suppression-flows.js` — noise suppression, both places it can run

Drives two tabs through a call in each mode (off / on this device / on the
server), asserting that the setting reaches the relay, that the relay's answer
comes back, and that audio still crosses the room. See
[docs/noise-suppression.md](../docs/noise-suppression.md).

```bash
bash tools/voice-fleet.sh                       # sets VOICE_DENOISE=on
bash tools/reset-dev.sh
node tools/suppression-flows.js                 # 21 assertions
NODE_ALLOWS=0 node tools/suppression-flows.js   # against a fleet without VOICE_DENOISE
```

A third lesson for the list above, and the one that cost the most time here:

- **Chromium's fake microphone is not speech.** It emits a pure tone, and
  DeepFilterNet's entire job is to remove what is not speech — so the fixture
  that proves audio moves with suppression *off* is the fixture the neural
  modes are supposed to delete. The first version of this suite read that as
  three failures. The checks that survive are "packets are still flowing" (a
  dead pipeline sends nothing) and "the tone is attenuated" (positive evidence
  the model ran).
- **There are no `<button>` elements.** idea-ui renders a Button as a styled
  `div`, so a selector reaching for HTML semantics finds nothing and reports it
  as a missing feature. Match on exact text, scoped to the row.

## `denoise-bench.js` — what DeepFilterNet costs in this browser

The native benchmark cannot answer this: `tract` on wasm has no SIMD and no
threads, and the engine runs inline on the thread that renders the UI. Prints
milliseconds of main thread per 10 ms hop, and the headroom that leaves.

```bash
node tools/denoise-bench.js       # SECONDS=30 for a longer run
```

## `fetch-model.sh` — put the model where the web client can reach it

Copies `DeepFilterNet3_onnx.tar.gz` out of cargo's git checkout into
`crates/hearth/assets/`, served at `/assets/…`. Not committed: it is 8 MB that
already exists on any machine that has built the workspace, and copying from
the checkout guarantees the model matches the engine compiled against it.

## `presence-flows.js` — who the server SAYS is in a room

Drives the live socket directly and counts frames, because the bug it exists
for was invisible in the database: one row per person, `joined_at` never
moving, `seen_at` renewed every 15 s — while the server announced a *join* on
every heartbeat. Everyone in the room heard the connect tone every fifteen
seconds and the speaker jumped to the bottom of the sidebar each time.

```bash
bash tools/reset-dev.sh
node tools/presence-flows.js       # 5 assertions
```

The lesson, for the list above: **assert on the event stream, not the table.**
A reducer's clients see events; a consistent table proves nothing about them.

## `volume-flows.js` — how loud everyone else is

The listener's own gain, per sender. Asserts the two things that can be wrong
in ways nothing else would notice:

- **Silence for a NAMED speaker.** A listener picks a volume against a person;
  the audio path applies it against a four-byte sender number derived from that
  person's account id. A wrong derivation turns down somebody else, which
  "quieter than before" cannot catch.
- **Still connected.** Every silence check is paired with a packet count. A
  volume control implemented by dropping packets, muting a track, or closing a
  sink would pass "it got quieter" and fail this.
- **Unmuting returns the LEVEL.** Ana is set to half volume before she is
  muted, so coming back at full is a failure. A mute implemented by writing
  zero into the slider passes every check that only asks "is there sound
  again", and loses the level the first time anyone uses it.

```bash
bash tools/reset-dev.sh
node tools/volume-flows.js       # 18 assertions, two browsers
```

The refresh check found a **relay** bug, not a volume one, and it is the reason
that check is here. A participant id is derived from an account and so survives
a reload, and the browser opens the new socket before the node has reaped the
old — so one id is claimed twice for a moment. The node kept a *set* of ids and
dropped the room when it emptied, so the stale claims draining deleted the room
out from under the people still in it; the next person to attach built a fresh
one under the same name and was inaudible in both directions. One name, two
rooms. Every other voice suite joins fresh and leaves, so none of them could
see it.

The menu interactions retry, and the reason is worth keeping: the roster and
the room list are live regions, and both used to rebuild several times a second
during a call — once per VAD event — taking any open menu with them. That is
fixed (the highlight repaints from inside a style closure now, and the live
reducer only bumps the revision for changes something keys on), but a rebuild
landing between two clicks is still not a failure of the thing under test.

## `vad-flows.js` — who is talking

Runs against `fixtures/speech-like.wav` rather than Chromium's default fake
device, and that substitution is the whole point. The default device is a pure
**tone**; a tone is not speech; removing what is not speech is exactly what the
suppressor does. A VAD suite built on it measures the suppressor and reports
the indicator as broken.

```bash
bash tools/reset-dev.sh
node tools/vad-flows.js       # 7 assertions, two browsers
```

The indicator has been wrong twice, in opposite directions, and neither showed
up anywhere else:

- **stuck on** — the model's verdict was computed against an input measurement
  that was almost always zero. The microphone delivers ~12 frames a second and
  the model emits 100 blocks, so an accumulator reset per block was empty seven
  times in eight, and a zero denominator reads as "the model added 100 dB".
- **stuck off** — the fixture was the tone described above.

So the assertions are *bounds*, not targets: the claim is that the indicator
MOVES with the audio, not that it agrees with a synthetic fixture to a
percentage point.
