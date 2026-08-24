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
node tools/suppression-flows.js                 # 19 assertions
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
