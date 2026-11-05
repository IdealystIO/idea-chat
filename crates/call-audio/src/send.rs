//! The send path: microphone to Opus datagrams.
//!
//! ```text
//!   mic ──▶ dehum ──▶ AudioStream ──▶ stage ──▶ condition ──▶ Packer ──▶ packets
//!                                      │
//!                      Direct: forwarded as captured
//!                      Neural: DeepFilterNet 3 (`denoise`), 48 kHz mono
//! ```
//!
//! The microphone writes into one [`AudioStream`] for the life of the call,
//! and exactly one stage reads it. Changing suppression swaps the stage — the
//! new one is built while the old one is still feeding the packer, then the
//! two are exchanged with nothing in between — so it takes effect mid-call
//! without a hole or a moment of doubled audio.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::{Rc, Weak};

use call_proto::{audio, TRACK_MAIN, TRACK_SCREEN};
use futures::channel::mpsc;
use media_stream::{AudioFrame, AudioStream, AudioSubscription, AudioWriter};

use crate::{log, now_ms, ModelSettings, Processing, Stats, Suppression, VoiceStages};

/// What the voice is coded at, until bitrate control says otherwise: fullband
/// CELT at about 40 kb/s, where speech sounds like the person.
pub const VOICE_RATE: Rate = Rate { bps: 40_000, fullband: true };

/// A screen share's audio: music, a video's soundtrack — content, which wants
/// more than a voice does, and the full band.
const SCREEN_AUDIO_RATE: Rate = Rate { bps: 64_000, fullband: true };

/// An encoder's bitrate and band. The band picks the codec — see
/// `audio_codec::Settings::max_bandwidth` — so changing it rebuilds the
/// encoder; changing the bitrate alone does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub bps: u32,
    /// To 20 kHz rather than 8.
    pub fullband: bool,
}

impl Rate {
    fn band(self) -> audio_codec::MaxBandwidth {
        if self.fullband { audio_codec::MaxBandwidth::Fullband } else { audio_codec::MaxBandwidth::Wideband }
    }
}

/// What the captured track is opened with. Browser suppression is off under
/// the neural stage: running both is worse than either, because the model is
/// handed audio another suppressor has already mangled.
fn capture_config(p: Processing) -> audio_io::CaptureConfig {
    audio_io::CaptureConfig {
        channels: 1,
        echo_cancellation: p.echo_cancellation,
        noise_suppression: p.suppression == Suppression::Browser,
        auto_gain_control: p.auto_gain_control,
        ..Default::default()
    }
}

/// Opus frames from PCM, and datagrams from Opus frames.
///
/// Outlives any one capture or stage, so neither the sequence nor the encoder
/// restarts when the mic is reopened or suppression changes.
struct Packer {
    /// Built from the first block, because that is what says the real rate;
    /// with the sample rate and the [`Rate`] it was built for.
    encoder: Option<(audio_codec::Encoder, u32, Rate)>,
    /// What to code at. Shared with the call so bitrate control can change it
    /// live; read at every block.
    rate: Rc<Cell<Rate>>,
    pending: Vec<f32>,
    /// When `pending[0]` was captured, in ms.
    anchor: Option<f64>,
    /// The mic stream's capture times, which stages read to say when what
    /// they hand over was captured.
    clock: Rc<RefCell<av_sync::CaptureClock>>,
    seq: u32,
    out: Vec<u8>,
    /// Which of our audio tracks this packs.
    track: u8,
    packets: mpsc::Sender<Vec<u8>>,
    stats: Rc<RefCell<Stats>>,
}

impl Packer {
    fn new(
        track: u8,
        rate: Rc<Cell<Rate>>,
        packets: mpsc::Sender<Vec<u8>>,
        stats: Rc<RefCell<Stats>>,
    ) -> Packer {
        Packer {
            encoder: None,
            pending: Vec::new(),
            anchor: None,
            clock: Rc::new(RefCell::new(av_sync::CaptureClock::default())),
            seq: 0,
            out: vec![0; audio_codec::MAX_PACKET],
            track,
            rate,
            packets,
            stats,
        }
    }

    /// `capture` is when `pcm`'s first sample was captured, in ms, if known.
    fn push(&mut self, pcm: &[f32], rate: u32, channels: u16, capture: Option<f64>) {
        let target = self.rate.get();
        match self.encoder.as_mut() {
            // Only the bitrate moved: the encoder takes it live.
            Some((e, r, built)) if *r == rate && built.fullband == target.fullband && built.bps != target.bps => {
                e.set_bitrate(target.bps as i32);
                *built = target;
            }
            _ => {}
        }
        if self.encoder.as_ref().map(|(_, r, built)| *r != rate || *built != target).unwrap_or(true) {
            // DTX off: a silent stretch should still be a steady stream, so
            // that a gap in what arrives means loss and nothing else.
            let settings = audio_codec::Settings {
                dtx: false,
                max_bandwidth: target.band(),
                bitrate_bps: target.bps as i32,
                ..Default::default()
            };
            match audio_codec::Encoder::new(settings, rate, 1) {
                Ok(e) => self.encoder = Some((e, rate, target)),
                // Opus takes 8/12/16/24/48 kHz and nothing else.
                Err(_) => {
                    self.stats.borrow_mut().codec_failed = true;
                    return;
                }
            }
            self.pending.clear();
        }

        // Where this block belongs on the capture clock. Carried on from the
        // previous block normally; re-anchored when the two disagree by more
        // than a few ms, which is a stage change moving the delay.
        if let Some(capture) = capture {
            let pending_ms = self.pending.len() as f64 * 1000.0 / rate.max(1) as f64;
            match self.anchor {
                Some(a) if (a + pending_ms - capture).abs() < 5.0 => {}
                _ => self.anchor = Some(capture - pending_ms),
            }
        }
        let channels = channels.max(1) as usize;
        if channels == 1 {
            self.pending.extend_from_slice(pcm);
        } else {
            self.pending.extend(pcm.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
        }
        if self.track == TRACK_MAIN {
            let mut s = self.stats.borrow_mut();
            s.out_peak = pcm.iter().fold(s.out_peak, |m, x| m.max(x.abs()));
        }

        let (enc, _, _) = self.encoder.as_mut().expect("built above");
        let n = enc.frame();
        while self.pending.len() >= n {
            let frame: Vec<f32> = self.pending.drain(..n).collect();
            // Whole ms, wrapping every ~49 days, like video's.
            let capture = self.anchor.map(|a| a.max(0.0) as u64 as u32).unwrap_or(0);
            if let Some(a) = self.anchor.as_mut() {
                *a += n as f64 * 1000.0 / rate.max(1) as f64;
            }
            if let Ok(opus) = enc.encode(&frame, &mut self.out) {
                let packet = audio::encode(self.track, self.seq, rate, capture, opus);
                if self.packets.try_send(packet).is_err() {
                    self.stats.borrow_mut().unsent += 1;
                }
            }
            self.seq = self.seq.wrapping_add(1);
        }
    }
}

/// Time constant for the levels the model's verdict is computed from: long
/// enough to ride over a quiet hop, short enough to follow a syllable.
const METER_MS: f32 = 60.0;

/// Everything after the stage and before the encoder. Built from the first
/// block, because that is what says the rate.
/// Whether what the stage emits is someone talking, decided per block.
///
/// With a neural stage, the model's own verdict: how much it kept of what
/// went in (`voice_dsp::model_gain_db`), which is what a speech model
/// concluded rather than what an envelope crossed — guarded by the output
/// level, because a ratio is only as good as its denominator and silence is
/// not speech however favourably it divides. Without one, by level alone.
/// Muted audio is silence by the time it gets here, so it never speaks.
struct Activity {
    /// What went into the model (shared with the mic-side meter); `None`
    /// without one.
    input: Option<Rc<Cell<f32>>>,
    output: voice_dsp::Meter,
    /// The model's latency, so its output is compared with the input it came
    /// from.
    align: voice_dsp::Alignment,
}

/// About -45 dBFS: above any reasonable room, below any speech. The level
/// test, with no model to ask.
const SPEECH_RMS: f32 = 0.006;
/// Below this the model's output is silence, whatever its gain says.
const SPEECH_FLOOR: f32 = 0.002;
/// Kept at least this much of its input: the model heard speech.
const VERDICT_DB: f32 = -9.0;

impl Activity {
    /// `hops` is the model's latency in 10 ms hops.
    fn new(input: Option<Rc<Cell<f32>>>, hops: usize) -> Activity {
        Activity { input, output: voice_dsp::Meter::new(METER_MS), align: voice_dsp::Alignment::new(hops.max(1)) }
    }

    /// Speaking, and the model's gain in dB (`None` without a model).
    fn observe(&mut self, samples: &[f32], rate: u32, channels: u16) -> (bool, Option<f32>) {
        let rms = voice_dsp::rms(samples);
        let Some(input) = &self.input else { return (rms > SPEECH_RMS, None) };
        let dt = (samples.len() / channels.max(1) as usize) as f32 / rate.max(1) as f32;
        let out = self.output.observe(rms, dt);
        let gain = voice_dsp::model_gain_db(out, self.align.delay(input.get()));
        (gain > VERDICT_DB && out > SPEECH_FLOOR, Some(gain))
    }
}

struct Conditioner {
    rate: u32,
    declicker: voice_dsp::Declicker,
    leveler: voice_dsp::Leveler,
    expander: voice_dsp::Expander,
    scratch: Vec<f32>,
}

impl Conditioner {
    fn new(rate: u32) -> Conditioner {
        Conditioner {
            rate,
            declicker: voice_dsp::Declicker::new(voice_dsp::Transients::default(), rate),
            leveler: voice_dsp::Leveler::new(voice_dsp::Leveling::default(), rate),
            expander: voice_dsp::Expander::new(voice_dsp::Residual::default(), rate),
            scratch: Vec::new(),
        }
    }

    /// Declick, level, expand — in that order, each depending on the last:
    /// a keystroke levelled first would duck the sentence around it, and
    /// nothing after the expander may make anything louder. `verdict` is the
    /// model's, which the expander needs (it abstains without one).
    fn process(&mut self, samples: &[f32], stages: VoiceStages, verdict: Option<f32>) -> &[f32] {
        self.scratch.clear();
        self.scratch.extend_from_slice(samples);
        if stages.declick {
            self.declicker.process(&mut self.scratch);
        }
        if stages.level {
            self.leveler.process(&mut self.scratch);
        }
        if stages.expand {
            self.expander.process(&mut self.scratch, verdict);
        }
        &self.scratch
    }
}

/// What reads the microphone and feeds the packer. Exactly one at a time.
///
/// Field order in `Neural` is teardown order: the subscription that feeds the
/// packer stops before the model's stream goes away.
enum Stage {
    Direct { _feed: AudioSubscription },
    /// DeepFilterNet, in the page.
    Neural {
        _feed: AudioSubscription,
        /// Measures what goes INTO the model, for the expander's verdict.
        _meter: AudioSubscription,
        _clean: AudioStream,
    },
}

/// Our audio on its way out: the microphone chain, and a screen share's
/// audio while there is one. Dropping the last `Sender` releases the mic.
///
/// The operations that await — [`reprocess`](Sender::reprocess),
/// [`retune`](Sender::retune), [`sync_screen`](Sender::sync_screen) — hold
/// it weakly while they wait, so hanging up mid-change still lets everything
/// go, and the change finds nothing to apply itself to.
#[derive(Clone)]
pub struct Sender(Rc<RefCell<Inner>>);

struct Inner {
    /// Field order is teardown order: the stage stops reading before the mic
    /// stream and the device behind it go.
    stage: Stage,
    capture: Option<audio_io::Capture>,
    mic: AudioStream,
    mic_writer: AudioWriter,
    packer: Rc<RefCell<Packer>>,
    processing: Processing,
    /// DeepFilterNet's tuning. Changing it rebuilds the model — see `retune`.
    model: ModelSettings,
    model_path: &'static str,
    /// A retune running, and whether another was asked for meanwhile.
    retuning: Rc<Cell<(bool, bool)>>,
    /// Which voice-processing stages run. Read per block by both the capture
    /// callback and the stage, so changing it needs nothing reopened.
    voice: Rc<Cell<VoiceStages>>,
    stats: Rc<RefCell<Stats>>,
    /// Whether the microphone is muted. Read per block: a muted mic sends
    /// silence, so the far side's jitter buffer sees an unbroken stream
    /// rather than a gap to conceal.
    muted: Rc<Cell<bool>>,
    /// A screen share's audio, while there is one: its capture and its packer.
    screen_audio: Option<(audio_io::Capture, Rc<RefCell<Packer>>)>,
    screen_audio_opening: bool,
}

/// How a [`Sender`] starts.
#[derive(Debug, Clone, Copy)]
pub struct SendSettings {
    pub processing: Processing,
    pub voice: VoiceStages,
    pub model: ModelSettings,
    /// Where the DeepFilterNet model is served from. Fetched the first time
    /// the neural stage is built, and kept for the life of the page.
    pub model_path: &'static str,
}

impl Sender {
    /// Open the microphone. Every datagram goes into `packets`, already in
    /// `call_proto::audio`'s format; whoever holds the other end sends it.
    pub async fn open(
        settings: SendSettings,
        packets: mpsc::Sender<Vec<u8>>,
        stats: Rc<RefCell<Stats>>,
    ) -> Result<Sender, String> {
        let packer = Rc::new(RefCell::new(Packer::new(
            TRACK_MAIN,
            Rc::new(Cell::new(VOICE_RATE)),
            packets,
            stats.clone(),
        )));
        let muted = Rc::new(Cell::new(false));
        let voice = Rc::new(Cell::new(settings.voice));
        let (mic, mic_writer) = AudioStream::new();
        let SendSettings { processing, model, model_path, .. } = settings;
        let stage = build_stage(&mic, &packer, &voice, processing.suppression, model, model_path).await?;
        let clock = packer.borrow().clock.clone();
        let capture =
            open_capture(mic_writer.clone(), stats.clone(), voice.clone(), clock, muted.clone(), processing).await?;
        Ok(Sender(Rc::new(RefCell::new(Inner {
            stage,
            capture: Some(capture),
            mic,
            mic_writer,
            packer,
            processing,
            model,
            model_path,
            retuning: Rc::new(Cell::new((false, false))),
            voice,
            stats,
            muted,
            screen_audio: None,
            screen_audio_opening: false,
        }))))
    }

    /// Mute or unmute the microphone, live.
    pub fn set_muted(&self, on: bool) {
        self.0.borrow().muted.set(on);
    }

    pub fn muted(&self) -> bool {
        self.0.borrow().muted.get()
    }

    /// The processing running now — what the mic was last (re)opened with.
    pub fn processing(&self) -> Processing {
        self.0.borrow().processing
    }

    /// What the voice is coded at from the next block — bitrate control's
    /// lever. A change of band rebuilds the encoder; of bitrate alone, not.
    pub fn set_rate(&self, rate: Rate) {
        self.0.borrow().packer.borrow().rate.set(rate);
    }

    /// What the voice is being coded at.
    pub fn rate(&self) -> Rate {
        self.0.borrow().packer.borrow().rate.get()
    }

    /// The cell the voice's rate is read from, for a bitrate controller to
    /// set without holding the sender — and with it the microphone — open.
    pub fn rate_cell(&self) -> Rc<Cell<Rate>> {
        self.0.borrow().packer.borrow().rate.clone()
    }

    /// Choose which voice-processing stages run, live.
    pub fn set_voice(&self, stages: VoiceStages) {
        self.0.borrow().voice.set(stages);
    }

    /// Change the processing in place.
    ///
    /// A new suppression stage is built before the old one is let go, so
    /// speech carries straight across the change. Echo cancellation and
    /// browser suppression are properties of the captured track, so changing
    /// either reopens the mic — which does leave a short gap, released first
    /// so two tracks never feed the stream at once.
    pub fn reprocess(&self, processing: Processing) -> impl Future<Output = Result<(), String>> {
        let weak = Rc::downgrade(&self.0);
        async move {
            let Some((old, mic, packer, voice, model, model_path)) = weak.upgrade().map(|s| {
                let s = s.borrow();
                (s.processing, s.mic.clone(), s.packer.clone(), s.voice.clone(), s.model, s.model_path)
            }) else {
                return Ok(());
            };
            let stage = if old.suppression.stage() != processing.suppression.stage() {
                Some(build_stage(&mic, &packer, &voice, processing.suppression, model, model_path).await?)
            } else {
                None
            };
            let (was, now) = (capture_config(old), capture_config(processing));
            let reopen = was.echo_cancellation != now.echo_cancellation
                || was.noise_suppression != now.noise_suppression
                || was.auto_gain_control != now.auto_gain_control;

            // Hung up while the stage was building: it is simply dropped.
            let Some(inner) = weak.upgrade() else { return Ok(()) };
            let (writer, stats, clock, muted) = {
                let mut s = inner.borrow_mut();
                if let Some(stage) = stage {
                    s.stage = stage;
                }
                if reopen {
                    s.capture = None;
                }
                s.processing = processing;
                let clock = s.packer.borrow().clock.clone();
                (s.mic_writer.clone(), s.stats.clone(), clock, s.muted.clone())
            };
            drop(inner);
            if reopen {
                let capture = open_capture(writer, stats, voice, clock, muted, processing).await?;
                if let Some(inner) = weak.upgrade() {
                    inner.borrow_mut().capture = Some(capture);
                }
            }
            Ok(())
        }
    }

    /// Change DeepFilterNet's tuning, live.
    ///
    /// libDF takes its parameters when the model is built, so this builds a
    /// new stage with them and swaps it in — while the old one keeps feeding,
    /// as a suppression change does. A slider being dragged asks for this many
    /// times a second, so requests coalesce: one build at a time, and when it
    /// finishes, one more with whatever the settings are by then if anything
    /// changed.
    pub fn retune(&self, model: ModelSettings) -> impl Future<Output = Result<(), String>> {
        let weak = Rc::downgrade(&self.0);
        let retuning = {
            let mut s = self.0.borrow_mut();
            s.model = model;
            // Not running: applied when DeepFilterNet is next chosen.
            (s.processing.suppression == Suppression::Neural).then(|| s.retuning.clone())
        };
        async move {
            let Some(retuning) = retuning else { return Ok(()) };
            if retuning.get().0 {
                retuning.set((true, true));
                return Ok(());
            }
            let result = retune_loop(&weak, &retuning).await;
            retuning.set((false, false));
            result
        }
    }

    /// Whether [`sync_screen`](Sender::sync_screen) has something to do:
    /// `share_audio` (whether the share has audio to send) and what is being
    /// sent disagree, and nothing is already opening.
    pub fn screen_out_of_sync(&self, share_audio: bool) -> bool {
        let s = self.0.borrow();
        !s.screen_audio_opening && share_audio != s.screen_audio.is_some()
    }

    /// Start or stop sending a screen share's audio, as audio track
    /// [`TRACK_SCREEN`], to match `stream` — the share's own stream when it
    /// has audio, `None` when it has none or has ended. `still_wanted` is
    /// asked once the capture is open, since the share may have stopped while
    /// it was opening.
    ///
    /// Sent as it is — no suppression, no levelling, a higher bitrate —
    /// because it is content, not a voice.
    pub fn sync_screen(
        &self,
        stream: Option<web_sys::MediaStream>,
        still_wanted: impl Fn() -> bool + 'static,
    ) -> impl Future<Output = ()> {
        let weak = Rc::downgrade(&self.0);
        let start = {
            let mut s = self.0.borrow_mut();
            match (stream, s.screen_audio.is_some()) {
                (None, true) => {
                    s.screen_audio = None;
                    None
                }
                (Some(stream), false) if !s.screen_audio_opening => {
                    s.screen_audio_opening = true;
                    let main = s.packer.borrow();
                    Some((stream, main.packets.clone(), s.stats.clone()))
                }
                _ => None,
            }
        };
        async move {
            let Some((stream, packets, stats)) = start else { return };
            let packer =
                Rc::new(RefCell::new(Packer::new(TRACK_SCREEN, Rc::new(Cell::new(SCREEN_AUDIO_RATE)), packets, stats)));
            let opened = {
                let packer = packer.clone();
                audio_io::Capture::open_stream(
                    &stream,
                    audio_io::CaptureConfig { channels: 1, ..Default::default() },
                    audio_io::Pipeline::new(),
                    move |pcm, rate, channels, _frames| {
                        // No stages, so no delay to account for: the block's
                        // first sample was captured a block's length before it
                        // arrived.
                        let frames = (pcm.len() / channels.max(1) as usize) as f64;
                        let capture = now_ms() - frames * 1000.0 / rate.max(1) as f64;
                        packer.borrow_mut().push(pcm, rate, channels, Some(capture));
                    },
                )
                .await
            };
            let Some(inner) = weak.upgrade() else { return };
            let mut s = inner.borrow_mut();
            s.screen_audio_opening = false;
            match opened {
                Ok(capture) if still_wanted() => {
                    log("[call-audio] sending the screen share's audio");
                    s.screen_audio = Some((capture, packer));
                }
                Ok(_) => {}
                Err(e) => log(&format!("[call-audio] cannot send the screen share's audio: {e}")),
            }
        }
    }
}

async fn retune_loop(weak: &Weak<RefCell<Inner>>, retuning: &Rc<Cell<(bool, bool)>>) -> Result<(), String> {
    loop {
        retuning.set((true, false));
        let Some((mic, packer, voice, model, suppression, model_path)) = weak.upgrade().map(|s| {
            let s = s.borrow();
            (s.mic.clone(), s.packer.clone(), s.voice.clone(), s.model, s.processing.suppression, s.model_path)
        }) else {
            return Ok(());
        };
        if suppression != Suppression::Neural {
            return Ok(());
        }
        let stage = build_stage(&mic, &packer, &voice, suppression, model, model_path).await?;
        if let Some(inner) = weak.upgrade() {
            inner.borrow_mut().stage = stage;
        }
        if !retuning.get().1 {
            return Ok(());
        }
    }
}

/// Build the stage for `suppression`, already feeding `packer` through the
/// conditioner.
async fn build_stage(
    mic: &AudioStream,
    packer: &Rc<RefCell<Packer>>,
    voice: &Rc<Cell<VoiceStages>>,
    suppression: Suppression,
    model: ModelSettings,
    model_path: &'static str,
) -> Result<Stage, String> {
    // `model_delay` is the model's latency in samples (0 for none). Every
    // stage emits exactly as many samples as it takes and only delays them, so
    // the n-th sample out of this one is mic sample `start + n - delays`.
    let clock = packer.borrow().clock.clone();
    let feed = |stream: &AudioStream, input: Option<Rc<Cell<f32>>>, hops: usize, model_delay: usize| {
        let packer = packer.clone();
        let voice = voice.clone();
        let clock = clock.clone();
        let start = clock.borrow().next() as i64;
        let mut emitted: i64 = 0;
        let mut conditioner: Option<Conditioner> = None;
        let mut activity = Activity::new(input, hops);
        stream.subscribe(move |f: &AudioFrame| {
            let stages = voice.get();
            let samples = f.samples;
            let (speaking, verdict) = activity.observe(samples, f.sample_rate, f.channels);
            let frames = (samples.len() / f.channels.max(1) as usize) as i64;
            let conditioned = stages.any_after_stage() && f.channels == 1;
            let lookahead_ms = if conditioned {
                (if stages.expand { voice_dsp::EXPANDER_LOOKAHEAD_MS } else { 0.0 })
                    + (if stages.declick { voice_dsp::DECLICKER_LOOKAHEAD_MS } else { 0.0 })
            } else {
                0.0
            };
            let delay = model_delay as i64 + (lookahead_ms / 1000.0 * f.sample_rate as f32) as i64;
            {
                let p = packer.borrow();
                let mut s = p.stats.borrow_mut();
                s.speaking = speaking;
                s.model_gain_db = verdict;
                s.send_delay_ms = s.input_latency_ms
                    + delay as f64 * 1000.0 / f.sample_rate.max(1) as f64
                    + crate::FRAME.ms() as f64;
            }
            let capture = clock.borrow().at(start + emitted - delay, f.sample_rate);
            emitted += frames;
            // The conditioner is mono; the stage's output always is (the
            // capture is opened mono and the model emits mono).
            if !conditioned {
                packer.borrow_mut().push(samples, f.sample_rate, f.channels, capture);
                return;
            }
            if conditioner.as_ref().map(|c| c.rate != f.sample_rate).unwrap_or(true) {
                conditioner = Some(Conditioner::new(f.sample_rate));
            }
            let out = conditioner.as_mut().expect("built above").process(samples, stages, verdict);
            packer.borrow_mut().push(out, f.sample_rate, 1, capture);
        })
    };
    if suppression != Suppression::Neural {
        return Ok(Stage::Direct { _feed: feed(mic, None, 1, 0) });
    }
    let crate::NeuralStage { clean, latency } = crate::neural::build(mic.clone(), model, model_path).await?;

    // What went in, carried across the gaps between mic blocks — a level, not
    // an accumulator: the mic delivers ~190 blocks a second and the model 100,
    // so a per-hop reset would read silence on most hops and call the model's
    // output a 100 dB boost.
    let input = Rc::new(Cell::new(0.0f32));
    let meter = {
        let input = input.clone();
        let mut meter = voice_dsp::Meter::new(METER_MS);
        mic.subscribe(move |f: &AudioFrame| {
            let dt = (f.samples.len() / f.channels.max(1) as usize) as f32 / f.sample_rate.max(1) as f32;
            input.set(meter.observe(voice_dsp::rms(f.samples), dt));
        })
    };
    log(&format!("[call-audio] neural suppression running: {model:?}"));
    Ok(Stage::Neural {
        // Latency in 10 ms hops, for lining the model's verdict up.
        _feed: feed(&clean, Some(input), (latency / 480).max(1), latency),
        _meter: meter,
        _clean: clean,
    })
}

/// Open the mic, writing every block into the call's microphone stream.
async fn open_capture(
    writer: AudioWriter,
    stats: Rc<RefCell<Stats>>,
    voice: Rc<Cell<VoiceStages>>,
    clock: Rc<RefCell<av_sync::CaptureClock>>,
    muted: Rc<Cell<bool>>,
    processing: Processing,
) -> Result<audio_io::Capture, String> {
    // Hum comes out first, before suppression or the leveller measure it.
    let mut dehum: Option<(voice_dsp::Dehum, u32)> = None;
    // The mic's own latency, once the track reports it: capture times are when
    // the sound happened, not when the browser handed it over.
    let input_latency = Rc::new(Cell::new(0.0f64));
    let latency = input_latency.clone();
    let mut silence: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let reported = stats.clone();
    let capture = audio_io::Capture::open(
        capture_config(processing),
        audio_io::Pipeline::new(),
        move |pcm, rate, channels, _frames| {
            // Muted is silence, not nothing: see `Inner::muted`.
            let pcm = if muted.get() {
                silence.clear();
                silence.resize(pcm.len(), 0.0);
                &silence[..]
            } else {
                pcm
            };
            {
                let mut s = stats.borrow_mut();
                s.rate = rate;
                s.mic_peak = pcm.iter().fold(s.mic_peak, |m, x| m.max(x.abs()));
            }
            let pcm = if voice.get().dehum && channels == 1 {
                if dehum.as_ref().map(|(_, r)| *r != rate).unwrap_or(true) {
                    dehum = Some((voice_dsp::Dehum::new(voice_dsp::Hum::default(), rate), rate));
                }
                scratch.clear();
                scratch.extend_from_slice(pcm);
                let d = &mut dehum.as_mut().expect("built above").0;
                d.process(&mut scratch);
                stats.borrow_mut().hum_notches = d.notched();
                &scratch[..]
            } else {
                stats.borrow_mut().hum_notches = 0;
                pcm
            };
            // On web this runs the stage — and the model — inline, so timing
            // it times the inference.
            let start = now_ms();
            // Marked before the write, which on web runs the stage inline and
            // reads the clock.
            clock.borrow_mut().mark((pcm.len() / channels.max(1) as usize) as u64, rate, start - latency.get());
            writer.write_pcm_f32(rate, channels, pcm);
            let mut s = stats.borrow_mut();
            s.dsp_ms += now_ms() - start;
            s.audio_ms += (pcm.len() / channels.max(1) as usize) as f64 * 1000.0 / rate.max(1) as f64;
        },
    )
    .await
    .map_err(|e| format!("microphone: {e}"))?;

    input_latency.set(capture.input_latency_ms());
    {
        let mut r = reported.borrow_mut();
        r.input_latency_ms = capture.input_latency_ms();
        r.off_main_thread = capture.opened().off_main_thread;
    }
    let opened = capture.opened();
    log(&format!(
        "[call-audio] capture {} Hz, {} frames/block, off main thread: {}, input latency {:.1} ms, {:?}",
        opened.sample_rate, opened.block_frames, opened.off_main_thread, input_latency.get(), processing,
    ));
    Ok(capture)
}

