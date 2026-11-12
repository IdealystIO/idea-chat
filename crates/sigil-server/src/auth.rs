//! Device sign-in by challenge.
//!
//! ```text
//! client                                 server
//!   ── challenge() ─────────────────────▶  32 random bytes, good for a minute
//!   sign("sigil/auth/v1", audience ‖ nonce) with the device key
//!   ── answer(device key, nonce, sig) ──▶  verified once → DeviceId
//! ```
//!
//! What a session then carries is the [`DeviceId`]; which person that is
//! comes from the credential in the device's leaf when it acts in a group.
//! Signing the audience in stops a challenge answered for one server being
//! replayed at another.

use std::collections::HashMap;

use ed25519_dalek::{Signature, VerifyingKey};
use sigil::{Error, Result};

use crate::DeviceId;

pub const LABEL: &str = "sigil/auth/v1";
/// How long a challenge may wait for its answer, in seconds.
pub const TTL: u64 = 60;

/// The bytes a device signs to answer `nonce` for `audience`. Matches
/// [`sigil::Device::sign`]`(LABEL, …)`.
pub fn signing_input(audience: &str, nonce: &[u8; 32]) -> Vec<u8> {
    [LABEL.as_bytes(), b"\0", audience.as_bytes(), b"\0", nonce].concat()
}

/// The payload a client passes to [`sigil::Device::sign`] with [`LABEL`].
pub fn payload(audience: &str, nonce: &[u8; 32]) -> Vec<u8> {
    [audience.as_bytes(), b"\0", nonce].concat()
}

pub struct Challenges {
    audience: String,
    open: HashMap<[u8; 32], u64>,
}

impl Challenges {
    pub fn new(audience: &str) -> Challenges {
        Challenges { audience: audience.to_string(), open: HashMap::new() }
    }

    /// A fresh challenge. `random` is 32 bytes from the platform.
    pub fn issue(&mut self, random: [u8; 32], now: u64) -> [u8; 32] {
        self.open.retain(|_, at| now < *at + TTL);
        self.open.insert(random, now);
        random
    }

    /// Check an answer. Each challenge answers once, right or wrong.
    pub fn answer(&mut self, device: &[u8], nonce: &[u8; 32], signature: &[u8], now: u64) -> Result<DeviceId> {
        let at = self.open.remove(nonce).ok_or(Error::Replay)?;
        if now >= at + TTL {
            return Err(Error::Expired);
        }
        let key: [u8; 32] = device.try_into().map_err(|_| Error::Unauthentic)?;
        let sig: [u8; 64] = signature.try_into().map_err(|_| Error::Unauthentic)?;
        VerifyingKey::from_bytes(&key)
            .map_err(|_| Error::Unauthentic)?
            .verify_strict(&signing_input(&self.audience, nonce), &Signature::from_bytes(&sig))
            .map_err(|_| Error::Unauthentic)?;
        Ok(device.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil::Device;

    #[test]
    fn a_device_signs_in_once() {
        let d = Device::new("alice").unwrap();
        let mut c = Challenges::new("chat.example");
        let n = c.issue([7; 32], 100);
        let sig = d.sign(LABEL, &payload("chat.example", &n)).unwrap();
        assert_eq!(c.answer(&d.public_key(), &n, &sig, 110).unwrap(), d.public_key());
        assert_eq!(c.answer(&d.public_key(), &n, &sig, 111), Err(Error::Replay));
    }

    #[test]
    fn refuses_other_servers_other_devices_and_late_answers() {
        let d = Device::new("alice").unwrap();
        let other = Device::new("mallory").unwrap();
        let mut c = Challenges::new("chat.example");
        let n = c.issue([1; 32], 100);
        let elsewhere = d.sign(LABEL, &payload("evil.example", &n)).unwrap();
        assert_eq!(c.answer(&d.public_key(), &n, &elsewhere, 101), Err(Error::Unauthentic));
        let n = c.issue([2; 32], 100);
        let sig = d.sign(LABEL, &payload("chat.example", &n)).unwrap();
        assert_eq!(c.answer(&other.public_key(), &n, &sig, 101), Err(Error::Unauthentic));
        let n = c.issue([3; 32], 100);
        let sig = d.sign(LABEL, &payload("chat.example", &n)).unwrap();
        assert_eq!(c.answer(&d.public_key(), &n, &sig, 100 + TTL), Err(Error::Expired));
    }
}
