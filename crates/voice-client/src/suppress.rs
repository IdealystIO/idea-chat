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
use media_stream::{AudioFrame, AudioStream, AudioSubscription};
use voice_dsp::{Leveler, Tuning};

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

/// A running client-side suppression chain. Dropping it returns capture to the
/// raw microphone stream.
///
/// Field order is teardown order: the levelling subscription stops first, so
/// the engine is never asked for output after the thing that consumes it has
/// gone away.
pub struct Suppressed {
    /// Levelling, applied to the model's output before anything reads it.
    _leveling: AudioSubscription,
    /// The cleaned stream, which is what the session packetises.
    pub stream: AudioStream,
}

/// Insert DeepFilterNet between `mic` and whatever comes next.
///
/// Returns a stream carrying levelled 48 kHz mono. The caller keeps `mic`
/// alive: the denoise pipeline subscribes to it, and a dropped input stream
/// stops producing.
pub async fn insert(mic: &AudioStream, tuning: Tuning) -> Result<Suppressed, String> {
    let weights = weights().await?;

    let mut denoiser = Denoiser::with_weights(weights).post_filter(tuning.post_filter);
    if let Some(limit) = tuning.atten_lim_db {
        denoiser = denoiser.attenuation_limit_db(limit);
    }
    let clean = denoiser
        .process(mic)
        .await
        .map_err(|e| format!("noise suppression could not start: {e}"))?;

    // Levelling has nowhere else to go on this side. The SDK hands back a
    // stream, not a callback chain, so the only place to touch the samples is
    // in a subscriber — and the samples a subscriber sees are borrowed, so
    // this stage re-publishes through a second stream rather than editing in
    // place. One extra copy per 10 ms hop, which is 480 floats.
    let (levelled, writer) = AudioStream::new();
    let mut leveler = tuning.leveling.map(|cfg| Leveler::new(cfg, 48_000));
    let mut scratch: Vec<f32> = Vec::new();
    let sub = clean.subscribe(move |frame: &AudioFrame| match leveler.as_mut() {
        Some(leveler) => {
            scratch.clear();
            scratch.extend_from_slice(frame.samples);
            leveler.process(&mut scratch);
            writer.write_pcm_f32(frame.sample_rate, frame.channels, &scratch);
        }
        None => writer.write_pcm_f32(frame.sample_rate, frame.channels, frame.samples),
    });

    // The denoised stream must outlive this function: `levelled` is what the
    // caller holds, and nothing else refers to `clean`.
    levelled.attach_stopper(move || drop(clean));

    Ok(Suppressed { _leveling: sub, stream: levelled })
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
