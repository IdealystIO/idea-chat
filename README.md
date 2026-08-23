# idea-chat

An exploration toward an open-source Discord alternative built on the idealyst
framework. Chat, voice, video — cross-platform, centralized server, no P2P.

Start with **[docs/framework-gap-analysis.md](docs/framework-gap-analysis.md)**.
It audits what the SDK does and does not provide, and §9 is the current plan.

## Where things stand

Text chat is fully served by existing SDKs. Voice needs one thing the framework
does not have — a **native `AudioStream` sink** — and the whole point of this
repo right now is to prove that by building around it.

Everything here lives outside the `idealyst-native` tree on purpose. What earns
promotion into the SDK gets decided after it works, not before.

## Hearth — the client

`crates/hearth` is the app: an implementation of the `Hearth.dc.html` design on
idealyst, composed from `idea-ui` components over a Hearth-specific theme.
Spaces, channels, always-open voice/video/share rooms, direct messages,
threads, search, a person menu and profile, moderation, seven settings, and a
device preview.

```bash
cd crates/hearth && idealyst build --web
idealyst serve dist/web --port 8099
node tools/hearth-ui.js          # 28 assertions through a real browser
```

Everything visible is a stock `idea-ui` component; the ~60 `stylesheet!` blocks
in `styles.rs` are layout only. Light/dark and four accents are token values —
one closure re-themes every component and every app stylesheet at once.

The seed content in `model.rs` is the design's own fixture. It is deliberately
the shape a server response has to arrive in: every view reads through the
accessors there, so wiring this to the real backend is a change to that module
and nowhere else.

**[docs/hearth-framework-findings.md](docs/hearth-framework-findings.md)** is
the other deliverable — eleven things in idealyst that cost debugging time or
forced a departure from the design, plus what worked. The largest by far: a
`#[component]` body is not a reactive scope, and nothing tells you.

## Step 1 — the voice loop (current)

Proves the out-and-in audio path with no codec, no QUIC, and no new framework
code, so every later decision is made against a pipeline that actually runs.

```text
OUT  microphone.open_stream() -> AudioStream
       .subscribe(|frame|)              interleaved f32 + rate + pts
       Packet::from_f32().encode()      raw i16, no codec yet
       WsSender::send(Binary)

IN   WebSocket::recv() -> Binary
       Packet::decode()
       AudioStream::new() -> (stream, AudioWriter)    one pair per peer
       writer.write_pcm_f32(rate, channels, &pcm)
       playback::play(&stream)                        <-- the only real gap
```

### Crates

| crate | what it is |
| --- | --- |
| `voice-proto` | The wire format. Zero deps, no framework types, fully unit-tested — the most likely candidate for promotion, so it stays portable. |
| `voice-server` | Binary-WebSocket room relay. Validates headers, fans out, never decodes audio. |
| `voice-client` | The idealyst app. `session.rs` is both transport directions; `playback.rs` is the stand-in for the missing `audio-output` SDK. |

### Running it

```bash
cargo run -p voice-server                  # relay on :8787
cd crates/voice-client && idealyst dev --web --local
```

Open **two tabs** and hit Join in both. Nothing to configure — each tab picks a
random ID. **Use headphones**; there is no echo cancellation on the native path
at all, and the browser's is off by default here (see below).

### Known-and-intended limitations

These are deliberate for step 1, not oversights:

- **Web only.** `playback::play` returns `NotSupported` on native, naming the
  gap. That error *is* the finding.
- **No codec.** Raw i16. Mono at 48 kHz is ~768 kbps. A codec slots in later as
  a pure `&[f32] -> Vec<u8> -> Vec<f32>` transform and changes nothing structural.
- **No jitter buffer.** The framework's WebAudio bridge ring buffer (~1 s cap,
  drops oldest) is standing in. Real pacing and loss concealment come later.
- **No signalling.** Peers are discovered by hearing audio from them. Roster,
  names, and mute belong on `#[channel]`, where JSON is the right shape.
- **Echo cancellation defaults OFF.** For a real call it should be on, but in a
  two-tab self-test the audio you are listening for is your own voice from the
  speakers — exactly what AEC removes. The toggle switches it.

## Next

2. `audio-output` (native) — the one real framework deliverable.
3. WebSocket → QUIC/WebTransport.
4. Jitter buffer + PLC.
5. Opus, benchmarked against this pipeline rather than a synthetic one.
6. AEC, then video on platform hardware codecs.

## Verification

`cargo test` — 12 wire-format tests (`voice-proto`) and 6 relay integration
tests (`voice-server`) against a real server on an ephemeral port, covering
no-self-echo, room isolation, and malformed-frame rejection.

`node tools/gateway-test.js` — three tabs through the single gateway origin,
plus an assertion that a redirect never leaks an internal address. Set
`ORIGIN=https://xxx.ngrok.app` to run it against a live tunnel.

`node tools/three-way.js` — three real tabs through the director onto the
containerized fleet: everyone hears both peers, one node owns the room, and the
assignment is released when the last tab leaves. 20/20 checks.

`node tools/loop-test.js` — two real Chromium tabs, real `getUserMedia`
capture, through the relay and back out an `<audio>` element. Last run:

```
tab1(id=1): sent 149 · recv 116 · dropped 0 · peers 1 · 48000 Hz · mic 100% · remote 100%
tab2(id=2): sent 117 · recv 118 · dropped 0 · peers 1 · 48000 Hz · mic  99% · remote 100%
all checks passed
```

![the loop running](docs/voice-loop.png)

Both directions carry a real signal end to end, with zero drops — on web, with
no new framework code.

## Devcontainer

`.devcontainer/` brings up the dev environment plus Postgres, Redis, and a
headless Chromium for the browser harnesses. Open the folder in VS Code and
"Reopen in Container".

Two things it gets right that are easy to get wrong:

- **It mounts `../..`, not this repo.** The workspace path-depends on
  `../idealyst-native`, so both checkouts must sit side by side inside the
  container exactly as they do on the host. Mounting only `idea-chat` breaks
  every framework dependency at manifest-parse time.
- **`wasm-bindgen-cli` is pinned from the framework's lockfile.** A version skew
  against the `wasm-bindgen` crate fails with an error that reads like a corrupt
  build rather than a version mismatch.

The coordination sidecars are reachable inside the container as
`postgres://postgres:dev@postgres:5432/voice` and `redis://redis:6379`, and are
published to the host on `55432` and `6399` for `psql`/`redis-cli`.

`deploy/Dockerfile` builds the fleet image. It uses `deploy/Cargo.server.toml`,
a server-only workspace, so the build context stays inside this repo — the
server tier depends on none of the framework, and copying an entire framework
checkout just so cargo can parse manifests it will never build is pure waste.
