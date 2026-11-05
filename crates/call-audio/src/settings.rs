//! What the voice pipeline is asked to do. Platform-free, so the page can
//! hold and edit it on any target.

/// Which noise suppression runs, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    Off,
    /// The browser's WebRTC suppressor, on the captured track. Steady noise.
    Browser,
    /// DeepFilterNet 3, in this page, before the encoder.
    Neural,
}

impl Suppression {
    pub fn id(self) -> &'static str {
        match self {
            Suppression::Off => "off",
            Suppression::Browser => "browser",
            Suppression::Neural => "neural",
        }
    }

    pub fn parse(id: &str) -> Suppression {
        match id {
            "browser" => Suppression::Browser,
            "neural" => Suppression::Neural,
            _ => Suppression::Off,
        }
    }

    /// Which stage this needs: Off and Browser share the direct one, since
    /// the browser's suppressor runs on the track rather than in the page.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn stage(self) -> u8 {
        match self {
            Suppression::Off | Suppression::Browser => 0,
            Suppression::Neural => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Processing {
    pub echo_cancellation: bool,
    pub suppression: Suppression,
    /// The browser's own automatic gain on the captured track. Off by
    /// default: it is one more stage moving the level before the model and
    /// the leveller see it, and stacked level processing is how quiet speech
    /// gets pumped and chopped.
    pub auto_gain_control: bool,
}

/// At or above this, DeepFilterNet's attenuation is unlimited — libDF's own
/// convention, which treats 100 dB as "no limit".
pub const UNLIMITED_DB: f32 = 100.0;

/// DeepFilterNet's tuning, as libDF exposes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelSettings {
    /// The most the model may attenuate anything, in dB — a share of the input
    /// mixed back under its output. [`UNLIMITED_DB`] or more is no limit.
    /// 12–24 dB keeps a little of the room underneath, which is what keeps
    /// breaths and word endings from being chopped.
    pub max_reduction_db: f32,
    /// libDF's speech post-filter: crisper voice, slightly more attenuation.
    pub post_filter: bool,
    /// Below this local-SNR estimate a frame is taken for noise and ZEROED —
    /// a hard gate. Lower lets quiet speech through. Upstream: -10.
    pub min_db_thresh: f32,
    /// Above this the frame is taken for clean speech and left alone.
    /// Upstream: 30.
    pub max_db_erb_thresh: f32,
    /// Above this the second, deep-filtering stage is skipped. Upstream: 20.
    pub max_db_df_thresh: f32,
}

impl Default for ModelSettings {
    fn default() -> Self {
        ModelSettings {
            max_reduction_db: UNLIMITED_DB,
            post_filter: false,
            min_db_thresh: -10.0,
            max_db_erb_thresh: 30.0,
            max_db_df_thresh: 20.0,
        }
    }
}

/// Which voice-processing stages run. Read per block, so changing it needs
/// nothing reopened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceStages {
    pub dehum: bool,
    /// Off by default. Measured on clean speech it takes 1.4 dB off 5–8 kHz
    /// and 2.1 dB off 8–12 kHz — it reads some sibilance as clicks — which is
    /// heard as a muffled voice. Worth it on a mechanical keyboard, not as a
    /// standing default.
    pub declick: bool,
    pub level: bool,
    pub expand: bool,
}

impl Default for VoiceStages {
    fn default() -> Self {
        VoiceStages { dehum: true, declick: false, level: true, expand: true }
    }
}

impl VoiceStages {
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn any_after_stage(self) -> bool {
        self.declick || self.level || self.expand
    }
}

/// Counters for the page. Cumulative unless noted. The send side and the
/// receive side each fill in their half.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub sent: u64,
    pub unsent: u64,
    pub received: u64,
    pub late: u64,
    /// Frames the codec had to invent because a packet never came.
    pub concealed: u64,
    pub underruns: u32,
    pub peers: usize,
    /// Deepest jitter-buffer target across peers, right now.
    pub playout_ms: f32,
    /// Loudest captured sample since the page last read it.
    pub mic_peak: f32,
    /// Loudest sample played from anyone — after our volume on them, so a
    /// person turned down or muted for us reads quieter — in the current
    /// half-second window (`remote_peak_ms` is when it began) — a
    /// level that describes now, however rarely it is read.
    pub remote_peak: f32,
    pub remote_peak_ms: f64,
    /// Loudest sample actually encoded — after suppression — since the page
    /// last read it. Next to `mic_peak`, this is what suppression is doing.
    pub out_peak: f32,
    /// Time spent inside the mic callback's write — which on web is where the
    /// stage, and so the model, runs — and how much audio that covered. Their
    /// ratio is whether this machine keeps up: past 100% it cannot.
    pub dsp_ms: f64,
    pub audio_ms: f64,
    /// Mains-hum notches the de-hum stage has switched on, right now.
    pub hum_notches: usize,
    pub rate: u32,
    /// How long our sound takes from the air to the wire, in ms: the mic's
    /// own latency, the processing stages' delay (DeepFilterNet's 30 ms, the
    /// expander's lookahead) and an Opus frame of packetising. Right now.
    pub send_delay_ms: f64,
    /// The microphone's own latency, as the browser reports it.
    pub input_latency_ms: f64,
    /// Whether the capture runs on an audio thread of its own (an
    /// AudioWorklet) rather than the page's.
    pub off_main_thread: bool,
    /// Whether we are talking, right now: the neural model's verdict when it
    /// runs, else the level. False while muted.
    pub speaking: bool,
    /// The neural model's gain over the last moment, in dB — how much of what
    /// went in it kept. `None` without a model.
    pub model_gain_db: Option<f32>,
    /// How long arriving sound waits before it is heard, in ms: the jitter
    /// buffer, the output's queue and the device's own latency — the longest
    /// across everyone we hear. Right now.
    pub listen_delay_ms: f64,
    pub max_datagram: usize,
    pub codec_failed: bool,
}

