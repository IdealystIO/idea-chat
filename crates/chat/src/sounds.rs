//! A call's cues: you're in, you're out, someone came, someone went, the
//! line dropped, the line's back.
//!
//! Synthesised, so there is nothing to load and nothing that can be missing
//! when it is needed. Each note is a struck, bell-ish tone — a fundamental
//! and three quieter partials (one a little off the harmonic series, which
//! is what makes a bell a bell rather than an organ), each dying away faster
//! the higher it is — through a gentle low-pass, so nothing is shrill at
//! volume. Rising for arrivals, falling for departures: old enough an
//! association to need no learning.
//!
//! # Robustness
//!
//! - **One context**, made on the click that joins a call (a user gesture,
//!   so the browser lets it run) and kept. A context per sound is a fresh
//!   chance to be refused, and browsers cap how many may exist.
//! - **Never late.** A context the browser suspended (a backgrounded tab) is
//!   resumed, and the cue played only if that takes under
//!   [`STALE_MS`]: a "someone joined" heard ten seconds on is worse than
//!   none.
//! - **Never a pile-up.** The same cue twice within [`REPEAT_MS`] plays
//!   once. When cues play — and which are kept quiet through a reconnect —
//!   is `media`'s business; see `media::web::watch_people`.

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// You are in the call.
    Joined,
    /// You left it.
    Left,
    /// Someone came into your call.
    PersonJoined,
    /// Someone left it.
    PersonLeft,
    /// Your connection to the call dropped; trying to get back.
    Lost,
    /// And you are back.
    Back,
}

impl Cue {
    pub fn name(self) -> &'static str {
        match self {
            Cue::Joined => "joined",
            Cue::Left => "left",
            Cue::PersonJoined => "person-joined",
            Cue::PersonLeft => "person-left",
            Cue::Lost => "lost",
            Cue::Back => "back",
        }
    }

    /// `(notes as (Hz, seconds after the first), level)`. D major's
    /// D–F♯–A, so every cue agrees with every other; your own are a fifth
    /// apart and louder, other people's a third apart and softer.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn notes(self) -> (&'static [(f32, f64)], f32) {
        const D5: f32 = 587.33;
        const FS5: f32 = 739.99;
        const A5: f32 = 880.0;
        const A4: f32 = 440.0;
        const FS4: f32 = 369.99;
        const D4: f32 = 293.66;
        match self {
            Cue::Joined => (&[(D5, 0.0), (A5, 0.085)], 0.16),
            Cue::Left => (&[(A5, 0.0), (D5, 0.085)], 0.14),
            Cue::PersonJoined => (&[(FS5, 0.0), (A5, 0.07)], 0.09),
            Cue::PersonLeft => (&[(A5, 0.0), (FS5, 0.07)], 0.08),
            // Low and falling, three steps: unmistakably not a person.
            Cue::Lost => (&[(A4, 0.0), (FS4, 0.11), (D4, 0.22)], 0.13),
            Cue::Back => (&[(D5, 0.0), (FS5, 0.06), (A5, 0.12)], 0.14),
        }
    }
}

/// Every cue's pitch, as a multiple of the notes below: 0.5 is an octave
/// down. One knob, so the set moves together and keeps its intervals.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const PITCH: f32 = 0.5;

/// A cue that cannot start within this long of being asked for is dropped.
pub const STALE_MS: f64 = 300.0;
/// The same cue again within this long is the same moment.
pub const REPEAT_MS: f64 = 400.0;

/// Make (or wake) the audio context. Call from a user gesture.
pub fn unlock() {
    #[cfg(target_arch = "wasm32")]
    web::unlock();
}

/// Play a cue, best effort.
pub fn play(cue: Cue) {
    #[cfg(target_arch = "wasm32")]
    web::play(cue);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = cue;
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;

    use wasm_bindgen::prelude::*;
    use web_sys::{AudioContext, AudioContextState, BiquadFilterType, OscillatorType};

    use super::{Cue, PITCH, REPEAT_MS, STALE_MS};

    /// Each partial: (multiple of the fundamental, relative level). The 4.2
    /// is the bell's inharmonic shimmer.
    const PARTIALS: [(f32, f32); 4] = [(1.0, 1.0), (2.0, 0.32), (3.0, 0.1), (4.2, 0.05)];
    /// How fast the fundamental dies away (exponential time constant, s);
    /// partial n dies n times as fast.
    const DECAY: f64 = 0.13;
    /// How long a note's oscillators run: long enough for the decay to be
    /// inaudible.
    const RING: f64 = 0.9;
    const ATTACK: f64 = 0.005;

    thread_local! {
        static CTX: RefCell<Option<AudioContext>> = const { RefCell::new(None) };
        static LAST: RefCell<Option<(Cue, f64)>> = const { RefCell::new(None) };
        /// What played, for the end-to-end test (`__chatCues`).
        static PLAYED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
        static PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn now_ms() -> f64 {
        js_sys::Date::now()
    }

    fn context() -> Option<AudioContext> {
        CTX.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_none() {
                *c = AudioContext::new().ok();
            }
            c.clone()
        })
    }

    pub fn unlock() {
        install_probe();
        if let Some(ctx) = context() {
            if ctx.state() != AudioContextState::Running {
                let _ = ctx.resume();
            }
        }
    }

    pub fn play(cue: Cue) {
        install_probe();
        let at = now_ms();
        let repeat = LAST.with(|l| {
            let mut l = l.borrow_mut();
            let repeat = matches!(*l, Some((c, t)) if c == cue && at - t < REPEAT_MS);
            if !repeat {
                *l = Some((cue, at));
            }
            repeat
        });
        if repeat {
            return;
        }
        let Some(ctx) = context() else { return };
        if ctx.state() == AudioContextState::Running {
            let _ = schedule(&ctx, cue);
            return;
        }
        // Suspended: wake it, and play only if that was quick.
        let Ok(waking) = ctx.resume() else { return };
        wasm_bindgen_futures::spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(waking).await;
            if now_ms() - at < STALE_MS && ctx.state() == AudioContextState::Running {
                let _ = schedule(&ctx, cue);
            }
        });
    }

    fn schedule(ctx: &AudioContext, cue: Cue) -> Result<(), JsValue> {
        PLAYED.with(|p| p.borrow_mut().push(cue.name()));
        let (notes, level) = cue.notes();
        // A little ahead, so the first note's attack is not clipped.
        let start = ctx.current_time() + 0.01;

        let tone = ctx.create_biquad_filter()?;
        tone.set_type(BiquadFilterType::Lowpass);
        tone.frequency().set_value(4_500.0);
        tone.q().set_value(0.5);
        tone.connect_with_audio_node(&ctx.destination())?;

        for &(hz, offset) in notes {
            let t = start + offset;
            for (n, &(ratio, rel)) in PARTIALS.iter().enumerate() {
                let osc = ctx.create_oscillator()?;
                osc.set_type(OscillatorType::Sine);
                osc.frequency().set_value(hz * PITCH * ratio);
                let env = ctx.create_gain()?;
                let peak = level * rel;
                let g = env.gain();
                g.set_value_at_time(0.0, t)?;
                g.linear_ramp_to_value_at_time(peak, t + ATTACK)?;
                g.set_target_at_time(0.0, t + ATTACK, DECAY / (n as f64 + 1.0))?;
                osc.connect_with_audio_node(&env)?;
                env.connect_with_audio_node(&tone)?;
                osc.start_with_when(t)?;
                osc.stop_with_when(t + RING)?;
            }
        }
        Ok(())
    }

    /// `globalThis.__chatCues()`: the cues played since it was last asked,
    /// as a JSON array of names. A seam for the end-to-end test.
    fn install_probe() {
        if PROBE.with(|p| p.replace(true)) {
            return;
        }
        let probe = Closure::wrap(Box::new(|| -> String {
            let played = PLAYED.with(|p| std::mem::take(&mut *p.borrow_mut()));
            format!("[{}]", played.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(","))
        }) as Box<dyn Fn() -> String>);
        let _ = js_sys::Reflect::set(&js_sys::global(), &JsValue::from_str("__chatCues"), probe.as_ref());
        probe.forget();
    }
}
