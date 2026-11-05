//! The receive path: datagrams to the speakers, one jitter buffer and one
//! output per (sender, audio track).
//!
//! # What drives playout
//!
//! Arrivals, paced by [`jitter::Clock`] — the same arrangement the chat client
//! uses, for the same reason: there is no timer on this platform that keeps
//! running in a background tab. Each packet pulls however many frames real
//! time has moved on by, and the output's cushion absorbs the burstiness.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use call_proto::{audio, TRACK_MAIN};

use crate::{log, now_ms, Stats, FRAME};

/// How long `Stats::remote_peak` holds its loudest sample.
pub const PEAK_WINDOW_MS: f64 = 500.0;

/// The most frames one arrival may pull, so a long stall does not come back as
/// a burst of concealment.
const MAX_CATCHUP: usize = 10;

/// Our gain on each (sender, audio track), linear: 1.0 as sent. Listener side
/// only — nobody else hears it. Cheap to clone; the page holds one to set,
/// the receiver reads it per packet.
#[derive(Clone, Default)]
pub struct Volumes(Rc<RefCell<HashMap<(u32, u8), f32>>>);

impl Volumes {
    /// Set our gain on one person's audio track, live: up to
    /// `voice_dsp::MAX_GAIN`. It ramps rather than steps, so dragging a slider
    /// does not click.
    pub fn set(&self, id: u32, track: u8, gain: f32) {
        self.0.borrow_mut().insert((id, track), gain.clamp(0.0, voice_dsp::MAX_GAIN));
    }

    fn get(&self, key: (u32, u8)) -> f32 {
        self.0.borrow().get(&key).copied().unwrap_or(1.0)
    }
}

/// Everyone we can hear, by id, with how loud their voice was in the last
/// audio that arrived — for a speaking indicator. Before our volume on them:
/// it says whether they are talking, not how loud we have them.
pub type Levels = Rc<RefCell<BTreeMap<u32, f32>>>;

/// Counters of streams that have closed, so the totals stay cumulative —
/// someone hanging up must not make "received" go backwards.
#[derive(Default, Clone, Copy)]
struct Retired {
    received: u64,
    late: u64,
    concealed: u64,
    underruns: u32,
}

impl Retired {
    fn add(&mut self, peer: &Peer) {
        let b = peer.buffer.stats();
        self.received += b.received;
        self.late += b.late;
        self.concealed += b.concealed + b.starved;
        self.underruns += peer.output.underruns();
    }
}

/// Everyone's audio, played.
pub struct Receiver {
    peers: HashMap<(u32, u8), Peer>,
    retired: Retired,
    volumes: Volumes,
    levels: Levels,
    stats: Rc<RefCell<Stats>>,
}

impl Receiver {
    pub fn new(stats: Rc<RefCell<Stats>>) -> Receiver {
        Receiver {
            peers: HashMap::new(),
            retired: Retired::default(),
            volumes: Volumes::default(),
            levels: Levels::default(),
            stats,
        }
    }

    pub fn volumes(&self) -> Volumes {
        self.volumes.clone()
    }

    pub fn levels(&self) -> Levels {
        self.levels.clone()
    }

    /// Play one packet from `from`. Returns its track's audio clock — what
    /// capture time is coming out of the speakers now — for lip sync, or
    /// `None` when it could not be played.
    pub async fn play(&mut self, from: u32, packet: audio::Packet<'_>) -> Option<av_sync::AudioClock> {
        let audio::Packet { track, seq, rate, capture, opus } = packet;
        let key = (from, track);
        let gain = self.volumes.get(key);
        // Opened outside any borrow of `peers`' entry, since it awaits.
        if self.peers.get(&key).map(|p| p.rate != rate).unwrap_or(true) {
            match Peer::open(rate, gain).await {
                Ok(peer) => {
                    if let Some(old) = self.peers.insert(key, peer) {
                        self.retired.add(&old);
                    }
                }
                Err(e) => {
                    log(&format!("[call-audio] cannot play #{from}/{track}: {e}"));
                    return None;
                }
            }
        }
        let peer = self.peers.get_mut(&key).expect("opened above");
        let now = now_ms();
        let (peak, played) = peer.feed(seq, opus, now, gain);
        if track == TRACK_MAIN {
            self.levels.borrow_mut().insert(from, peak);
        }
        {
            let mut s = self.stats.borrow_mut();
            if now - s.remote_peak_ms > PEAK_WINDOW_MS {
                s.remote_peak = played;
                s.remote_peak_ms = now;
            } else {
                s.remote_peak = s.remote_peak.max(played);
            }
        }
        let clock = av_sync::AudioClock { capture: peer.audible(capture as f64), at: now };
        tally(&self.stats, &self.peers, self.retired);
        Some(clock)
    }

    /// Someone hung up: their outputs close.
    pub fn left(&mut self, id: u32) {
        let gone: Vec<(u32, u8)> = self.peers.keys().filter(|(sender, _)| *sender == id).copied().collect();
        for key in gone {
            if let Some(peer) = self.peers.remove(&key) {
                self.retired.add(&peer);
            }
        }
        self.levels.borrow_mut().remove(&id);
        tally(&self.stats, &self.peers, self.retired);
    }
}

/// One person we can hear.
struct Peer {
    output: audio_io::Output,
    /// Our gain on this stream — see [`Volumes`]. Ramps and limits.
    fader: voice_dsp::Fader,
    buffer: jitter::Buffer<audio_codec::Decoder>,
    clock: jitter::Clock,
    rate: u32,
    frame: Vec<f32>,
}

impl Peer {
    /// `gain` is applied from the first sample: someone turned down before
    /// they arrived must not be loud for the word that announced them.
    async fn open(rate: u32, gain: f32) -> Result<Peer, String> {
        let decoder = audio_codec::Decoder::new(FRAME, rate, 1).map_err(|e| e.to_string())?;
        let output = audio_io::Output::open(audio_io::OutputConfig {
            channels: 1,
            // Two packets of cushion: the producer hands over a frame at an
            // instant and then nothing for 20 ms, and a cushion of exactly one
            // touches empty on every cycle.
            cushion_frames: Some(2 * FRAME.ms() * audio_io::PREFERRED_RATE / 1000),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
        let buffer = jitter::Buffer::new(jitter::Tuning::default(), decoder, rate, 1);
        // Started after the await, so the time spent opening the output is not
        // counted as a backlog.
        let clock = jitter::Clock::new(buffer.frame_ms(), MAX_CATCHUP, now_ms());
        let frame = vec![0.0; buffer.frame()];
        Ok(Peer { output, fader: voice_dsp::Fader::new(rate, gain), buffer, clock, rate, frame })
    }

    /// What capture time is coming out of the speakers now, given that the
    /// packet just fed was captured at `capture`: its end, less everything
    /// still between it and the speaker — the packets the jitter buffer holds
    /// and the audio the output has queued. The output's figure is the
    /// worklet's own report, up to ~50 ms old, so this is good to a packet or
    /// two; lip sync needs ~40 ms. The device's own latency — the context to
    /// the speaker — comes off too, which on Bluetooth is the biggest term.
    fn audible(&self, capture: f64) -> f64 {
        let queued_ms = self.output.queued_frames() as f64 * 1000.0 / self.output.opened().sample_rate.max(1) as f64;
        av_sync::audible(
            capture,
            FRAME.ms() as f64,
            self.buffer.depth_ms() as f64,
            queued_ms,
            self.output.device_latency_ms(),
        )
    }

    /// `gain` is our volume on this stream, applied (ramped) to what plays.
    /// Returns the loudest sample before our gain (whether they are talking)
    /// and after it (what we actually hear).
    fn feed(&mut self, seq: u32, opus: &[u8], now: f64, gain: f32) -> (f32, f32) {
        let _ = self.buffer.push(seq, opus, now);
        let due = self.clock.due(now);
        let mut pcm = Vec::with_capacity(due * self.frame.len());
        for _ in 0..due {
            self.buffer.pull(&mut self.frame);
            pcm.extend_from_slice(&self.frame);
        }
        let peak = pcm.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        self.fader.set_gain(gain);
        self.fader.process(&mut pcm);
        let played = pcm.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        self.output.write_at(&pcm, self.rate);
        (peak, played)
    }
}

fn tally(stats: &Rc<RefCell<Stats>>, peers: &HashMap<(u32, u8), Peer>, retired: Retired) {
    let mut s = stats.borrow_mut();
    // Live streams summed fresh, on top of the ones that have closed.
    let (mut received, mut late, mut concealed, mut underruns, mut playout) =
        (retired.received, retired.late, retired.concealed, retired.underruns, 0.0f32);
    let mut listen = 0.0f64;
    for peer in peers.values() {
        let b = peer.buffer.stats();
        received += b.received;
        late += b.late;
        concealed += b.concealed + b.starved;
        underruns += peer.output.underruns();
        playout = playout.max(peer.buffer.target_ms());
        let queued_ms =
            peer.output.queued_frames() as f64 * 1000.0 / peer.output.opened().sample_rate.max(1) as f64;
        listen = listen.max(peer.buffer.depth_ms() as f64 + queued_ms + peer.output.device_latency_ms());
    }
    s.listen_delay_ms = listen;
    s.received = received;
    s.late = late;
    s.concealed = concealed;
    s.underruns = underruns;
    s.playout_ms = playout;
    // People, not streams: someone sharing a screen with sound is still one.
    s.peers = peers.keys().map(|&(id, _)| id).collect::<std::collections::BTreeSet<_>>().len();
}

