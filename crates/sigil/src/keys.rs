//! Per-service keys from the MLS exporter (spec §5.2, §5.3).
//!
//! ```text
//! sframe_base(svc)  = Expand(MLS-Exporter("sigil/svc/v1",   ctx(svc), 32), "sframe", 16)
//! state_key(svc)    =        MLS-Exporter("sigil/state/v1", ctx(svc), 32)
//! record_key        = Expand(state_key, "record", 16)
//! record_nonce      = Expand(state_key, "nonce", 12)
//! ```
//!
//! Every commit starts an epoch, and every epoch gives each stateful
//! service a new state key, so whoever is in the epoch has it and nobody
//! else does — nothing is sent, and nothing can be withheld. Members keep the
//! state keys of the epochs they were in, so stored records stay readable
//! after MLS has deleted the epoch's own secrets; `link` records (§5.3)
//! chain each epoch's key to the one before.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::service::{Kind, Service};
use crate::{Error, Result};

pub const LABEL_SERVICE: &str = "sigil/svc/v1";
pub const LABEL_STATE: &str = "sigil/state/v1";

/// Something that can run the MLS exporter for the current epoch. The
/// [`Group`](crate::Group) is one; tests use a fixed secret.
pub trait Exporter {
    fn export(&self, label: &str, context: &[u8], len: usize) -> Result<Vec<u8>>;
}

fn expand<const N: usize>(prk: &[u8], info: &str) -> [u8; N] {
    let hk = Hkdf::<Sha256>::from_prk(prk).expect("exporter output is 32 bytes");
    let mut out = [0u8; N];
    hk.expand(info.as_bytes(), &mut out).expect("N is small");
    out
}

/// The base key a lossy service's SFrame sender keys come from (§6.3).
pub fn sframe_base(ex: &impl Exporter, service: &Service) -> Result<[u8; 16]> {
    if service.kind != Kind::Lossy {
        return Err(Error::NotAllowed(format!("{} is not a lossy service", service.name)));
    }
    let secret = ex.export(LABEL_SERVICE, &service.context(), 32)?;
    Ok(expand(&secret, "sframe"))
}

/// A stateful service's key for one epoch.
#[derive(Clone, PartialEq, Eq)]
pub struct StateKey([u8; 32]);

impl std::fmt::Debug for StateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StateKey(…)")
    }
}

impl Drop for StateKey {
    fn drop(&mut self) {
        // Best effort: no zeroize dependency, and the optimiser may elide it.
        self.0 = [0; 32];
    }
}

impl StateKey {
    /// The current epoch's key for a stateful `service`.
    pub fn derive(ex: &impl Exporter, service: &Service) -> Result<StateKey> {
        if service.kind != Kind::Stateful {
            return Err(Error::NotAllowed(format!("{} is not a stateful service", service.name)));
        }
        let bytes = ex.export(LABEL_STATE, &service.context(), 32)?;
        Ok(StateKey(bytes.try_into().map_err(|_| Error::Malformed("exported key"))?))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> StateKey {
        StateKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn record_key(&self) -> [u8; 16] {
        expand(&self.0, "record")
    }

    pub(crate) fn record_nonce(&self) -> [u8; 12] {
        expand(&self.0, "nonce")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use hkdf::Hkdf;

    /// An exporter over a fixed secret, standing in for one epoch.
    pub struct Fixed(pub [u8; 32]);

    impl Exporter for Fixed {
        fn export(&self, label: &str, context: &[u8], len: usize) -> Result<Vec<u8>> {
            let hk = Hkdf::<Sha256>::new(None, &self.0);
            let mut info = label.as_bytes().to_vec();
            info.extend_from_slice(context);
            let mut out = vec![0; len];
            hk.expand(&info, &mut out).unwrap();
            Ok(out)
        }
    }

    #[test]
    fn keys_differ_by_service_and_epoch() {
        let a = Service::new("a", Kind::Stateful, 1).unwrap();
        let b = Service::new("b", Kind::Stateful, 1).unwrap();
        let e1 = Fixed([1; 32]);
        let e2 = Fixed([2; 32]);
        assert_ne!(StateKey::derive(&e1, &a).unwrap(), StateKey::derive(&e1, &b).unwrap());
        assert_ne!(StateKey::derive(&e1, &a).unwrap(), StateKey::derive(&e2, &a).unwrap());
        assert_eq!(StateKey::derive(&e1, &a).unwrap(), StateKey::derive(&e1, &a).unwrap());
    }

    #[test]
    fn kinds_get_only_their_own_keys() {
        let ex = Fixed([3; 32]);
        let voice = Service::new("voice", Kind::Lossy, 1).unwrap();
        let notes = Service::new("notes", Kind::Stateful, 1).unwrap();
        assert!(StateKey::derive(&ex, &voice).is_err());
        assert!(sframe_base(&ex, &notes).is_err());
        assert!(sframe_base(&ex, &voice).is_ok());
    }
}
