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
