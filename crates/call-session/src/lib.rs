//! A call over one link to a call-relay: everything between the app and the
//! media crates.
//!
//! ```text
//!   call_audio::Sender ──▶ queue ──┐
//!   call_video::Video  ──▶ queue ──┴──▶ Link ──▶ relay
//!   relay ──▶ Link ──▶ untag ──┬──▶ call_audio::Receiver ──▶ speakers
//!                              │            │ audio clock (av_sync)
//!                              └──▶ call_video::Video ◀──┘
//! ```
//!
//! Neither media crate knows the link: each hands its datagrams to a queue
//! the session drains, and is handed what arrives. Audio and video have a
//! queue each, so a keyframe's burst never delays a voice packet.
//!
//! Given a room key ([`Access::key`]), the session seals every datagram on
//! its way out and opens every one on its way in (`call_e2ee`), so the media
//! crates never see ciphertext and the relay never sees anything else. What
//! does not open is dropped, and its sender listed in
//! [`Roster::unreadable`].
//!
//! The session also reads the relay's control messages (`call_proto::relay`)
//! and keeps a [`Roster`] — who is who, by their grants, and who has their mic
//! off — so an app can map its own users to relay numbers. It announces our
//! mic state, measures the round trip, and renews our grant halfway through
//! its life from wherever the app says ([`Renew`]): a URL, or a callback such
//! as a server function behind the user's session.
//!
//! Shared by quic-call and Hearth. Web-only, like the media under it.

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;
