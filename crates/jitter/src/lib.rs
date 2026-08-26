//! Turning packets that arrive whenever they like into audio that plays on time.
//!
//! Two clocks that nothing keeps in step: the sender's, which emits a packet
//! every frame, and the receiver's, which consumes one every frame. Between
//! them is a network that delays each packet by a different amount, sometimes
//! reorders them, sometimes duplicates them, and sometimes loses them outright.
//!
//! # Why this suddenly matters
//!
//! It did not, while the transport was TCP: TCP delivers everything, in order,
//! exactly once, and pays for that with head-of-line blocking. The receiving
//! code could therefore assume a packet stream that never lost or reordered
//! anything, and it did — the sequence number was written into every packet
//! header and never read.
//!
//! QUIC datagrams withdraw all three guarantees, deliberately, because for
//! speech a packet that arrives late is worthless and a packet that arrives
//! late *and* delayed everything behind it is worse than worthless. What the
//! transport stops doing, this has to start doing.
//!
//! # The three failures, and what each needs
//!
//! **Reordering** needs a window. Packets are held briefly and played in
//! sequence order rather than arrival order, which costs exactly as much delay
//! as the window is wide.
//!
//! **Loss** needs concealment — see [`Concealer`]. Silence is the wrong answer:
//! a hole in a vowel is a click, and clicks are what the rest of this stack
//! spends its time removing.
//!
//! **Delay variation** needs the window to be the right width, which is not a
//! constant. Too narrow and normal jitter reads as loss; too wide and the call
//! is needlessly laggy. So the target adapts to what the network is actually
//! doing — see [`Buffer::target_ms`].
//!
//! # What this deliberately does not do
//!
//! It does not resample, so it cannot correct **clock drift** between the two
//! machines. A few hundred parts per million is normal, which is a sample every
//! few seconds; over a long call that either drains the buffer or grows it. The
//! honest fix is a fractional resampler driven by the buffer's own depth, and
//! it is not here yet. [`Stats::drift_events`] counts the symptom so that the
//! absence is visible rather than mysterious.

#![forbid(unsafe_code)]

mod clock;
mod conceal;
mod pcm;

pub use clock::Clock;
pub use conceal::Concealer;
pub use pcm::Pcm;

use std::collections::VecDeque;

/// Turning one packet into one frame of audio — and, crucially, turning the
/// ABSENCE of a packet into one frame of audio too.
///
/// The buffer holds packets still encoded and decodes them at playout, rather
/// than decoding on arrival. That ordering is what makes this trait the right
/// place for concealment: a codec conceals a gap far better than anything
/// downstream can, because it still has the signal model — Opus will resynthesise
/// a plausible continuation from its own decoder state, where a PCM concealer
/// can only repeat what it heard. Handing loss to whoever holds the model is
/// the difference between a dropout you notice and one you do not.
///
/// It also keeps this crate dependency-free, which is the other half of why it
/// is a trait: the buffer never needs to know what a packet contains.
pub trait Decode {
    /// Samples per decoded frame, interleaved across channels. Fixed for the
    /// life of the stream — every time constant in the buffer is derived from
    /// it.
    fn frame(&self) -> usize;

    /// Decode one packet into `out`, which is exactly [`Decode::frame`] long.
    ///
    /// `None` means the packet is not coming: conceal. Implementations must
    /// still fill `out` — the playback side asked on its own clock and
    /// something has to come back.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]);
}

/// How the buffer decides how much delay to carry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Never hold less than this, however calm the network looks. A buffer
    /// that trusts a quiet moment is a buffer that stutters at the first
    /// hiccup.
    pub min_ms: f32,
    /// Never hold more than this, however bad it looks. Past a point a call is
    /// better off dropping audio than becoming a walkie-talkie.
    pub max_ms: f32,
    /// Multiple of measured jitter to carry as headroom. 3 covers the vast
    /// majority of a typical delay distribution without chasing its tail.
    pub jitter_factor: f32,
    /// Time constant for *growing* the target. Short: arriving late is
    /// audible immediately and the buffer should react at once.
    pub grow_ms: f32,
    /// Time constant for *shrinking* it. Long, because shrinking is how delay
    /// is recovered and doing it eagerly re-creates the problem. Delay is only
    /// ever given back during silence — see [`Buffer::pull`].
    pub shrink_ms: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            // Two frames at 10 ms. One is not a buffer, it is a hope.
            min_ms: 20.0,
            // 200 ms of mouth-to-ear is already the edge of comfortable
            // conversation, and the rest of the pipeline spends most of it.
            max_ms: 120.0,
            jitter_factor: 3.0,
            grow_ms: 50.0,
            shrink_ms: 4000.0,
        }
    }
}

/// What the buffer has been doing, for meters and for tests.
///
/// Every field is a count of something that actually happened rather than a
/// rate, because a rate hides the difference between "one bad second" and
/// "consistently bad", and those want different responses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets handed over by the transport.
    pub received: u64,
    /// Packets that arrived after their turn had already been played, and were
    /// therefore thrown away. This is the number that says the buffer is too
    /// shallow.
    pub late: u64,
    /// Packets that arrived twice.
    pub duplicate: u64,
    /// Packets that arrived out of order but early enough to still be used.
    /// Not a failure — this is the buffer doing its job.
    pub reordered: u64,
    /// Frames played that no packet was available for, concealed instead.
    pub concealed: u64,
    /// Frames the buffer emitted because it had nothing at all — a starved
    /// buffer rather than a lost packet. Distinct from `concealed` because the
    /// causes are different: this one means the sender stopped, or the target
    /// is too shallow for the link.
    pub starved: u64,
    /// Times the sequence jumped so far that the buffer concluded it was
    /// looking at a different stream and started over. See
    /// [`RESYNC_FRAMES`].
    ///
    /// Should be zero, or one per rejoin. Anything else means sequence numbers
    /// are not what this buffer thinks they are.
    pub resyncs: u64,
    /// Times the buffer had to discard audio to stop unbounded growth. With a
    /// healthy link this stays at zero; a number that climbs steadily is clock
    /// drift, which this crate cannot correct — see the module docs.
    pub drift_events: u64,
}

/// How far a packet may sit from the expected one before the buffer concludes
/// it is a *different stream* rather than a late or early one.
///
/// Two hundred frames — four seconds at 20 ms — which is orders of magnitude
/// beyond any reorder a network produces and comfortably beyond the buffer's
/// own ceiling.
///
/// Without this the buffer deadlocks, and it did, in a real call. A sender's
/// sequence restarts whenever its session does: a rejoin, a redirect to another
/// node, a reconnect. The participant id survives all three, so the receiver
/// keeps the same buffer, its `next` is still up in the thousands, and every
/// packet of the new stream is refused as [`Rejected::Late`].
///
/// The refusal is self-sustaining, which is what makes it fatal rather than
/// noisy: `next` only ever advances when a packet PLAYS, and nothing can play
/// while everything is being refused. That listener is deaf for the rest of the
/// call, in one direction only, with every counter except this one looking
/// healthy.
pub const RESYNC_FRAMES: u32 = 200;

/// One packet, as the buffer stores it — still encoded. See [`Decode`].
struct Slot {
    seq: u32,
    payload: Vec<u8>,
}

/// An adaptive jitter buffer over fixed-size audio frames.
///
/// Frames are fixed size because the sender emits them that way and because
/// every time constant in here is expressed in frames underneath. A packet of
/// the wrong length is rejected rather than accommodated: silently accepting
/// one would corrupt the mapping from sequence number to playout time, which
/// is the only thing the buffer actually knows.
pub struct Buffer<D: Decode> {
    tuning: Tuning,
    decoder: D,
    rate: u32,
    channels: u16,
    /// Samples per decoded frame, interleaved across channels.
    frame: usize,
    /// Held packets, kept sorted by sequence.
    slots: VecDeque<Slot>,
    /// The sequence number [`Buffer::pull`] will play next, once playing has
    /// started. `None` until the first packet arrives.
    next: Option<u32>,
    /// False until the buffer has filled to its target once. Starting to play
    /// on the first packet guarantees an immediate underrun.
    playing: bool,
    /// Current target depth, in milliseconds.
    target: f32,
    /// Smoothed interarrival jitter, in milliseconds. RFC 3550's estimator:
    /// the mean deviation between how far apart packets were SENT and how far
    /// apart they ARRIVED.
    jitter: f32,
    /// Arrival bookkeeping for that estimator.
    last_arrival: Option<(u32, f64)>,
    stats: Stats,
}

/// Why a packet was not accepted. Returned rather than logged so the caller
/// decides what is worth reporting — a late packet is routine, a malformed one
/// is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// Its turn had already been played.
    Late,
    /// Already held, or already played.
    Duplicate,
    /// Empty payload. An empty packet is how [`Decode`] spells "lost", so one
    /// arriving over the wire would be indistinguishable from a gap.
    Empty,
}

impl<D: Decode> Buffer<D> {
    /// Build a buffer that plays out through `decoder` at `rate` Hz.
    pub fn new(tuning: Tuning, decoder: D, rate: u32, channels: u16) -> Buffer<D> {
        let rate = rate.max(1);
        let channels = channels.max(1);
        let frame = decoder.frame().max(1);
        Buffer {
            tuning,
            decoder,
            rate,
            channels,
            frame,
            slots: VecDeque::new(),
            next: None,
            playing: false,
            target: tuning.min_ms,
            jitter: 0.0,
            last_arrival: None,
            stats: Stats::default(),
        }
    }

    /// The decoder this buffer plays out through.
    ///
    /// Exposed because a decoder has things to say that the buffer cannot say
    /// for it — "no decoder for this encoding", "that rate is not one I take" —
    /// and the buffer deliberately does not know what those are. Borrowing it
    /// back is how a caller reads them without this crate learning about
    /// codecs.
    pub fn decoder(&self) -> &D {
        &self.decoder
    }

    /// Samples per frame — what [`Buffer::pull`] must be given.
    pub fn frame(&self) -> usize {
        self.frame
    }

    /// How long one packet lasts, in milliseconds.
    pub fn frame_ms(&self) -> f32 {
        let frames = self.frame / self.channels as usize;
        frames as f32 * 1000.0 / self.rate as f32
    }

    /// The delay the buffer is currently aiming to carry.
    pub fn target_ms(&self) -> f32 {
        self.target
    }

    /// Smoothed interarrival jitter, in milliseconds.
    pub fn jitter_ms(&self) -> f32 {
        self.jitter
    }

    /// How much audio is held right now, in milliseconds.
    pub fn depth_ms(&self) -> f32 {
        self.slots.len() as f32 * self.frame_ms()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Accept one packet.
    ///
    /// `arrival_ms` is a local monotonic clock reading. It is used only to
    /// measure jitter, never to decide playout — playout is driven by the pull
    /// side, which is the consumer's clock and the only one that matters for
    /// whether audio comes out smoothly.
    pub fn push(&mut self, seq: u32, payload: &[u8], arrival_ms: f64) -> Result<(), Rejected> {
        if payload.is_empty() {
            return Err(Rejected::Empty);
        }
        self.stats.received += 1;

        // Before anything else looks at `seq`, because everything else assumes
        // the two streams are the same one — the late test would refuse this
        // packet and the jitter estimator would take a nonsense interval from
        // it. See [`RESYNC_FRAMES`].
        if self.next.map(|next| distance(seq, next) > RESYNC_FRAMES).unwrap_or(false) {
            self.restart(seq);
        }

        self.observe_arrival(seq, arrival_ms);

        if let Some(next) = self.next {
            if seq_before(seq, next) {
                // Its turn has been and gone. Playing it now would be audible
                // as a jump backwards in time.
                self.stats.late += 1;
                return Err(Rejected::Late);
            }
            if seq != next {
                self.stats.reordered += 1;
            }
        } else {
            self.next = Some(seq);
        }

        match self.slots.binary_search_by(|s| order(s.seq, seq)) {
            Ok(_) => {
                self.stats.duplicate += 1;
                return Err(Rejected::Duplicate);
            }
            Err(at) => self.slots.insert(at, Slot { seq, payload: payload.to_vec() }),
        }

        // Once enough has accumulated, start. Waiting for the target rather
        // than for one packet is the difference between a buffer and a delay.
        if !self.playing && self.depth_ms() >= self.target {
            self.playing = true;
        }
        self.enforce_ceiling();
        Ok(())
    }

    /// Produce exactly one frame of audio.
    ///
    /// Always fills `out`, whatever the network did. That is the contract the
    /// playback side needs: it asks on its own clock and something has to come
    /// back, so the only question is whether the answer is a real packet or a
    /// concealed one.
    ///
    /// Returns `true` if a real packet was played.
    pub fn pull(&mut self, out: &mut [f32]) -> bool {
        if out.len() != self.frame {
            // A caller asking for the wrong size cannot be served meaningfully.
            // Silence is the only honest answer, and the stats will show it.
            out.fill(0.0);
            self.stats.starved += 1;
            return false;
        }
        if !self.playing {
            // Still filling. Silence is right here — there is no audio yet, as
            // opposed to audio that went missing, and asking the codec to
            // conceal a stream that has not started would have it invent one.
            out.fill(0.0);
            return false;
        }

        let want = self.next.unwrap_or(0);
        let head_matches = self.slots.front().map(|s| s.seq == want).unwrap_or(false);

        if head_matches {
            let slot = self.slots.pop_front().expect("checked");
            self.decoder.decode(Some(&slot.payload), out);
            self.next = Some(want.wrapping_add(1));
            self.adapt();
            return true;
        }

        if self.slots.is_empty() {
            // Nothing at all: the sender stopped, or the link died. Conceal —
            // the fade in every implementation is what stops this becoming a
            // drone — but do NOT advance `next`. A gap in SUPPLY is not a lost
            // packet, and skipping sequence numbers here would make every
            // packet that eventually arrives look late, turning one stall into
            // a permanently broken stream.
            self.decoder.decode(None, out);
            self.stats.starved += 1;
            self.adapt();
            return false;
        }

        // Something is held, but not the one that is due — so that one is
        // genuinely missing rather than merely slow, or it would be here by
        // now given everything after it already is.
        self.decoder.decode(None, out);
        self.stats.concealed += 1;
        self.next = Some(want.wrapping_add(1));
        self.adapt();
        false
    }

    /// Begin again at `seq`, keeping nothing.
    ///
    /// Everything held belongs to a stream that has ended, and every measurement
    /// describes it: the jitter estimate, the arrival bookkeeping, and the fact
    /// that playout had started. Carrying any of it over would mean the new
    /// stream inherits the old one's idea of the network.
    ///
    /// The target depth is deliberately NOT reset. It is a property of the link,
    /// which has not changed just because the far side reconnected over it, and
    /// rebuilding it from `min_ms` would spend the first second of the new
    /// stream rediscovering what was already known.
    fn restart(&mut self, seq: u32) {
        self.slots.clear();
        self.next = Some(seq);
        self.playing = false;
        self.jitter = 0.0;
        self.last_arrival = None;
        self.stats.resyncs += 1;
    }

    /// Interarrival jitter, RFC 3550's estimator.
    ///
    /// It compares how far apart two packets were *sent* — which sequence
    /// numbers give exactly, at a fixed frame size — with how far apart they
    /// *arrived*. The difference is what the network added, and the mean of
    /// its magnitude is what a buffer has to cover.
    fn observe_arrival(&mut self, seq: u32, arrival_ms: f64) {
        if let Some((last_seq, last_ms)) = self.last_arrival {
            let sent_apart = distance(last_seq, seq) as f64 * self.frame_ms() as f64;
            let arrived_apart = arrival_ms - last_ms;
            let d = (arrived_apart - sent_apart).abs() as f32;
            // 1/16, as in the RFC: slow enough not to chase one bad packet.
            self.jitter += (d - self.jitter) / 16.0;
        }
        self.last_arrival = Some((seq, arrival_ms));
    }

    /// Move the target toward what the measured jitter says is needed.
    ///
    /// Called once per pulled frame, so "per frame" is the unit of time here
    /// and the time constants convert into it — the pull side is the clock
    /// that matters, and it is the one that is regular.
    fn adapt(&mut self) {
        let wanted = (self.jitter * self.tuning.jitter_factor)
            .clamp(self.tuning.min_ms, self.tuning.max_ms);
        let tau = if wanted > self.target { self.tuning.grow_ms } else { self.tuning.shrink_ms };
        let alpha = (self.frame_ms() / tau.max(self.frame_ms())).min(1.0);
        self.target += (wanted - self.target) * alpha;
    }

    /// Stop the buffer growing without bound.
    ///
    /// Reached by a sender that is genuinely faster than this receiver — clock
    /// drift, or a burst after a stall. Dropping the OLDEST is right: it is the
    /// most stale, and keeping it would mean carrying its delay for the rest of
    /// the call.
    fn enforce_ceiling(&mut self) {
        let ceiling = self.tuning.max_ms * 2.0;
        while self.depth_ms() > ceiling && self.slots.len() > 1 {
            let dropped = self.slots.pop_front().expect("len > 1");
            self.stats.drift_events += 1;
            self.next = Some(dropped.seq.wrapping_add(1));
        }
    }
}

/// Sequence comparison that survives wrapping.
///
/// A `u32` of 10 ms frames wraps after about 500 days of continuous talking,
/// so this is not a practical concern — but the comparison is written once and
/// getting it wrong would present as an entire call being rejected as late,
/// which is not a failure anybody would guess at.
fn seq_before(a: u32, b: u32) -> bool {
    a != b && b.wrapping_sub(a) < u32::MAX / 2
}

fn order(a: u32, b: u32) -> std::cmp::Ordering {
    if a == b {
        std::cmp::Ordering::Equal
    } else if seq_before(a, b) {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    }
}

/// How many frames apart two sequence numbers are, in either direction.
fn distance(a: u32, b: u32) -> u32 {
    let forward = b.wrapping_sub(a);
    let backward = a.wrapping_sub(b);
    forward.min(backward)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    /// 10 ms mono at 48 kHz — the packet size QUIC datagrams force on us, and
    /// what the suppression path already emits.
    const FRAME: usize = 480;

    /// Packets carry i16, so the test's "wire" does too.
    fn payload(seq: u32) -> Vec<u8> {
        // A 200 Hz tone, phase-continuous across packets, so a glitch at a
        // packet boundary is visible as a discontinuity rather than hidden.
        (0..FRAME)
            .map(|i| {
                let t = (seq as usize * FRAME + i) as f32 / RATE as f32;
                let v = (t * 200.0 * std::f32::consts::TAU).sin() * 0.5;
                (v * i16::MAX as f32) as i16
            })
            .flat_map(|s| s.to_le_bytes())
            .collect()
    }

    fn buffer() -> Buffer<Pcm> {
        Buffer::new(Tuning::default(), Pcm::new(RATE, 1, FRAME), RATE, 1)
    }

    /// Run `packets` through a buffer whose arrivals follow `arrival`, pulling
    /// at real-time rate, and report the delay it settles on.
    ///
    /// The pull side is paced by the same clock the arrivals are, which is what
    /// makes this a measurement of the ARRIVAL PATTERN rather than of the
    /// consumer.
    fn settles_at(packets: u32, arrival: impl Fn(u32) -> f64) -> f32 {
        let mut buffer = buffer();
        let mut out = vec![0.0f32; FRAME];
        let mut clock = Clock::new(buffer.frame_ms(), 10, arrival(0));
        for seq in 0..packets {
            let at = arrival(seq);
            let _ = buffer.push(seq, &payload(seq), at);
            for _ in 0..clock.due(at) {
                buffer.pull(&mut out);
            }
        }
        buffer.target_ms()
    }

    #[test]
    fn a_sender_that_bursts_costs_more_delay_than_the_network_does() {
        // The finding this test exists to pin down, and the reason it is here
        // rather than asserted in prose: on THIS project's web client, capture
        // arrives 85 ms at a time and is then cut into four 20 ms packets that
        // all go out at once. Nothing about the network did that. But the
        // receiver cannot tell a burst it was sent from a burst the network
        // made, so it sizes its buffer for it — and the capture block ends up
        // paid for twice, once as capture latency and once as playout delay.
        //
        // Same packet count, same total duration, same code. Only the shape of
        // the arrivals differs.
        let steady = settles_at(200, |seq| seq as f64 * 10.0);
        let bursty = settles_at(200, |seq| (seq / 4) as f64 * 40.0);

        assert!(
            steady <= Tuning::default().min_ms + 1.0,
            "an evenly paced sender needs no headroom at all, got {steady} ms",
        );
        assert!(
            bursty > steady * 2.0,
            "bursts of four should cost real delay: steady {steady} ms, bursty {bursty} ms",
        );
        // Left in as a record of the magnitude, not as a bound to defend.
        println!("steady {steady:.1} ms, bursty {bursty:.1} ms");
    }

    #[test]
    fn a_sender_that_restarts_its_sequence_is_not_rejected_forever() {
        // Measured in a real call before it was a test. Every packet arrived,
        // every packet decoded, and every one was refused as `Late` — so the
        // buffer held nothing, every pull starved, and that listener heard
        // silence for the rest of the session while the far side sounded fine.
        // One direction only, which is what makes it so hard to place from
        // outside.
        //
        // Sequence numbers restart whenever a sender's session does — a rejoin,
        // a redirect to another node, a reconnect. The id is stable across
        // those, so the receiver keeps the same buffer and its `next` stays up
        // in the thousands while the sender counts from zero again.
        //
        // The deadlock is the important part: `next` only ever advances when a
        // packet PLAYS, and nothing can play while everything is rejected. It
        // cannot recover on its own, however long you wait.
        let mut buffer = buffer();
        let mut out = vec![0.0f32; FRAME];

        prime(&mut buffer, 5_000, 12);
        let mut played = 0;
        for _ in 0..12 {
            if buffer.pull(&mut out) {
                played += 1;
            }
        }
        assert!(played > 0, "the first stream should have played");

        // The far side rejoins and starts again from zero.
        let mut restarted = 0;
        for seq in 0..40u32 {
            if buffer.push(seq, &payload(seq), 20_000.0 + seq as f64 * 10.0).is_ok() {
                restarted += 1;
            }
        }
        assert!(
            restarted > 0,
            "every packet of the new stream was refused; this listener is deaf for the rest \
             of the call",
        );

        let mut after = 0;
        for _ in 0..40 {
            if buffer.pull(&mut out) {
                after += 1;
            }
        }
        assert!(after > 0, "the buffer accepted the new stream but never played any of it");
    }

    #[test]
    fn a_restart_is_counted_once_and_the_stream_carries_on() {
        // The other way this could go wrong: a threshold that re-triggers on
        // every packet would clear the buffer each time, so nothing would ever
        // accumulate and playout could never start — silence again, by a
        // different route and with a counter racing instead of frozen.
        let mut buffer = buffer();
        let mut out = vec![0.0f32; FRAME];
        prime(&mut buffer, 9_000, 12);
        for _ in 0..12 {
            buffer.pull(&mut out);
        }

        // Interleaved, because a call is: pushing sixty frames before pulling
        // any would trip the anti-drift ceiling, which is correct behaviour
        // that would make this measure the wrong thing.
        let mut played = 0;
        for seq in 0..60u32 {
            let _ = buffer.push(seq, &payload(seq), 30_000.0 + seq as f64 * 10.0);
            if buffer.pull(&mut out) {
                played += 1;
            }
        }
        assert_eq!(buffer.stats().resyncs, 1, "one discontinuity, one restart");
        assert!(played > 40, "only {played} of 60 frames played after the restart");
    }

    #[test]
    fn an_ordinary_late_packet_is_still_just_late() {
        // The resync must not swallow the case it sits next to. A packet a few
        // frames behind is the network doing what networks do, and treating it
        // as a new stream would throw away everything buffered every time one
        // turned up.
        let mut buffer = buffer();
        let mut out = vec![0.0f32; FRAME];
        prime(&mut buffer, 100, 12);
        for _ in 0..6 {
            buffer.pull(&mut out);
        }
        let played_to = 100 + 6;
        assert!(matches!(buffer.push(played_to - 3, &payload(1), 500.0), Err(Rejected::Late)));
        assert_eq!(buffer.stats().resyncs, 0, "a late packet is not a new stream");
    }

    /// Push enough to get past the initial fill, at a steady arrival rate.
    fn prime(b: &mut Buffer<Pcm>, from: u32, count: u32) {
        for k in 0..count {
            let seq = from + k;
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
        }
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// The buffer must not start playing on the first packet. Doing so
    /// guarantees an underrun on the second, because there is by definition no
    /// slack — which is the difference between a buffer and a delay line.
    #[test]
    fn it_fills_before_it_plays() {
        let mut b = buffer();
        let mut out = vec![0.0; FRAME];
        let _ = b.push(0, &payload(0), 0.0);
        assert!(!b.pull(&mut out), "played on the first packet");
        assert_eq!(peak(&out), 0.0, "and it should be silence, not concealment");

        prime(&mut b, 1, 4);
        assert!(b.pull(&mut out), "should be playing once the target is met");
    }

    /// The whole reason the buffer exists: packets that arrive out of order
    /// play in order.
    #[test]
    fn it_reorders() {
        let mut b = buffer();
        for seq in [0u32, 1, 4, 3, 2, 5, 6, 7] {
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
        }
        let mut out = vec![0.0; FRAME];
        for expected in 0..8u32 {
            assert!(b.pull(&mut out), "frame {expected} should be real");
            let want = payload(expected);
            let first = i16::from_le_bytes([want[0], want[1]]) as f32 / i16::MAX as f32;
            assert!((out[0] - first).abs() < 1e-3, "frame {expected} played out of order");
        }
        assert!(b.stats().reordered > 0, "the reordering should be counted");
    }

    /// A packet whose turn has passed is discarded, not played — playing it
    /// would be a jump backwards in time, which is far more audible than the
    /// gap it is trying to fill.
    #[test]
    fn a_late_packet_is_dropped_not_played() {
        let mut b = buffer();
        prime(&mut b, 0, 4);
        let mut out = vec![0.0; FRAME];
        for _ in 0..4 {
            b.pull(&mut out);
        }
        assert_eq!(b.push(1, &payload(1), 100.0), Err(Rejected::Late));
        assert_eq!(b.stats().late, 1);
    }

    #[test]
    fn a_duplicate_is_counted_and_ignored() {
        let mut b = buffer();
        prime(&mut b, 0, 4);
        assert_eq!(b.push(2, &payload(2), 20.0), Err(Rejected::Duplicate));
        assert_eq!(b.stats().duplicate, 1);
    }

    /// An empty payload is how `Decode` spells "lost", so one arriving over
    /// the wire has to be refused — otherwise a peer could inject silence that
    /// the buffer would faithfully treat as audio.
    #[test]
    fn an_empty_packet_is_refused() {
        let mut b = buffer();
        assert_eq!(b.push(0, &[], 0.0), Err(Rejected::Empty));
    }

    /// A hole in the middle of a stream is concealed, and the sequence
    /// continues past it — the packets after the hole are not late.
    #[test]
    fn a_lost_packet_is_concealed_and_the_stream_continues() {
        let mut b = buffer();
        for seq in [0u32, 1, 2, 3, /* 4 lost */ 5, 6, 7] {
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
        }
        let mut out = vec![0.0; FRAME];
        let mut real = 0;
        for _ in 0..7 {
            if b.pull(&mut out) {
                real += 1;
            }
        }
        assert_eq!(b.stats().concealed, 1, "the hole should be concealed once");
        assert_eq!(real, 6, "and every other frame played for real");
    }

    /// Concealment has to actually conceal. Silence in the middle of a vowel
    /// is two clicks, which is worse than the wrong vowel.
    #[test]
    fn concealment_is_not_silence() {
        let mut b = buffer();
        for seq in [0u32, 1, 2, 3, 4, 5, /* 6 lost */ 7, 8] {
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
        }
        let mut out = vec![0.0; FRAME];
        for _ in 0..6 {
            b.pull(&mut out);
        }
        // Frame 6: the hole.
        assert!(!b.pull(&mut out));
        assert!(peak(&out) > 0.2, "concealed a tone with near-silence: peak {}", peak(&out));
    }

    /// And it must not click on the way in or out. A concealed frame that
    /// steps away from the audio before it — or that the returning real frame
    /// steps away from — has replaced one dropout with two clicks.
    #[test]
    fn concealment_does_not_click() {
        let mut b = buffer();
        for seq in 0..12u32 {
            if seq == 8 {
                continue;
            }
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
        }
        let mut played: Vec<f32> = Vec::new();
        let mut out = vec![0.0; FRAME];
        for _ in 0..11 {
            b.pull(&mut out);
            played.extend_from_slice(&out);
        }
        // A 200 Hz sine at 0.5 moves at most ~0.013 per sample at 48 kHz. Ten
        // times that is a generous ceiling that still catches a real step.
        let biggest = played.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(biggest < 0.13, "stepped by {biggest} — that is a click");
    }

    /// A long outage must fade out rather than repeat forever. A drone is a
    /// worse failure than a dropout because it sounds like the software is
    /// broken rather than like the network is.
    #[test]
    fn a_long_outage_fades_to_silence() {
        let mut b = buffer();
        prime(&mut b, 0, 6);
        let mut out = vec![0.0; FRAME];
        for _ in 0..6 {
            b.pull(&mut out);
        }
        // Nothing more arrives. Well past Concealer::FADE_MS.
        let mut last = 1.0;
        for _ in 0..20 {
            b.pull(&mut out);
            last = peak(&out);
        }
        assert!(last < 1e-3, "still droning at {last}");
        assert!(b.stats().starved > 0);
    }

    /// A stall is not a loss. If the sender pauses and resumes, the packets it
    /// resumes with must still be accepted — advancing the sequence during a
    /// starve would mark all of them late and break the stream permanently.
    #[test]
    fn a_stall_does_not_poison_the_sequence() {
        let mut b = buffer();
        prime(&mut b, 0, 4);
        let mut out = vec![0.0; FRAME];
        for _ in 0..4 {
            b.pull(&mut out);
        }
        for _ in 0..10 {
            b.pull(&mut out); // starving
        }
        // The sender comes back with the very next sequence number.
        assert_eq!(b.push(4, &payload(4), 200.0), Ok(()));
        assert_eq!(b.stats().late, 0, "the resumed packet was treated as late");
    }

    /// The target must widen when the network is erratic, or normal jitter
    /// reads as loss.
    #[test]
    fn the_target_grows_with_jitter() {
        let mut b = buffer();
        let calm = b.target_ms();
        // Same packets, wildly irregular arrivals.
        let mut when = 0.0;
        for seq in 0..80u32 {
            when += if seq % 2 == 0 { 1.0 } else { 60.0 };
            let _ = b.push(seq, &payload(seq), when);
            let mut out = vec![0.0; FRAME];
            b.pull(&mut out);
        }
        assert!(b.jitter_ms() > 5.0, "jitter not measured: {}", b.jitter_ms());
        assert!(b.target_ms() > calm, "target did not grow: {} -> {}", calm, b.target_ms());
        assert!(b.target_ms() <= Tuning::default().max_ms, "target ran past its ceiling");
    }

    /// A steady link must not accumulate delay. The buffer giving back what it
    /// does not need is the difference between adaptive and merely safe.
    #[test]
    fn a_steady_link_stays_shallow() {
        let mut b = buffer();
        let mut out = vec![0.0; FRAME];
        for seq in 0..200u32 {
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
            b.pull(&mut out);
        }
        assert!(b.jitter_ms() < 1.0, "invented jitter on a perfect link: {}", b.jitter_ms());
        assert_eq!(b.target_ms(), Tuning::default().min_ms);
        assert!(b.depth_ms() <= 30.0, "hoarding {} ms on a steady link", b.depth_ms());
    }

    /// A sender faster than this receiver — clock drift, or a burst after a
    /// stall — must not grow the buffer without bound. Latency that only ever
    /// climbs is the failure mode people describe as "it gets worse the longer
    /// you talk".
    #[test]
    fn it_refuses_to_grow_without_bound() {
        let mut b = buffer();
        for seq in 0..400u32 {
            let _ = b.push(seq, &payload(seq), seq as f64 * 10.0);
        }
        assert!(
            b.depth_ms() <= Tuning::default().max_ms * 2.0 + 10.0,
            "grew to {} ms",
            b.depth_ms()
        );
        assert!(b.stats().drift_events > 0, "dropped audio without saying so");
    }

    /// Sequence numbers wrap. Getting the comparison wrong presents as an
    /// entire call being rejected as late, which is not a failure anyone would
    /// guess at from the symptom.
    #[test]
    fn sequence_wrap_is_not_a_cliff() {
        let mut b = buffer();
        let base = u32::MAX - 2;
        for k in 0..6u32 {
            let seq = base.wrapping_add(k);
            assert_eq!(b.push(seq, &payload(seq), k as f64 * 10.0), Ok(()), "k={k}");
        }
        let mut out = vec![0.0; FRAME];
        let mut real = 0;
        for _ in 0..6 {
            if b.pull(&mut out) {
                real += 1;
            }
        }
        assert_eq!(real, 6, "wrapped sequence was not played through");
        assert_eq!(b.stats().late, 0);
    }
}
