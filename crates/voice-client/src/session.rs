//! The out-and-in audio path: mic -> wire, wire -> speakers.
//!
//! This is the file the whole exploration exists to write. Both directions run
//! entirely on seams the framework already provides:
//!
//! ```text
//! OUT  microphone.open_stream() -> AudioStream
//!        .subscribe(|frame| ...)          <- interleaved f32 + rate + pts
//!        Packetiser -> 20 ms frames
//!        codec::Encoded::packet(..).encode()
//!        carrier::Publish::binary(..)     <- datagram, or WebSocket frame
//!
//! IN   carrier::Receive::next() -> Incoming::Binary
//!        Packet::decode(..)
//!        jitter::Buffer, played out on a clock       <- one per sender
//!        audio_io::Output::write_at(&pcm, rate)
//! ```
//!
//! Note what is NOT here: no resampling — the packet carries its own rate and
//! the platform sink resamples. Which transport carries the packets is
//! [`carrier`](crate::carrier)'s business, not this file's; what this file owns
//! is that both look the same from the audio path.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use media_stream::{AudioFrame, AudioStream, AudioSubscription, AudioWriter};
use net::WebSocket;
use serde::Deserialize;
use voice_proto::codec::Encoded;
use voice_proto::Packet;

use crate::carrier::{self, Incoming, Join, Publish, Receive};

/// Milliseconds of audio in one packet on the wire.
///
/// 20 ms, which is not a tuning choice so much as the industry's unit: it is
/// what Opus is built around, what jitter buffers assume, and small enough that
/// the encoded frame fits a QUIC datagram with room to spare.
///
/// Note this is the *packet* size, not the capture block size. Capture still
/// arrives 85 ms at a time on web and that is where the latency is — see
/// docs/hearth-framework-findings.md finding 10c.
const PACKET_MS: u32 = 20;

/// The most frames one arrival may play out at once.
///
/// Ten, which is 200 ms. The playout clock is real time (see [`PeerSink::feed`])
/// and real time keeps running while a tab is backgrounded or a laptop is
/// asleep — so without a cap, the first packet after a resume would try to play
/// every frame the clock says is owed. That audio is stale by definition; the
/// clock is resynchronised to now instead, and the buffer's own ceiling has
/// already discarded what it could not hold.
const MAX_CATCHUP: usize = 10;

/// A local monotonic clock, in milliseconds.
///
/// Used only to MEASURE — jitter on the arrival side, elapsed time on the
/// playout side. Nothing schedules against it, because there is nothing on this
/// platform to schedule with.
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        thread_local! {
            static CLOCK: Option<web_sys::Performance> =
                web_sys::window().and_then(|w| w.performance());
        }
        // `Date::now` rather than zero when there is no `performance`: a clock
        // stuck at zero would report no time passing, the playout side would
        // never pull a frame, and the call would be silent with every other
        // counter climbing normally.
        CLOCK.with(|clock| clock.as_ref().map(|p| p.now()).unwrap_or_else(js_sys::Date::now))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        thread_local! {
            static START: std::time::Instant = std::time::Instant::now();
        }
        START.with(|start| start.elapsed().as_secs_f64() * 1000.0)
    }
}

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
/// Payload bytes sent since join, so a bitrate can be measured rather than
/// assumed. Wraps after ~4 GB, which no call reaches.
static BYTES_OUT: AtomicU32 = AtomicU32::new(0);
/// Packets the transport would not take: a closed socket, a queue that had not
/// drained, or a payload too large for a datagram. Counted because on an
/// unreliable carrier "we sent it" and "it left" are genuinely different
/// claims, and the gap between them is the one number that says the send path
/// is in trouble.
static PACKETS_UNSENT: AtomicU32 = AtomicU32::new(0);
/// Which encoding this client settled on. Stored as a
/// [`voice_proto::Encoding::code`] so it fits an atomic.
static CODEC: AtomicU32 = AtomicU32::new(0);
/// Which transport this call is on: 0 unknown, 1 WebSocket, 2 WebTransport.
static TRANSPORT: AtomicU32 = AtomicU32::new(0);
static PACKETS_IN: AtomicU32 = AtomicU32::new(0);
/// Frames of audio the codec invented because no packet was there to play —
/// concealed holes and starved pulls together. The number that says the link is
/// lossy, as distinct from `dropped`, which is packets that arrived and could
/// not be used.
static CONCEALED: AtomicU32 = AtomicU32::new(0);
/// The delay the jitter buffer is currently carrying, in milliseconds, for the
/// sender heard from most recently. One number for what is really a per-peer
/// quantity: as a meter it says whether the buffer is coping, and every peer on
/// one call is behind the same network.
static PLAYOUT_MS: AtomicU32 = AtomicU32::new(0);
/// Whether capture actually reaches this app on a thread of its own.
///
/// Reported rather than assumed. The whole point of moving to an
/// `AudioWorklet` is that a busy main thread can no longer cost us samples, and
/// "we did that" is worth exactly nothing unless it is checkable from outside.
static OFF_MAIN_THREAD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Times an output ran dry. After the move to the audio thread this can only
/// mean the producer could not keep up — the render callback itself cannot be
/// late any more.
static UNDERRUNS: AtomicU32 = AtomicU32::new(0);
/// Times a sender's stream restarted far enough that the jitter buffer began
/// again. One per rejoin is ordinary; anything more is not.
///
/// Surfaced because the failure it stands for looked completely healthy from
/// outside: packets sent, packets received, nothing unsent, and one listener
/// hearing silence for the rest of the call.
static RESYNCS: AtomicU32 = AtomicU32::new(0);
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
/// Whether this client is speaking right now.
///
/// Set from the capture chain — by the model's own verdict when there is a
/// model, and by level when there is not. See `suppress::Prepared::install`.
static SPEAKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// How much the model changed the last block, in dB. Diagnostics: a call that
/// sounds noisy and one where the model is doing nothing look identical from
/// outside, and this is what tells them apart.
static MODEL_GAIN_DB: AtomicU32 = AtomicU32::new(0);

/// Report voice activity from a capture chain.
///
/// Forced to "not speaking" while muted. The chain runs either way — it is
/// still measuring a live microphone — but a muted person's name lighting up
/// in everyone else's sidebar is a lie about what the room can hear, and this
/// verdict is broadcast.
pub fn report_activity(speaking: bool, model_gain_db: f32) {
    SPEAKING.store(speaking && !muted(), Ordering::Relaxed);
    MODEL_GAIN_DB.store(model_gain_db.to_bits(), Ordering::Relaxed);
}

/// Whether this microphone is muted.
static MUTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Stop transmitting, without stopping the stream.
///
/// An atomic rather than a method on [`Session`] because the capture callback
/// reads it on the audio path, where there is nothing to borrow and nobody to
/// lock against — the same reason every meter here is one.
///
/// Deliberately NOT reset by [`reset_stats`]: mute is a decision a person made
/// about their microphone, and a call that silently unmuted them by starting
/// is the one behaviour a mute button must never have.
pub fn set_muted(muted: bool) {
    MUTED.store(muted, Ordering::Relaxed);
}

/// Whether this microphone is muted.
pub fn muted() -> bool {
    MUTED.load(Ordering::Relaxed)
}

/// What conditioning the room is actually getting, as reported by the node.
///
/// Stored as a `Mode` discriminant rather than the mode this client ASKED for,
/// because the two differ whenever a node declines server-side suppression —
/// and that difference is the only thing that explains a call which still
/// sounds noisy with the feature switched on.
static SUPPRESSION: AtomicU32 = AtomicU32::new(0);

fn mode_code(mode: voice_dsp::Mode) -> u32 {
    match mode {
        voice_dsp::Mode::Off => 0,
        voice_dsp::Mode::Client => 1,
        voice_dsp::Mode::Server => 2,
    }
}

fn mode_of(code: u32) -> voice_dsp::Mode {
    match code {
        1 => voice_dsp::Mode::Client,
        2 => voice_dsp::Mode::Server,
        _ => voice_dsp::Mode::Off,
    }
}

/// Record one packet that reached the network.
///
/// Called from [`carrier`], which is where the two transports differ about
/// when that has actually happened: a WebSocket frame is counted when the
/// socket accepts it, a datagram when the send resolves.
pub(crate) fn record_sent(bytes: u32) {
    PACKETS_OUT.fetch_add(1, Ordering::Relaxed);
    BYTES_OUT.fetch_add(bytes, Ordering::Relaxed);
}

/// Record one packet that did not.
pub(crate) fn record_unsent() {
    PACKETS_UNSENT.fetch_add(1, Ordering::Relaxed);
}

/// Say something on the platform's console.
///
/// Not a logging framework — one line, used where a failure degrades instead
/// of propagating and would otherwise leave no trace at all.
pub(crate) fn web_log(message: &str) {
    #[cfg(target_arch = "wasm32")]
    web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(message));
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{message}");
}

/// Update a peak meter, holding the level and letting it fall.
///
/// Not `store(peak)`, and the difference became visible the moment packets
/// went from 85 ms to 20 ms: a meter showing only the newest chunk sits at zero
/// between syllables and reads as "nobody can hear me". See
/// [`voice_dsp::hold_peak`].
///
/// `dt_secs` is the audio the chunk covers, not wall time. The two agree while
/// a call is keeping up, and when it is not, metering against the audio is the
/// one that stays honest — a burst of late packets should not fast-forward the
/// meter's fall.
fn meter(cell: &AtomicU32, samples: &[f32], rate: u32, channels: u16) -> f32 {
    let frames = samples.len() / channels.max(1) as usize;
    let dt = frames as f32 / rate.max(1) as f32;
    let held = voice_dsp::hold_peak(
        f32::from_bits(cell.load(Ordering::Relaxed)),
        voice_proto::peak(samples),
        dt,
    );
    cell.store(held.to_bits(), Ordering::Relaxed);
    held
}

/// A snapshot of live counters, read once per frame by the UI.
#[derive(Clone, Copy, PartialEq)]
pub struct Stats {
    /// Peak amplitude of the most recent captured chunk, `0.0..=1.0`.
    pub mic_peak: f32,
    /// Peak amplitude of the most recent received chunk, `0.0..=1.0`.
    pub remote_peak: f32,
    /// Packets sent since join.
    pub sent: u32,
    /// Payload bytes sent since join. With `sent`, this is the outgoing
    /// bitrate — the number that says whether the codec is actually on.
    pub bytes_sent: u32,
    /// What this client is putting on the wire. Opus unless the capture format
    /// ruled it out; see [`voice_proto::codec::Encoded`].
    pub codec: voice_proto::Encoding,
    /// Which transport carries this call. `"websocket"` until a node offers
    /// WebTransport *and* this machine can actually reach it.
    pub transport: &'static str,
    /// Packets the transport refused to carry. Zero on a healthy call; a
    /// number that climbs is a send path that has stopped keeping up, which on
    /// a datagram carrier is otherwise indistinguishable from a quiet room.
    pub unsent: u32,
    /// Packets received since join.
    pub received: u32,
    /// Frames the receiver had to invent — a hole the codec concealed, or a
    /// pull with nothing behind it. Zero on a clean link; this is what a lossy
    /// one looks like from inside.
    pub concealed: u32,
    /// How much delay the jitter buffer is carrying right now, in milliseconds.
    /// It grows with measured jitter and is given back slowly, so a number that
    /// stays high is a network that stays bad.
    pub playout_ms: u32,
    /// Whether audio I/O runs on a thread of its own. False means the device
    /// fell back to the main thread, where a busy frame costs samples.
    pub off_main_thread: bool,
    /// Times an output ran dry since joining.
    pub underruns: u32,
    /// Times a sender's stream restarted and its buffer began again.
    pub resyncs: u32,
    /// Whether this microphone is muted. Reported so "the button says muted"
    /// and "nothing is being transmitted" can be checked as separate claims —
    /// they were separate for a long time, and only one of them was true.
    pub muted: bool,
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
    /// The conditioning the room is actually getting — the node's answer, not
    /// this client's request.
    pub suppression: voice_dsp::Mode,
    /// Whether this client is speaking, as the capture chain sees it.
    pub speaking: bool,
    /// How much the model changed the last block, in dB. Zero when there is no
    /// model in the chain.
    pub model_gain_db: f32,
}

/// Read the atomics. Cheap enough to call every frame.
pub fn stats() -> Stats {
    Stats {
        mic_peak: f32::from_bits(MIC_PEAK.load(Ordering::Relaxed)),
        remote_peak: f32::from_bits(REMOTE_PEAK.load(Ordering::Relaxed)),
        sent: PACKETS_OUT.load(Ordering::Relaxed),
        bytes_sent: BYTES_OUT.load(Ordering::Relaxed),
        codec: voice_proto::Encoding::from_code(CODEC.load(Ordering::Relaxed) as u8)
            .unwrap_or(voice_proto::Encoding::PcmS16Le),
        transport: match TRANSPORT.load(Ordering::Relaxed) {
            2 => "webtransport",
            1 => "websocket",
            _ => "none",
        },
        unsent: PACKETS_UNSENT.load(Ordering::Relaxed),
        received: PACKETS_IN.load(Ordering::Relaxed),
        concealed: CONCEALED.load(Ordering::Relaxed),
        playout_ms: PLAYOUT_MS.load(Ordering::Relaxed),
        off_main_thread: OFF_MAIN_THREAD.load(Ordering::Relaxed),
        underruns: UNDERRUNS.load(Ordering::Relaxed),
        resyncs: RESYNCS.load(Ordering::Relaxed),
        muted: muted(),
        dropped: PACKETS_DROPPED.load(Ordering::Relaxed),
        peers: PEER_COUNT.load(Ordering::Relaxed),
        sample_rate: SAMPLE_RATE.load(Ordering::Relaxed),
        duplicate_id: DUPLICATE_ID.load(Ordering::Relaxed),
        no_capacity: NO_CAPACITY.load(Ordering::Relaxed),
        connect_failed: CONNECT_FAILED.load(Ordering::Relaxed),
        redirects: REDIRECTS.load(Ordering::Relaxed),
        suppression: mode_of(SUPPRESSION.load(Ordering::Relaxed)),
        speaking: SPEAKING.load(Ordering::Relaxed),
        model_gain_db: f32::from_bits(MODEL_GAIN_DB.load(Ordering::Relaxed)),
    }
}

fn reset_stats() {
    for c in [
        &MIC_PEAK, &REMOTE_PEAK, &PACKETS_OUT, &BYTES_OUT, &PACKETS_UNSENT, &PACKETS_IN,
        &PACKETS_DROPPED, &CONCEALED, &PLAYOUT_MS, &UNDERRUNS, &RESYNCS, &PEER_COUNT,
        &SAMPLE_RATE, &CODEC, &TRANSPORT,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    DUPLICATE_ID.store(false, Ordering::Relaxed);
    NO_CAPACITY.store(false, Ordering::Relaxed);
    CONNECT_FAILED.store(false, Ordering::Relaxed);
    REDIRECTS.store(0, Ordering::Relaxed);
    SUPPRESSION.store(0, Ordering::Relaxed);
    SPEAKING.store(false, Ordering::Relaxed);
    MODEL_GAIN_DB.store(0.0f32.to_bits(), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Per-peer receive path
// ---------------------------------------------------------------------------

/// One remote participant's playback chain.
struct PeerSink {
    /// This sender's own output device. Dropping it detaches the node and
    /// stops the sound.
    ///
    /// One per sender rather than a mix, because each carries its own gain and
    /// its own clock. They share an `AudioContext` — `audio_io` keeps one for
    /// the process, which is what stops a seven-person room hitting the
    /// browser's limit on concurrent contexts.
    output: audio_io::Output,
    /// This listener's gain on this sender. See [`voice_dsp::Fader`] — the gain
    /// is applied to the PCM rather than to an output element, so it works
    /// above unity and on platforms that have no element.
    fader: voice_dsp::Fader,
    /// This sender's jitter buffer, holding their packets still encoded and
    /// decoding at playout. Built from their first packet, because that packet
    /// is what says the rate, the channel count and how much audio a frame is.
    ///
    /// Per sender for the same reason the decoder inside it is: sequence
    /// numbers, arrival timing and codec state all belong to one talker, and
    /// two talkers sharing any of the three would be nonsense.
    buffer: Option<jitter::Buffer<voice_proto::codec::Playout>>,
    /// The format the buffer was built for. A sender that changes any of it
    /// invalidates everything the buffer knows.
    format: (u32, u16, usize),
    /// Reused pull target, so playout allocates nothing per frame.
    frame: Vec<f32>,
    /// How much to pull, and when. Built with the buffer, because its frame
    /// length is the buffer's. See [`jitter::Clock`] and [`PeerSink::feed`].
    clock: Option<jitter::Clock>,
    /// Concealed + starved frames already reported to [`CONCEALED`], so the
    /// global counter can be advanced by the difference rather than recomputed.
    reported_conceal: u64,
    /// Restarts already reported to [`RESYNCS`], for the same reason.
    reported_resyncs: u64,
    /// Underruns already reported to [`UNDERRUNS`], for the same reason.
    reported_underruns: u32,
}

impl PeerSink {
    /// Open an output for `sender` and route it to the speakers.
    ///
    /// Async, unlike the version this replaces, because loading the worklet
    /// module is. That is a real consequence and it shaped the caller: `pump`
    /// now opens the sink BEFORE it borrows the peer map, because holding a
    /// `RefCell` borrow across an await is how a reactive app panics on its own
    /// re-entry.
    ///
    /// `gain` is applied from the first sample, not ramped into: peers appear
    /// when they first speak, and someone who was turned down before they
    /// arrived must not be loud for the first 20 ms of the word that announced
    /// them.
    async fn open(sender: u32, rate: u32, gain: f32) -> Result<PeerSink, String> {
        let output = audio_io::Output::open(audio_io::OutputConfig {
            sample_rate: None,
            channels: 1,
            latency: audio_io::Latency::Interactive,
            // TWO packets, and the second one is the whole point. The producer
            // here is the jitter buffer, driven by arrivals: it hands over a
            // packet of audio at an instant and then nothing until the next one
            // lands ~20 ms later. A cushion equal to that burst drains to
            // exactly zero on every cycle, so the trough touches empty and the
            // first one counts as an underrun — measured, at one per output.
            // A cushion of two keeps the trough a full burst above the floor.
            //
            // Paying for smoothing twice, and knowingly: the jitter buffer
            // above already holds 20 ms. The architectural fix is for the
            // buffer to be pulled by the audio clock rather than by arrivals,
            // which means living in the worklet beside the output — see
            // `audio_io::Pipeline`. Until then this is the honest number.
            cushion_frames: Some(2 * PACKET_MS * 48_000 / 1000),
        })
        .await
        .map_err(|e| format!("peer {sender}: {e}"))?;
        Ok(PeerSink {
            output,
            fader: voice_dsp::Fader::new(rate, gain),
            buffer: None,
            format: (0, 0, 0),
            frame: Vec::new(),
            clock: None,
            reported_conceal: 0,
            reported_resyncs: 0,
            reported_underruns: 0,
        })
    }

    /// Take one packet and hand back whatever audio is now due.
    ///
    /// # Why the arrival drives the playout
    ///
    /// A jitter buffer is *pulled* on a playout clock, and this platform has
    /// nothing to run one on: `runtime_core` has no sleep, and both
    /// `requestAnimationFrame` and `setInterval` are throttled to about once a
    /// second in a background tab — which is precisely when a call has to keep
    /// working. So the arrival is the trigger and the clock is the pacer: each
    /// packet pulls however many frames real time has moved on by, and the
    /// sink's own ring buffer downstream absorbs the burstiness. Smoothing a
    /// bursty producer is what that ring is for; it was standing in for a jitter
    /// buffer before this, and it is now doing the job it is actually good at.
    ///
    /// The consequence worth knowing: when nothing arrives, nothing is
    /// produced. A talker who stops mid-word gets concealment on the packets
    /// that follow the gap, and silence — not a concealed drone — for a stall
    /// with no end in sight. For a talker who simply stopped, silence is the
    /// right answer.
    ///
    /// Returns an empty vec when no frame is due yet, which is the ordinary
    /// case for the first packet of a call.
    fn feed(&mut self, packet: &Packet, now_ms: f64) -> Vec<f32> {
        let format = (packet.sample_rate, packet.channels, packet.sample_count as usize);
        if self.buffer.is_none() || self.format != format {
            let (rate, channels, frame) = format;
            let decoder = voice_proto::codec::Playout::open(rate, channels, frame);
            self.buffer =
                Some(jitter::Buffer::new(jitter::Tuning::default(), decoder, rate, channels));
            self.format = format;
            self.frame = vec![0.0; frame.max(1)];
            // The clock restarts with the buffer. Carrying the old reading over
            // would make the first packet of the new format look enormously
            // overdue and play a burst of concealment.
            let frame_ms = self.buffer.as_ref().expect("just built").frame_ms();
            self.clock = Some(jitter::Clock::new(frame_ms, MAX_CATCHUP, now_ms));
            self.reported_conceal = 0;
            self.reported_resyncs = 0;
        }
        let buffer = self.buffer.as_mut().expect("built above");
        let clock = self.clock.as_mut().expect("built with it");

        // A rejection is routine on this transport, not a fault: a late packet
        // is the ordinary cost of a link with jitter. The buffer counts each
        // kind, and `pump` folds the total into `dropped`.
        let held = buffer.push(packet.seq, &voice_proto::codec::Playout::tagged(packet), now_ms);

        let due = clock.due(now_ms);
        let mut pcm = Vec::with_capacity(due * self.frame.len());
        for _ in 0..due {
            buffer.pull(&mut self.frame);
            pcm.extend_from_slice(&self.frame);
        }

        // Reported by difference, because the buffer's counters are cumulative
        // and per peer while the meter is neither.
        let stats = buffer.stats();
        let invented = stats.concealed + stats.starved;
        CONCEALED.fetch_add(
            invented.saturating_sub(self.reported_conceal) as u32,
            Ordering::Relaxed,
        );
        self.reported_conceal = invented;
        RESYNCS.fetch_add(
            stats.resyncs.saturating_sub(self.reported_resyncs) as u32,
            Ordering::Relaxed,
        );
        self.reported_resyncs = stats.resyncs;
        PLAYOUT_MS.store(buffer.target_ms() as u32, Ordering::Relaxed);

        // The worklet's own count, reported the same way: by difference, since
        // it is cumulative and per output while the meter is neither.
        let underruns = self.output.underruns();
        UNDERRUNS.fetch_add(underruns.saturating_sub(self.reported_underruns), Ordering::Relaxed);
        self.reported_underruns = underruns;

        if held.is_err() {
            PACKETS_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        pcm
    }

    /// Whatever the decoder wants said, once.
    fn note(&self) -> Option<&str> {
        self.buffer.as_ref().and_then(|b| b.decoder().note())
    }
}

// ---------------------------------------------------------------------------
// Listener-side volume
// ---------------------------------------------------------------------------

/// What this machine does to what it receives, per sender.
///
/// Held apart from the peer map on purpose, and this is the load-bearing part:
/// a peer only exists once they have spoken, so a volume set before that has
/// nowhere to live. Keeping the wishes separate from the sinks means turning
/// someone down works whether or not they happen to be talking at the time.
struct Volumes {
    /// Applied to everyone. `1.0` is "as it arrived".
    master: f32,
    /// Applied to one sender on top of the master. Absent means `1.0`.
    per: HashMap<u32, f32>,
}

/// Unity, not zero. A derived `Default` would give a master of `0.0`, which is
/// a silent call that looks like a broken one — the wrong way round for a
/// setting whose absence means "no opinion".
impl Default for Volumes {
    fn default() -> Volumes {
        Volumes { master: 1.0, per: HashMap::new() }
    }
}

impl Volumes {

    /// The product, which is the only number the audio path ever sees.
    ///
    /// Multiplying rather than choosing is what makes the two controls
    /// composable: the master is headroom, and a person set above 100% spends
    /// it. It also means boosting somebody past unity is only possible while
    /// the master is *below* unity, which is a real limit and the reason this
    /// app ships a master default under 100%.
    fn gain_for(&self, sender: u32) -> f32 {
        self.master * self.per.get(&sender).copied().unwrap_or(1.0)
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

/// Open a connection to `node_url`, on the best carrier this machine can reach.
///
/// One function for the join and for a redirect, which is what keeps the two
/// from diverging: a redirect that silently dropped back to the WebSocket would
/// present as a call that got worse after a node handover, with nothing in the
/// UI to say so.
///
/// The WebSocket is not a failure path. UDP is blocked on plenty of networks
/// and a call that connects beats a call that is theoretically better.
async fn connect(
    node_url: &str,
    join: &Join<'_>,
) -> Result<(Publish, Receive), String> {
    #[cfg(target_arch = "wasm32")]
    {
        match crate::transport::choose(node_url, join).await {
            crate::transport::Choice::WebTransport(link) => {
                web_log("[voice] transport: webtransport");
                // One session, two halves. `Link` is a handle to a shared
                // session rather than a socket, so cloning it is how the send
                // task and the receive loop each own their own direction.
                return Ok((carrier::datagrams(link.clone()), Receive::Datagram(link)));
            }
            crate::transport::Choice::WebSocket(why) => {
                web_log(&format!("[voice] transport: websocket — {why}"));
            }
        }
    }

    let url = format!("{}/ws?{}", node_url.trim_end_matches('/'), join.query());
    let socket = WebSocket::connect(&url).await.map_err(|e| format!("connect {url}: {e}"))?;
    Ok((Publish::Socket(socket.sender()), Receive::Socket(socket)))
}

/// What [`Stats::transport`] will read back. See `TRANSPORT`.
fn transport_code(publish: &Publish) -> u32 {
    match publish.name() {
        "webtransport" => 2,
        _ => 1,
    }
}

/// Publish half, shared with the capture callback and swapped on a redirect.
///
/// `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` because on NATIVE the capture
/// callback runs on the audio thread, so `AudioStream::subscribe` requires a
/// `Send` closure. On web the bound is relaxed (one thread) and this is merely
/// redundant — but writing it for the stricter target keeps one code path
/// instead of a cfg-split over something this small.
///
/// [`Publish`] stays `Send` on every target, including the QUIC arm, because
/// the arm holds a queue rather than a session — the session lives on the task
/// that drains it. See [`carrier`].
type SharedSender = Arc<Mutex<Publish>>;

pub struct Session {
    /// Swapped when a redirect moves us to another node, so the capture
    /// callback keeps publishing without knowing the connection changed — or,
    /// now, without knowing which transport it changed to.
    sender: SharedSender,
    /// The raw microphone. Every capture chain reads from it.
    ///
    /// Replaceable, because the browser's own noise suppression is fixed when
    /// the device is opened — so changing where suppression runs means opening
    /// it again with different flags. See [`open_capture`].
    mic: RefCell<AudioStream>,
    /// What the packetiser reads, for the life of the call.
    ///
    /// The indirection is what makes suppression switchable mid-call: the thing
    /// that turns audio into packets subscribes ONCE, to this, and whichever
    /// chain is installed writes into it. Re-subscribing the packetiser instead
    /// would restart its sequence numbering, which is a gap to every receiver.
    _wire: AudioStream,
    /// The write end of `_wire`, handed to whichever chain is installed.
    wire_writer: AudioWriter,
    /// The stage between the microphone and the wire. Replaced by
    /// [`Session::install`]; dropping it stops that stage feeding the wire.
    chain: RefCell<Option<crate::suppress::Chain>>,
    _wire_sub: AudioSubscription,
    /// Shared with the receive task, which inserts new peers as they are heard.
    peers: Rc<RefCell<HashMap<u32, PeerSink>>>,
    /// This listener's volume wishes, shared with the receive task so a peer
    /// heard for the first time opens at the level they were already set to.
    volumes: Rc<RefCell<Volumes>>,
    /// Set when the session is torn down, so the receive task stops rather than
    /// resurrecting peers after a leave.
    closed: Rc<RefCell<bool>>,
}

impl Session {
    /// Output gain applied to every peer. `1.0` is "as it arrived".
    ///
    /// This is the listener's own decision and travels nowhere: nobody else in
    /// the room can tell it happened, which is the difference between it and
    /// every other stage in `voice_dsp`.
    pub fn set_volume(&self, volume: f32) {
        self.volumes.borrow_mut().master = volume.max(0.0);
        self.apply_volumes();
    }

    /// Output gain applied to one sender, on top of the master.
    ///
    /// Takes effect over a few milliseconds rather than instantly — see
    /// [`voice_dsp::Fader`] — so dragging a slider is a slide and not a burst
    /// of clicks.
    pub fn set_peer_volume(&self, sender: u32, volume: f32) {
        self.volumes.borrow_mut().per.insert(sender, volume.max(0.0));
        self.apply_volumes();
    }

    /// The gain currently asked for on one sender, ignoring the master.
    pub fn peer_volume(&self, sender: u32) -> f32 {
        self.volumes.borrow().per.get(&sender).copied().unwrap_or(1.0)
    }

    /// Push the current wishes onto whichever peers are actually here.
    ///
    /// Peers that are not are unaffected by design: their fader is built from
    /// the same table when they arrive.
    fn apply_volumes(&self) {
        let volumes = self.volumes.borrow();
        for (sender, peer) in self.peers.borrow_mut().iter_mut() {
            peer.fader.set_gain(volumes.gain_for(*sender));
        }
    }

    /// The raw capture stream, cloned.
    ///
    /// Exposed so a replacement chain can be BUILT before this session is
    /// borrowed to install it — the build is async and holding a `RefCell`
    /// borrow across an await is how a reactive app panics on its own
    /// re-entry.
    pub fn capture(&self) -> AudioStream {
        self.mic.borrow().clone()
    }

    /// Where a capture chain writes.
    pub fn wire(&self) -> AudioWriter {
        self.wire_writer.clone()
    }

    /// Put `chain` between the microphone and the wire, in place of whatever
    /// was there.
    ///
    /// The old chain is dropped FIRST. Installing before dropping would leave
    /// two stages writing into the same wire for an instant, and the far side
    /// would hear the overlap as a stutter — the one artefact a noise
    /// suppression setting must not produce on its way to being switched on.
    /// `mic` replaces the capture device — it must be the one `chain` reads
    /// from, or the new stage will be fed by a microphone that is about to be
    /// dropped.
    pub fn install(
        &self,
        mic: AudioStream,
        chain: crate::suppress::Chain,
        mode: voice_dsp::Mode,
    ) {
        {
            let mut slot = self.chain.borrow_mut();
            slot.take();
            *slot = Some(chain);
        }
        // After the chain, so the old device is released only once nothing is
        // reading it. Dropping it first would silence the call for as long as
        // the swap takes.
        *self.mic.borrow_mut() = mic;
        // Only the client half of the story: `Mode::Server` is settled at
        // connect and reported by the node, so it is not something this can
        // claim. See `voice::set_suppression`.
        SUPPRESSION.store(mode_code(mode), Ordering::Relaxed);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        *self.closed.borrow_mut() = true;
        if let Ok(mut publish) = self.sender.lock() {
            publish.close();
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

/// Open the microphone with the processing flags this mode wants.
///
/// `mono()` keeps the wire cost down; the rate is whatever the device gives,
/// and it travels in every packet so nothing resamples.
///
/// These processing flags are WEB-ONLY in the microphone SDK (Android is a
/// documented no-op, and there is no Apple VoiceProcessingIO path), so on
/// native none of this applies — that is the `audio-processing` gap.
///
/// Echo cancellation and noise suppression come apart here. AEC stays on for
/// every real call: it needs the far-end reference signal, which only the
/// browser has, and DeepFilterNet explicitly does not do it. The browser's
/// NOISE suppression stands down the moment a neural pass is going to run,
/// because running both is worse than running either — see `voice_dsp`.
///
/// Public because that last rule is why changing the mode on a live call has
/// to reopen the device rather than just swapping the chain: whether WebRTC is
/// suppressing is a property of the captured track, fixed when it was opened.
/// A swap that only changed the neural stage would leave the two stacked,
/// which is the one combination the whole design exists to prevent. Reopening
/// does not prompt — the permission has already been granted.
pub async fn open_capture(
    capture: Capture,
    mode: voice_dsp::Mode,
) -> Result<AudioStream, String> {
    let config = audio_io::CaptureConfig {
        // Left to the crate, which asks for 48 kHz. Deliberately not the
        // device's own rate: 44.1 kHz hardware is real, Opus takes 8, 12, 16,
        // 24 or 48 kHz and nothing else, and one resample inside the audio
        // graph is far cheaper than discovering at encode time that the codec
        // will not start and sending raw PCM instead.
        sample_rate: None,
        channels: 1,
        // The intent, not a block size. On this backend it is 256 frames
        // (~5 ms); on a `ScriptProcessorNode` it would have been 1024, and a
        // caller cannot know which it got. See `audio_io::Latency`.
        latency: audio_io::Latency::Interactive,
        echo_cancellation: capture == Capture::Processed,
        noise_suppression: capture == Capture::Processed && mode.browser_noise_suppression(),
        auto_gain_control: true,
    };

    // The compat tap. `AudioStream` stays the currency everything downstream
    // speaks — the suppression chain, `denoise`, the packetiser — so replacing
    // the capture BACKEND changes none of them. What changed is that the
    // samples reached here having never been at risk from a busy main thread.
    let (stream, writer) = AudioStream::new();
    let device = audio_io::Capture::open(
        config,
        // Empty for now. This is where a stage belongs once the graph can run
        // in the worklet — see `audio_io::Pipeline` for why that day needs no
        // change here.
        audio_io::Pipeline::new(),
        move |pcm, rate, channels, _frames_elapsed| {
            writer.write_pcm_f32(rate, channels, pcm);
        },
    )
    .await
    .map_err(|e| format!("microphone: {e}"))?;

    let opened = device.opened();
    OFF_MAIN_THREAD.store(opened.off_main_thread, Ordering::Relaxed);
    web_log(&format!(
        "[voice] capture: {} Hz, {} frames ({:.1} ms), off main thread: {}",
        opened.sample_rate,
        opened.block_frames,
        opened.block_ms(),
        opened.off_main_thread,
    ));

    // The device lives exactly as long as the stream does. `attach_stopper`
    // runs when the last clone drops, which is the same contract the capture
    // stream already had — so nothing above here has to learn that the device
    // is now a separate object.
    stream.attach_stopper(move || drop(device));
    Ok(stream)
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
    mode: voice_dsp::Mode,
    ticket: Ticket<'_>,
) -> Result<Session, String> {
    reset_stats();

    // 1. Capture, configured for this mode. See `open_capture`.
    let mic = open_capture(capture, mode).await?;

    // 1b. The wire: one stream, for the life of the call, that the packetiser
    //     reads. What feeds it is swappable — see `Session::install`.
    //
    //     Default buffering, and it costs nothing: this stream is only ever
    //     subscribed to, never handed to a sink, so it never builds a WebAudio
    //     graph and `Playout` never applies to it. Only a stream that reaches
    //     `native_source()` pays for its output block — here, that is
    //     `PeerSink`.
    let (wire, wire_writer) = AudioStream::new();

    //     A call always starts on the direct chain, even when suppression is
    //     switched on. That is a code-splitting decision, not an audio one:
    //     DeepFilterNet lives in a lazily-loaded wasm chunk, and a `join` that
    //     could reach it would pull the whole inference engine back into the
    //     main bundle for every visitor. The app swaps the neural chain in from
    //     inside the chunk — see `hearth::views::suppressor`.
    //
    //     The microphone is already opened with the right flags for `mode`, so
    //     the swap does not have to reopen it at join time.
    let chain = crate::suppress::direct(&mic, wire_writer.clone());

    // 2. Placement. A `ws://` origin bypasses the director for single-node use;
    //    anything else is asked where the room lives.
    let node_url = if director_url.starts_with("ws://") || director_url.starts_with("wss://") {
        director_url.trim_end_matches('/').to_string()
    } else {
        resolve(director_url, room).await?
    };

    // 3. Transport. Datagrams where they work, the WebSocket where they do
    //    not — and the choice is made by DIALLING, not by asking, because a
    //    published endpoint is not a reachable one. Deciding here, before any
    //    audio path exists, makes a blocked UDP port a quiet fallback rather
    //    than a call that connects and then has nowhere to put its audio.
    let join = Join { room, id, suppress: mode, ticket };
    let (publish, receive) = connect(&node_url, &join).await?;
    TRANSPORT.store(transport_code(&publish), Ordering::Relaxed);
    let sender: SharedSender = Arc::new(Mutex::new(publish));

    // 4. OUT: every captured block is re-cut into PACKET_MS frames, and each
    //    of those becomes one packet.
    //
    //    The capture block is whatever the platform chose — 4096 frames on web,
    //    85 ms — and one packet per block is a size no datagram can carry. The
    //    packetiser is therefore not a QUIC detail: it is what makes the stream
    //    describable in units a transport, a codec and a jitter buffer all
    //    agree on. It costs nothing on the WebSocket path and finer loss
    //    granularity is a straight gain there too.
    let mic_sub = {
        let sender = sender.clone();
        let seq = std::cell::Cell::new(0u32);
        // Built on the first block, because the block is what says what rate
        // and channel count the microphone actually gave us.
        let cutter: RefCell<Option<(voice_proto::Packetiser, Encoded)>> = RefCell::new(None);
        // Reused, so muting allocates nothing per block on the audio path.
        let silence: RefCell<Vec<f32>> = RefCell::new(Vec::new());
        wire.subscribe(move |frame: &AudioFrame| {
            // What is actually LEAVING, so a muted person's own level meter
            // falls to zero. A meter that kept moving while muted would be the
            // same lie the button used to tell.
            //
            // Metered against a silent buffer of the REAL length, not an empty
            // one: `hold_peak` takes its fall from the audio's own duration, and
            // a zero-length block is zero seconds, which it holds through rather
            // than dividing by. An empty slice freezes the meter at whatever it
            // last read — which looked exactly like mute not working.
            let outgoing = muted().then(|| {
                let mut silence = silence.borrow_mut();
                silence.clear();
                silence.resize(frame.samples.len(), 0.0);
            });
            let borrowed = silence.borrow();
            let samples: &[f32] = if outgoing.is_some() { &borrowed } else { frame.samples };
            meter(&MIC_PEAK, samples, frame.sample_rate, frame.channels);
            SAMPLE_RATE.store(frame.sample_rate, Ordering::Relaxed);

            /// Cut and encoder for one capture format, built together because
            /// the encoder has to agree with the packetiser about frame size.
            fn open(rate: u32, channels: u16) -> (voice_proto::Packetiser, Encoded) {
                let cutter = voice_proto::Packetiser::new(PACKET_MS, rate, channels);
                let wire = Encoded::open(rate, channels, cutter.frame());
                CODEC.store(wire.encoding().code() as u32, Ordering::Relaxed);
                // Said once, at the point the choice is made. A fallback that
                // went unlogged would present later as "voice uses a lot of
                // bandwidth on this machine" with nothing to connect it to.
                if let Some(note) = wire.note() {
                    web_log(&format!("[voice] {note}"));
                }
                (cutter, wire)
            }

            let mut slot = cutter.borrow_mut();
            let (cutter, codec) =
                slot.get_or_insert_with(|| open(frame.sample_rate, frame.channels));
            // A microphone that changes format mid-call would desynchronise the
            // cut and invalidate the encoder; rebuild both rather than emit
            // frames of the wrong size.
            if cutter.rate() != frame.sample_rate || cutter.channels() != frame.channels {
                let (fresh_cutter, fresh_codec) = open(frame.sample_rate, frame.channels);
                *cutter = fresh_cutter;
                *codec = fresh_codec;
            }

            // Muted sends SILENCE, not nothing.
            //
            // Dropping the packets instead would leave a hole in the far side's
            // jitter buffer, and a jitter buffer conceals a hole by asking the
            // codec to invent a plausible continuation — so a mute would come
            // out the other end as muttering. Sending silence keeps the
            // sequence unbroken, and DTX makes it cost a byte or two a frame.
            cutter.push(samples, frame.pts_micros);
            drop(borrowed);
            let rate = frame.sample_rate;
            let channels = frame.channels;
            cutter.drain(|samples, pts_micros| {
                let n = seq.get();
                seq.set(n.wrapping_add(1));

                let packet = codec.packet(id, n, rate, channels, pts_micros, samples);
                // Whether that reaches anyone is the carrier's business, and so
                // is counting it: the two transports disagree about when a
                // packet has actually left. See `carrier::Publish::binary`.
                if let Ok(mut publish) = sender.lock() {
                    publish.binary(packet.encode());
                } else {
                    record_unsent();
                }
            });
        })
    };

    // 5. IN: drive the socket, routing each sender to its own playback chain,
    //    following a redirect if this node turns out not to own the room.
    let peers: Rc<RefCell<HashMap<u32, PeerSink>>> = Rc::new(RefCell::new(HashMap::new()));
    let volumes = Rc::new(RefCell::new(Volumes::default()));
    let closed = Rc::new(RefCell::new(false));
    spawn_receive_loop(
        receive,
        room.to_string(),
        id,
        mode,
        ticket.map(str::to_string),
        sender.clone(),
        peers.clone(),
        volumes.clone(),
        closed.clone(),
    );

    Ok(Session {
        sender,
        mic: RefCell::new(mic),
        _wire: wire,
        wire_writer,
        chain: RefCell::new(Some(chain)),
        _wire_sub: mic_sub,
        peers,
        volumes,
        closed,
    })
}

/// Own the connection and pump it until it closes, following redirects.
///
/// The future is `!Send` (it holds `Rc` and `!Send` AudioStreams) and lives as
/// long as the page, which is exactly what `spawn_async` supports on web. It
/// relies on a backend-installed async executor: without one, the native
/// fallback is `pollster::block_on`, which would block the calling thread on a
/// loop that never ends. Step 1 targets web only for this reason.
fn spawn_receive_loop(
    receive: Receive,
    room: String,
    id: u32,
    // Re-presented on a redirect for the same reason the ticket is: the node
    // we are sent to makes its own decision, and dropping the request here
    // would silently turn server-side suppression off mid-call.
    mode: voice_dsp::Mode,
    // Carried so a redirect can re-present it. The node we are sent to runs the
    // same check as the one that sent us, so dropping the ticket here turns the
    // cold-room race into an authentication failure — which would look exactly
    // like a flaky relay and only ever happen under load.
    ticket: Option<String>,
    sender: SharedSender,
    peers: Rc<RefCell<HashMap<u32, PeerSink>>>,
    volumes: Rc<RefCell<Volumes>>,
    closed: Rc<RefCell<bool>>,
) {
    runtime_core::driver::spawn_async(async move {
        let mut receive = receive;
        // Bounded: a redirect loop between two nodes that each disown the room
        // would otherwise spin forever. Three hops is far more than the one the
        // cold-room race can produce.
        let mut hops = 0;
        loop {
            match pump(&mut receive, &peers, &volumes, &closed).await {
                Pump::Finished => break,
                Pump::Redirect(url) if hops < 3 => {
                    hops += 1;
                    let join = Join {
                        room: &room,
                        id,
                        suppress: mode,
                        ticket: ticket.as_deref(),
                    };
                    // The full choice again, against the node we are being sent
                    // to: its WebTransport port and certificate are its own, so
                    // there is nothing about the old connection worth reusing.
                    match connect(&url, &join).await {
                        Ok((publish, next)) => {
                            // Swap the publish half so the capture callback
                            // keeps working without knowing anything moved.
                            // Assigning drops the old half, which is what
                            // closes the connection we are leaving — including
                            // the QUIC send task, whose queue this was.
                            if let Ok(mut slot) = sender.lock() {
                                TRANSPORT.store(transport_code(&publish), Ordering::Relaxed);
                                *slot = publish;
                            }
                            receive = next;
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
    /// Connection closed, or the session was torn down.
    Finished,
    /// This node does not own the room; reconnect at this URL.
    Redirect(String),
}

/// Drain one connection until it closes or hands us a redirect.
async fn pump(
    receive: &mut Receive,
    peers: &Rc<RefCell<HashMap<u32, PeerSink>>>,
    volumes: &Rc<RefCell<Volumes>>,
    closed: &Rc<RefCell<bool>>,
) -> Pump {
    loop {
        let incoming = receive.next().await;
        if *closed.borrow() {
            return Pump::Finished;
        }
        let bytes = match incoming {
            Incoming::Binary(bytes) => bytes,
            Incoming::Closed => return Pump::Finished,
            Incoming::Text(text) => {
                // Notices, never audio. Reliable on both carriers — on QUIC
                // these arrive on a control stream rather than as datagrams,
                // because a lost redirect is not a glitch, it is a client that
                // spends the rest of the call talking to the wrong node.
                if let Some(url) = text.strip_prefix("redirect:") {
                    return Pump::Redirect(url.to_string());
                }
                if text.contains("duplicate-id") {
                    DUPLICATE_ID.store(true, Ordering::Relaxed);
                }
                if text.contains("no-capacity") {
                    NO_CAPACITY.store(true, Ordering::Relaxed);
                }
                // The node's answer to what we asked for. Overwrites the
                // optimistic value set at join, so a refused server-side
                // request shows as `off` rather than as what we hoped for.
                if let Some(mode) = text.strip_prefix("info:suppress=") {
                    SUPPRESSION.store(mode_code(voice_dsp::Mode::parse(mode)), Ordering::Relaxed);
                }
                continue;
            }
        };

        let packet = match Packet::decode(&bytes) {
            Ok(p) => p,
            Err(_) => {
                PACKETS_DROPPED.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // The gain this listener wants on this sender, read fresh: it can be
        // changed from the UI between any two packets.
        let gain = volumes.borrow().gain_for(packet.sender);

        // Opened BEFORE the map is borrowed, because opening is async now —
        // the worklet module has to load — and holding a `RefCell` borrow
        // across an await is how a reactive app panics on its own re-entry.
        if !peers.borrow().contains_key(&packet.sender) {
            match PeerSink::open(packet.sender, packet.sample_rate, gain).await {
                Ok(sink) => {
                    // The await above is a suspension point: the session can
                    // have been torn down, or this sender heard from twice,
                    // while it was running.
                    if *closed.borrow() {
                        return Pump::Finished;
                    }
                    let mut map = peers.borrow_mut();
                    if map.insert(packet.sender, sink).is_none() {
                        PEER_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    // No output on this platform (native today). Count it and
                    // keep draining so the connection does not stall.
                    web_log(&format!("[voice] {e}"));
                    PACKETS_DROPPED.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        }

        let mut map = peers.borrow_mut();
        let sink = map.get_mut(&packet.sender).expect("opened just above");

        // A sender that changes rate mid-call needs new coefficients, not
        // re-used ones — every time constant in the fader is derived from it.
        // Rebuilding settled at the current gain rather than at the wish keeps
        // a rate change from also being a volume step.
        if sink.fader.rate() != packet.sample_rate {
            sink.fader = voice_dsp::Fader::new(packet.sample_rate, gain);
        }

        // Counted on ARRIVAL, not on playout. A packet that arrives while the
        // buffer is still filling has been received; counting at the far end of
        // the buffer instead would make a healthy call look lossy for its first
        // few packets, and would count nothing at all for a packet held and
        // played as part of a later batch.
        PACKETS_IN.fetch_add(1, Ordering::Relaxed);

        // Held, ordered, and played out on a clock — not decoded on arrival.
        // On the WebSocket there was nothing to reorder and nothing to lose, so
        // arrival order WAS playout order; on datagrams it is not, and this is
        // the piece that makes the difference audible as a concealed frame
        // rather than a click. See `PeerSink::feed`.
        let pcm = sink.feed(&packet, now_ms());
        if let Some(note) = sink.note() {
            web_log(&format!("[voice] {note}"));
        }
        // Nothing due yet: the buffer is still filling, which is the ordinary
        // case for the first packet or two of a call.
        if pcm.is_empty() {
            continue;
        }
        let mut pcm = pcm;

        sink.fader.set_gain(gain);
        sink.fader.process(&mut pcm);

        // Metered AFTER the fader, because this meter is "what you are
        // hearing". A peak that ignored the listener's own volume would report
        // a loud room to somebody who had turned it down.
        meter(&REMOTE_PEAK, &pcm, packet.sample_rate, packet.channels);

        // Straight to the device. `write_at` resamples if this sender's rate
        // is not the output's, which is what keeps a 44.1 kHz caller from
        // playing back fast in a room that is otherwise at 48.
        //
        // The packet's own `pts_micros` is deliberately unused here — it is
        // the sender's timeline, and reconciling the two is the jitter
        // buffer's job, which it does by sequence number.
        #[cfg(target_arch = "wasm32")]
        sink.output.write_at(&pcm, packet.sample_rate);
        #[cfg(not(target_arch = "wasm32"))]
        sink.output.write(&pcm);
    }
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
