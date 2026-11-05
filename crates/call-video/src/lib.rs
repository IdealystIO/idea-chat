//! A call's video: cameras and screen shares out as VP8 datagrams, and
//! everyone else's back in, each a stream a `<video>` can play.
//!
//! ```text
//!   camera ──▶ VideoEncoder (VP8, realtime) ──▶ call_proto::video::fragment ──▶ datagrams
//!   datagrams ──▶ Assembler ──▶ VideoDecoder ──▶ held for the audio ──▶ <canvas> ──▶ MediaStream
//! ```
//!
//! It knows nothing of the link: datagrams go into a channel and arrive as
//! `call_proto::video::Fragment`s, and keyframe requests go out through a
//! callback, so whoever owns the connection carries them. Received video is
//! held in step with the audio clocks it is given (`av_sync`).
//!
//! Web-only: the codec is the browser's.

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::{player, summary, Video, VideoStats, MAX_HOLD_MS, SEND_QUEUE};

/// Registers the SDK's `Video`, which [`player`] builds on. Call from the
/// app's `register_scene_extensions`. A no-op off the web.
pub fn register_scene_extensions<H: idealyst::SceneHost>(registry: &mut runtime_scene::Registry<H>) {
    #[cfg(target_arch = "wasm32")]
    video_sdk::register(registry);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = registry;
}
