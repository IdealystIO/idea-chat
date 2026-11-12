//! The Sigil delivery service (spec §8): everything a server does for Sigil
//! groups, without the transport.
//!
//! A [`Hub`] follows each group's *public* state — the ratchet tree, the
//! GroupContext and so the services table and policy — from the commits
//! members send in the clear. That is enough to do the server's whole job
//! without holding a key:
//!
//! - accept exactly one commit per epoch, and only one the group's policy
//!   allows (the same check members make, so a commit the server takes is
//!   one members will take too);
//! - store KeyPackages and hand each out once;
//! - store records, checking each one's signature against the leaf that
//!   wrote it, and page them by server sequence;
//! - route Welcomes to the devices they add, and reliable messages to every
//!   member;
//! - act on signed [`sigil::action`]s;
//! - authenticate devices by a signed challenge ([`auth`]).
//!
//! Devices are named by their signature public key ([`DeviceId`]), which is
//! also how they appear as leaves in groups.

pub mod auth;
pub mod hub;
pub mod store;

pub use auth::Challenges;
pub use hub::{Accepted, Done, Hub};
pub use store::{MemoryStore, Store};

/// A device, as the server names it: its Sigil signature public key.
pub type DeviceId = Vec<u8>;
