//! What conditioning a voice stream gets, and where it happens.
//!
//! One stream, three places it can be cleaned up, and only one of them can be
//! the *last* word — whatever comes out of the chosen stage is what everyone
//! in the room hears.
//!
//! ```text
//!   mic ──▶ [browser AEC/NS] ──▶ [client DFN3] ──▶ wire ──▶ [server DFN3] ──▶ peers
//!            always on            Mode::Client                Mode::Server
//! ```
//!
//! # Why this is a mode and not a pair of booleans
//!
//! Neural suppression twice is worse than once. DeepFilterNet is trained on
//! *noisy speech*; feed it speech that has already been through a suppressor
//! and it works on a signal unlike anything in its training set, chewing on
//! the artefacts of the first pass. Two booleans would let the UI express
//! "both", and someone would eventually tick both boxes and conclude the
//! feature sounds bad. [`Mode`] cannot express it.
//!
//! The browser's own suppression is the same argument, which is why [`Mode`]
//! also decides that: `Client` and `Server` both turn WebRTC's noise
//! suppression OFF and leave echo cancellation ON. AEC is a different job —
//! it needs the far-end reference signal, which only the browser has, and
//! DeepFilterNet explicitly does not do it.
//!
//! # Why the client asks and the server decides
//!
//! `Mode::Server` spends *relay* CPU — roughly one DeepFilterNet instance per
//! talking participant, on a machine shared by everyone in the fleet. A client
//! that could simply take that by asking would be a free amplification lever,
//! so the node publishes what it is willing to do and the answer comes back on
//! the wire (see `voice_server::suppress`). What the client asked for and what
//! it got are separate values everywhere in this code, because presenting a
//! refusal as a grant is how "why does this still sound noisy" becomes
//! unanswerable.

#![forbid(unsafe_code)]

mod level;
mod residual;
mod transient;
mod verdict;

pub use level::{Leveler, Leveling};
pub use residual::{Expander, Residual};
pub use transient::{Declicker, Transients};
pub use verdict::{model_gain_db, rms, Alignment, Meter, MODEL_HOPS};

/// Where neural noise suppression runs for a session.
///
/// Ordered by how much of the pipeline the choice commits: `Off` commits
/// nothing, `Client` commits the speaker's own CPU, `Server` commits the
/// fleet's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// No neural pass. The browser's WebRTC noise suppression is left on, so
    /// this is not "raw" — it is the ordinary-videocall baseline, and the one
    /// worth A/B-ing the other two against.
    #[default]
    Off,
    /// DeepFilterNet runs on the speaker's machine, before the wire.
    ///
    /// The clean signal is what gets transmitted, so it costs bandwidth
    /// nothing and the relay nothing. On web it costs an 8 MB model fetch and
    /// one inference per 10 ms on the main thread.
    Client,
    /// DeepFilterNet runs on the relay, once per sender, after the wire.
    ///
    /// Nobody downloads a model and no phone burns battery on inference — but
    /// the noise has already been paid for in bandwidth by the time it is
    /// removed, and the relay stops being a relay.
    Server,
}

impl Mode {
    /// The wire spelling. Carried as a query parameter on the relay socket and
    /// echoed back by the node, so it must survive a round trip unchanged.
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Client => "client",
            Mode::Server => "server",
        }
    }

    /// Parse the wire spelling. Anything unrecognised is [`Mode::Off`] rather
    /// than an error: an old client naming a mode this build has never heard
    /// of should get an ordinary call, not a refused connection.
    pub fn parse(s: &str) -> Mode {
        match s {
            "client" => Mode::Client,
            "server" => Mode::Server,
            _ => Mode::Off,
        }
    }

    /// Whether the browser's own noise suppression should be enabled.
    ///
    /// False whenever a neural pass will run, anywhere — see the module docs
    /// on why stacking the two is worse than either.
    pub const fn browser_noise_suppression(self) -> bool {
        matches!(self, Mode::Off)
    }

    /// Whether this mode asks the *relay* to do work.
    pub const fn burdens_relay(self) -> bool {
        matches!(self, Mode::Server)
    }
}

/// How hard to run one stage.
///
/// Presets rather than raw decibels, because the numbers underneath are not
/// independent — a declicker's threshold and its depth have to move together
/// or the setting means something different at each end of its range. Four
/// choices a person can pick by ear beat six numbers they have to model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strength {
    /// The stage does not run at all.
    Off,
    /// Audible only on the worst of it. The right answer when a stage is
    /// causing artefacts and you want to know whether it was the culprit.
    Light,
    #[default]
    /// The default. Tuned to be inaudible on speech.
    Standard,
    /// For a genuinely bad room. Trades some naturalness for quiet.
    Strong,
}

impl Strength {
    /// The stored spelling. Round-trips through [`parse`](Self::parse).
    pub const fn as_str(self) -> &'static str {
        match self {
            Strength::Off => "off",
            Strength::Light => "light",
            Strength::Standard => "standard",
            Strength::Strong => "strong",
        }
    }

    /// Parse a stored spelling. Anything unrecognised is the default rather
    /// than an error: a preference written by a newer build should degrade to
    /// something sensible, not refuse to load.
    pub fn parse(s: &str) -> Strength {
        match s {
            "off" => Strength::Off,
            "light" => Strength::Light,
            "strong" => Strength::Strong,
            _ => Strength::Standard,
        }
    }

    /// What the user sees on the control.
    pub const fn label(self) -> &'static str {
        match self {
            Strength::Off => "Off",
            Strength::Light => "Light",
            Strength::Standard => "Standard",
            Strength::Strong => "Strong",
        }
    }

    /// Transient suppression at this strength.
    ///
    /// Threshold and depth move together: a lighter setting is both more
    /// reluctant to call something a keystroke and gentler when it does, which
    /// is what makes "Light" mean one thing rather than two.
    pub fn transients(self) -> Option<Transients> {
        let base = Transients::default();
        match self {
            Strength::Off => None,
            Strength::Light => {
                Some(Transients { onset_ratio: 6.0, max_reduction_db: 14.0, ..base })
            }
            Strength::Standard => Some(base),
            Strength::Strong => {
                Some(Transients { onset_ratio: 3.0, max_reduction_db: 34.0, low_share: 0.5, ..base })
            }
        }
    }

    /// Residual suppression at this strength.
    ///
    /// `verdict_db` is the interesting one: a stricter threshold means more
    /// blocks count as "the model rejected this", so Strong expands more of
    /// what is left — and risks taking quiet speech the model was unsure about.
    pub fn residual(self) -> Option<Residual> {
        let base = Residual::default();
        match self {
            Strength::Off => None,
            Strength::Light => Some(Residual { verdict_db: -14.0, depth_db: 8.0, ..base }),
            Strength::Standard => Some(base),
            Strength::Strong => Some(Residual { verdict_db: -6.0, depth_db: 20.0, ..base }),
        }
    }
}

/// How hard to suppress, and what to do afterwards.
///
/// Shared by both stages so a client-side and a server-side call are the same
/// experiment with the work moved, not two different sounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Cap on background attenuation, in dB. `None` is unlimited suppression,
    /// which is DeepFilterNet's own default.
    ///
    /// This was 24 dB, on the theory that leaving some of the room in keeps the
    /// noise floor continuous and a continuous quiet floor sounds like a room
    /// rather than a sequence of disconnections. That is true of *hum* — and it
    /// is also a hard ceiling on how much of anything else can be removed, so a
    /// door closing came through at 24 dB down, which is still plainly audible.
    /// Somebody on a real call does not want a tasteful amount of their
    /// neighbour's drilling.
    ///
    /// Unlimited is the right default for a chat app. A finite value is the
    /// knob for the opposite complaint — "it sounds like they keep cutting out"
    /// — and 12-24 dB is the useful range there.
    pub atten_lim_db: Option<f32>,
    /// DeepFilterNet's speech post-filter. Sharpens consonants at the cost of
    /// a little more attenuation.
    pub post_filter: bool,
    /// Residual suppression, applied last — after the levelling, so nothing
    /// downstream can amplify what it removed. `None` leaves the model's
    /// residue alone.
    pub residual: Option<Residual>,
    /// Transient suppression, applied between the model and the levelling.
    ///
    /// Before the leveller on purpose: a keystroke is the loudest thing in a
    /// frame, so a leveller that saw it first would duck the whole sentence
    /// around it. `None` leaves impulsive noise alone.
    pub transients: Option<Transients>,
    /// Loudness levelling applied after suppression. `None` leaves the signal
    /// at whatever level the speaker's microphone produced.
    pub leveling: Option<Leveling>,
}

impl Tuning {
    /// Build a tuning from the choices a person actually made.
    ///
    /// Every stage is independently switchable, which is what makes this
    /// debuggable: when a call sounds wrong the question is always *which
    /// stage*, and the only reliable way to answer it is to turn them off one
    /// at a time and listen.
    pub fn from_choices(transients: Strength, residual: Strength, leveling: bool) -> Tuning {
        Tuning {
            transients: transients.transients(),
            residual: residual.residual(),
            leveling: if leveling { Some(Leveling::default()) } else { None },
            ..Tuning::default()
        }
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            atten_lim_db: None,
            post_filter: true,
            transients: Some(Transients::default()),
            leveling: Some(Leveling::default()),
            residual: Some(Residual::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_spelling_round_trips() {
        for mode in [Mode::Off, Mode::Client, Mode::Server] {
            assert_eq!(Mode::parse(mode.as_str()), mode);
        }
    }

    // An unknown mode must degrade to an ordinary call. A future client naming
    // a mode this node has never heard of is a compatibility event, not an
    // error: refusing the socket would take voice down for the whole room.
    #[test]
    fn unknown_mode_degrades_to_off() {
        assert_eq!(Mode::parse("neural-v9"), Mode::Off);
        assert_eq!(Mode::parse(""), Mode::Off);
    }

    // The rule the module docs exist for: a neural pass anywhere means the
    // browser's suppressor stands down. If this ever inverts, the product
    // ships double-suppressed audio and sounds worse with the feature on.
    #[test]
    fn neural_modes_stand_the_browser_down() {
        assert!(Mode::Off.browser_noise_suppression());
        assert!(!Mode::Client.browser_noise_suppression());
        assert!(!Mode::Server.browser_noise_suppression());
    }

    #[test]
    fn only_server_mode_spends_relay_cpu() {
        assert!(Mode::Server.burdens_relay());
        assert!(!Mode::Client.burdens_relay());
        assert!(!Mode::Off.burdens_relay());
    }
}
