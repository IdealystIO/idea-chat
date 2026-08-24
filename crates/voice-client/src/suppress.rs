//! Client-side neural suppression: DeepFilterNet before the wire.
//!
//! ```text
//!   mic ──▶ AudioStream ──▶ DFN3 ──▶ Leveler ──▶ Packet ──▶ relay ──▶ peers
//!            48 kHz mono      ─── all on this machine ───
//! ```
//!
//! The clean signal is what gets transmitted, so this is the version of the
//! feature that scales: the relay stays a relay, the noise costs no bandwidth
//! because it never leaves the room it was recorded in, and the ~30 ms of
//! algorithmic latency lands at the edge of the path rather than in its
//! middle.
//!
//! # What it costs, and who pays
//!
//! On web the model is deliberately not compiled into the wasm binary — it
//! would add 8 MB to a bundle every visitor downloads, including the ones who
//! never open a voice room. So it is fetched on first join instead:
//!
//! - **8.0 MB**, once, then in the HTTP cache. Bigger than the app itself.
//! - **one inference per 10 ms hop, on the main thread**, because wasm has no
//!   threads and the SDK's web backend runs the engine inline in the subscribe
//!   callback. Native measures 0.11 ms per hop; whatever wasm's multiple of
//!   that turns out to be, it is spent in the same place the UI renders.
//!
//! Neither cost exists server-side. That is the trade the two modes exist to
//! let you hear.
//!
//! # Why the fetch is deliberately not eager
//!
//! Starting it at app boot would spend 8 MB of someone's connection on a
//! feature they may never turn on, and on a phone that is a real number. It
//! starts when a room is joined with suppression asked for, and the first join
//! is correspondingly slower — reported as such rather than hidden, because a
//! call that takes four seconds to connect for an unexplained reason is worse
//! than one that says what it is waiting for.

use std::cell::RefCell;

use denoise::Denoiser;
use media_stream::{AudioFrame, AudioStream, AudioSubscription, AudioWriter};
use std::sync::{Arc, Mutex};

use voice_dsp::{Alignment, Declicker, Expander, Leveler, Tuning};

/// Where the model is served from, relative to the app's origin.
///
/// Hearth serves it out of the same directory as the wasm bundle; see
/// `tools/fetch-model.sh` for how it gets there. Kept as a path rather than a
/// full URL so a deployment behind any hostname works without configuration.
pub const MODEL_PATH: &str = "/assets/DeepFilterNet3_onnx.tar.gz";

thread_local! {
    /// The fetched model, kept for the life of the page.
    ///
    /// `&'static [u8]` because upstream's `DfParams::from_bytes` demands it,
    /// which makes this a deliberate one-time leak rather than an accident: a
    /// denoising model is loaded once and outlives every call made with it.
    /// A `thread_local` and not a `static` because wasm is single-threaded and
    /// `OnceLock` would only add a lock this target cannot contend for.
    static WEIGHTS: RefCell<Option<&'static [u8]>> = const { RefCell::new(None) };
}

/// Fetch the model, or return the copy already fetched.
///
/// Concurrent callers are not coordinated — two rooms joined in the same
/// instant would fetch twice and leak the loser. Joining two voice rooms
/// simultaneously is not a thing the app can do (see `hearth::voice::LIVE`),
/// so a flight-in-progress map would be machinery guarding an impossible case.
pub async fn weights() -> Result<&'static [u8], String> {
    if let Some(cached) = WEIGHTS.with(|w| *w.borrow()) {
        return Ok(cached);
    }

    let response = net::Client::new()
        .get(MODEL_PATH)
        .send()
        .await
        .map_err(|e| format!("could not fetch the noise-suppression model: {e}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("could not read the noise-suppression model: {e}"))?;

    // A truncated or misconfigured asset (an SPA fallback serving index.html,
    // say) would otherwise reach `DfParams::from_bytes` and come back as an
    // opaque model-init failure. The real model is ~8 MB.
    if bytes.len() < 1_000_000 {
        return Err(format!(
            "the model at {MODEL_PATH} is only {} bytes — is it actually being served?",
            bytes.len(),
        ));
    }

    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    WEIGHTS.with(|w| *w.borrow_mut() = Some(leaked));
    Ok(leaked)
}

/// Time constant for the levels the model's verdict is computed from.
///
/// Long enough to ride over a hop that happened to be quiet, short enough to
/// follow a syllable. Both sides of the ratio use it.
const METER_MS: f32 = 60.0;

/// Below this output level nothing counts as speech, whatever the ratio says.
/// About -54 dBFS.
const SPEECH_FLOOR: f32 = 0.002;

/// The stage between the microphone and the wire.
///
/// Exactly one is installed at a time, and swapping it is what makes the
/// setting take effect on a call already in progress. Dropping a `Chain` stops
/// it feeding the wire; dropping the neural one also tears down the model.
///
/// Field order is teardown order: the subscription that writes stops before
/// the stream it reads from goes away.
pub struct Chain {
    _stage: Stage,
}

enum Stage {
    /// The microphone, forwarded verbatim.
    ///
    /// A copy per hop that a direct subscription would not need — the wire
    /// stream exists so the packetiser has ONE thing to read for the life of
    /// the call, and 480 floats every 10 ms is a cheap price for being able to
    /// change your mind mid-call.
    Direct { _sub: AudioSubscription },
    /// DeepFilterNet, then the stages that clean up after it.
    Neural {
        _sub: AudioSubscription,
        /// The second measurement of the microphone, for the verdict.
        _mic_sub: AudioSubscription,
        _clean: AudioStream,
    },
}

/// Forward the microphone to `out` untouched.
///
/// Still reports voice activity, by level — there is no model to ask, so the
/// meter falls back to the only thing available. Worth doing rather than
/// leaving the indicator dark: a speaking light that only works with
/// suppression on is a speaking light nobody trusts.
pub fn direct(mic: &AudioStream, out: AudioWriter) -> Chain {
    // About -45 dBFS. Above any reasonable room, below any speech.
    const SPEECH: f32 = 0.006;
    let sub = mic.subscribe(move |frame: &AudioFrame| {
        crate::session::report_activity(voice_dsp::rms(frame.samples) > SPEECH, 0.0);
        out.write_pcm_f32(frame.sample_rate, frame.channels, frame.samples);
    });
    Chain { _stage: Stage::Direct { _sub: sub } }
}

/// A neural chain that is running but not yet connected to anything.
///
/// The two-phase build is the point. Fetching the model and planning the graph
/// takes a few hundred milliseconds, and doing that *after* unplugging the
/// current chain would drop the caller's audio for the duration — an audible
/// hole every time somebody changes a setting. So the model is built while the
/// old chain is still feeding the wire, and the swap itself is two moves with
/// nothing between them.
pub struct Prepared {
    clean: AudioStream,
    /// Kept so `install` can take its own measurement of what went INTO the
    /// model. The verdict is a ratio, and half of it is only available here.
    mic: AudioStream,
    tuning: Tuning,
}

/// Build a neural chain without installing it. See [`Prepared`].
pub async fn prepare(mic: &AudioStream, tuning: Tuning) -> Result<Prepared, String> {
    let weights = weights().await?;

    let mut denoiser = Denoiser::with_weights(weights).post_filter(tuning.post_filter);
    if let Some(limit) = tuning.atten_lim_db {
        denoiser = denoiser.attenuation_limit_db(limit);
    }
    let clean = denoiser
        .process(mic)
        .await
        .map_err(|e| format!("noise suppression could not start: {e}"))?;

    Ok(Prepared { clean, mic: mic.clone(), tuning })
}

impl Prepared {
    /// Connect the prepared model to `out`.
    ///
    /// The post-model stages live here because a subscriber sees *borrowed*
    /// samples: this copies once, works in place, and republishes. One extra
    /// copy per 10 ms hop, which is 480 floats.
    ///
    /// Order is fixed and each step depends on the one before it:
    ///
    /// 1. **declick**, before levelling — a keystroke is the loudest thing in
    ///    its frame, so a leveller that saw it first would duck the whole
    ///    sentence around it and then let go, audible as a lurch per keypress.
    /// 2. **level**, so everyone in the room arrives at a similar volume.
    /// 3. **expand**, last, because nothing after it may amplify what it
    ///    removed — and the leveller boosts by up to 18 dB.
    ///
    /// The expander is driven by the model's own verdict, which is why this
    /// also measures what went *in*: see `voice_dsp::verdict`.
    pub fn install(self, out: AudioWriter) -> Chain {
        // The input level, carried across the gaps between microphone frames.
        //
        // NOT an accumulator that resets per output hop. The microphone
        // delivers about twelve frames a second and the model emits a hundred
        // blocks, so seven hops in eight would find it empty, read zero input,
        // and conclude the model had ADDED 100 dB — which is exactly what
        // happened: a speaking light that never went out and a residual stage
        // that never ran.
        let input: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
        let mic_sub = {
            let input = input.clone();
            let mut meter = voice_dsp::Meter::new(METER_MS);
            self.mic.subscribe(move |frame: &AudioFrame| {
                let frames = frame.samples.len() / frame.channels.max(1) as usize;
                let dt = frames as f32 / frame.sample_rate.max(1) as f32;
                let level = meter.observe(voice_dsp::rms(frame.samples), dt);
                if let Ok(mut shared) = input.lock() {
                    *shared = level;
                }
            })
        };

        let mut declicker = self.tuning.transients.map(|cfg| Declicker::new(cfg, 48_000));
        let mut leveler = self.tuning.leveling.map(|cfg| Leveler::new(cfg, 48_000));
        let mut expander = self.tuning.residual.map(|cfg| Expander::new(cfg, 48_000));
        let verdict_db = self.tuning.residual.map(|cfg| cfg.verdict_db).unwrap_or(-9.0);
        let mut align = Alignment::new(voice_dsp::MODEL_HOPS);
        // The output's own meter, same constant as the input's, so the ratio
        // between them is a ratio of like for like.
        let mut output = voice_dsp::Meter::new(METER_MS);
        let mut scratch: Vec<f32> = Vec::new();

        let sub = self.clean.subscribe(move |frame: &AudioFrame| {
            // What the model did to this block, aligned across its latency.
            let frames = frame.samples.len() / frame.channels.max(1) as usize;
            let dt = frames as f32 / frame.sample_rate.max(1) as f32;
            let out_level = output.observe(voice_dsp::rms(frame.samples), dt);
            let in_level = input.lock().map(|v| *v).unwrap_or(0.0);
            let gain_db = voice_dsp::model_gain_db(out_level, align.delay(in_level));

            // The model's verdict IS a voice-activity decision, and a much
            // better one than a level threshold: it is what a speech model
            // concluded, not what an envelope crossed.
            //
            // Guarded by the output level as well, because a ratio is only as
            // good as its denominator — at the start of a call there is no
            // input measurement yet, and silence is not speech however
            // favourably it divides.
            crate::session::report_activity(
                gain_db > verdict_db && out_level > SPEECH_FLOOR,
                gain_db,
            );

            scratch.clear();
            scratch.extend_from_slice(frame.samples);
            if let Some(declicker) = declicker.as_mut() {
                declicker.process(&mut scratch);
            }
            if let Some(leveler) = leveler.as_mut() {
                leveler.process(&mut scratch);
            }
            if let Some(expander) = expander.as_mut() {
                expander.process(&mut scratch, Some(gain_db));
            }
            out.write_pcm_f32(frame.sample_rate, frame.channels, &scratch);
        });

        Chain { _stage: Stage::Neural { _sub: sub, _mic_sub: mic_sub, _clean: self.clean } }
    }
}

/// Time this machine's DeepFilterNet, in the browser, on the real bundle.
///
/// The number this returns is the one that decides whether client-side
/// suppression is a feature or a stutter, and it cannot be inferred from the
/// native benchmark: `tract` on wasm has no SIMD and no threads, and the
/// engine runs on the same thread as the UI. A multiple of native that is fine
/// on a desktop may not be fine on a phone.
///
/// Measurable at all because of an accident of the web backend: with no
/// threads, `write_pcm_f32` runs the subscribe callback — and therefore the
/// inference — *inline*. So wall-clock around the writes IS the inference
/// time, with no waiting and no instrumentation inside the SDK.
///
/// Returns JSON. Exposed to the page by `hearth::voice::install_stats_probe`,
/// for the same reason the transport counters are: whether audio is *keeping
/// up* is invisible from the UI, and shaping the product around a test would
/// be worse than one honest seam.
pub async fn bench(seconds: u32) -> Result<String, String> {
    let weights = weights().await?;

    let (input, writer) = AudioStream::new();
    let built = now_ms();
    let clean = Denoiser::with_weights(weights)
        .attenuation_limit_db(24.0)
        .post_filter(true)
        .process(&input)
        .await
        .map_err(|e| format!("model build: {e}"))?;
    let build_ms = now_ms() - built;

    let hops = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let _sub = {
        let hops = hops.clone();
        clean.subscribe(move |_: &AudioFrame| hops.set(hops.get() + 1))
    };

    // Speech-shaped-ish: two voiced harmonics under hiss. The content does not
    // change the cost — DeepFilterNet spends the same arithmetic on silence —
    // but benchmarking on zeros would be a suspicious fixture.
    let total = 48_000 * seconds as usize;
    let mut seed = 0x9E37_79B9u32;
    let audio: Vec<f32> = (0..total)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let hiss = (seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0;
            let t = i as f32 / 48_000.0;
            (t * 140.0 * std::f32::consts::TAU).sin() * 0.25 + hiss * 0.15
        })
        .collect();

    let started = now_ms();
    for chunk in audio.chunks(480) {
        writer.write_pcm_f32(48_000, 1, chunk);
    }
    let process_ms = now_ms() - started;

    let audio_ms = seconds as f64 * 1000.0;
    Ok(format!(
        r#"{{"buildMs":{:.0},"processMs":{:.0},"rtf":{:.4},"perHopMs":{:.3},"hops":{}}}"#,
        build_ms,
        process_ms,
        process_ms / audio_ms,
        process_ms / hops.get().max(1) as f64,
        hops.get(),
    ))
}

/// A millisecond clock. `performance.now()` on web; a monotonic instant
/// elsewhere, so the benchmark is at least compilable off-target.
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
    }
}
