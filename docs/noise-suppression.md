# Noise suppression: two places to put the same model

Hearth can run **DeepFilterNet 3** over a voice stream in one of two places,
chosen per client in Settings → Audio & video:

```
  mic ──▶ [browser AEC/NS] ──▶ [DFN3] ──▶ wire ──▶ [DFN3] ──▶ peers
           always on           "This device"        "Server"
```

The DSP is identical — the same model, the same tuning, the same levelling
afterwards. Only *who pays* changes, and that turns out to be the whole
question.

`voice-dsp` names the modes and holds the levelling; `voice-client/src/suppress.rs`
is the client stage; `voice-server/src/suppress.rs` is the relay stage.

## The numbers

Measured on this machine — an 8-core Ryzen 7 7700X — with
`cargo run --release -p voice-server --bin dfn-bench` and
`node tools/denoise-bench.js`. A **hop** is 10 ms of audio, so anything under
10 ms per hop keeps up with the microphone.

| | native (relay) | wasm (browser) |
| --- | --- | --- |
| per 10 ms hop | **0.11 ms** | **0.61 ms** |
| real-time factor | 0.011 | 0.061 |
| headroom | 90× | 16× |
| model build, per call | 75 ms | 244 ms |

Concurrency on the relay, same benchmark:

```
 2 streams   130.7x real time aggregate  (0.015 rtf each)
 4 streams   277.2x real time aggregate  (0.014 rtf each)
 8 streams   443.6x real time aggregate  (0.018 rtf each)
16 streams   275.5x real time aggregate  (0.058 rtf each)
```

Sixteen streams saturate eight physical cores and *still* run 17× faster than
real time each. Suppression is not what a relay runs out of.

The surprise is how little wasm costs: 5.5× native, not the 20× that no SIMD
and no threads might suggest. 16× headroom means a phone at a quarter of this
desktop's speed still has four times the budget it needs.

## What each mode actually costs

**On this device** — the wire carries clean audio, so the relay stays a relay
and the noise costs no bandwidth because it never leaves the room it was
recorded in. The ~30 ms of algorithmic latency lands at the edge of the path
rather than in its middle. Against that:

- **The wasm bundle triples.** Release, measured at the same build stage with
  and without the inference engine:

  | | post-bindgen | post-split |
  | --- | --- | --- |
  | without | 7.84 MB | **3.43 MB** |
  | with | 23.78 MB | **10.23 MB** |

  That is +6.8 MB on every first visit, including the visitors who never open a
  voice room, to carry a feature that is off by default.
- **An 8 MB model download** on the first call that uses it, then cached.
  Deliberately not compiled into the wasm — see `tools/fetch-model.sh`.
- **One inference per 10 ms on the main thread**, because wasm has no threads
  and the SDK's web backend runs the engine inline in the subscribe callback.
  At 0.61 ms that is fine; the way out if it ever is not is an AudioWorklet.

**On the server** — nothing to download, no battery burned on a phone, and one
model instance per *sender* rather than per listener. Against that: the noise
has already been paid for in bandwidth by the time it is removed, and the 30 ms
of latency lands mid-path. It also spends a shared machine's CPU, which is why
it is gated (below).

Neither is strictly better. Server-side is the one to reach for by default —
the bundle cost of the other is real and falls on everyone — but on-device is
what scales to a large deployment, because it is the only version whose cost
does not grow with the number of people talking.

## Why it is a mode and not two switches

Neural suppression twice is worse than once. DeepFilterNet is trained on *noisy
speech*; feed it speech that has already been through a suppressor and it works
on a signal unlike anything in its training set, chewing on the first pass's
artefacts. Two booleans would let the UI express "both", and someone would
eventually tick both boxes and conclude the feature sounds bad. `Mode` cannot
express it.

The same argument settles the browser's own suppression: `Client` and `Server`
both turn WebRTC noise suppression **off** and leave echo cancellation **on**.
AEC is a different job — it needs the far-end reference signal, which only the
browser has, and DeepFilterNet explicitly does not do it.

## Changing your mind mid-call

Picking a mode applies to the call you are already in, without rejoining. That
needs two things, and the second is easy to miss:

```text
  mic --> [ chain ] --> wire --> packetiser --> relay
           swappable    stable
```

**A stable wire.** The packetiser subscribes once, for the life of the call, to
a stream that whichever chain is installed writes into. Re-subscribing it
instead would restart its sequence numbering, which every receiver reads as a
gap. Swapping means: build the replacement while the old one is still feeding
the wire (the model takes ~250 ms and the call must not go silent), then drop
the old and install the new with nothing in between.

**A new microphone.** Whether the *browser* is suppressing is fixed when the
track is opened, so a swap that reused the running capture would leave WebRTC's
suppressor stacked underneath DeepFilterNet — the one combination this whole
design exists to prevent. So the swap reopens the device with the flags the new
mode wants. It does not prompt; the permission has already been granted.

Server-side is the exception, and the settings page says so: the node decides
when the relay socket opens, and revisiting that means opening another one.

One trap worth recording, because it is the third time it has cost time in this
project: **`Signal::set` stages.** The guard that discards a superseded swap
originally compared `app.suppress.get()` against the mode being applied — and
the caller sets that signal on the line before, so the read returned the
previous value and the guard threw away the swap it was meant to protect. The
setting silently did nothing, with no error anywhere. It is a thread-local
generation counter now, bumped synchronously before anything can await.

## Levelling

Suppression decides what is noise. Levelling decides how loud what is left
should be, and a call is a mix: one person at -35 dBFS and another at -8 dBFS
is not fixed by removing either one's hiss. `voice_dsp::Leveler` targets
-20 dBFS RMS with a +18/-12 dB range, a 15 ms attack, a 400 ms release, and a
-1 dBFS peak limiter.

The load-bearing part is the **gate**, and it needs two detectors rather than
one. An RMS-targeting gain applied to silence asks the impossible — no gain
makes zero reach -20 dBFS — so the gain climbs to its limit and the next
syllable arrives like a slammed door. Freezing the gain below a threshold is
not enough on its own: the level detector is deliberately slow, so at the
moment speech stops it spends hundreds of milliseconds *decaying* toward that
threshold, and every sample of the decay reports a quieter speaker, which is
exactly the input that makes a gain climb. So a fast detector (5 ms) answers
"is anyone speaking right now" and freezes the slow one before it can decay
into a lie.

This is worse after neural suppression than before it: the suppressor's output
between words is far closer to true digital silence than any microphone's.

## The gate on server-side

A node relays unless told otherwise. `VOICE_DENOISE=on` permits server-side
suppression; without it a client that asks is **told so** on the socket
(`info:suppress=off`) and gets an ordinary relay.

Cheap per stream is not the same as free, and a client that could spend a
shared machine's CPU by asking is a lever worth keeping a hand on. What the
client asked for and what it got are separate values everywhere in the code —
presenting a refusal as a grant is how "why does this still sound noisy"
becomes unanswerable.

The node says which mode it is in at startup, next to the ticket mode, for the
same reason: a capacity decision visible only in an environment variable is one
nobody checks until the machine is already saturated.

## Testing it

```console
bash tools/voice-fleet.sh                 # sets VOICE_DENOISE=on
node tools/suppression-flows.js           # 19 assertions, both modes, two browsers
NODE_ALLOWS=0 node tools/suppression-flows.js   # against a fleet started with VOICE_DENOISE=off
node tools/denoise-bench.js               # what the model costs in this browser
cargo run --release -p voice-server --bin dfn-bench
```

One trap worth knowing about, because it looks like a failure and is not:
Chromium's fake microphone emits a periodic **pure tone**, and a sine is not
speech, so DeepFilterNet removes almost all of it. The fixture that proves
audio moves with suppression off is precisely the fixture the neural modes are
supposed to delete. `suppression-flows.js` therefore checks that packets keep
flowing *and* that the tone is attenuated, rather than that the far side hears
a loud signal — the attenuation is the positive evidence that the model ran.

## Known gaps

- **The bundle cost is unmitigated.** The inference engine is compiled into
  every build. A cargo feature that drops it, or a lazily-loaded `wasm-split`
  chunk, would make on-device suppression cost nothing to the visitors who
  never use it. The latter is the better answer and the larger lift.
- **The shippable bundle size is unmeasured.** `idealyst build --web --release`
  cannot finish here: `wasm-opt` needs `binaryen`, which is not installed. Every
  size above is post-split, pre-opt, with debug info still in — real for
  comparison, pessimistic in absolute terms.
- **`DeepFilterNet3_ll_onnx`** — the low-latency variant, `df_lookahead = 0`
  and `conv_lookahead = 0` — would cut algorithmic latency from ~30 ms to
  ~10 ms. It is 39 MB unpacked against 8.6 MB and roughly 4× the compute
  (`emb_hidden_dim` 512 vs 256), which makes it plausible on the relay and not
  on the web. Untried.
- **Server-side allocates per packet.** `Packet::to_f32` builds a `Vec` 100
  times a second per talker. Irrelevant next to the inference, but it is the
  obvious thing to remove first if it ever matters.
