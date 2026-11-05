//! The web backend: capture and output on the audio rendering thread.
//!
//! # No silent fallback, deliberately
//!
//! If the worklet module will not load, this returns an error naming why. It
//! does **not** quietly drop back to a `ScriptProcessorNode` — running on the
//! main thread is the exact failure this crate exists to remove, and a fallback
//! that did it silently would be indistinguishable from success right up until
//! someone's call started glitching under load. `Opened::off_main_thread` is
//! therefore always true here, and the field is honest rather than decorative.
//!
//! An app that would rather have main-thread audio than no audio still has
//! `microphone` and `media-stream`, which do exactly that.
//!
//! # One AudioContext for the process
//!
//! Not one per stream. Browsers cap concurrent `AudioContext`s — Chrome has
//! historically allowed about six — so a per-stream context is a room that
//! stops working at the seventh participant. Sharing one also means the worklet
//! module is compiled once, and every stream is already on the same clock.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Array, Float32Array, Object, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioContextOptions, AudioWorkletNode, AudioWorkletNodeOptions, MediaStream,
    MediaStreamAudioSourceNode, MediaStreamConstraints, MessageEvent, MessagePort,
};

use crate::latency::{resolve, Limits};
use crate::{Block, CaptureConfig, Error, Opened, OutputConfig, Pipeline, PREFERRED_RATE};

/// The processors, compiled into the binary. See the module for why a Blob URL
/// rather than an asset.
const WORKLET_JS: &str = include_str!("worklet.js");

/// An `AudioWorklet`'s render quantum. Fixed by the spec, on every browser, and
/// not something a caller or a backend gets to choose.
const QUANTUM: u32 = 128;

/// What the worklet backend can do. See [`crate::latency`].
const WORKLET: Limits = Limits {
    min_frames: QUANTUM,
    max_frames: 16_384,
    granularity: QUANTUM,
    power_of_two: false,
    // ~43 ms. Nobody is waiting on a Relaxed stream, and a bigger block is
    // fewer messages across the thread boundary.
    relaxed_frames: 2048,
    // ~5.3 ms, and two quanta rather than one: at 128 frames the post rate is
    // 375/s, which spends more in message overhead than it saves in delay.
    interactive_frames: 256,
};

fn js_error(context: &str, value: JsValue) -> Error {
    Error::Backend(format!(
        "{context}: {}",
        value.as_string().unwrap_or_else(|| format!("{value:?}")),
    ))
}

// ---------------------------------------------------------------------------
// The shared context
// ---------------------------------------------------------------------------

thread_local! {
    /// The process's one `AudioContext`, with the worklet module already
    /// loaded. `None` until the first device is opened — building it earlier
    /// would trip autoplay policy, which only lets a context start inside a
    /// user gesture.
    static SHARED: RefCell<Option<AudioContext>> = const { RefCell::new(None) };
}

/// The shared context, building it and loading the worklet module once.
async fn context(rate: u32) -> Result<AudioContext, Error> {
    if let Some(ctx) = SHARED.with(|c| c.borrow().clone()) {
        // Autoplay policy can suspend a context between calls; resuming an
        // already-running one is free.
        let _ = ctx.resume();
        return Ok(ctx);
    }

    let options = AudioContextOptions::new();
    options.set_sample_rate(rate as f32);
    let ctx = AudioContext::new_with_context_options(&options)
        .map_err(|e| js_error("AudioContext", e))?;
    let _ = ctx.resume();

    let worklet = ctx.audio_worklet().map_err(|e| {
        Error::Backend(format!(
            "this browser has no AudioWorklet ({}), and audio-io will not fall back to the \
             main thread — see the module docs",
            e.as_string().unwrap_or_default(),
        ))
    })?;

    // A Blob URL, so the crate ships a string instead of an asset an app would
    // have to serve at a path this code could not know.
    let parts = Array::of1(&JsValue::from_str(WORKLET_JS));
    let bag = web_sys::BlobPropertyBag::new();
    bag.set_type("text/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &bag)
        .map_err(|e| js_error("Blob", e))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|e| js_error("createObjectURL", e))?;
    let added = worklet.add_module(&url).map_err(|e| js_error("addModule", e))?;
    let result = JsFuture::from(added).await;
    let _ = web_sys::Url::revoke_object_url(&url);
    result.map_err(|e| js_error("addModule", e))?;

    SHARED.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    Ok(ctx)
}

/// Build a worklet node, with `processorOptions` set from `options`.
fn node(
    ctx: &AudioContext,
    name: &str,
    inputs: u32,
    outputs: u32,
    channels: u16,
    options: &[(&str, f64)],
) -> Result<AudioWorkletNode, Error> {
    let processor_options = Object::new();
    let _ = Reflect::set(
        &processor_options,
        &"channels".into(),
        &JsValue::from_f64(channels as f64),
    );
    for (key, value) in options {
        let _ = Reflect::set(&processor_options, &(*key).into(), &JsValue::from_f64(*value));
    }

    let node_options = AudioWorkletNodeOptions::new();
    node_options.set_number_of_inputs(inputs);
    node_options.set_number_of_outputs(outputs);
    if outputs > 0 {
        // The graph must agree with the processor about channel count, or the
        // processor writes one channel and the destination reads two.
        let counts = Array::of1(&JsValue::from_f64(channels as f64));
        node_options.set_output_channel_count(&counts);
    }
    node_options.set_processor_options(Some(&processor_options));

    AudioWorkletNode::new_with_options(ctx, name, &node_options)
        .map_err(|e| js_error(&format!("AudioWorkletNode({name})"), e))
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// The per-block state the message handler owns.
struct Captured {
    pipeline: Pipeline,
    on_block: Box<dyn FnMut(&[f32], u32, u16, u64)>,
    /// Reused, so the receive path allocates nothing per block.
    scratch: Vec<f32>,
    frames_elapsed: u64,
}

/// A live microphone. Dropping it stops capture and releases the device.
pub struct Capture {
    node: AudioWorkletNode,
    source: MediaStreamAudioSourceNode,
    stream: MediaStream,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    opened: Opened,
}

impl Capture {
    /// Open the microphone and start delivering blocks.
    ///
    /// `pipeline` runs before `on_block`. Today both run on the main thread —
    /// the samples get there having never been at risk, which is the part that
    /// changed. See the crate docs for what is left.
    pub async fn open(
        config: CaptureConfig,
        pipeline: Pipeline,
        on_block: impl FnMut(&[f32], u32, u16, u64) + 'static,
    ) -> Result<Capture, Error> {
        let wanted_rate = config.sample_rate.unwrap_or(PREFERRED_RATE);
        let ctx = context(wanted_rate).await?;
        let stream = get_user_media(&config, ctx.sample_rate() as u32).await?;
        Capture::start(ctx, stream, config, pipeline, on_block)
    }

    /// Capture from a stream the caller already has — a screen share's or a
    /// tab's audio, from `getDisplayMedia` — instead of opening a microphone.
    ///
    /// Web only: there is no such stream anywhere else. The processing flags
    /// in `config` do not apply (they are `getUserMedia` constraints); its
    /// rate, channels and latency do.
    ///
    /// The capture runs on CLONES of `stream`'s audio tracks, so closing it
    /// stops only those: the caller's stream, and its video, carry on.
    pub async fn open_stream(
        stream: &MediaStream,
        config: CaptureConfig,
        pipeline: Pipeline,
        on_block: impl FnMut(&[f32], u32, u16, u64) + 'static,
    ) -> Result<Capture, Error> {
        let audio = stream.get_audio_tracks();
        if audio.length() == 0 {
            return Err(Error::NoDevice);
        }
        let own = MediaStream::new().map_err(|e| js_error("MediaStream", e))?;
        for track in audio.iter() {
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                own.add_track(&track.clone());
            }
        }
        let ctx = context(config.sample_rate.unwrap_or(PREFERRED_RATE)).await?;
        Capture::start(ctx, own, config, pipeline, on_block)
    }

    fn start(
        ctx: AudioContext,
        stream: MediaStream,
        config: CaptureConfig,
        pipeline: Pipeline,
        on_block: impl FnMut(&[f32], u32, u16, u64) + 'static,
    ) -> Result<Capture, Error> {
        // The context's rate is authoritative — the browser is free to have
        // clamped what we asked for, and everything downstream (the codec, the
        // packetiser, the jitter buffer) has to agree with reality rather than
        // with the request.
        let rate = ctx.sample_rate() as u32;
        let channels = config.channels.max(1);
        let block_frames = resolve(config.latency, &WORKLET);

        let source =
            ctx.create_media_stream_source(&stream).map_err(|e| js_error("createMediaStreamSource", e))?;

        // ZERO outputs, and NOT connected to the destination.
        //
        // The first version gave it one output and wired it to the speakers, on
        // the reasoning that a node with no outputs might not be pulled — and
        // relied on the processor never writing to its output for that path to
        // be silent. That is a microphone wired to the speakers whose silence
        // depends on an implementation detail, in a graph that also carries
        // every peer's audio to the same destination. The spec provides a sink
        // node precisely so this is unnecessary: an `AudioWorkletNode` with no
        // outputs is processed for as long as it has an active input, which a
        // live microphone is.
        let node = node(&ctx, "audio-io-capture", 1, 0, channels, &[(
            "blockFrames",
            block_frames as f64,
        )])?;
        source
            .connect_with_audio_node(&node)
            .map_err(|e| js_error("connect(source -> capture)", e))?;

        let state = Rc::new(RefCell::new(Captured {
            pipeline,
            on_block: Box::new(on_block),
            scratch: Vec::new(),
            frames_elapsed: 0,
        }));

        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Ok(array) = event.data().dyn_into::<Float32Array>() else { return };
            let Ok(mut state) = state.try_borrow_mut() else {
                // Re-entrancy would mean a stage posted a message that came
                // straight back. Dropping the block beats a panic on the audio
                // path, and it cannot happen with a stage that follows the
                // rules.
                return;
            };
            let state = &mut *state;

            state.scratch.resize(array.length() as usize, 0.0);
            array.copy_to(&mut state.scratch);

            let elapsed = state.frames_elapsed;
            let frames = (state.scratch.len() / channels.max(1) as usize) as u64;
            state.frames_elapsed += frames;

            {
                let mut block = Block {
                    samples: &mut state.scratch,
                    sample_rate: rate,
                    channels,
                    frames_elapsed: elapsed,
                };
                state.pipeline.process(&mut block);
            }
            (state.on_block)(&state.scratch, rate, channels, elapsed);
        });
        let port = node.port().map_err(|e| js_error("capture port", e))?;
        port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        Ok(Capture {
            node,
            source,
            stream,
            _on_message: on_message,
            opened: Opened { sample_rate: rate, channels, block_frames, off_main_thread: true },
        })
    }

    /// What the platform actually gave us.
    pub fn opened(&self) -> Opened {
        self.opened
    }

    /// How long the microphone takes to deliver a sound, in ms: the track's
    /// own `latency` setting, where the browser reports one (Chromium does),
    /// else 0. For anyone timestamping what was captured — lip sync, for one.
    pub fn input_latency_ms(&self) -> f64 {
        self.stream
            .get_audio_tracks()
            .get(0)
            .dyn_into::<web_sys::MediaStreamTrack>()
            .ok()
            .and_then(|t| Reflect::get(&t.get_settings(), &"latency".into()).ok())
            .and_then(|v| v.as_f64())
            .map(|s| s * 1000.0)
            .unwrap_or(0.0)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Ok(port) = self.node.port() {
            port.set_onmessage(None);
        }
        let _ = self.source.disconnect();
        let _ = self.node.disconnect();
        // Releases the device, and turns off the browser's recording indicator.
        // Leaving this out is how a "hung up" call keeps the microphone light on.
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
    }
}

/// `getUserMedia`, with the processing flags this config asks for.
///
/// Built as a plain object rather than the typed dictionary: the flags are a
/// handful of well-known keys, and `MediaTrackConstraints` has had a different
/// shape in three consecutive web-sys releases.
async fn get_user_media(config: &CaptureConfig, rate: u32) -> Result<MediaStream, Error> {
    let window = web_sys::window().ok_or(Error::Unsupported("no window"))?;
    let devices = window
        .navigator()
        .media_devices()
        .map_err(|e| js_error("mediaDevices", e))?;

    let audio = Object::new();
    let set = |key: &str, value: JsValue| {
        let _ = Reflect::set(&audio, &key.into(), &value);
    };
    set("sampleRate", JsValue::from_f64(rate as f64));
    set("channelCount", JsValue::from_f64(config.channels.max(1) as f64));
    set("echoCancellation", JsValue::from_bool(config.echo_cancellation));
    set("noiseSuppression", JsValue::from_bool(config.noise_suppression));
    set("autoGainControl", JsValue::from_bool(config.auto_gain_control));

    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&audio);

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| js_error("getUserMedia", e))?;
    let value = JsFuture::from(promise).await.map_err(|e| {
        // A refusal and a missing device are different problems with different
        // answers, and the browser distinguishes them by name.
        let name = Reflect::get(&e, &"name".into())
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        match name.as_str() {
            "NotAllowedError" | "SecurityError" => Error::PermissionDenied,
            "NotFoundError" | "OverconstrainedError" => Error::NoDevice,
            _ => js_error("getUserMedia", e),
        }
    })?;
    value
        .dyn_into::<MediaStream>()
        .map_err(|_| Error::Backend("getUserMedia did not return a MediaStream".into()))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// A live speaker. Dropping it detaches the node and stops the sound.
pub struct Output {
    node: AudioWorkletNode,
    port: MessagePort,
    /// Last reported `(queued frames, underruns)` from the worklet. The main
    /// thread cannot see its queue, and an underrun is otherwise
    /// indistinguishable from a quiet room.
    telemetry: Rc<Cell<(u32, u32)>>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    /// Resampler state, for a sender whose rate is not the device's.
    tail: RefCell<Vec<f32>>,
    position: Cell<f64>,
    converted: RefCell<Vec<f32>>,
    cushion: u32,
    opened: Opened,
}

impl Output {
    /// Open the default output device.
    pub async fn open(config: OutputConfig) -> Result<Output, Error> {
        let wanted_rate = config.sample_rate.unwrap_or(PREFERRED_RATE);
        let ctx = context(wanted_rate).await?;
        let rate = ctx.sample_rate() as u32;
        let channels = config.channels.max(1);
        let block_frames = resolve(config.latency, &WORKLET);

        // The cushion covers jitter in the PRODUCER, and nothing else. The
        // render callback is on the audio thread and cannot be late any more,
        // which is what used to make a large prime necessary — so what is left
        // is purely "how bursty is whoever is feeding this".
        let cushion = config.cushion_frames.unwrap_or(block_frames).max(block_frames);
        let node = node(&ctx, "audio-io-output", 0, 1, channels, &[(
            "primeFrames",
            cushion as f64,
        )])?;
        node.connect_with_audio_node(&ctx.destination())
            .map_err(|e| js_error("connect(output -> destination)", e))?;

        let telemetry = Rc::new(Cell::new((0u32, 0u32)));
        let on_message = {
            let telemetry = telemetry.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let data = event.data();
                let number = |key: &str| {
                    Reflect::get(&data, &key.into()).ok().and_then(|v| v.as_f64()).unwrap_or(0.0)
                };
                telemetry.set((number("queued") as u32, number("underruns") as u32));
            })
        };
        let port = node.port().map_err(|e| js_error("output port", e))?;
        port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        Ok(Output {
            node,
            port,
            telemetry,
            _on_message: on_message,
            tail: RefCell::new(Vec::new()),
            position: Cell::new(0.0),
            converted: RefCell::new(Vec::new()),
            cushion,
            opened: Opened { sample_rate: rate, channels, block_frames, off_main_thread: true },
        })
    }

    pub fn opened(&self) -> Opened {
        self.opened
    }

    /// Frames of cushion this output builds before playing.
    pub fn cushion_frames(&self) -> u32 {
        self.cushion
    }

    /// Queue interleaved PCM that is already at this device's rate.
    pub fn write(&self, pcm: &[f32]) {
        if pcm.is_empty() {
            return;
        }
        let array = Float32Array::from(pcm);
        let _ = self.port.post_message(&array);
    }

    /// Queue interleaved PCM at `rate`, resampling if the device disagrees.
    ///
    /// A room is not all one sample rate — a caller whose hardware reports
    /// 44.1 kHz sends 44.1 kHz — and a device runs at exactly one. Without this
    /// that caller plays back fast, which sounds like a fault in their
    /// microphone rather than in this code.
    pub fn write_at(&self, pcm: &[f32], rate: u32) {
        if rate == self.opened.sample_rate {
            self.write(pcm);
            return;
        }
        let mut converted = self.converted.borrow_mut();
        converted.clear();
        let mut tail = self.tail.borrow_mut();
        let mut position = self.position.get();
        crate::resample::linear(
            pcm,
            self.opened.channels,
            rate,
            self.opened.sample_rate,
            &mut tail,
            &mut position,
            &mut converted,
        );
        self.position.set(position);
        self.write(&converted);
    }

    /// Frames waiting in the worklet's queue, as of its last report (~50 ms).
    pub fn queued_frames(&self) -> u32 {
        self.telemetry.get().0
    }

    /// Times the output ran dry since it opened.
    ///
    /// Non-zero means the producer could not keep up — which after this change
    /// is the only way to underrun, because the render callback itself can no
    /// longer be late.
    pub fn underruns(&self) -> u32 {
        self.telemetry.get().1
    }

    /// How long audio takes from this output's context to the speaker, past
    /// the queue [`queued_frames`](Self::queued_frames) measures: the
    /// context's `baseLatency` + `outputLatency`, in ms. Tens of ms on a wired
    /// device, a couple of hundred over Bluetooth; 0 where the browser does
    /// not say. Read through `Reflect` because `web-sys` gates both getters
    /// behind its unstable APIs.
    pub fn device_latency_ms(&self) -> f64 {
        let ctx = self.node.context();
        let get = |key: &str| Reflect::get(&ctx, &key.into()).ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        (get("baseLatency") + get("outputLatency")) * 1000.0
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.port.set_onmessage(None);
        let _ = self.node.disconnect();
    }
}
