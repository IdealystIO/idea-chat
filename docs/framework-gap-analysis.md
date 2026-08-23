# idea-chat — framework gap analysis

**Question:** can an open-source Discord alternative (chat + voice + video, cross-platform,
centralized server) be built with only what the idealyst SDK provides?

**Answer:** **text chat — yes, comprehensively. Voice and video — no, not today.**
The blockers are all in one place: there is no realtime media *transport* and no live-audio
*output*. Everything else on Discord's surface area has a first-class SDK path.

Audited against `idealyst-native` @ `58da3e0a` (2026-08-23), by reading the crate sources,
not just the MCP catalog.

---

## 1. Green — covered by existing SDKs

| Discord feature | idealyst path |
| --- | --- |
| Guilds / channels / DMs / message CRUD | `server` — `#[server] async fn`, typed errors, `use_state`/`use_request_header` extractors |
| Realtime message fan-out | `#[subscription]` (server→client stream over WS) + `pubsub` typed `Topic<T>`; redis / postgres LISTEN-NOTIFY backends give cross-instance fan-out |
| Typing indicators, presence, read receipts, reactions | `#[channel]` — `Socket<In, Out>`, one shared message enum, no protocol to keep in sync |
| Message scrollback | `virtualizer` primitive (real `UICollectionView` / `RecyclerView` / `NSCollectionView` recycling) + `virtualized::list` |
| Composer | `text_area` primitive; `Autocomplete` (idea-ui) for `@mentions` / `#channels`; `AnchoredOverlay` for the popup |
| Attachments | `file-picker` (lazily-streamed `PickedFile`, never loads whole file into RAM), `files`, `file-export`, `share` |
| Message formatting | `markdown` SDK (CommonMark/GFM); `codeblock` for fenced code |
| Avatars / badges / tabs / modals / toasts / menus | idea-ui (`Avatar`, `Badge`, `Tabs`, `Modal`, `Toast`, `Menu`, `Popover`, `Chip`, `Skeleton`, …) |
| App shell — server rail, channel list, member list | `swap-navigator` + `stack-navigator` + idea-ui-nav `AppShell` / `Drawer` / `TabBar` |
| Auth, sessions, CSRF, rate limiting | `server-kit` — `Auth<T>` extractor (real 401), `Sessions` on the `cache` trait, `csrf_guard`, `rate_limit` + `RedisLimitStore` |
| Token storage / biometric unlock | `credentials` (Keychain / Keystore), `biometrics` |
| Notifications | `notifications` — local + scheduled, `push_token()` seam |
| Offline history + queued sends | `sync` — partitions, durable outbox, per-entity `Merge` policy, reactive `Signal` per partition |
| Background work (transcode, push fan-out, cleanup) | `jobs` — `#[job]`, retries/backoff/DLQ, redis/postgres/sqs |
| Invite links | `deep-link` (`initial_link()` / `on_link()`) |
| Reconnect UX | `connectivity` (`watch()` online/offline + transport) |
| Channel reorder / drag-to-folder | `dnd` (`DragContext` / `Draggable` / `Droppable`) |
| i18n, theming, dark mode | `i18n` (compile-checked translations, live locale switch), `idea-theme` |
| Emails (verify, password reset) | `email` — renders an idealyst `#[component]` to email-safe HTML |
| Boot config for the whole server tier | `idealyst-config` — one `configure_all().await` from `idealyst.toml` |
| Camera capture | `camera` → `MediaStream` |
| Screen share capture | `screen-recorder` → `MediaStream` (ReplayKit / ScreenCaptureKit / MediaProjection / getDisplayMedia) |
| Mic capture | `microphone` → `AudioStream` (interleaved f32 PCM) |
| Noise suppression | `denoise` — DeepFilterNet 3 as an `AudioStream → AudioStream` transformer. Genuinely ahead of most stacks. |
| Video layout composition | `video-compose` — GPU pipeline, crop / watermark / `overlay_stream` PiP → a new `MediaStream` |

Notably, **both stream types have a producer half** — `MediaStream::new() -> (MediaStream, FrameWriter)`
and `AudioStream::new() -> (AudioStream, AudioWriter)`. So a *remote* participant's decoded frames
can be pushed into a synthetic stream and handed to the `video` primitive (`VideoProps::source`
accepts `stream(...)`). The video display path for a remote peer already exists — what's missing
is everything that produces those frames from the network.

---

## 2. Red — the actual blockers

### B1. No realtime media transport (no WebRTC, no RTP)
Zero occurrences of WebRTC / ICE / STUN / TURN / RTP / SRTP anywhere in the workspace.
`net` gives HTTP, WebSocket, and SSE. `server`'s `Socket<In, Out>` frames are **JSON text on send**
(binary accepted only on recv, for a future postcard codec) — so the framework's own realtime layer
can't carry media efficiently either. You'd drop to raw `net::WebSocket` (which does support
`WsMessage::Binary`) and then hand-build: packetization, sequencing, a jitter buffer, loss
concealment, NACK/FEC, and congestion control. Centralizing through your own SFU removes NAT
traversal from the problem but not any of the above.

### B2. No live-audio output — **the sharpest gap**
`audio` plays a *prepared* sound: `AudioSource::{Bytes, Path, Url}` → `AVAudioPlayer` /
`MediaPlayer` / `HTMLAudioElement` / GStreamer. It cannot consume an `AudioStream`.
Grepping every `AudioStream` consumer in the repo: `media-writer` (writes an .mp4),
`denoise` (transforms), `video-decode` (produces). **Nothing sends an `AudioStream` to the speakers.**
The media guide even calls it "a future audio-playback layer."

Consequence: you can capture and record a voice call, but you cannot *hear* the other person.
There's also no mixer — N participants need summing with per-stream gain before the sink.

### B3. No codec layer for realtime
`media-writer` encodes to a **file** (AVAssetWriter / MediaCodec+MediaMuxer / MediaRecorder) — a
muxed container, not a low-latency packet stream. `video-decode` decodes a **file or URL**, not a
network byte stream. No Opus anywhere. So neither direction of a realtime codec path exists.

### B4. No acoustic echo cancellation on native
`microphone`'s `echo_cancellation` / `noise_suppression` / `auto_gain_control` config fields are
documented **web-only** (they map to `getUserMedia` constraints); Android is an explicit no-op and
there's no Apple `VoiceProcessingIO` path. `denoise`'s README states plainly it is *not* AEC —
"that needs a far-end reference signal and is a different SDK." Without AEC, native speakerphone
calls echo. Web gets it free from the browser.

### B5. No background-audio / call lifecycle
`audio`'s README: background audio is out of scope — it needs iOS `UIBackgroundModes: audio` plus
an active `AVAudioSession` category, and an Android foreground service. There's no CallKit /
ConnectionService integration, so: no lock-screen call UI, no system ring, and iOS suspends the
call the moment the app backgrounds.

---

## 3. Amber — real work, but no framework change required

- **Server-side push delivery.** `push_token()` hands you the APNs token / FCM token / web-push
  subscription; sending is explicitly your server's job. Buildable on `net` + `jobs`, but you write
  APNs JWT signing and FCM v1 yourself. Also: `push_token()` returns `NotSupported` until the *host*
  wires `registerForRemoteNotifications` / an FCM project / a service worker.
- **File upload.** `net`'s `IntoBody` covers bytes / JSON / text — no multipart helper, no streaming
  request body, no upload progress callback. Fine for avatars and screenshots; a 50 MB attachment
  means manual chunking or a presigned direct-to-S3 upload.
- **Chat scrollback ergonomics.** `virtualizer` has `scroll_to_index` and
  `FlatListItemSize::{Known, Measured}`, but no inverted axis and no "maintain visible position on
  prepend" anchor. Load-older-on-scroll-up without a jump is the one place *text* chat may need a
  framework tweak. Worth prototyping early.
- **Emoji picker / rich text.** No emoji-picker component and no rich-text editor; `text_area` is
  plain text (`codeblock`'s `code_editor` is decoration-by-byte-range, aimed at code). Build the
  picker from `virtualized::grid` + `Pressable`.
- **Persistence.** No DB/ORM in the framework — deliberate. sqlx / SeaORM behind your `server`
  feature, per the recommended `api/` + `ui/` + `server-bin/` crate split.
- **MCP catalog drift.** `list_sdks` is missing ~10 crates that exist on disk: `denoise`, `sync`,
  `graphql`, `dnd`, `virtualized`, `offload`, `pdf`, `video-compose`, `gesture`, `auto-update`.
  Anyone (agent or human) discovering capabilities via MCP will under-count what's available.

---

## 4. What closing the voice/video gap costs

Three new SDK crates, in dependency order:

1. **`audio-output`** (or extend `audio`) — an `AudioStream` **sink**, plus an N-stream mixer with
   per-stream gain. Mechanisms: AVAudioEngine/AudioUnit (Apple), AudioTrack (Android), Web Audio
   `AudioWorklet` (web), cpal (desktop) — exactly the "diverge in mechanism, converge in behavior"
   shape rule 7 asks for. **Highest-value single addition**, and independently useful well beyond
   this project (voice notes, music, live monitoring, TTS playback).
2. **`audio-processing`** — AEC / AGC sitting beside `denoise`. Needs the far-end reference signal
   that #1 produces, which is why it's second. Per-platform: `VoiceProcessingIO`,
   `AcousticEchoCanceler`, or a shared WebRTC APM build.
3. **`rtc`** — the transport. Two credible shapes:
   - **Wrap `webrtc-rs` on native + the browser's `RTCPeerConnection` on web** behind one API.
     Larger crate, but it's the standard answer and it lets you point at any existing SFU
     (LiveKit, mediasoup, Janus) instead of writing one. Recommended.
   - **Roll your own UDP + Opus + custom SFU protocol.** Smaller API surface; you inherit congestion
     control and loss recovery, which is years of work.

Plus a **`call`** lifecycle piece (CallKit / ConnectionService + background modes) so a call
survives backgrounding and gets a system call UI.

---

## 5. Suggested path

**Phase 1 — build the entire text app now.** Nothing blocks it, and it's ~90% of Discord's surface
area. It also stress-tests the framework in exactly the places worth stress-testing: realtime
subscriptions, offline sync + outbox, virtualized scrollback, multi-target navigation. Expect the
scrollback anchor (§3) to be the one thing that needs a framework patch.

**Phase 2 — voice on web only.** On the wasm32 target an app can reach `RTCPeerConnection` through
`web-sys` directly, gated to that target. That validates the SFU, the signalling (`#[channel]` is a
perfect fit for SDP/ICE exchange), and the room model — without waiting on any new SDK. Native
targets show "voice unavailable" until phase 3.

**Phase 3 — the three SDK crates**, native parity, in the order above.

Note phase 2 is an *app-level* target gate, not a framework per-platform hack — but it does leave
four backends without voice, so it's a staging decision, not an architecture.

---

## 6. Scaffold, when you're ready

```
idealyst new idea-chat        # from crates/tools/cli — `New` / `Init` subcommands exist
```
Recommended layout, per the server-functions guide's provable-boundary split:

```
api/         #[server] fns + #[channel]/#[subscription] + DTOs + domain errors
             (does NOT depend on runtime-core, so a body cannot name Signal)
ui/          the app — idea-ui, navigators, depends on `api` for the stubs
server-bin/  depends on api with features = ["server"]; server::router() + configure_all()
```

---

## 7. Addendum — can WebRTC/RTP be a framework standard on native?

**Yes, and the right target is QUIC, not WebRTC.**

### Decompose "WebRTC"
1. Signalling (SDP) — not the wire spec; `#[channel]` covers it.
2. **NAT traversal (ICE/STUN/TURN) — not needed.** A centralized SFU means client→server
   only, and an outbound UDP flow to a public address punches its own mapping. The most
   notorious chunk of WebRTC is deleted by architecture.
3. Transport security (DTLS-SRTP).
4. Media transport (RTP/RTCP, jitter buffer, NACK/FEC, BWE).
5. Media processing (Opus, AEC3, NetEQ).

RTP is a 12-byte header — easy. **The years of work are in #5.** Any plan treating transport
as the hard part has it backwards.

### Paths
- **A — libwebrtc (C++) native + browser WebRTC on web.** Strongest interop argument: the
  browser *is* libwebrtc, so AEC3/NetEQ/transport-cc converge because they're the same
  implementation. LiveKit `rust-sdks` has maintained bindings + prebuilts for every native
  target. Costs ~10–30 MB/arch, vendored blobs, breaks the pure-Rust/rustls discipline.
- **B — webrtc-rs native. Ruled out.** Transport only (ICE/DTLS/SRTP/RTP); no codecs, no AEC,
  no NetEQ, no capture. All the hard media work *and* no control of the web leg.
- **C — own protocol over QUIC. Recommended.** No ICE ⇒ no SDP. Opus over own framing, QUIC
  datagrams replace DTLS-SRTP. Native `quinn` (pure Rust, fits the codebase grain); web
  **WebTransport** — the same QUIC datagrams.

### The fact that decides it
WebTransport reached **Baseline in March 2026**; Safari 26.4 ships it unflagged on macOS and
iOS, joining Chrome 97+, Edge 98+, Firefox 114+, Opera 83+, Samsung Internet 18+. Path C was
previously blocked on Safari; it no longer is. Residual caveat is *version reach*, not
capability — older-iOS **web** visitors lack it for a while; the native iOS app is unaffected.

### Repo precedents that make this shippable
- `[package.metadata.idealyst]` `capabilities` + per-platform `runtime_kotlin` / iOS shim file
  lists — how a crate ships Swift/Kotlin beside Rust. `screen-recorder` is the worked example,
  including the mandatory Android foreground service a call SDK also needs.
- **`denoise`** — a heavy third-party DSP dep (DeepFilterNet, git-pinned, ONNX via tract),
  cfg-split native/wasm, running *the same engine* on both legs. The convergence philosophy
  already applied to audio DSP; it argues for C's one-stack-everywhere over A's
  two-implementations-that-interop.
- `NotSupported` fallback (audio on Windows) — an rtc crate may fail honestly on
  roku/terminal/cpu/email rather than support all twelve backends.
- `AudioWriter` / `FrameWriter` + `video`'s stream source — the receive-side integration
  point already exists.

### Frictions to decide deliberately
- **No async runtime on the client, by design.** `net` drives tungstenite on a blocking worker
  thread, SSE uses `reqwest::blocking` ("its own runtime"), the driver is `pollster::block_on`.
  `quinn` and webrtc-rs are tokio-first. Workable via an owned runtime on a worker thread +
  `futures-channel` (the established shape), but rtc becomes the heaviest client dep by far.
- **iOS/Android deliberately avoid reqwest/rustls** "to avoid bundling a second TLS stack."
  Any QUIC/DTLS stack reintroduces exactly that.

### Ordering
`audio-output` first regardless — transport-agnostic, independently valuable, commits to
nothing. Path A partly masks the need (libwebrtc's ADM owns playback) but then yields sound
without an `AudioStream`, so no call recording via `media-writer`.

**Recommendation: Path C**, on the strength of the WebTransport baseline — one pure-Rust
stack, no vendored C++, both legs on QUIC datagrams, jitter buffer and PLC owned and
byte-identical everywhere. Counter-argument for A is real: AEC3 + NetEQ + transport-cc for
free is worth years. Choose A if Discord-quality voice in months beats framework purity.

Sources: WebTransport baseline / Safari 26.4 — https://webrtc.ventures/2026/04/webtransport-is-now-baseline-what-it-means-for-real-time-media/ ,
https://caniuse.com/webtransport , https://developer.mozilla.org/en-US/docs/Web/API/WebTransport

---

## 8. Revision — pure-Rust constraint (supersedes §7's reasoning)

Constraint set by the project: **pure Rust stacks, all platforms.** This revises §7.

### §7's ruling-out of webrtc-rs was based on a premise that no longer holds
It rested on "no pure-Rust Opus / AEC / media layer exists." Candidates now exist:

| Piece | Pure-Rust candidate | Status |
| --- | --- | --- |
| Opus enc+dec | `opus-rs` (restsend) — port of libopus 1.6 | UNAUDITED; young |
| Opus dec | `opus-decoder` — no unsafe, no FFI, passes all 12 RFC 8251 vectors | UNAUDITED |
| AEC | `aec3-rs` (port of WebRTC AEC3); `fdaf-aec`, `decibri-aec` (classical) | UNAUDITED; young |
| WebRTC transport | `webrtc-rs` (ICE/DTLS/SRTP/RTP; media crate in monorepo) | real, pure Rust |
| QUIC transport | `quinn` / **`quinn-proto` (sans-io)** | mature |
| AV1 encode | `rav1e` (Mozilla/Xiph) | mature as an encoder, NOT call-realtime |

**Treat every "UNAUDITED" row as a candidate to evaluate, not a solution.**

### The transport is now the SMALL decision
Both paths assemble the same parts — transport + opus-rs + AEC + own jitter/PLC + video codec.
Everything above transport is identical work. So the question collapses to the web leg:

- **WebRTC on web** → browser `RTCPeerConnection` owns codec, jitter buffer, PLC, BWE. Your Rust
  media stack runs on native ONLY; web runs a different C++ implementation you don't control.
  One media stack is structurally impossible.
- **WebTransport on web** → you compile YOUR Rust to wasm. Same opus-rs, same jitter buffer, same
  PLC everywhere. Only the datagram transport differs, and both legs are QUIC.

**Decision: WebTransport/QUIC.** It is the only path where the web leg runs our Rust. This is a
stronger argument than §7's and supersedes it. WebRTC is rejected not because it fails
cross-platform — it is *designed* for that — but because on web it mandates a non-Rust
implementation we don't own.

Fit detail: **`quinn-proto` is sans-io** (deterministic state machine, no runtime), matching this
repo's deliberate no-async-runtime-on-client discipline. Full `quinn` and `webrtc-rs` are both
tokio-first.

### Video: the line is drawn deliberately
**Pure-Rust realtime video encoding is NOT practical.** `rav1e` is the only pure-Rust
realtime-capable encoder and its own docs target "cases where libaom is too slow" — desktop
near-realtime. AV1 software encode at 720p30 on a phone is not viable. There is **no pure-Rust
H.264 or VP8 encoder**. Decode is better (rav1d/AV1); encode is the binding constraint.

⇒ **Video uses platform hardware codecs** (VideoToolbox / MediaCodec / WebCodecs). Not pure Rust,
but the same category as using UIKit to draw a view: diverge in mechanism, converge in output
(rule 7), and the only path that doesn't destroy battery. `media-writer` already reaches for these.

**Policy: pure Rust for audio; platform codecs for video.**

### Where the audio risk actually is
1. **opus-rs *performance*, not correctness.** libopus ships hand-tuned NEON asm; a Rust port may
   be several× slower. Correctness is testable (RFC 8251 vectors). **Speed is the go/no-go gate —
   measure on mid-tier Android + iPhone at 20 ms frames BEFORE building on it.**
2. **No pure-Rust NetEQ equivalent.** Adaptive jitter buffer + PLC + time-stretch is the largest
   piece of original work and the one users hear directly.

AEC lands better than feared: `getUserMedia`'s `echoCancellation` works regardless of transport
(and `microphone` already exposes the flag), so web gets browser AEC free while native runs
aec3-rs — the same algorithm on both sides if the port is faithful.

`denoise` is the proof the shape works (DeepFilterNet, pure Rust, same engine native + wasm) — and
its `web.rs` already documents the main-thread-inference jank problem, naming AudioWorklet as the
fix. The voice pipeline hits that same wall: **budget the AudioWorklet lift up front.**

### Work order
1. `audio-output` — needed under every path, commits to nothing.
2. **Benchmark opus-rs on real mid-tier mobile.** Go/no-go gate.
3. `rtc-transport` on `quinn-proto` + WebTransport.
4. Jitter buffer + PLC (long pole).
5. AEC — evaluate `aec3-rs`.
6. Video last, on platform hardware codecs.

Sources: https://github.com/restsend/opus-rs · https://lib.rs/crates/opus-decoder ·
https://github.com/RubyBit/aec3-rs · https://crates.io/crates/fdaf-aec ·
https://lib.rs/crates/decibri-aec · https://github.com/webrtc-rs/webrtc · https://github.com/xiph/rav1e

---

## 9. Correction + narrowing — the transport seams already exist

Scope refocused: mic capture is already abstracted; the question is **how we stream out and in**.
Reading the actual seams changes the plan and **corrects §2/B2**.

### OUT — solved on every platform
`AudioStream::subscribe(|frame| …)` yields
`AudioFrame { samples: &[f32], sample_rate, channels, pts_micros }` — interleaved normalized f32
+ format + a timestamp on the process-wide monotonic capture clock. Nothing to build.

### IN (memory) — solved on every platform
`AudioStream::new() -> (AudioStream, AudioWriter)`. **`AudioWriter` is `Send` + cheap to clone;
`AudioStream` is `!Send`** (main-thread consumer) — exactly the split needed: writer on the
network/decode thread, stream handle on the UI thread. `write_pcm_f32` stamps a **sample-accurate
PTS from a running frame count** (anchored to the clock at first chunk), deliberately NOT
wall-clock-at-delivery — the correct behavior for a playback stream fed by a jitter buffer.
`write_pcm_f32_at` covers real hardware timestamps. The module doc states its own motive as
**"playback / transport delegation."**

### IN (speakers) — CORRECTION: web already works
§2/B2 said nothing plays an `AudioStream`. **True for native, too broad overall.**
`media-stream/src/audio_web_bridge.rs` exists for exactly this: on the first `native_source()`
call against a synthetic (AudioWriter-fed) stream it lazily builds
`AudioContext → MediaStreamAudioDestinationNode` → a real `MediaStream` with a **live audio
track**, fed from a ring buffer by a `subscribe` callback. Attach to `<audio srcObject>` ⇒
speakers. Its stated motive is preserving "the `AudioStream` promise of *plug into inputs/outputs
on every platform*."

⇒ **A full web↔web voice loop is buildable TODAY with zero new framework code and no codec.**

Web-leg caveats (all documented in-tree):
- `ScriptProcessorNode` (deprecated, main-thread). The sync-accessor rationale is written down;
  upgrade path is an inline-Blob AudioWorklet.
- Ring buffer hard-caps ~1 s @ 48 kHz, drops oldest on stall — a crude jitter policy to replace.
- Bridged track runs on WebAudio's own clock, independent of `pts_micros`.
- **wasm capture clock is `Date.now()`-rebased, NOT monotonic.** Drive jitter timing off sample
  counts / the WebAudio clock on web, never `pts_micros`.

### The framework gap is ONE box, native only
```
                     OUT: subscribe()          OK every platform
microphone --AudioStream--> encode --> transport --> server
                                                        |
speakers <-- ??? <-- AudioStream <-- AudioWriter <-------+
             ^                       OK every platform
             +-- web:    OK  bridge -> <audio srcObject>
                 native: GAP  <-- the only missing piece
```
Anticipated by the architecture: `native_source()`'s doc names what a playback layer downcasts —
"an Apple `AVAudioEngine`, an Android `AudioTrack` feed."

`audio-output` sketch (native-only; web delegates to the existing bridge so the API stays uniform):
```rust
let out = audio_output::Output::default_device().await?;
let playing = out.play(&remote_stream)?;   // RAII, mirrors audio::Playback
playing.set_volume(0.8);

let mixed = audio_output::Mixer::new()
    .add(&alice).add(&bob)                 // per-source gain handles
    .stream();
```
Mechanism: AVAudioEngine (Apple) / AudioTrack (Android) / cpal (desktop).

### This defuses the codec question
**Ship raw PCM for v1.** Mono i16 @ 16 kHz ≈ 256 kbps (genuinely usable); f32 @ 48 kHz ≈ 1.5 Mbps
(fine on wifi for two-party tests). Opus slots in later as a pure `[f32] -> bytes -> [f32]`
transform between the tap and the transport — structurally a no-op.

⇒ The opus-rs benchmark drops from **prerequisite** to **optimization**, and will then be measured
against a real pipeline rather than a synthetic one. This supersedes §8's work-order gate.

### Revised order
1. **Web↔web voice loop, raw PCM, over plain `net::WebSocket` binary frames.** No new SDK, no
   codec, no QUIC. Proves server, room model, signalling end-to-end. Days, not months.
2. **`audio-output` (native)** — the one real framework deliverable.
3. Swap WebSocket → QUIC/WebTransport.
4. Jitter buffer + PLC (replaces the bridge's ring buffer).
5. Opus, benchmarked against the working pipeline.
6. AEC, then video on hardware codecs.

---

## 10. Step 1 result — the loop runs (2026-08-23)

Built in `crates/` and **verified end to end on web**. §9's prediction held: the
full out-and-in path needed **zero new framework code**.

### Evidence
- `voice-proto`: 12 wire-format tests (roundtrip, clipping symmetry, truncation,
  bad magic/version, zero-format rejection).
- `voice-server`: 6 integration tests against a real server on an ephemeral
  port — verbatim relay, **no self-echo**, room isolation, malformed-frame drop,
  N-way fan-out, text-frame tolerance.
- `tools/loop-test.js`: two real Chromium tabs, real `getUserMedia`, through the
  relay and out an `<audio>` element:

```
tab1(id=1): sent 149 · recv 116 · dropped 0 · peers 1 · 48000 Hz · mic 100% · remote 100%
tab2(id=2): sent 117 · recv 118 · dropped 0 · peers 1 · 48000 Hz · mic  99% · remote 100%
all checks passed
```

Non-zero remote peak on BOTH sides is the load-bearing number: packets counted
would only prove the transport, whereas a real signal at the far end proves the
synthetic-`AudioStream` -> WebAudio bridge -> `<audio srcObject>` chain actually
carries audio.

### Confirmed in practice
- `AudioStream::subscribe` -> `Packet::from_f32` -> binary WS is the entire OUT
  path. No resampling anywhere: the packet carries its own rate and the sink
  resamples.
- `AudioStream::new()` + `write_pcm_f32` + `native_source()` is the entire IN
  path on web. The bridge materializes a live track on first call exactly as
  documented.
- `write_pcm_f32`'s frame-count-derived PTS is the right choice for playback —
  the output clock advances with the audio, not with the network.

### API friction worth recording
**`text_input`'s `value` prop is one-way.** It drives widget-from-signal;
signal-from-widget requires an explicit `on_change = move |t| sig.set(t)`.
Without it the field edits visibly while the signal holds its initial value.
This cost a debugging cycle and presented misleadingly: both tabs joined under
ID 1, and the relay's (correct) no-self-echo rule suppressed all audio, which
looks exactly like a broken transport. The catalog doc for `value` — "Reads
reflect the widget's current text" — reads as if the binding were two-way.
The canonical form is in `storage/src/recipes.rs`.

### Still open, unchanged
Native playback (`audio-output`) remains the one real gap;
`playback::play` returns `NotSupported` there and names it. Next per §9: build
`audio-output`, then swap WebSocket for QUIC/WebTransport.

---

## 11. Two self-inflicted bugs worth recording (2026-08-23)

Both were found by the user testing manually, and both were invisible to the
automated harness. That is the interesting part.

### 11a. A constant default ID guaranteed the collision it was meant to avoid
§10 shipped a client defaulting every tab to id `1`, with a README note claiming
generated ids "would collide silently." That reasoning is backwards: a random
32-bit id essentially never collides, while a constant default ALWAYS does. Two
tabs both claimed `1`, the relay's (correct) no-self-echo rule suppressed every
packet between them, and both sat at `peers 0` in silence. It presents exactly
like a dead network.

Fixed three ways, because a silent failure mode deserves more than one guard:
- client defaults to `random_id()`;
- the relay tracks live ids per room and sends `error:duplicate-id` to a
  colliding newcomer, which the client surfaces as a status line;
- ids are released on disconnect, so a page refresh is not a collision with its
  own stale entry.

Regression tests: `duplicate_id_in_a_room_is_reported_not_silently_swallowed`,
`a_distinct_id_in_the_same_room_is_not_flagged`,
`reconnecting_with_the_same_id_after_leaving_is_not_a_duplicate`, plus a browser
check that two tabs work with ZERO configuration.

### 11b. The harness could pass while the app was silent
Two compounding measurement errors:
1. The `remote` meter is computed in the receive loop **before** the writer, so
   it only ever proved decode — never that sound reached an output. §10 claimed
   it proved the whole bridge chain. It did not.
2. The harness launched Chromium with `--autoplay-policy=no-user-gesture-required`,
   which would mask any real autoplay failure.

Neither the ID collision nor a silent output could fail that harness, because it
configured ids explicitly and never measured the end of the chain.

**Lesson for the next stage:** measure the END of the pipeline, and exercise the
DEFAULT path. A harness that hand-configures its way past the default is testing
a configuration no user will use. Before the jitter buffer lands, the harness
needs an `AnalyserNode` tap on the played track asserting real amplitude —
otherwise every future audio change is unverifiable in the same way.

(Playback itself was never broken: the `<audio>` element plays under the real
autoplay policy — `paused: false`, `currentTime` advancing, `readyState: 4` —
and the user independently confirmed hearing the harness tones.)

---

## 12. Step 2 — the coordinated voice fleet (2026-08-23)

Requirement: many clients, a cluster of general-purpose servers, a pool of shared
voice nodes, and an **atomic** room→node mapping that is freed when the channel
goes inactive.

### The hard part, isolated
Room→node assignment and the participant count must never disagree. Every failure
of that invariant is quiet and severe: joiners sent to a node that tore the room
down, a room pinned to a corpse, or two people "in the same room" on different
nodes hearing nothing with no error raised.

All three are read-modify-write races, so a get/set cache cannot express the fix.
(The framework's own `cache` crate says so: it deliberately excludes atomic RMW.)
`RoomStore` is therefore defined in **compound operations** — `join`, `leave` —
each individually atomic. With that, both interleavings are correct: join-first
takes 1→2 then leave→1; leave-first closes the room and the join assigns fresh.

### Is Redis the answer? Mostly no.
Three backends, one conformance suite (17 cases × 3 = 51 tests):

| backend | atomicity mechanism | when |
| --- | --- | --- |
| **postgres** (default) | `pg_advisory_xact_lock(hashtext(room))` in a transaction — serializes ONE room, not the fleet; released on commit or rollback | already deployed; durable assignments; a few hundred writes/sec is nothing |
| redis | Lua on the single-threaded core | coordination outgrows the primary DB, or Redis already runs |
| memory | one mutex across the operation | single node, tests |

**Redis is an optimization here, not a requirement.** Coordination traffic is one
write per user per room transition — never per audio packet. Postgres has had
the atomicity primitive longer than Redis has existed, and its advisory lock is
per-room where Redis's script lock is global.

### Tests that can fail
`tests/teeth.rs` builds the naive get-then-set store deliberately and asserts the
invariants **break** (rooms split, counts lost). Without it, a passing
concurrency suite proves nothing — the lesson from §11b, applied.

### Failure handling
- **Node dies:** TTL-backed registration; the next join prunes it and reassigns.
  Self-healing, no reaper process.
- **Zombie returns:** monotonic per-room epoch, kept in a record that OUTLIVES the
  room. A stale claim is fenced and its sockets closed.
- **Client crashes:** `leave` is bound to socket close, so an increment cannot
  leak. The director deliberately does NOT count — placement-time increments leak
  whenever a placed client never connects.
- **Wrong placement:** the node redirects to the real owner and undoes its own
  increment. Directors stay stateless and horizontally scalable because the store
  settles every disagreement.
- **Fleet full:** 503 from the director — retryable, and the scale-out signal.

### Verified live
Containerized fleet (Postgres + director + 3 nodes), three real browser tabs:
all 20 checks — 3 participants, one owning node, both peers audible in every tab,
zero drops, room released to 404 on empty. One run recorded `moved=1`: a tab was
hinted at node-b, lost the cold-room race, and followed the redirect to node-a.

A fourth node joined a running fleet with no reconfiguration (`accepting` 3→4);
`POST /nodes/node-a/drain` took it back to 3 without touching live calls.

### New finding: rAF-driven meters freeze in background tabs
The level meters use `raf_loop`, and browsers throttle `requestAnimationFrame` to
about 1 fps in background tabs. Audio keeps flowing (Web Audio is not throttled)
but the meter samples so rarely it lands in silence and reads 0 permanently.
This produced a false failure until the harness focused each tab before reading.
Relevant to any idealyst app whose UI reports live data from a background tab —
drive those from a timer, not rAF.

### Still open
Native playback (`audio-output`) is unchanged as the one real framework gap.

---

## 13. Step 2b — single-origin gateway for tunnelled testing (2026-08-23)

Requirement: stream everything through ONE endpoint so `ngrok http 8080` is
enough to test with remote people.

### Why a proxy and not just port-forwarding
A tunnel gives one public origin, but the system natively needs three (web app,
director, per-node `ws://`). Two of those are addresses a remote machine
**cannot resolve**: `ws://127.0.0.1:8787` means "the visitor's own laptop".

### The two rewrites
1. **Placement** — the director's `ws_url` is replaced with
   `{origin}/_voice/node/{id}`, which the gateway proxies to the real node.
2. **Redirects** — a node that does not own a room replies with
   `redirect:ws://other:8787`. Same problem, and easy to miss because it fires
   ONLY on the cold-room race: without this, remote calls work most of the time
   and fail exactly when two people join an empty room simultaneously.

The gateway's WS proxy is therefore message-aware — it inspects text frames
rather than blindly splicing bytes.

### Origin derived, never configured
Rewritten URLs are built from the request's own `Host` / `X-Forwarded-Proto`, so
one binary serves localhost and a tunnel identically, and `X-Forwarded-Proto:
https` yields `wss://` — which a secure page requires.

The client's director field defaults to `window.location.origin + "/_voice"`, so
a remote visitor presses Join with nothing to configure. A hardcoded
`127.0.0.1` default would send every visitor to their own machine — a failure
that looks like "broken for everyone but you".

### HTTPS is load-bearing, not cosmetic
`getUserMedia` requires a secure context. A LAN IP over plain HTTP is not one and
fails as a **silently muted mic**, not an error. The tunnel's HTTPS origin is
what makes remote testing work at all — a reason to use one, not a convenience.

### Verified
Against a **real ngrok tunnel** (not simulated headers): three browsers over
`https://…ngrok.app`, 14/14 checks — 3 participants, one owning node, every tab
hearing both peers, zero drops, and a redirect rewritten to
`wss://…ngrok.app/_voice/node/node-a` with no internal address leaked.
