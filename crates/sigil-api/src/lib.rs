//! The Sigil API.
//!
//! Every function a device calls is a `#[server]` function in one of the
//! modules below, under the wire path `sigil/<name>` so it can share a
//! router with another API; [`live::live`] is the one long-lived
//! connection. Built without the `server` feature (as a client builds it),
//! each is an HTTP stub; built with it, each is a thin wrapper that checks
//! the session and calls [`srv::ops`], where the work happens.
//!
//! # What crosses the wire
//!
//! Only ciphertext and public structures: KeyPackages, GroupInfo, commits,
//! Welcomes, sealed records, MLS application messages, signed actions. All of
//! it travels as unpadded base64url text ([`b64`]), group ids as 32 hex
//! digits, devices as their signature key in hex.
//!
//! # Who is calling
//!
//! A device signs in by answering a challenge with its Sigil signature key
//! ([`auth`]); its session's [`Principal`] is that key. Which *person* it is
//! comes from the credential in its leaf, whenever it acts in a group.

#![cfg_attr(not(feature = "server"), allow(unused_imports))]

pub mod auth;
pub mod groups;
pub mod live;
pub mod records;

#[cfg(feature = "server")]
pub mod srv;

use serde::{Deserialize, Serialize};
use server::ServerError;

/// What a session stands for: one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// The device's signature public key, hex.
    pub device: String,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum ApiError {
    SignedOut,
    /// Bytes or ids that don't parse.
    Invalid { message: String },
    NotFound,
    /// The group's policy, or membership, doesn't allow it.
    Forbidden { message: String },
    /// Another commit got there first: fetch it, process it, and try again.
    Stale,
    Unavailable { message: String },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::SignedOut => f.write_str("This device is signed out."),
            ApiError::Invalid { message } | ApiError::Forbidden { message } | ApiError::Unavailable { message } => f.write_str(message),
            ApiError::NotFound => f.write_str("That isn't there."),
            ApiError::Stale => f.write_str("The group moved on; catch up and try again."),
        }
    }
}

pub type Failure = ServerError<ApiError>;
pub type Answer<T> = Result<T, Failure>;

/// A session for a signed-in device. `token` is set for native clients,
/// which send it as a bearer token; the web gets an httpOnly cookie instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub token: Option<String>,
    pub device: String,
}

/// A stored record and its server sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub seq: u64,
    pub record: String,
}

/// Where and how to join a lossy service's relay room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayTicket {
    /// The relay's WebTransport URL.
    pub url: String,
    /// Its certificate's SHA-256 in hex, when self-signed; empty for a CA one.
    pub cert_hash: String,
    pub room: String,
    /// A grant; fetch a fresh one at half its life.
    pub grant: String,
}

/// Unpadded base64url, the encoding of every byte string in this API.
pub mod b64 {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    pub fn encode(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        URL_SAFE_NO_PAD.decode(text).ok()
    }
}

/// Lowercase hex, for group ids and device keys.
pub mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        if !text.len().is_multiple_of(2) {
            return None;
        }
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
    }
}
