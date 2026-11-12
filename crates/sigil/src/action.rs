//! Actions (spec §11): signed requests one device makes of one server, or of
//! the identity service.
//!
//! A server can't read content, so it can't decide on its own whom to act
//! for; an action is a member's signature saying exactly what to do, for
//! whom, and until when. [`verify`] checks everything that needs no
//! knowledge of the group: signature, audience, lifetime and replay. The
//! receiver then checks the issuer's credential and, for a group action, the
//! group's policy.

use std::collections::HashMap;

use ed25519_dalek::{Signature, VerifyingKey};
use openmls_traits::signatures::Signer;

use crate::wire::{Reader, Writer};
use crate::{Error, GroupId, Result};

const LABEL: &[u8] = b"sigil/action/v1";
pub const VERSION: u8 = 1;
/// The longest an action may live, in seconds.
pub const MAX_LIFETIME: u64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// `record.delete`, `records.prune`, `push.notify`, …
    pub kind: String,
    /// The one receiver that may act on it.
    pub audience: String,
    /// The group and epoch it concerns, if any.
    pub group: Option<(GroupId, u64)>,
    pub body: Vec<u8>,
    pub nonce: [u8; 16],
    /// Seconds since the Unix epoch.
    pub issued_at: u64,
    pub expires_at: u64,
}

/// Who signed: their MLS credential (serialized) and leaf signature key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issuer {
    pub credential: Vec<u8>,
    pub signature_key: Vec<u8>,
}

impl Action {
    fn signed_part(&self, issuer: &Issuer) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION).str(&self.kind).str(&self.audience);
        match &self.group {
            Some((g, epoch)) => w.u8(1).fixed(g).u64(*epoch),
            None => w.u8(0),
        };
        w.bytes(&self.body).fixed(&self.nonce).u64(self.issued_at).u64(self.expires_at);
        w.bytes(&issuer.credential).bytes(&issuer.signature_key);
        w.finish()
    }

    /// Sign this action as `issuer`.
    pub fn sign(&self, issuer: &Issuer, signer: &impl Signer) -> Result<Vec<u8>> {
        if self.expires_at <= self.issued_at || self.expires_at - self.issued_at > MAX_LIFETIME {
            return Err(Error::Expired);
        }
        let signed = self.signed_part(issuer);
        let sig = signer.sign(&[LABEL, &signed].concat()).map_err(|e| Error::Mls(format!("{e:?}")))?;
        let mut w = Writer::default();
        w.bytes(&signed).bytes(&sig);
        Ok(w.finish())
    }
}

fn decode(signed: &[u8]) -> Result<(Action, Issuer)> {
    let mut r = Reader::new(signed, "action");
    if r.u8()? != VERSION {
        return Err(Error::Malformed("action version"));
    }
    let kind = r.str()?;
    let audience = r.str()?;
    let group = match r.u8()? {
        0 => None,
        1 => Some((r.array()?, r.u64()?)),
        _ => return Err(Error::Malformed("action group")),
    };
    let action = Action { kind, audience, group, body: r.bytes()?.to_vec(), nonce: r.array()?, issued_at: r.u64()?, expires_at: r.u64()? };
    let issuer = Issuer { credential: r.bytes()?.to_vec(), signature_key: r.bytes()?.to_vec() };
    r.done()?;
    Ok((action, issuer))
}

/// Nonces seen, kept until their action would have expired.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    seen: HashMap<[u8; 16], u64>,
}

impl ReplayGuard {
    fn admit(&mut self, nonce: [u8; 16], expires_at: u64, now: u64) -> Result<()> {
        self.seen.retain(|_, exp| *exp > now);
        if self.seen.contains_key(&nonce) {
            return Err(Error::Replay);
        }
        self.seen.insert(nonce, expires_at);
        Ok(())
    }
}

/// Check an action addressed to `me` at time `now`, and remember its nonce.
/// The issuer's credential and the group's policy are the caller's to check.
pub fn verify(bytes: &[u8], me: &str, now: u64, guard: &mut ReplayGuard) -> Result<(Action, Issuer)> {
    let mut r = Reader::new(bytes, "signed action");
    let signed = r.bytes()?;
    let sig = r.bytes()?;
    r.done()?;
    let (action, issuer) = decode(signed)?;
    let key: [u8; 32] = issuer.signature_key.as_slice().try_into().map_err(|_| Error::Unauthentic)?;
    let sig: [u8; 64] = sig.try_into().map_err(|_| Error::Unauthentic)?;
    VerifyingKey::from_bytes(&key)
        .map_err(|_| Error::Unauthentic)?
        .verify_strict(&[LABEL, signed].concat(), &Signature::from_bytes(&sig))
        .map_err(|_| Error::Unauthentic)?;
    if action.audience != me {
        return Err(Error::WrongAudience);
    }
    let lifetime_ok = action.expires_at > action.issued_at && action.expires_at - action.issued_at <= MAX_LIFETIME;
    if !lifetime_ok || now >= action.expires_at || now + 60 < action.issued_at {
        return Err(Error::Expired);
    }
    guard.admit(action.nonce, action.expires_at, now)?;
    Ok((action, issuer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::tests::signer;

    fn action() -> Action {
        Action {
            kind: "record.delete".into(),
            audience: "chat.example".into(),
            group: Some(([1; 16], 7)),
            body: b"seq 42".to_vec(),
            nonce: [9; 16],
            issued_at: 1_000,
            expires_at: 1_300,
        }
    }

    fn signed() -> (Vec<u8>, Issuer) {
        let s = signer();
        let issuer = Issuer { credential: b"alice@id".to_vec(), signature_key: s.to_public_vec() };
        (action().sign(&issuer, &s).unwrap(), issuer)
    }

    #[test]
    fn verifies_once() {
        let (bytes, issuer) = signed();
        let mut g = ReplayGuard::default();
        assert_eq!(verify(&bytes, "chat.example", 1_100, &mut g).unwrap(), (action(), issuer));
        assert_eq!(verify(&bytes, "chat.example", 1_101, &mut g), Err(Error::Replay));
    }

    #[test]
    fn refuses_other_audiences_and_stale_or_long_lived_actions() {
        let (bytes, _) = signed();
        assert_eq!(verify(&bytes, "other.example", 1_100, &mut ReplayGuard::default()), Err(Error::WrongAudience));
        assert_eq!(verify(&bytes, "chat.example", 1_300, &mut ReplayGuard::default()), Err(Error::Expired));
        let s = signer();
        let issuer = Issuer { credential: vec![], signature_key: s.to_public_vec() };
        let long = Action { expires_at: 1_000 + MAX_LIFETIME + 1, ..action() };
        assert_eq!(long.sign(&issuer, &s), Err(Error::Expired));
    }

    #[test]
    fn refuses_tampering() {
        let (bytes, _) = signed();
        for i in 4..bytes.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 1;
            assert!(verify(&bad, "chat.example", 1_100, &mut ReplayGuard::default()).is_err(), "byte {i}");
        }
    }
}
