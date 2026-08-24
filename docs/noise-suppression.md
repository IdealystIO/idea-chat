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

## Asking the model what it thought

DeepFilterNet does not report a confidence, but it attenuates what it believes
is noise and passes what it believes is speech — so **the ratio of what came out
to what went in is its judgement on that block**, in dB, for free. A hop that
came out 20 dB down was called noise; one that came out unchanged was called
speech, at whatever volume.

That is a far better signal than level, and it is what drives the last stage.
The alternative — a tracked noise floor — could not be made to work, and not
because it was badly tuned: a floor that learns during speech climbs until it
swallows the speaker, a floor that only learns while the expander is closed can
never open, and a floor primed from the first sample is primed from whatever
that sample happened to be. Every variant traded ducking quiet speech against
not working, because *a quietly-spoken word and a residual chirp are the same
level* and level is all that measurement has.

So `voice_dsp::Expander` takes the verdict, and where there is no verdict it
passes audio through untouched rather than guessing.

Two details that are easy to get wrong:

- **Alignment.** The model has ~30 ms of latency, so the block coming out now
  corresponds to audio that went in three hops ago. Comparing them as they
  arrive puts a word's onset against the silence before it and reads a huge
  attenuation exactly where speech is starting. `voice_dsp::Alignment` is a
  delay line for the input measurement, sized in hops.
- **The lookahead leads by more than one hop.** A word's first hop can still
  read as rejected, because the model's own lookahead means its output has not
  caught up yet. The expander's 12 ms delay is longer than one 10 ms hop for
  exactly that reason: the audio leaving is from the previous block while the
  verdict being applied is from the current one.

### Both sides must be measured the same way

The model emits a block every 10 ms. A microphone does not — on this machine it
delivers about twelve frames a second, roughly 85 ms each. So "the input energy
for this hop" is a quantity that mostly does not exist.

Accumulating input between output blocks and resetting each time is the obvious
answer and is wrong: seven hops in eight then find an empty accumulator, read
zero input, and conclude the model *added* 100 dB. The symptoms were a speaking
indicator that never went out and a residual stage that never once ran, both
while reporting that everything was fine — the ratio was being computed, it was
just meaningless.

`voice_dsp::Meter` carries a level across the gaps with a real time constant,
and takes the duration of each block, so it settles identically whether it is
fed 85 ms frames or 10 ms ones. Both sides run one with the same constant. The
ratio is clamped to ±40 dB as well, because a division that meant nothing
should not arrive downstream as a confident number.

The same verdict is the **voice-activity indicator** — the sidebar highlights
your own row while you are talking, and everyone else's row from what their
client broadcast (`Event::VoiceSpeaking`). It is a neural speech decision rather than
an energy threshold, and it is free, having already been computed. Without a
model in the chain the indicator falls back to level, because a speaking light
that only works with suppression on is a speaking light nobody trusts.

## Every stage switches off on its own

| stage | control | default |
| --- | --- | --- |
| neural model | Off / This device / Server | **This device** |
| keystrokes and knocks | Off / Light / Standard / Strong | Standard |
| residual hiss | Off / Light / Standard / Strong | Standard |
| even out volume | On / Off | On |

On by default. Suppression is what people expect a call to do, and a feature
that has to be found in Settings before a call sounds right is a feature most
people never get. The bundle cost of that default is paid on the first voice
join rather than on every page load: the lazy boundary now mounts only when
there is a room to apply it to.

Presets rather than raw decibels, because the numbers underneath are not
independent — a declicker's threshold and its depth have to move together or
the setting means something different at each end of its range.

Independently switchable is the point rather than a nicety: when a call sounds
wrong the question is always *which stage*, and the only reliable way to answer
it is to turn them off one at a time and listen. Changing any of them rebuilds
the capture chain under the running call.

The choices are remembered per device (`crate::prefs`), which is the right
scope: whether to spend a machine's CPU on a model depends on that machine, and
how hard to chase keystrokes depends on that keyboard. Account-level
preferences — who may notify you, read receipts — are deliberately not stored
this way and should be server-side when they are done properly.

## Transients — the keystroke half

DeepFilterNet is a speech *enhancement* model and it is very good at what it was
trained on: noise with structure that persists. Fans, traffic, room tone. Its
weakness is the other kind — a key bottoming out, a mug on a desk — because an
impulse occupies one or two of its 10 ms hops and looks, inside that hop, a
great deal like the onset of a word. This is the gap between DeepFilterNet and
what Discord gets from Krisp, and it is the complaint that actually reaches
users, because a keystroke is *louder* than the hum it sits on.

`voice_dsp::Declicker` runs between the model and the leveller, on the property
the model does not use: **how a sound starts, and whether it goes on**.

Three measurements, all of which must agree:

- **Brightness** — energy in the first difference against energy in the signal.
  A click is almost all edge; a vowel is almost none. Separates clicks from
  voiced speech and does nothing at all for `/s/`.
- **Onset ratio** — that edge energy against a slowly-tracked estimate of what
  it has recently been.
- **Persistence**, implicitly. The floor rises over ~120 ms, so anything that
  lasts is absorbed into it and stops being detected. A 10 ms click is long gone
  before the floor moves; a 250 ms fricative outlives the suspicion it starts
  with. That constant *is* the definition of "transient".

### Lookahead is not optional

A click's loudest sample is its first one, so a detector watching the sample it
is about to output has already let that one through. The first version
attenuated a keystroke by **0.8 dB** while correctly detecting every one. The
signal is delayed 3 ms and the detector runs on the undelayed copy, which took
the same fixture to **7.8 dB** — enough that the keystroke no longer stands
above the voice, which is the threshold that matters by ear.

### What it cannot do

It ducks; it does not un-mix. A broadband gain reduction removes a keystroke
cleanly when it lands between words and dulls ~20 ms of speech when it lands on
top of one. That is the honest limit of the time domain, and it is why the
reduction is capped at 18 dB rather than total: a capped duck on a plosive is a
slightly soft `/t/`, an uncapped one is a hole.

Removing a click from *underneath* a vowel needs the spectral domain — an FFT
and a subtraction per bin. That is the next thing to build if this is not
enough, and a much larger one.

### Chain latency

| stage | added |
| --- | --- |
| DeepFilterNet 3 | ~30 ms |
| declicker lookahead | 3 ms |
| leveller | 0 |
| expander lookahead | 12 ms |
| **total one-way** | **~45 ms** |

Against a mouth-to-ear budget of about 150 ms, which leaves room for the
network. Each stage's lookahead is the price of that stage being correct rather
than merely present — both were built without one first, and both were
measurably useless that way.

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

## Listener volume — the only stage nobody else hears

Everything above is a decision the *speaker* makes on everyone's behalf: what
leaves the microphone is what the room gets. `voice_dsp::Fader` is the other
half — one gain per sender, applied on the listening machine, after the wire.
Nothing is sent, the speaker is not told, and two people in the same room can
have completely different mixes.

Two controls, and the difference matters:

| | what it answers | where |
|---|---|---|
| **Master** | everyone is too loud | Settings → Audio & video |
| **Per person** | everyone is fine except this one | the person's menu |

They **multiply**, which is what makes the master more than a volume: turning
somebody *up* is only possible if there is room above where they already are.
That is why the shipped default is 70% rather than 100% — at unity the
per-person control can only ever go down. Slider position maps to gain as
`position²`, so 100% is exactly "as it arrived" and the bottom of the travel is
not a cliff.

Mute-for-me is this control at zero, not a second mechanism. Two paths to
silence eventually disagree about which one wins, and the report is "unmute is
broken" from somebody whose slider was still down.

### Why not the `<audio>` element's volume

It was there first, and it is wrong twice over. It cannot exceed unity, so
"turn this person up" is inexpressible — the one case people actually reach for
the control. And it exists only on web, so the native path would need a second
implementation with different behaviour. Scaling the PCM before the writer has
neither problem, costs one multiply on a path already touching every sample,
and is testable without a browser.

The fader **ramps** (20 ms) because a gain that changes between blocks is a step
discontinuity, which is a click — the exact artefact the rest of this chain
spends its time removing. It **limits** (-1 dBFS) because levelling already
used the headroom, so any boost at all would clip.

### The mapping is the risky part

A listener chooses a volume against a *person*; the audio path applies it
against the four-byte sender number in the packet header. That derivation lives
in `voice_proto::participant_id` and is called by both the server minting
tickets and the client setting gains. A second spelling of it would present as
a slider that turns down somebody else — which is why `volume-flows.js` asserts
silence for a *named* speaker rather than just "quieter than before".

### One thing this found

The roster is a live region keyed on a revision counter, and every VAD
start/stop bumped it — so during a call the people list rebuilt several times a
second, taking any open menu with it. A menu that cannot survive a call is a
menu whose slider cannot be dragged during one. `livewire::put` now bumps the
revision only for changes something actually keys on; `speaking_now` has its own
signal and its own subscribers.

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

## The bundle cost, and where it went

The inference engine is ~6.8 MB of release wasm. Suppression is off by default,
so every first visit paid for it — including the visits that never open a voice
room.

It lives in a lazily-loaded chunk now, behind a `#[lazy_component]` boundary
(`views/suppressor.rs`). Debug build, before and after:

| | main bundle | chunk |
| --- | --- | --- |
| before | 22.1 MB | — |
| after | **8.6 MB** | 14.9 MB, on demand |

The boundary is a component that draws nothing — it is a lifecycle hook wearing
a component's clothes, because a component is the unit the splitter works on.
It mounts whenever the setting is "This device", call or no call, so choosing
the option starts the chunk fetch and the model download immediately rather
than making your first call the one that waits for 8 MB of weights.

**The rule that makes it work** is reachability: `wasm-split` moves code that
can be reached *only* from a lazy body. There is exactly one path from the app
into the engine — `voice::engage`, `voice::warm` and
`voice::install_denoise_probe`, all called only from that body. A second caller
anywhere pulls the whole engine back into the main bundle, and nothing errors;
the chunk just stops being worth having. Two consequences worth knowing:

- `session::join` no longer builds the neural chain even when suppression is on.
  A call starts on the direct chain and the chunk swaps in, which costs the
  first ~250 ms of a call.
- The wasm benchmark ships inside the chunk too, so `tools/denoise-bench.js`
  has to switch the setting on before the probe exists.

`suppression-flows.js` asserts both halves: no chunk request on an ordinary
visit, exactly one after the setting is chosen.

## Testing it

```console
bash tools/voice-fleet.sh                 # sets VOICE_DENOISE=on
node tools/suppression-flows.js           # 21 assertions, both modes, two browsers
NODE_ALLOWS=0 node tools/suppression-flows.js   # against a fleet started with VOICE_DENOISE=off
node tools/volume-flows.js                # 9 assertions, listener-side gain
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
