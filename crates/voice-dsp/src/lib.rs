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

pub use level::{Leveler, Leveling};

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
    /// Loudness levelling applied after suppression. `None` leaves the signal
    /// at whatever level the speaker's microphone produced.
    pub leveling: Option<Leveling>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            atten_lim_db: None,
            post_filter: true,
            leveling: Some(Leveling::default()),
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
