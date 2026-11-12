//! Group policy (spec §9): who may change the group, and each service's
//! rules.
//!
//! The policy lives in the group's GroupContext, so every member holds the
//! same one, and so does the server: commits are sent in the clear, which is
//! what lets a server enforce `may_send` and check a checkpoint's trust
//! without reading anything. Members check every commit against the policy
//! of the epoch it applies to ([`Policy::check`]); one that fails never
//! becomes an epoch.

use std::collections::BTreeMap;

use crate::service::ServiceId;
use crate::wire::{Reader, Writer};
use crate::{Error, Result, UserId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Member,
    Admin,
    Owner,
}

impl Role {
    fn code(self) -> u8 {
        match self {
            Role::Member => 0,
            Role::Admin => 1,
            Role::Owner => 2,
        }
    }
    fn from_code(c: u8) -> Result<Role> {
        match c {
            0 => Ok(Role::Member),
            1 => Ok(Role::Admin),
            2 => Ok(Role::Owner),
            _ => Err(Error::Malformed("role")),
        }
    }
}

/// A set of roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Roles(u8);

impl Roles {
    pub const NONE: Roles = Roles(0);
    pub const ALL: Roles = Roles(0b111);

    pub fn of(roles: &[Role]) -> Roles {
        Roles(roles.iter().fold(0, |m, r| m | 1 << r.code()))
    }
    /// `role` and every role above it.
    pub fn at_least(role: Role) -> Roles {
        Roles::of(&[Role::Member, Role::Admin, Role::Owner].into_iter().filter(|r| *r >= role).collect::<Vec<_>>())
    }
    pub fn contains(self, role: Role) -> bool {
        self.0 & (1 << role.code()) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum History {
    /// Newcomers read what came before them.
    Full,
    /// Newcomers read from when they joined.
    None,
}

/// When a checkpoint counts as trusted (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustRule {
    /// Distinct people who must agree, the poster included.
    pub people: u8,
    /// Or: any one of these roles agreeing is enough.
    pub or_any_of: Roles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePolicy {
    pub may_send: Roles,
    // Stateful services only.
    pub history: History,
    /// Seconds between key-update commits; none for no schedule.
    pub rotate_every: Option<u32>,
    /// Records between checkpoints; none for no checkpoints.
    pub checkpoint_every: Option<u32>,
    pub may_checkpoint: Roles,
    pub trust_checkpoint: TrustRule,
}

impl Default for ServicePolicy {
    fn default() -> Self {
        ServicePolicy {
            may_send: Roles::ALL,
            history: History::Full,
            rotate_every: None,
            checkpoint_every: None,
            may_checkpoint: Roles::ALL,
            trust_checkpoint: TrustRule { people: 2, or_any_of: Roles::at_least(Role::Admin) },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Everyone not listed is a [`Role::Member`].
    pub roles: BTreeMap<UserId, Role>,
    pub may_add: Roles,
    pub may_remove: Roles,
    pub may_change_policy: Roles,
    pub services: BTreeMap<ServiceId, ServicePolicy>,
}

/// What a commit does, as the policy needs to see it.
#[derive(Debug, Clone, Default)]
pub struct Change {
    pub committer: UserId,
    /// Each proposal's author, and what it does.
    pub proposals: Vec<(UserId, Proposal)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proposal {
    Add(UserId),
    /// The user whose leaf is removed.
    Remove(UserId),
    /// The policy or the services table changes.
    Extensions,
}

impl Policy {
    /// A group `owner` made: they own it, admins and up may add and remove,
    /// only the owner may change the policy.
    pub fn new(owner: &str) -> Policy {
        Policy {
            roles: BTreeMap::from([(owner.to_string(), Role::Owner)]),
            may_add: Roles::at_least(Role::Admin),
            may_remove: Roles::at_least(Role::Admin),
            may_change_policy: Roles::of(&[Role::Owner]),
            services: BTreeMap::new(),
        }
    }

    pub fn role_of(&self, user: &str) -> Role {
        self.roles.get(user).copied().unwrap_or(Role::Member)
    }

    pub fn service(&self, id: ServiceId) -> ServicePolicy {
        self.services.get(&id).cloned().unwrap_or_default()
    }

    /// Whether this policy allows `change`. Leaving is always allowed:
    /// removing one's own leaf needs no role.
    pub fn check(&self, change: &Change) -> Result<()> {
        let allowed = |who: &str, roles: Roles| roles.contains(self.role_of(who));
        for (author, p) in &change.proposals {
            let ok = match p {
                Proposal::Add(_) => allowed(author, self.may_add),
                Proposal::Remove(target) => target == author || allowed(author, self.may_remove),
                Proposal::Extensions => allowed(author, self.may_change_policy),
            };
            if !ok {
                return Err(Error::NotAllowed(format!("{author} may not {p:?}")));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(self.roles.len() as u32);
        for (u, r) in &self.roles {
            w.str(u).u8(r.code());
        }
        w.u8(self.may_add.0).u8(self.may_remove.0).u8(self.may_change_policy.0);
        w.u16(self.services.len() as u16);
        for (id, s) in &self.services {
            w.u16(*id).u8(s.may_send.0);
            w.u8(match s.history {
                History::Full => 0,
                History::None => 1,
            });
            w.u32(s.rotate_every.unwrap_or(0)).u32(s.checkpoint_every.unwrap_or(0));
            w.u8(s.may_checkpoint.0).u8(s.trust_checkpoint.people).u8(s.trust_checkpoint.or_any_of.0);
        }
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Policy> {
        let mut r = Reader::new(bytes, "policy");
        let roles_mask = |m: u8| if m & !Roles::ALL.0 == 0 { Ok(Roles(m)) } else { Err(Error::Malformed("roles")) };
        let nonzero = |v: u32| (v != 0).then_some(v);
        let mut roles = BTreeMap::new();
        for _ in 0..r.u32()? {
            let u = r.str()?;
            roles.insert(u, Role::from_code(r.u8()?)?);
        }
        let may_add = roles_mask(r.u8()?)?;
        let may_remove = roles_mask(r.u8()?)?;
        let may_change_policy = roles_mask(r.u8()?)?;
        let mut services = BTreeMap::new();
        for _ in 0..r.u16()? {
            let id = r.u16()?;
            let may_send = roles_mask(r.u8()?)?;
            let history = match r.u8()? {
                0 => History::Full,
                1 => History::None,
                _ => return Err(Error::Malformed("history")),
            };
            let s = ServicePolicy {
                may_send,
                history,
                rotate_every: nonzero(r.u32()?),
                checkpoint_every: nonzero(r.u32()?),
                may_checkpoint: roles_mask(r.u8()?)?,
                trust_checkpoint: TrustRule { people: r.u8()?, or_any_of: roles_mask(r.u8()?)? },
            };
            services.insert(id, s);
        }
        r.done()?;
        Ok(Policy { roles, may_add, may_remove, may_change_policy, services })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        let mut p = Policy::new("owner@id");
        p.roles.insert("admin@id".into(), Role::Admin);
        p.services.insert(1, ServicePolicy { history: History::None, checkpoint_every: Some(500), ..Default::default() });
        p
    }

    fn change(author: &str, p: Proposal) -> Change {
        Change { committer: author.into(), proposals: vec![(author.into(), p)] }
    }

    #[test]
    fn round_trips() {
        assert_eq!(Policy::decode(&policy().encode()).unwrap(), policy());
    }

    #[test]
    fn roles_gate_changes() {
        let p = policy();
        assert!(p.check(&change("admin@id", Proposal::Add("x@id".into()))).is_ok());
        assert!(p.check(&change("member@id", Proposal::Add("x@id".into()))).is_err());
        assert!(p.check(&change("admin@id", Proposal::Remove("member@id".into()))).is_ok());
        assert!(p.check(&change("member@id", Proposal::Remove("admin@id".into()))).is_err());
        assert!(p.check(&change("admin@id", Proposal::Extensions)).is_err());
        assert!(p.check(&change("owner@id", Proposal::Extensions)).is_ok());
    }

    #[test]
    fn anyone_may_leave() {
        assert!(policy().check(&change("member@id", Proposal::Remove("member@id".into()))).is_ok());
    }

    #[test]
    fn every_proposal_is_checked_against_its_author() {
        let p = policy();
        let c = Change {
            committer: "admin@id".into(),
            proposals: vec![("admin@id".into(), Proposal::Add("x@id".into())), ("member@id".into(), Proposal::Add("y@id".into()))],
        };
        assert!(p.check(&c).is_err());
    }
}
