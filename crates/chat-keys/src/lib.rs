//! Device keys, and how a call's room key moves between devices.
//!
//! A call is end-to-end encrypted with one room key every participant holds
//! (`call-e2ee`). This crate is how that key gets from the devices already in
//! a call to one joining it, through a server that must not learn it:
//!
//! ```text
//!   joining device D ── "key for room R, please" + D's public key ──▶ server
//!   server ── that request ──▶ devices already in R
//!   device A: seal(room key, to D) ──▶ server ──▶ D: open(…, from A)
//! ```
//!
//! # The construction
//!
//! Each device generates an X25519 [`DeviceKey`] once and publishes only its
//! public half. Sealing is HPKE's *auth* mode in miniature: a fresh
//! ephemeral key per message, and the sender's static key, both agreed with
//! the recipient's static key —
//!
//! ```text
//! ikm   = X25519(eph, R) ‖ X25519(S, R)
//! key, nonce = HKDF-SHA256(salt = "idea-chat/call-key/v1", ikm, info = context)
//! sealed = eph_pub ‖ ChaCha20-Poly1305(key, nonce, room key, aad = context)
//! context = room ‖ sender device ‖ recipient device ‖ eph_pub ‖ S_pub ‖ R_pub
//! ```
//!
//! The ephemeral half gives each seal its own key; the static half
//! authenticates the sender, since only the holder of `S` (or of `R`) can
//! compute `X25519(S, R)`. Binding the room and both device ids into the
//! context means a sealed key cannot be replayed into another call or passed
//! off as from another device.
//!
//! # What it does not do
//!
//! The server keeps the directory of which public key belongs to which
//! device. A server that lied about it — handing out its own key as a
//! newcomer's — could have a real participant seal the room key to it. The
//! defence is out of band: each device has a [`fingerprint`] people can
//! compare, and clients warn when someone's devices change. This protects
//! against a passive or compromised relay and a server that only reads what
//! passes through it; it is not a substitute for comparing fingerprints with
//! someone you need to be sure of.
//!
//! There is no forward secrecy across calls beyond the room key being new
//! for each call instance, and none for device keys: a stolen device key
//! opens the room keys that were sealed to it.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as XPublic, StaticSecret};
use zeroize::Zeroizing;

const SALT: &[u8] = b"idea-chat/call-key/v1";
/// The most a sealed secret may hold. A room key is 22 characters.
pub const MAX_SECRET: usize = 256;

/// Why a key or a sealed message was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not valid base64, or the wrong length.
    Malformed,
    /// It did not open: the wrong key, the wrong context, or tampered with.
    Rejected,
    /// The secret is longer than [`MAX_SECRET`].
    TooLong,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Malformed => "malformed key material",
            Error::Rejected => "the sealed key did not open",
            Error::TooLong => "secret too long to seal",
        })
    }
}

impl std::error::Error for Error {}

/// A device's private key. Kept on the device; never sent anywhere.
#[derive(Clone)]
pub struct DeviceKey(StaticSecret);

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.public().as_text())
    }
}

impl DeviceKey {
    /// A new key from 32 bytes of the platform's CSPRNG.
    pub fn generate(random: [u8; 32]) -> DeviceKey {
        DeviceKey(StaticSecret::from(random))
    }

    /// The key as text, for the device's own secure storage.
    pub fn to_text(&self) -> Zeroizing<String> {
        Zeroizing::new(B64.encode(self.0.to_bytes()))
    }

    /// A key read back from [`DeviceKey::to_text`].
    pub fn from_text(text: &str) -> Result<DeviceKey, Error> {
        let bytes = Zeroizing::new(B64.decode(text.trim()).map_err(|_| Error::Malformed)?);
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| Error::Malformed)?;
        Ok(DeviceKey(StaticSecret::from(arr)))
    }

    /// The half that is published.
    pub fn public(&self) -> PublicKey {
        PublicKey(XPublic::from(&self.0))
    }
}

/// A device's public key, as the directory lists it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(XPublic);

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.as_text())
    }
}

impl PublicKey {
    /// Base64url, no padding: 43 characters.
    pub fn as_text(&self) -> String {
        B64.encode(self.0.as_bytes())
    }

    pub fn from_text(text: &str) -> Result<PublicKey, Error> {
        let bytes = B64.decode(text.trim()).map_err(|_| Error::Malformed)?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| Error::Malformed)?;
        Ok(PublicKey(XPublic::from(arr)))
    }

    fn bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

/// Who is sealing to whom, about what. Both ends must name the same parties
/// for a seal to open; every field is bound into the ciphertext.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The call instance the key is for.
    pub room: &'a str,
    pub sender_device: &'a str,
    pub recipient_device: &'a str,
}

impl Context<'_> {
    fn bytes(&self, eph: &[u8; 32], sender: &[u8; 32], recipient: &[u8; 32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        for part in [self.room.as_bytes(), self.sender_device.as_bytes(), self.recipient_device.as_bytes()] {
            // Length-prefixed, so ("ab","c") and ("a","bc") differ.
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part);
        }
        out.extend_from_slice(eph);
        out.extend_from_slice(sender);
        out.extend_from_slice(recipient);
        out
    }
}

fn derive(ikm: &[u8], info: &[u8]) -> (Zeroizing<[u8; 32]>, [u8; 12]) {
    let hk = Hkdf::<Sha256>::new(Some(SALT), ikm);
    let mut okm = Zeroizing::new([0u8; 44]);
    hk.expand(info, okm.as_mut()).expect("44 bytes is a valid HKDF-SHA256 length");
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&okm[..32]);
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&okm[32..]);
    (key, nonce)
}

/// Seal `secret` from `sender` to the device whose public key is `recipient`.
/// `ephemeral` is 32 fresh random bytes, used once. Returns the sealed text
/// to hand to the server.
pub fn seal(
    secret: &str,
    sender: &DeviceKey,
    recipient: &PublicKey,
    context: Context<'_>,
    ephemeral: [u8; 32],
) -> Result<String, Error> {
    if secret.len() > MAX_SECRET {
        return Err(Error::TooLong);
    }
    let eph = StaticSecret::from(ephemeral);
    let eph_pub = XPublic::from(&eph);
    let mut ikm = Zeroizing::new([0u8; 64]);
    ikm[..32].copy_from_slice(eph.diffie_hellman(&recipient.0).as_bytes());
    ikm[32..].copy_from_slice(sender.0.diffie_hellman(&recipient.0).as_bytes());
    let info = context.bytes(eph_pub.as_bytes(), sender.public().bytes(), recipient.bytes());
    let (key, nonce) = derive(ikm.as_ref(), &info);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: secret.as_bytes(), aad: &info })
        .map_err(|_| Error::Rejected)?;
    let mut out = Vec::with_capacity(32 + ct.len());
    out.extend_from_slice(eph_pub.as_bytes());
    out.extend_from_slice(&ct);
    Ok(B64.encode(out))
}

/// Open a sealed secret addressed to `me`, from the device whose public key
/// is `sender`. Fails unless `context` names exactly what the sender sealed.
pub fn open(sealed: &str, me: &DeviceKey, sender: &PublicKey, context: Context<'_>) -> Result<Zeroizing<String>, Error> {
    let bytes = B64.decode(sealed.trim()).map_err(|_| Error::Malformed)?;
    // eph (32) + at least the tag (16)
    if bytes.len() < 48 {
        return Err(Error::Malformed);
    }
    let eph_bytes: [u8; 32] = bytes[..32].try_into().expect("checked length");
    let eph_pub = XPublic::from(eph_bytes);
    let mut ikm = Zeroizing::new([0u8; 64]);
    ikm[..32].copy_from_slice(me.0.diffie_hellman(&eph_pub).as_bytes());
    ikm[32..].copy_from_slice(me.0.diffie_hellman(&sender.0).as_bytes());
    let info = context.bytes(&eph_bytes, sender.bytes(), me.public().bytes());
    let (key, nonce) = derive(ikm.as_ref(), &info);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let pt = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &bytes[32..], aad: &info })
            .map_err(|_| Error::Rejected)?,
    );
    String::from_utf8(pt.to_vec()).map(Zeroizing::new).map_err(|_| Error::Rejected)
}

/// A public key's fingerprint for people to compare: twelve groups of five
/// digits drawn from its SHA-256, the way a phone number is easier to read
/// aloud than hex.
pub fn fingerprint(key: &PublicKey) -> String {
    let digest = Sha256::digest(key.bytes());
    digest
        .chunks(2)
        .take(12)
        .map(|pair| format!("{:05}", u16::from_be_bytes([pair[0], pair[1]]) as u32 % 100_000))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> DeviceKey {
        DeviceKey::generate([seed; 32])
    }

    fn ctx<'a>(room: &'a str, from: &'a str, to: &'a str) -> Context<'a> {
        Context { room, sender_device: from, recipient_device: to }
    }

    #[test]
    fn a_sealed_key_opens_for_its_recipient() {
        let (a, d) = (key(1), key(2));
        let sealed = seal("room-key-text", &a, &d.public(), ctx("r", "A", "D"), [9; 32]).unwrap();
        let opened = open(&sealed, &d, &a.public(), ctx("r", "A", "D")).unwrap();
        assert_eq!(opened.as_str(), "room-key-text");
    }

    #[test]
    fn it_does_not_open_for_anyone_else() {
        let (a, d, eve) = (key(1), key(2), key(3));
        let sealed = seal("secret", &a, &d.public(), ctx("r", "A", "D"), [9; 32]).unwrap();
        assert_eq!(open(&sealed, &eve, &a.public(), ctx("r", "A", "D")).unwrap_err(), Error::Rejected);
    }

    /// Sender authentication: a seal made with someone else's static key
    /// does not open as if from A.
    #[test]
    fn a_forged_sender_is_rejected() {
        let (a, d, eve) = (key(1), key(2), key(3));
        let forged = seal("secret", &eve, &d.public(), ctx("r", "A", "D"), [9; 32]).unwrap();
        assert_eq!(open(&forged, &d, &a.public(), ctx("r", "A", "D")).unwrap_err(), Error::Rejected);
    }

    #[test]
    fn a_seal_cannot_be_replayed_into_another_room_or_device() {
        let (a, d) = (key(1), key(2));
        let sealed = seal("secret", &a, &d.public(), ctx("r1", "A", "D"), [9; 32]).unwrap();
        assert!(open(&sealed, &d, &a.public(), ctx("r2", "A", "D")).is_err());
        assert!(open(&sealed, &d, &a.public(), ctx("r1", "B", "D")).is_err());
        assert!(open(&sealed, &d, &a.public(), ctx("r1", "A", "E")).is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let (a, d) = (key(1), key(2));
        let sealed = seal("secret", &a, &d.public(), ctx("r", "A", "D"), [9; 32]).unwrap();
        let mut bytes = B64.decode(&sealed).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(open(&B64.encode(bytes), &d, &a.public(), ctx("r", "A", "D")).unwrap_err(), Error::Rejected);
    }

    #[test]
    fn fresh_ephemerals_make_distinct_seals() {
        let (a, d) = (key(1), key(2));
        let one = seal("secret", &a, &d.public(), ctx("r", "A", "D"), [9; 32]).unwrap();
        let two = seal("secret", &a, &d.public(), ctx("r", "A", "D"), [10; 32]).unwrap();
        assert_ne!(one, two);
    }

    #[test]
    fn keys_round_trip_through_text() {
        let a = key(7);
        let back = DeviceKey::from_text(&a.to_text()).unwrap();
        assert_eq!(back.public(), a.public());
        let pub_back = PublicKey::from_text(&a.public().as_text()).unwrap();
        assert_eq!(pub_back, a.public());
        assert_eq!(PublicKey::from_text("short").unwrap_err(), Error::Malformed);
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        let f = fingerprint(&key(1).public());
        assert_eq!(f, fingerprint(&key(1).public()));
        assert_ne!(f, fingerprint(&key(2).public()));
        assert_eq!(f.split(' ').count(), 12);
        assert!(f.split(' ').all(|g| g.len() == 5 && g.chars().all(|c| c.is_ascii_digit())));
    }

    #[test]
    fn an_oversized_secret_is_refused() {
        let (a, d) = (key(1), key(2));
        let long = "x".repeat(MAX_SECRET + 1);
        assert_eq!(seal(&long, &a, &d.public(), ctx("r", "A", "D"), [9; 32]).unwrap_err(), Error::TooLong);
    }
}
