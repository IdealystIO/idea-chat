//! The out-and-in audio path: mic -> wire, wire -> speakers.
//!
//! This is the file the whole exploration exists to write. Both directions run
//! entirely on seams the framework already provides:
//!
//! ```text
//! OUT  microphone.open_stream() -> AudioStream
//!        .subscribe(|frame| ...)          <- interleaved f32 + rate + pts
//!        Packet::from_f32(..).encode()
//!        WsSender::send(WsMessage::Binary(..))
//!
//! IN   WebSocket::recv() -> WsMessage::Binary
//!        Packet::decode(..)
//!        AudioStream::new() -> (stream, AudioWriter)   <- one pair per peer
//!        writer.write_pcm_f32(rate, channels, &pcm)
//!        playback::play(&stream)                        <- the only gap
//! ```
//!
//! Note what is NOT here: no resampling (the packet carries its own rate and
//! the platform sink resamples), no codec (raw i16 for now), and no jitter
//! buffer (the web bridge's ring buffer is standing in — see the caveats in
//! docs/framework-gap-analysis.md §9).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use media_stream::{AudioFrame, AudioStream, AudioSubscription, AudioWriter};
use microphone::{AudioStreamConfig, Microphone};
use net::{WebSocket, WsMessage, WsSender};
use serde::Deserialize;
use voice_proto::Packet;

use crate::playback::{self, Playing};

// ---------------------------------------------------------------------------
// Audio -> UI bridge.
//
// The mic `subscribe` callback runs on the audio thread on native and the main
// thread on web, so it can never touch the reactive runtime directly. It writes
// lock-free atomics here; a `raf_loop` on the main thread folds them into
// signals once per frame. This is the same pattern `mic-demo` establishes.
// f32s are stored as bit patterns because std has no AtomicF32.
// ---------------------------------------------------------------------------

static MIC_PEAK: AtomicU32 = AtomicU32::new(0);
static REMOTE_PEAK: AtomicU32 = AtomicU32::new(0);
static PACKETS_OUT: AtomicU32 = AtomicU32::new(0);
static PACKETS_IN: AtomicU32 = AtomicU32::new(0);
static PACKETS_DROPPED: AtomicU32 = AtomicU32::new(0);
static PEER_COUNT: AtomicU32 = AtomicU32::new(0);
static SAMPLE_RATE: AtomicU32 = AtomicU32::new(0);
/// Set when the relay reports our id is already in the room. Surfaced loudly:
/// the self-echo filter otherwise makes a duplicate id indistinguishable from a
/// dead network.
static DUPLICATE_ID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set when the fleet had no node with capacity — the scale-out signal, surfaced
/// to the user rather than presenting as a connection that simply does nothing.
static NO_CAPACITY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set when reconnecting after a redirect failed.
static CONNECT_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// How many times this session was moved to another node.
static REDIRECTS: AtomicU32 = AtomicU32::new(0);

/// A snapshot of live counters, read once per frame by the UI.
#[derive(Clone, Copy, PartialEq)]
pub struct Stats {
    /// Peak amplitude of the most recent captured chunk, `0.0..=1.0`.
    pub mic_peak: f32,
    /// Peak amplitude of the most recent received chunk, `0.0..=1.0`.
    pub remote_peak: f32,
    /// Packets sent since join.
    pub sent: u32,
    /// Packets received since join.
    pub received: u32,
    /// Packets that arrived but could not be decoded or played.
    pub dropped: u32,
    /// Distinct remote senders heard from.
    pub peers: u32,
    /// Capture sample rate the mic actually gave us.
    pub sample_rate: u32,
    /// The relay says this id is already connected to this room.
    pub duplicate_id: bool,
    /// The fleet had no capacity for a new room.
    pub no_capacity: bool,
    /// Reconnecting after a redirect failed.
    pub connect_failed: bool,
    /// Times this session followed a redirect to another node.
    pub redirects: u32,
}

/// Read the atomics. Cheap enough to call every frame.
pub fn stats() -> Stats {
    Stats {
        mic_peak: f32::from_bits(MIC_PEAK.load(Ordering::Relaxed)),
        remote_peak: f32::from_bits(REMOTE_PEAK.load(Ordering::Relaxed)),
        sent: PACKETS_OUT.load(Ordering::Relaxed),
        received: PACKETS_IN.load(Ordering::Relaxed),
        dropped: PACKETS_DROPPED.load(Ordering::Relaxed),
        peers: PEER_COUNT.load(Ordering::Relaxed),
        sample_rate: SAMPLE_RATE.load(Ordering::Relaxed),
        duplicate_id: DUPLICATE_ID.load(Ordering::Relaxed),
        no_capacity: NO_CAPACITY.load(Ordering::Relaxed),
        connect_failed: CONNECT_FAILED.load(Ordering::Relaxed),
        redirects: REDIRECTS.load(Ordering::Relaxed),
    }
}

fn reset_stats() {
    for c in [
        &MIC_PEAK, &REMOTE_PEAK, &PACKETS_OUT, &PACKETS_IN, &PACKETS_DROPPED, &PEER_COUNT,
        &SAMPLE_RATE,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    DUPLICATE_ID.store(false, Ordering::Relaxed);
    NO_CAPACITY.store(false, Ordering::Relaxed);
    CONNECT_FAILED.store(false, Ordering::Relaxed);
    REDIRECTS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Per-peer receive path
// ---------------------------------------------------------------------------

/// One remote participant's playback chain.
///
/// Field order matters for teardown: `playing` detaches the sink first, then
/// the stream drops (running its stopper), then the writer. Dropping the writer
/// while the sink is still pulling would starve the graph rather than stop it.
struct PeerSink {
    playing: Playing,
    stream: AudioStream,
    writer: AudioWriter,
}

impl PeerSink {
    /// Build a synthetic stream for `sender` and route it to the speakers.
    ///
    /// `AudioStream::new` gives the producer/consumer pair; the writer takes
    /// network PCM, and `playback::play` binds the consumer side to an output.
    /// On web `native_source()` lazily materializes a real MediaStreamTrack for
    /// this synthetic stream, which is what makes the whole approach work.
    fn open(sender: u32) -> Result<PeerSink, String> {
        let (stream, writer) = AudioStream::new();
        let playing = playback::play(&stream)
            .map_err(|e| format!("peer {sender}: {e}"))?;
        Ok(PeerSink { playing, stream, writer })
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A joined voice channel. Dropping it stops capture, closes the socket, and
/// tears down every peer's playback.
/// The director's answer to "where does this room live?".
#[derive(Deserialize)]
struct Route {
    ws_url: String,
}

/// Ask the director which node to dial.
///
/// The answer is a HINT for a cold room — the node's atomic join settles who
/// really owns it, and a wrong guess comes back as a redirect. That is what lets
/// directors be stateless and horizontally scaled with no coordination.
async fn resolve(director: &str, room: &str) -> Result<String, String> {
    let url = format!("{}/rooms/{}/route", director.trim_end_matches('/'), room);
    let response = net::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("director {url}: {e}"))?;
    let route: Route = response
        .json()
        .await
        .map_err(|e| format!("director returned unusable JSON: {e}"))?;
    Ok(route.ws_url)
}

/// Publish half, shared with the capture callback and swapped on a redirect.
///
/// `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` because on NATIVE the capture
/// callback runs on the audio thread, so `AudioStream::subscribe` requires a
/// `Send` closure. On web the bound is relaxed (one thread) and this is merely
/// redundant — but writing it for the stricter target keeps one code path
/// instead of a cfg-split over something this small.
type SharedSender = Arc<Mutex<WsSender>>;

pub struct Session {
    /// Swapped when a redirect moves us to another node, so the capture
    /// callback keeps publishing without knowing the connection changed.
    sender: SharedSender,
    /// Held to keep capture alive; the subscription below borrows from it.
    _mic: AudioStream,
    _mic_sub: AudioSubscription,
    /// Shared with the receive task, which inserts new peers as they are heard.
    peers: Rc<RefCell<HashMap<u32, PeerSink>>>,
    /// Set when the session is torn down, so the receive task stops rather than
    /// resurrecting peers after a leave.
    closed: Rc<RefCell<bool>>,
}

impl Session {
    /// Output gain applied to every peer.
    pub fn set_volume(&self, volume: f32) {
        for peer in self.peers.borrow().values() {
            peer.playing.set_volume(volume);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        *self.closed.borrow_mut() = true;
        if let Ok(s) = self.sender.lock() {
            s.close();
        }
        self.peers.borrow_mut().clear();
        PEER_COUNT.store(0, Ordering::Relaxed);
    }
}

/// Join `room` as `id`, relaying through the server at `base_url`.
///
/// `base_url` is the relay origin, e.g. `ws://127.0.0.1:8787`.
/// Whether to let the browser pre-process the captured signal.
///
/// This matters more than it looks for a SELF test. With two tabs on one
/// machine, the audio you are listening for is your own voice coming back out
/// of the speakers — which is precisely what acoustic echo cancellation exists
/// to remove. AEC sees it as echo of the far end and nulls it, so a loopback
/// test can go silent while capture and transport are both working perfectly.
///
/// `Processed` is what a real call wants. `Raw` is what a loopback test wants.
/// AGC is left ON in both: turning it off makes the web mic dramatically
/// quieter, which is a second silent-looking failure.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Browser AEC + noise suppression on. Correct for real calls.
    Processed,
    /// AEC and noise suppression off, AGC on. Correct for a two-tab self-test.
    Raw,
}

/// Proof that the caller may be in this room, when the relay asks for it.
///
/// `None` is the loopback demo, where the node admits anyone. Hearth always
/// passes one — see `voice_auth`.
pub type Ticket<'a> = Option<&'a str>;

pub async fn join(
    director_url: &str,
    room: &str,
    id: u32,
    capture: Capture,
    ticket: Ticket<'_>,
) -> Result<Session, String> {
    reset_stats();

    // 1. Capture. `mono()` keeps the wire cost down; the rate is whatever the
    //    device gives, and it travels in every packet so nothing resamples.
    //
    //    These processing flags are WEB-ONLY in the microphone SDK (Android is
    //    a documented no-op, and there is no Apple VoiceProcessingIO path), so
    //    on native none of this applies — that is the `audio-processing` gap.
    let config = AudioStreamConfig::default()
        .mono()
        .with_auto_gain_control(true)
        .with_echo_cancellation(capture == Capture::Processed)
        .with_noise_suppression(capture == Capture::Processed);

    let mic = Microphone::new()
        .open_stream(config)
        .await
        .map_err(|e| format!("microphone: {e}"))?;

    // 2. Placement. A `ws://` origin bypasses the director for single-node use;
    //    anything else is asked where the room lives.
    let node_url = if director_url.starts_with("ws://") || director_url.starts_with("wss://") {
        director_url.trim_end_matches('/').to_string()
    } else {
        resolve(director_url, room).await?
    };

    // 3. Transport. Binary frames, because the framework's typed Socket is JSON
    //    text on send. This is the seam that becomes QUIC datagrams later.
    let mut url = format!("{}/ws?room={}&id={}", node_url.trim_end_matches('/'), room, id);
    if let Some(t) = ticket {
        url.push_str("&token=");
        url.push_str(t);
    }
    let socket = WebSocket::connect(&url)
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;
    let sender: SharedSender = Arc::new(Mutex::new(socket.sender()));

    // 4. OUT: every captured chunk becomes one packet.
    let mic_sub = {
        let sender = sender.clone();
        let seq = std::cell::Cell::new(0u32);
        mic.subscribe(move |frame: &AudioFrame| {
            MIC_PEAK.store(voice_proto::peak(frame.samples).to_bits(), Ordering::Relaxed);
            SAMPLE_RATE.store(frame.sample_rate, Ordering::Relaxed);

            let n = seq.get();
            seq.set(n.wrapping_add(1));

            let packet = Packet::from_f32(
                id,
                n,
                frame.sample_rate,
                frame.channels,
                frame.pts_micros,
                frame.samples,
            );
            // A send failure means the socket closed; the receive task will
            // notice and report it. Counting here keeps the hot path silent.
            let sent = sender
                .lock()
                .map(|s| s.send(WsMessage::Binary(packet.encode())).is_ok())
                .unwrap_or(false);
            if sent {
                PACKETS_OUT.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    // 5. IN: drive the socket, routing each sender to its own playback chain,
    //    following a redirect if this node turns out not to own the room.
    let peers: Rc<RefCell<HashMap<u32, PeerSink>>> = Rc::new(RefCell::new(HashMap::new()));
    let closed = Rc::new(RefCell::new(false));
    spawn_receive_loop(
        socket,
        room.to_string(),
        id,
        ticket.map(str::to_string),
        sender.clone(),
        peers.clone(),
        closed.clone(),
    );

    Ok(Session { sender, _mic: mic, _mic_sub: mic_sub, peers, closed })
}

/// Own the socket and pump it until it closes, following redirects.
///
/// The future is `!Send` (it holds `Rc` and `!Send` AudioStreams) and lives as
/// long as the page, which is exactly what `spawn_async` supports on web. It
/// relies on a backend-installed async executor: without one, the native
/// fallback is `pollster::block_on`, which would block the calling thread on a
/// loop that never ends. Step 1 targets web only for this reason.
fn spawn_receive_loop(
    socket: WebSocket,
    room: String,
    id: u32,
    // Carried so a redirect can re-present it. The node we are sent to runs the
    // same check as the one that sent us, so dropping the ticket here turns the
    // cold-room race into an authentication failure — which would look exactly
    // like a flaky relay and only ever happen under load.
    ticket: Option<String>,
    sender: SharedSender,
    peers: Rc<RefCell<HashMap<u32, PeerSink>>>,
    closed: Rc<RefCell<bool>>,
) {
    runtime_core::driver::spawn_async(async move {
        let mut socket = socket;
        // Bounded: a redirect loop between two nodes that each disown the room
        // would otherwise spin forever. Three hops is far more than the one the
        // cold-room race can produce.
        let mut hops = 0;
        loop {
            match pump(&mut socket, &peers, &closed).await {
                Pump::Finished => break,
                Pump::Redirect(url) if hops < 3 => {
                    hops += 1;
                    let mut next =
                        format!("{}/ws?room={}&id={}", url.trim_end_matches('/'), room, id);
                    if let Some(t) = ticket.as_deref() {
                        next.push_str("&token=");
                        next.push_str(t);
                    }
                    match WebSocket::connect(&next).await {
                        Ok(s) => {
                            // Swap the publish half so the capture callback
                            // keeps working without knowing anything moved.
                            if let Ok(mut slot) = sender.lock() {
                                *slot = s.sender();
                            }
                            socket = s;
                            REDIRECTS.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            CONNECT_FAILED.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                Pump::Redirect(_) => {
                    CONNECT_FAILED.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
}

/// Why the pump stopped.
enum Pump {
    /// Socket closed, or the session was torn down.
    Finished,
    /// This node does not own the room; reconnect at this URL.
    Redirect(String),
}

/// Drain one connection until it closes or hands us a redirect.
async fn pump(
    socket: &mut WebSocket,
    peers: &Rc<RefCell<HashMap<u32, PeerSink>>>,
    closed: &Rc<RefCell<bool>>,
) -> Pump {
    while let Some(message) = socket.recv().await {
        if *closed.borrow() {
            return Pump::Finished;
        }
        let bytes = match message {
            Ok(WsMessage::Binary(b)) => b,
            Ok(WsMessage::Text(t)) => {
                // Text carries node-level notices, never audio.
                if let Some(url) = t.strip_prefix("redirect:") {
                    return Pump::Redirect(url.to_string());
                }
                if t.contains("duplicate-id") {
                    DUPLICATE_ID.store(true, Ordering::Relaxed);
                }
                if t.contains("no-capacity") {
                    NO_CAPACITY.store(true, Ordering::Relaxed);
                }
                continue;
            }
            Err(_) => return Pump::Finished,
        };

        let packet = match Packet::decode(&bytes) {
            Ok(p) => p,
            Err(_) => {
                PACKETS_DROPPED.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let pcm = packet.to_f32();
        REMOTE_PEAK.store(voice_proto::peak(&pcm).to_bits(), Ordering::Relaxed);

        let mut map = peers.borrow_mut();
        let sink = match map.entry(packet.sender) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                match PeerSink::open(packet.sender) {
                    Ok(sink) => {
                        let sink = slot.insert(sink);
                        PEER_COUNT.fetch_add(1, Ordering::Relaxed);
                        sink
                    }
                    Err(_) => {
                        // No sink on this platform (native today). Count it and
                        // keep draining so the socket does not stall.
                        PACKETS_DROPPED.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
            }
        };

        // `write_pcm_f32` derives a sample-accurate PTS from a running frame
        // count rather than arrival time, which is what a playback stream
        // wants: the output clock should advance with the audio, not with the
        // network. The packet's own `pts_micros` is deliberately unused here —
        // it is the sender's timeline, and pacing against it is a jitter-buffer
        // job, not a writer job.
        sink.writer.write_pcm_f32(packet.sample_rate, packet.channels, &pcm);
        let _ = &sink.stream;

        PACKETS_IN.fetch_add(1, Ordering::Relaxed);
    }
    Pump::Finished
}

/// A per-client id that will not collide in practice.
///
/// A constant default is the wrong answer here and was the original bug: two
/// tabs both claiming id 1 land in the same room, the self-echo filter
/// suppresses everything between them, and both sit at zero peers in silence
/// with nothing logged. Randomness makes the pathological case vanishingly
/// unlikely, and the relay now reports it outright when it does happen.
pub fn random_id() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        // 1..=u32::MAX: zero is reserved as "unset" in the query string.
        (js_sys::Math::random() * (u32::MAX as f64 - 1.0)) as u32 + 1
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u8(0);
        (h.finish() as u32).max(1)
    }
}

/// Where to look for the director, defaulting to THIS page's origin.
///
/// Same-origin is what makes tunnelled testing work: a remote visitor loads
/// `https://xxx.ngrok.app`, and the default already points at the gateway that
/// served the page. Hardcoding a `127.0.0.1` director would send every remote
/// visitor to their own machine — the failure looks like "the app is broken for
/// everyone but you".
pub fn default_director() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(origin) = web_sys::window().and_then(|w| w.location().origin().ok()) {
            return format!("{origin}/_voice");
        }
        "http://127.0.0.1:9000".to_string()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "http://127.0.0.1:9000".to_string()
    }
}
