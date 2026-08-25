# Audio off the main thread

`crates/audio-io` is device I/O — a microphone in, a speaker out — running on
the audio rendering thread rather than the main one.

It does **not** replace `media_stream::AudioStream`. That type is the whole
point of the media SDK: one way to move audio between parts of an app whatever
the platform underneath. A second currency would fork the ecosystem for nothing.
What this replaces is the *backend*: where samples are produced and consumed.

## The one thing it fixes

Every native backend already delivers audio on an audio thread. `cpal` does,
`AudioRecord` does, and the `Send` bound on a capture callback says so out loud.

The web backend was the exception. `ScriptProcessorNode` runs its callback on
the **main thread**, sharing a deadline with layout, garbage collection and
every re-render the app does. Miss that deadline and captured audio is not late,
it is *gone*; on the output side you get a gap.

An `AudioWorklet` runs on the audio rendering thread at a fixed 128-frame
quantum. A busy main thread can no longer cost you samples. That is a
**robustness** argument before it is a latency one, and it is why this was worth
doing even though the latency win is modest.

## What is and is not off the main thread

| | where it runs |
| --- | --- |
| capture I/O | audio thread |
| output I/O | audio thread |
| the processing graph | **main thread, still** |

The third row is the honest one. Moving Rust into an `AudioWorkletGlobalScope`
means a second wasm module with its own linear memory — a build-tooling project,
not an audio one. It does *not* need `SharedArrayBuffer` or COOP/COEP, which is
the thing most often assumed about it; the bytes go in through
`processorOptions` and instantiate there.

What matters is that the API does not stand in the way of that day. See below.

## Why a Stage is a value and not a closure

The obvious API for "do something to each block" is `FnMut(&mut [f32])`. It is
also the one thing that would permanently nail processing to whichever thread
built it: a closure captures its environment, and an environment cannot be
relocated.

A [`Stage`] is a value. It owns its state, it is `Send`, and it is handed to the
backend rather than called by the caller. So the backend decides where the graph
runs and can decide differently later **with no caller changing** — which is the
entire reason for the shape.

The rule that follows: **audio never crosses a thread boundary; only numbers
do.** A stage reporting a level, or being told about a gain change, does it
through an atomic it was built with — the same seam the app already uses to get
meters into the UI. A stage that reaches for an `Rc`, a signal, or a channel back
to the UI has quietly re-attached itself to one thread, and `Send` is what stops
it compiling.

## Latency is an intent, not a number

```rust
pub enum Latency { Relaxed, Interactive, Frames(u32) }
```

`Interactive` is 256 frames on an `AudioWorklet` and would be 1024 on a
`ScriptProcessorNode`; on `cpal` it is the device minimum, on Android
`getMinBufferSize()`, on iOS `setPreferredIOBufferDuration`. A caller cannot know
which backend it got, so asking for a *number* would be writing one platform's
idiom into an API that has to serve all of them. Asking for the intent means the
answer improves when the backend does.

Every backend declares its own `Limits` and one tested resolver turns an intent
into a block it will accept — adjusting rather than refusing, because failing to
open a device over a latency preference is never the trade the caller was making.
What they actually got comes back in `Opened`.

`Opened::off_main_thread` exists so that "we took it off the main thread" is
**checkable** rather than asserted. `tools/voice-flows.js` asserts it.

## No silent fallback

If the worklet module will not load, opening fails and says why. It does not
quietly drop back to a `ScriptProcessorNode`: running on the main thread is the
exact failure this crate exists to remove, and a fallback that did it silently
would be indistinguishable from success until someone's call started glitching
under load. An app that would rather have main-thread audio than no audio still
has `microphone` and `media-stream`.

## One AudioContext for the process

Not one per stream, which is what `media-stream`'s bridge does. Browsers cap
concurrent `AudioContext`s — Chrome has historically allowed about six — so a
per-stream context is a room that stops working at the seventh participant. This
was a latent bug rather than an observed one; it is fixed by construction here.

## The budget

| stage | frames | at 48 kHz |
| --- | --- | --- |
| capture block | 256 | 5.3 ms |
| jitter buffer, measured | — | 20.0 ms |
| output cushion | 1920 | 40.0 ms |
| output render quantum | 128 | 2.7 ms |
| | | **~68 ms** |

### The cushion is the interesting row, and it is a real cost

An output's cushion has to exceed the **producer's burst size**, not equal it.
The producer here is the jitter buffer, driven by packet arrivals: it hands over
a packet's worth of audio at an instant and then nothing for ~20 ms. A cushion
equal to that burst drains to exactly zero every cycle — measured, at one
underrun per output. Two bursts keeps the trough a burst above the floor, and
underruns go to zero.

So this pays for smoothing twice: 20 ms in the jitter buffer and 40 ms here. The
architectural fix is for the buffer to be **pulled by the audio clock** rather
than by arrivals — which means living in the worklet beside the output, which is
the same project as moving the graph there. Until then, 68 ms is the honest
number and 48 ms is what that project is worth.

### A correction to the previous measurement

An earlier commit recorded ~63 ms after shrinking `media-stream`'s playout to
512 frames of render block and 512 of cushion. That number was nominal, not
achieved: a 512-frame cushion is 10.7 ms against a 20 ms burst producer, so it
must have been running dry regularly.

Nothing reported it, and that is the point. `audio_web_bridge` fills an underrun
with silence, never un-primes, and keeps no counter — so a starved output and a
quiet room are the same thing from outside. The `underruns` counter here is what
makes that class of bug visible at all, and the first thing it did was find one I
had introduced myself.

## A microphone must not be wired to the speakers

The first version of the capture node had one output connected to
`ctx.destination()`, on the reasoning that a node with **no** outputs might not
be pulled by the implementation. Its silence then depended on the processor
never writing to its output — an implementation detail, in a graph that also
carries every peer's audio to that same destination, with a live microphone at
the other end of it.

The spec provides a sink node exactly so this is unnecessary: an
`AudioWorkletNode` with `numberOfOutputs: 0` is processed for as long as it has
an active input, which a microphone is. Capture is one now and connects to
nothing. Verified rather than assumed — the browser suite still measures a mic
peak of 99 and 718 packets sent.

## What a model in the path costs, measured

Running DeepFilterNet on the main thread drops packets. Same fixture, same
duration, only the suppression mode changed:

| mode | unsent | concealed | underruns |
| --- | --- | --- | --- |
| off | 0 | 0 | 0 |
| client (DeepFilterNet) | 5 | 5 | 0 |
| server | 0 | 0 | 1 |

Every unsent packet is a 20 ms hole in what the far side hears. Roughly 1% here,
which is occasional clicks rather than a broken call — but it is a floor set by
where the model runs, not by anything tunable. Raising the capture block from
256 to 1024 frames changed it to 4 and 2, which is noise: **the block size is
not the cause, the main thread is.** That is the same argument as everything
above, arriving with a number attached.

## Not done

- **The graph still runs on the main thread on web.** The API is ready; the
  build tooling is not.
- **No native backend.** Every entry point returns `Error::Unsupported` naming
  what would fix it. Capture is the easy one — `cpal` already delivers on an
  audio thread. Output is the `audio-output` gap, which has never existed
  anywhere in this tree.
- **The jitter buffer is still pulled by arrivals**, which is what the 40 ms
  cushion is paying for.
