//! Sigil: end-to-end encrypted groups, and the services built on them.
//!
//! The design is `docs/e2ee-protocol.md`; this crate is its protocol layer
//! and knows nothing about chat. A [`Group`] is an MLS group of devices. A
//! [`Service`] is a named use of one, of three kinds:
//!
//! - **stateful**: [`record`]s the server stores as ciphertext, each signed
//!   by its sender and sealed under a per-epoch [`StateKey`] that members keep;
//! - **realtime, reliable**: MLS application messages, never stored;
//! - **realtime, lossy**: SFrame datagrams keyed from [`keys::sframe_base`].
//!
//! Around those: [`blob`] for large payloads, [`checkpoint`] for trusting a
//! service's state without replaying it, [`policy`] for who may change the
//! group, and [`action`] for signed requests a server acts on.
//!
//! # What each part depends on
//!
//! Everything except [`group`] is plain functions over bytes, keys and
//! signatures, so it runs and tests anywhere, the server included: a server
//! can parse a record's header, check an action, or decide whether a
//! checkpoint is trusted without holding a key. [`group`] is the one place
//! that touches openmls.
//!
//! # Never in the clear
//!
//! There is no unsealed mode. Every function that seals takes a key, not an
//! `Option` of one; a missing key is an [`Error`], never a quiet fallback.

pub mod action;
pub mod blob;
pub mod checkpoint;
pub mod group;
pub mod keys;
pub mod policy;
pub mod record;
pub mod service;
mod wire;

pub use group::{Commit, Credentials, Device, Group, Incoming, Unverified};
pub use keys::StateKey;
pub use policy::{Policy, Role};
pub use service::{Kind, Service, ServiceId, Services};

/// A person, as the identity service names them (`user@identity-domain`).
pub type UserId = String;

/// A group's id: 16 random bytes, chosen by its creator.
pub type GroupId = [u8; 16];

/// Everything that can go wrong. None of it is recoverable by retrying the
/// same input, and none of it falls back to sending anything in the clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Bytes that don't parse as what they claim to be.
    Malformed(&'static str),
    /// A signature or an AEAD tag didn't check out.
    Unauthentic,
    /// A service id the group doesn't have.
    UnknownService(ServiceId),
    /// No state key held for this service at this epoch.
    NoKey { service: ServiceId, epoch: u64 },
    /// The group's policy doesn't allow it.
    NotAllowed(String),
    /// A credential that didn't verify.
    Credential(String),
    /// An action seen before.
    Replay,
    /// An action past its expiry, or one that would live too long.
    Expired,
    /// An action meant for someone else.
    WrongAudience,
    /// openmls refused.
    Mls(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed(what) => write!(f, "malformed {what}"),
            Error::Unauthentic => f.write_str("signature or tag did not verify"),
            Error::UnknownService(id) => write!(f, "no service {id} in this group"),
            Error::NoKey { service, epoch } => write!(f, "no state key for service {service} at epoch {epoch}"),
            Error::NotAllowed(why) => write!(f, "not allowed: {why}"),
            Error::Credential(why) => write!(f, "credential: {why}"),
            Error::Replay => f.write_str("action already seen"),
            Error::Expired => f.write_str("action expired"),
            Error::WrongAudience => f.write_str("action meant for someone else"),
            Error::Mls(why) => write!(f, "mls: {why}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
