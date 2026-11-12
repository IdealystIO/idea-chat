//! Services: the named uses of a group (spec §5.1).
//!
//! A group's [`Services`] table lives in its GroupContext, so every member
//! agrees on what each [`ServiceId`] means; changing it is a commit.

use std::collections::BTreeMap;

use crate::wire::{Reader, Writer};
use crate::{Error, Result};

/// The 16-bit number a service has in one group, used on the wire.
pub type ServiceId = u16;

/// What a service puts on the wire, and whether the server keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Signed records the server stores as ciphertext (§6.1).
    Stateful,
    /// MLS application messages, delivered in order and never stored (§6.2).
    Reliable,
    /// SFrame datagrams through the relay, best effort (§6.3).
    Lossy,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::Stateful => 0,
            Kind::Reliable => 1,
            Kind::Lossy => 2,
        }
    }
    fn from_code(c: u8) -> Result<Kind> {
        match c {
            0 => Ok(Kind::Stateful),
            1 => Ok(Kind::Reliable),
            2 => Ok(Kind::Lossy),
            _ => Err(Error::Malformed("service kind")),
        }
    }
}

/// A service definition: `messages`, `voice`, `app/<app id>/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub kind: Kind,
    pub version: u16,
}

/// The longest service name.
pub const MAX_NAME: usize = 64;

impl Service {
    /// A service, if `name` is 1–64 printable ASCII characters.
    pub fn new(name: &str, kind: Kind, version: u16) -> Result<Service> {
        let ok = !name.is_empty() && name.len() <= MAX_NAME && name.bytes().all(|b| b.is_ascii_graphic());
        if !ok {
            return Err(Error::Malformed("service name"));
        }
        Ok(Service { name: name.to_string(), kind, version })
    }

    /// The exporter context that names this service: its name, then its
    /// version. Length-prefixed, so no two services share one.
    pub(crate) fn context(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.str(&self.name).u16(self.version);
        w.finish()
    }
}

/// A group's services, by id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Services(BTreeMap<ServiceId, Service>);

impl Services {
    pub fn new() -> Services {
        Services::default()
    }

    /// Add `service` under `id`. Refuses a taken id or a name already used.
    pub fn with(mut self, id: ServiceId, service: Service) -> Result<Services> {
        if self.0.contains_key(&id) || self.id_of(&service.name).is_some() {
            return Err(Error::NotAllowed(format!("service {id} or {} already defined", service.name)));
        }
        self.0.insert(id, service);
        Ok(self)
    }

    pub fn get(&self, id: ServiceId) -> Result<&Service> {
        self.0.get(&id).ok_or(Error::UnknownService(id))
    }

    pub fn id_of(&self, name: &str) -> Option<ServiceId> {
        self.0.iter().find(|(_, s)| s.name == name).map(|(id, _)| *id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (ServiceId, &Service)> {
        self.0.iter().map(|(id, s)| (*id, s))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u16(self.0.len() as u16);
        for (id, s) in &self.0 {
            w.u16(*id).str(&s.name).u8(s.kind.code()).u16(s.version);
        }
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Services> {
        let mut r = Reader::new(bytes, "services");
        let mut out = Services::new();
        for _ in 0..r.u16()? {
            let id = r.u16()?;
            let name = r.str()?;
            let kind = Kind::from_code(r.u8()?)?;
            let version = r.u16()?;
            out = out.with(id, Service::new(&name, kind, version)?).map_err(|_| Error::Malformed("services"))?;
        }
        r.done()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let s = Services::new()
            .with(1, Service::new("messages", Kind::Stateful, 1).unwrap())
            .unwrap()
            .with(2, Service::new("voice", Kind::Lossy, 1).unwrap())
            .unwrap();
        assert_eq!(Services::decode(&s.encode()).unwrap(), s);
        assert_eq!(s.id_of("voice"), Some(2));
    }

    #[test]
    fn refuses_bad_names_and_duplicates() {
        assert!(Service::new("", Kind::Lossy, 1).is_err());
        assert!(Service::new("has space", Kind::Lossy, 1).is_err());
        assert!(Service::new(&"x".repeat(65), Kind::Lossy, 1).is_err());
        let s = Services::new().with(1, Service::new("a", Kind::Lossy, 1).unwrap()).unwrap();
        assert!(s.clone().with(1, Service::new("b", Kind::Lossy, 1).unwrap()).is_err());
        assert!(s.with(2, Service::new("a", Kind::Stateful, 1).unwrap()).is_err());
    }

    #[test]
    fn contexts_differ_by_name_and_version() {
        let a = Service::new("ab", Kind::Stateful, 1).unwrap().context();
        let b = Service::new("ab", Kind::Stateful, 2).unwrap().context();
        let c = Service::new("a", Kind::Stateful, 1).unwrap().context();
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
