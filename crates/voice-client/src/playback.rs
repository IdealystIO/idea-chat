//! Getting a network-fed [`AudioStream`] to the speakers.
//!
//! This module is the whole of the framework gap, isolated in one file so that
//! promoting it into an `audio-output` SDK later is a move, not a rewrite.
//!
//! # Web — already solved by the framework
//!
//! `media-stream` bridges a *synthetic* stream (one fed by an `AudioWriter`
//! rather than by `getUserMedia`) to a real `web_sys::MediaStream` on the first
//! `native_source()` call: it stands up an `AudioContext`, a
//! `MediaStreamAudioDestinationNode`, and a `ScriptProcessorNode` fed from a
//! ring buffer by a `subscribe` callback. All that is left for us is attaching
//! the resulting track to an `<audio>` element, which is exactly what the
//! `video` SDK does for camera streams (`set_src_object`).
//!
//! # Native — the gap
//!
//! There is no `AudioStream` sink. `audio` plays a prepared `AudioSource`
//! (bytes / path / url) and cannot consume a live stream; every other consumer
//! in the tree either writes a file (`media-writer`) or transforms
//! (`denoise`). So this returns [`PlaybackError::NotSupported`] and names what
//! would fix it. `AudioStream::native_source`'s own documentation already
//! anticipates the shape: "an Apple `AVAudioEngine`, an Android `AudioTrack`
//! feed".

use media_stream::AudioStream;

/// Why a stream could not be routed to an output device.
#[derive(Debug, Clone)]
pub enum PlaybackError {
    /// The platform has no `AudioStream` sink yet. Carries the note that
    /// becomes the `audio-output` SDK's reason for existing.
    NotSupported(&'static str),
    /// The platform refused to build the output graph.
    Failed(String),
}

impl core::fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlaybackError::NotSupported(why) => write!(f, "{why}"),
            PlaybackError::Failed(e) => write!(f, "playback failed: {e}"),
        }
    }
}

/// A live output. Dropping it detaches the sink and stops the sound, mirroring
/// the RAII contract of `audio::Playback` so the eventual SDK feels the same.
pub struct Playing {
    #[cfg(target_arch = "wasm32")]
    element: web_sys::HtmlAudioElement,
}

impl Playing {
    /// Output gain in `[0.0, 1.0]`.
    pub fn set_volume(&self, volume: f32) {
        #[cfg(target_arch = "wasm32")]
        {
            self.element.set_volume(volume.clamp(0.0, 1.0) as f64);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = volume;
        }
    }
}

impl Drop for Playing {
    fn drop(&mut self) {
        #[cfg(target_arch = "wasm32")]
        {
            use web_sys::HtmlMediaElement;
            let media: &HtmlMediaElement = self.element.as_ref();
            media.pause().ok();
            media.set_src_object(None);
            if let Some(parent) = self.element.parent_node() {
                parent.remove_child(&self.element).ok();
            }
        }
    }
}

/// Route `stream` to the default output device.
#[cfg(target_arch = "wasm32")]
pub fn play(stream: &AudioStream) -> Result<Playing, PlaybackError> {
    use wasm_bindgen::JsCast;
    use web_sys::HtmlMediaElement;

    // Materializes the WebAudio bridge on first call for a synthetic stream,
    // then caches it for the stream's life. The track is live (and initially
    // silent) immediately, so attaching before any PCM arrives is correct.
    let native = stream
        .native_source()
        .ok_or(PlaybackError::NotSupported("no native audio source for this stream"))?;
    let media_stream = native
        .downcast_ref::<web_sys::MediaStream>()
        .ok_or(PlaybackError::NotSupported("native source was not a MediaStream"))?;

    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| PlaybackError::Failed("no document".into()))?;
    let element = document
        .create_element("audio")
        .map_err(|_| PlaybackError::Failed("could not create <audio>".into()))?
        .dyn_into::<web_sys::HtmlAudioElement>()
        .map_err(|_| PlaybackError::Failed("element was not <audio>".into()))?;

    let media: &HtmlMediaElement = element.as_ref();
    media.set_autoplay(true);
    media.set_src_object(Some(media_stream));

    // Some browsers will not start a media element that is not in the
    // document, so park it hidden rather than leaving it detached.
    element.set_attribute("style", "display:none").ok();
    if let Some(body) = document.body() {
        body.append_child(&element)
            .map_err(|_| PlaybackError::Failed("could not attach <audio>".into()))?;
    }

    // `play()` returns a promise that rejects under autoplay policy. Joining is
    // user-gesture-initiated so it normally resolves; the element also has
    // `autoplay` set, so a rejection here is not fatal and must not abort the
    // join. Ignore it deliberately.
    let _ = media.play();

    Ok(Playing { element })
}

/// Route `stream` to the default output device.
#[cfg(not(target_arch = "wasm32"))]
pub fn play(stream: &AudioStream) -> Result<Playing, PlaybackError> {
    let _ = stream;
    Err(PlaybackError::NotSupported(
        "no AudioStream sink on this platform yet — this is the `audio-output` \
         gap (AVAudioEngine / AudioTrack / cpal). See docs/framework-gap-analysis.md §9.",
    ))
}
