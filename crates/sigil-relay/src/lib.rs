//! The relay for Sigil's lossy services.
//!
//! Forwarding is call-relay's, as it is: one room per (group, lossy
//! service), datagrams passed to everyone else in the room, admission and
//! renewal by short HMAC grants, revocation that takes effect at once. What
//! this crate adds is the Sigil side of admission:
//!
//! - a room is named by its group and service ([`room`]), and a peer by its
//!   device key ([`peer`]), so a grant is good for exactly one device in one
//!   service of one group;
//! - an [`Issuer`] — the Sigil server, which knows the roster — grants only
//!   member devices, with what the group's policy lets them send, and
//!   revokes a device's grants when a commit removes it.
//!
//! The relay never sees a key. Frames are [`sigil-media`] envelopes, whose
//! first two bytes are call-proto's kind and track, which is all the relay
//! reads. Who sent a frame is in its SFrame KID, as a leaf index; the
//! relay's own numbering is for routing only.
//!
//! [`sigil-media`]: ../sigil_media/index.html

use std::time::Duration;

use call_auth::{Action, Grant, Revocation, Send};
use sigil::{GroupId, Kind, Policy, Role, ServiceId, Services};

pub use call_relay::{now_ms, Config, Handle};

/// How long a grant lasts; clients renew at half-life.
pub const GRANT_TTL: Duration = Duration::from_secs(10 * 60);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The relay room for a group's lossy service.
pub fn room(group: &GroupId, service: ServiceId) -> String {
    format!("{}-{service}", hex(group))
}

/// The relay's name for a device: its signature key, in hex.
pub fn peer(device: &[u8]) -> String {
    hex(device)
}

/// What a role may send on a lossy service, under the group's policy.
pub fn send_for(policy: &Policy, service: ServiceId, role: Role) -> Send {
    if policy.service(service).may_send.contains(role) {
        Send::ALL
    } else {
        Send::NONE
    }
}

/// Why a grant wasn't issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NotLossy,
    NotAMember,
}

/// One lossy service of one group, as the issuer needs to see it.
#[derive(Clone, Copy)]
pub struct Room<'a> {
    pub group: &'a GroupId,
    pub services: &'a Services,
    pub policy: &'a Policy,
    pub service: ServiceId,
}

/// Mints grants and revocations. Holds the secret the relay checks them
/// with; lives in the Sigil server.
pub struct Issuer {
    secret: Vec<u8>,
}

impl Issuer {
    /// `secret` must be at least 16 bytes, and the same one the relay has.
    pub fn new(secret: &[u8]) -> Issuer {
        assert!(secret.len() >= 16, "relay secret too short");
        Issuer { secret: secret.to_vec() }
    }

    /// A grant for `device`, whose person holds `role` (none if not a
    /// member), to join the room.
    pub fn grant(&self, at: Room<'_>, device: &[u8], role: Option<Role>, now_ms: u64) -> Result<String, Refused> {
        let lossy = at.services.get(at.service).is_ok_and(|s| s.kind == Kind::Lossy);
        if !lossy {
            return Err(Refused::NotLossy);
        }
        let role = role.ok_or(Refused::NotAMember)?;
        let send = send_for(at.policy, at.service, role);
        Ok(Grant::mint(&self.secret, &room(at.group, at.service), &peer(device), send, now_ms, GRANT_TTL.as_millis() as u64))
    }

    /// Revocations that take `device` out of every lossy room of the group.
    pub fn remove(&self, group: &GroupId, services: &Services, device: &[u8], now_ms: u64) -> Vec<String> {
        lossy(services)
            .map(|id| Revocation::mint(&self.secret, Some(&room(group, id)), &peer(device), Action::Kick, now_ms))
            .collect()
    }

    /// Revocations that cap what `device` may send after a policy change.
    pub fn limit(&self, group: &GroupId, policy: &Policy, services: &Services, device: &[u8], role: Role, now_ms: u64) -> Vec<String> {
        lossy(services)
            .map(|id| {
                let send = send_for(policy, id, role);
                Revocation::mint(&self.secret, Some(&room(group, id)), &peer(device), Action::Limit(send), now_ms)
            })
            .collect()
    }
}

fn lossy(services: &Services) -> impl Iterator<Item = ServiceId> + '_ {
    services.iter().filter(|(_, s)| s.kind == Kind::Lossy).map(|(id, _)| id)
}

/// Relay settings for Sigil: grants required, checked with `secret`.
pub fn config(secret: &[u8]) -> Config {
    Config {
        auth: Some(call_relay::Auth { secret: secret.to_vec(), max_ttl: GRANT_TTL + Duration::from_secs(60) }),
        ..Default::default()
    }
}

/// Run a Sigil relay on `listener` in the background.
pub fn spawn(listener: datagram::Listener, secret: &[u8]) -> Handle {
    call_relay::spawn(listener, config(secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil::policy::{Roles, ServicePolicy};
    use sigil::Service;

    fn setup() -> (Services, Policy) {
        let s = Services::new()
            .with(1, Service::new("voice", Kind::Lossy, 1).unwrap())
            .unwrap()
            .with(2, Service::new("notes", Kind::Stateful, 1).unwrap())
            .unwrap();
        let mut p = Policy::new("alice");
        p.services.insert(1, ServicePolicy { may_send: Roles::at_least(Role::Admin), ..Default::default() });
        (s, p)
    }

    #[test]
    fn grants_name_one_device_in_one_room_with_the_policys_send() {
        let (s, p) = setup();
        let i = Issuer::new(b"sixteen bytes at least");
        let g = [9; 16];
        let at = Room { group: &g, services: &s, policy: &p, service: 1 };
        let token = i.grant(at, &[1, 2], Some(Role::Member), 1_000).unwrap();
        let grant = Grant::verify(b"sixteen bytes at least", &token).unwrap();
        assert_eq!((grant.room.as_str(), grant.user.as_str()), (room(&g, 1).as_str(), "0102"));
        assert_eq!(grant.send, Send::NONE, "members listen only under this policy");
        let admin = Grant::verify(b"sixteen bytes at least", &i.grant(at, &[1, 2], Some(Role::Owner), 1_000).unwrap()).unwrap();
        assert_eq!(admin.send, Send::ALL);
    }

    #[test]
    fn refuses_non_members_and_non_lossy_services() {
        let (s, p) = setup();
        let i = Issuer::new(b"sixteen bytes at least");
        let g = [0; 16];
        assert_eq!(i.grant(Room { group: &g, services: &s, policy: &p, service: 1 }, &[1], None, 0), Err(Refused::NotAMember));
        assert_eq!(i.grant(Room { group: &g, services: &s, policy: &p, service: 2 }, &[1], Some(Role::Owner), 0), Err(Refused::NotLossy));
        assert_eq!(i.remove(&[0; 16], &s, &[1], 0).len(), 1, "one revocation per lossy service");
    }
}
