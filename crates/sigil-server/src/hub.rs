//! The hub: groups' public state, and every request a device makes of it.

use std::collections::{BTreeMap, HashMap};

use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::*;
use openmls_memory_storage::MemoryStorage;
use openmls_rust_crypto::RustCrypto;
use sigil::action::{self, ReplayGuard};
use sigil::group::{describe_commit, read_extensions};
use sigil::policy::Role;
use sigil::record::{self, RecordKind};
use sigil::{Credentials, Error, GroupId, Kind, Policy, Result, ServiceId, Services, UserId};

use crate::store::Store;
use crate::DeviceId;

fn mls<E: std::fmt::Debug>(e: E) -> Error {
    Error::Mls(format!("{e:?}"))
}

/// Who held each leaf, from one epoch on.
type Roster = HashMap<u32, (UserId, DeviceId)>;

struct Tracked {
    public: PublicGroup,
    services: Services,
    policy: Policy,
    rosters: BTreeMap<u64, Roster>,
}

impl Tracked {
    fn epoch(&self) -> u64 {
        self.public.group_context().epoch().as_u64()
    }

    fn roster_at(&self, epoch: u64) -> Option<&Roster> {
        self.rosters.range(..=epoch).next_back().map(|(_, r)| r)
    }

    fn holder(&self, epoch: u64, leaf: u32) -> Result<&(UserId, DeviceId)> {
        self.roster_at(epoch).and_then(|r| r.get(&leaf)).ok_or_else(|| Error::Credential(format!("no member at leaf {leaf} in epoch {epoch}")))
    }

    fn devices(&self) -> Vec<DeviceId> {
        self.roster_at(self.epoch()).into_iter().flatten().map(|(_, (_, d))| d.clone()).collect()
    }

    /// The user behind `device` now, if it is a member.
    fn member(&self, device: &[u8]) -> Option<&UserId> {
        self.roster_at(self.epoch())?.values().find(|(_, d)| d == device).map(|(u, _)| u)
    }

    fn refresh(&mut self, creds: &impl Credentials) -> Result<()> {
        let (services, policy) = read_extensions(self.public.group_context().extensions())?;
        let mut roster = Roster::new();
        for m in self.public.members() {
            let user = creds.verify(&m.credential, &m.signature_key)?;
            roster.insert(m.index.u32(), (user, m.signature_key));
        }
        self.services = services;
        self.policy = policy;
        let epoch = self.epoch();
        self.rosters.insert(epoch, roster);
        Ok(())
    }
}

/// A commit the hub took.
#[derive(Debug, PartialEq, Eq)]
pub struct Accepted {
    pub epoch: u64,
    /// Members before the commit: who to send the commit to.
    pub notify: Vec<DeviceId>,
    /// Devices the commit added: their Welcome is waiting for them.
    pub welcomed: Vec<DeviceId>,
}

/// What an action did.
#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    Deleted { service: ServiceId, seq: u64 },
    Pruned { service: ServiceId, removed: usize },
}

pub struct Hub<S: Store, C: Credentials> {
    /// This server's name: the audience actions and sign-ins must name.
    name: String,
    store: S,
    creds: C,
    crypto: RustCrypto,
    storage: MemoryStorage,
    groups: HashMap<GroupId, Tracked>,
    guard: ReplayGuard,
}

fn parse(bytes: &[u8]) -> Result<MlsMessageBodyIn> {
    Ok(MlsMessageIn::tls_deserialize_exact(bytes).map_err(mls)?.extract())
}

impl<S: Store, C: Credentials> Hub<S, C> {
    pub fn new(name: &str, store: S, creds: C) -> Hub<S, C> {
        Hub {
            name: name.to_string(),
            store,
            creds,
            crypto: RustCrypto::default(),
            storage: MemoryStorage::default(),
            groups: HashMap::new(),
            guard: ReplayGuard::default(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn group(&self, id: GroupId) -> Result<&Tracked> {
        self.groups.get(&id).ok_or(Error::NotAllowed("no such group".into()))
    }

    // === KeyPackages ===

    /// Store a KeyPackage `device` published for itself.
    pub fn publish_key_package(&mut self, device: &DeviceId, bytes: Vec<u8>) -> Result<()> {
        let kp = KeyPackageIn::tls_deserialize_exact(bytes.as_slice())
            .map_err(mls)?
            .validate(&self.crypto, ProtocolVersion::Mls10)
            .map_err(mls)?;
        let leaf = kp.leaf_node();
        if leaf.signature_key().as_slice() != device.as_slice() {
            return Err(Error::NotAllowed("a device may only publish its own KeyPackages".into()));
        }
        self.creds.verify(leaf.credential(), leaf.signature_key().as_slice())?;
        self.store.put_key_package(device, bytes);
        Ok(())
    }

    /// Hand out one of `device`'s KeyPackages. Each goes to one caller.
    pub fn claim_key_package(&mut self, device: &DeviceId) -> Option<Vec<u8>> {
        self.store.take_key_package(device)
    }

    pub fn key_packages_left(&self, device: &DeviceId) -> usize {
        self.store.key_packages_left(device)
    }

    // === Groups ===

    /// Start following a new group from its creator's GroupInfo. The
    /// caller must be the group's only member.
    pub fn create(&mut self, device: &DeviceId, group_info: &[u8]) -> Result<GroupId> {
        let MlsMessageBodyIn::GroupInfo(info) = parse(group_info)? else {
            return Err(Error::Malformed("group info"));
        };
        let id: GroupId = info.group_id().as_slice().try_into().map_err(|_| Error::Malformed("group id"))?;
        if self.groups.contains_key(&id) {
            return Err(Error::NotAllowed("that group id is taken".into()));
        }
        let tree = info.extensions().ratchet_tree().ok_or(Error::Malformed("group info without a ratchet tree"))?.ratchet_tree().clone();
        let (public, _) = PublicGroup::from_external(&self.crypto, &self.storage, tree, info, ProposalStore::new()).map_err(mls)?;
        let mut t = Tracked { public, services: Services::new(), policy: Policy::new(""), rosters: BTreeMap::new() };
        t.refresh(&self.creds)?;
        let members = t.devices();
        if members != [device.clone()] {
            return Err(Error::NotAllowed("a new group must have its creator as its only member".into()));
        }
        self.groups.insert(id, t);
        Ok(id)
    }

    pub fn epoch(&self, group: GroupId) -> Result<u64> {
        Ok(self.group(group)?.epoch())
    }

    pub fn members(&self, group: GroupId) -> Result<Vec<DeviceId>> {
        Ok(self.group(group)?.devices())
    }

    pub fn is_member(&self, group: GroupId, device: &[u8]) -> bool {
        self.groups.get(&group).is_some_and(|t| t.member(device).is_some())
    }

    pub fn services(&self, group: GroupId) -> Result<&Services> {
        Ok(&self.group(group)?.services)
    }

    pub fn policy(&self, group: GroupId) -> Result<&Policy> {
        Ok(&self.group(group)?.policy)
    }

    /// Take a commit, if it's for the current epoch and the group's policy
    /// allows it. `welcome`, for a commit that adds devices, waits for them.
    pub fn commit(&mut self, group: GroupId, bytes: &[u8], welcome: Option<Vec<u8>>) -> Result<Accepted> {
        let MlsMessageBodyIn::PublicMessage(msg) = parse(bytes)? else {
            return Err(Error::Malformed("commits travel as public messages"));
        };
        let creds = &self.creds;
        let crypto = &self.crypto;
        let t = self.groups.get_mut(&group).ok_or(Error::NotAllowed("no such group".into()))?;
        let epoch = t.epoch();
        if msg.epoch().as_u64() != epoch {
            return Err(Error::NotAllowed(format!("stale commit: the group is at epoch {epoch}")));
        }
        let processed = t.public.process_message(crypto, msg).map_err(mls)?;
        let Sender::Member(leaf) = processed.sender() else {
            return Err(Error::NotAllowed("only members may commit".into()));
        };
        let committer = t.holder(epoch, leaf.u32())?.0.clone();
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(Error::Malformed("not a commit"));
        };
        let change = describe_commit(&committer, &staged, |leaf| Ok(t.holder(epoch, leaf)?.0.clone()), creds)?;
        t.policy.check(&change)?;
        let notify = t.devices();
        t.public.merge_commit(&self.storage, *staged).map_err(mls)?;
        t.refresh(creds)?;
        let welcomed: Vec<DeviceId> = t.devices().into_iter().filter(|d| !notify.contains(d)).collect();
        let epoch = t.epoch();
        match (welcome, welcomed.is_empty()) {
            (Some(w), false) => welcomed.iter().for_each(|d| self.store.put_welcome(d, w.clone())),
            (None, false) => return Err(Error::Malformed("a commit that adds devices needs its welcome")),
            _ => {}
        }
        Ok(Accepted { epoch, notify, welcomed })
    }

    /// Welcomes waiting for `device`.
    pub fn welcomes(&mut self, device: &DeviceId) -> Vec<Vec<u8>> {
        self.store.take_welcomes(device)
    }

    // === Records ===

    /// Store a record, if its writer was a member when it was written, its
    /// signature is theirs, and the policy lets them write it. Returns its
    /// server sequence.
    pub fn post(&mut self, group: GroupId, bytes: Vec<u8>) -> Result<u64> {
        let t = self.group(group)?;
        let parsed = record::parse(&bytes)?;
        let h = parsed.header;
        if h.group != group {
            return Err(Error::Malformed("record for another group"));
        }
        let svc = t.services.get(h.service)?;
        if svc.kind != Kind::Stateful {
            return Err(Error::NotAllowed(format!("{} does not store records", svc.name)));
        }
        if h.epoch > t.epoch() {
            return Err(Error::NotAllowed("record from an epoch that hasn't happened".into()));
        }
        let (user, key) = t.holder(h.epoch, h.sender)?;
        record::verify(&parsed, key)?;
        let rules = t.policy.service(h.service);
        let role = t.policy.role_of(user);
        let allowed = match h.kind {
            RecordKind::Data => rules.may_send.contains(role),
            RecordKind::Checkpoint => rules.may_checkpoint.contains(role),
            RecordKind::Link | RecordKind::Cosign => true,
        };
        if !allowed {
            return Err(Error::NotAllowed(format!("{user} may not write {:?} records on {}", h.kind, svc.name)));
        }
        Ok(self.store.append(group, h.service, bytes))
    }

    pub fn records(&self, group: GroupId, service: ServiceId, after: u64, limit: usize) -> Result<Vec<(u64, Vec<u8>)>> {
        self.group(group)?.services.get(service)?;
        Ok(self.store.records(group, service, after, limit.min(1000)))
    }

    // === Reliable messages ===

    /// Check a reliable realtime message and return who to deliver it to.
    /// Its sender is hidden from the server, so `may_send` for these is
    /// enforced by members.
    pub fn relay(&self, group: GroupId, bytes: &[u8]) -> Result<Vec<DeviceId>> {
        let MlsMessageBodyIn::PrivateMessage(msg) = parse(bytes)? else {
            return Err(Error::Malformed("messages travel as private messages"));
        };
        let t = self.group(group)?;
        if msg.group_id().as_slice() != group || msg.epoch().as_u64() != t.epoch() {
            return Err(Error::NotAllowed("message for another group or epoch".into()));
        }
        let aad = msg.aad();
        let service = u16::from_be_bytes(aad.get(..2).and_then(|a| a.try_into().ok()).ok_or(Error::Malformed("message service"))?);
        if t.services.get(service)?.kind != Kind::Reliable {
            return Err(Error::NotAllowed(format!("service {service} is not reliable realtime")));
        }
        Ok(t.devices())
    }

    // === Actions ===

    /// Act on a signed action addressed to this server.
    pub fn act(&mut self, bytes: &[u8], now: u64) -> Result<Done> {
        let (action, issuer) = action::verify(bytes, &self.name, now, &mut self.guard)?;
        let credential = Credential::tls_deserialize_exact(issuer.credential.as_slice()).map_err(mls)?;
        let user = self.creds.verify(&credential, &issuer.signature_key)?;
        let (group, epoch) = action.group.ok_or(Error::Malformed("group action without a group"))?;
        let t = self.group(group)?;
        if epoch != t.epoch() || t.member(&issuer.signature_key) != Some(&user) {
            return Err(Error::NotAllowed("the issuer isn't a member of the group now".into()));
        }
        let body = action.body.as_slice();
        let field = |at: usize, n: usize| body.get(at..at + n).ok_or(Error::Malformed("action body"));
        let service = u16::from_be_bytes(field(0, 2)?.try_into().unwrap());
        let seq = u64::from_be_bytes(field(2, 8)?.try_into().unwrap());
        if body.len() != 10 {
            return Err(Error::Malformed("action body"));
        }
        let role = t.policy.role_of(&user);
        let moderator = t.policy.may_remove.contains(role);
        let done = match action.kind.as_str() {
            // Body: service ‖ server sequence of the record.
            "record.delete" => {
                let rec = self.store.record(group, service, seq).ok_or(Error::NotAllowed("no such record".into()))?;
                let h = record::parse(&rec)?.header;
                let author = &t.holder(h.epoch, h.sender)?.0;
                if author != &user && !moderator {
                    return Err(Error::NotAllowed(format!("{user} may not delete {author}'s record")));
                }
                self.store.remove(group, service, seq, &|s, _| s == seq);
                Done::Deleted { service, seq }
            }
            // Body: service ‖ the server sequence a trusted checkpoint covers
            // through. The server can't read checkpoints or cosigns, so it
            // takes a prune from someone the checkpoint rule already trusts
            // on their own, or a moderator.
            "records.prune" => {
                let rules = t.policy.service(service);
                if !(rules.trust_checkpoint.or_any_of.contains(role) || moderator) {
                    return Err(Error::NotAllowed(format!("{user} may not prune")));
                }
                let data = |_: u64, r: &[u8]| record::parse(r).is_ok_and(|p| p.header.kind == RecordKind::Data);
                let removed = self.store.remove(group, service, seq, &data);
                Done::Pruned { service, removed }
            }
            other => return Err(Error::NotAllowed(format!("unknown action {other}"))),
        };
        self.store.tombstone(group, bytes.to_vec());
        Ok(done)
    }

    /// Signed actions that removed records, for members to audit.
    pub fn tombstones(&self, group: GroupId) -> Vec<Vec<u8>> {
        self.store.tombstones(group)
    }

    /// The role `device`'s person holds in `group`, if it's a member.
    pub fn role(&self, group: GroupId, device: &[u8]) -> Option<Role> {
        let t = self.groups.get(&group)?;
        t.member(device).map(|u| t.policy.role_of(u))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use sigil::action::Action;
    use sigil::group::Unverified;
    use sigil::policy::ServicePolicy;
    use sigil::{Device, Group, Incoming, Service};

    const NOTES: ServiceId = 1;
    const PRESENCE: ServiceId = 2;
    const HUB: &str = "sigil.example";

    fn services() -> Services {
        Services::new()
            .with(NOTES, Service::new("notes", Kind::Stateful, 1).unwrap())
            .unwrap()
            .with(PRESENCE, Service::new("presence", Kind::Reliable, 1).unwrap())
            .unwrap()
    }

    fn policy() -> Policy {
        let mut p = Policy::new("alice");
        p.services.insert(NOTES, ServicePolicy::default());
        p
    }

    struct World {
        hub: Hub<MemoryStore, Unverified>,
        alice: Device,
        a: Group,
        bob: Device,
        b: Group,
        id: GroupId,
    }

    /// Alice creates a group through the hub and adds Bob through it.
    fn world() -> World {
        let mut hub = Hub::new(HUB, MemoryStore::default(), Unverified);
        let alice = Device::new("alice").unwrap();
        let bob = Device::new("bob").unwrap();
        hub.publish_key_package(&bob.public_key(), bob.key_package().unwrap()).unwrap();

        let mut a = Group::create(&alice, [5; 16], services(), policy(), &Unverified).unwrap();
        let id = hub.create(&alice.public_key(), &a.group_info(&alice).unwrap()).unwrap();
        let kp = hub.claim_key_package(&bob.public_key()).unwrap();
        let c = a.add(&alice, &[kp], &Unverified).unwrap();
        let accepted = hub.commit(id, &c.commit, c.welcome).unwrap();
        assert_eq!(accepted, Accepted { epoch: 1, notify: vec![alice.public_key()], welcomed: vec![bob.public_key()] });
        a.confirm(&alice, &Unverified).unwrap();
        let welcome = hub.welcomes(&bob.public_key()).pop().unwrap();
        let b = Group::join(&bob, &welcome, &Unverified).unwrap();
        World { hub, alice, a, bob, b, id }
    }

    #[test]
    fn a_group_runs_through_the_hub() {
        let mut w = world();
        assert_eq!(w.hub.epoch(w.id).unwrap(), 1);
        assert!(w.hub.is_member(w.id, &w.bob.public_key()));
        assert_eq!(w.hub.claim_key_package(&w.bob.public_key()), None, "each KeyPackage is handed out once");

        let rec = w.a.seal(&w.alice, NOTES, b"hi bob").unwrap();
        let seq = w.hub.post(w.id, rec).unwrap();
        let fetched = w.hub.records(w.id, NOTES, 0, 10).unwrap();
        assert_eq!(fetched[0].0, seq);
        assert_eq!(w.b.open(&fetched[0].1, &Unverified).unwrap().body, b"hi bob");

        let m = w.b.send(&w.bob, PRESENCE, b"typing").unwrap();
        assert_eq!(w.hub.relay(w.id, &m).unwrap().len(), 2);
        assert!(matches!(w.a.process(&w.alice, &m, &Unverified).unwrap(), Incoming::Message { .. }));
    }

    #[test]
    fn the_hub_refuses_what_members_would() {
        let mut w = world();
        // A commit the policy forbids: Bob, a member, adding Carol.
        let carol = Device::new("carol").unwrap();
        assert!(w.b.add(&w.bob, &[carol.key_package().unwrap()], &Unverified).is_err());
        // A stale commit: two commits for the same epoch, the second refused.
        let c1 = w.a.rotate(&w.alice).unwrap();
        let c2 = w.b.rotate(&w.bob).unwrap();
        w.hub.commit(w.id, &c1.commit, None).unwrap();
        assert!(matches!(w.hub.commit(w.id, &c2.commit, None), Err(Error::NotAllowed(_))));
        w.a.confirm(&w.alice, &Unverified).unwrap();
        w.b.discard(&w.bob).unwrap();
        assert_eq!(w.b.process(&w.bob, &c1.commit, &Unverified).unwrap(), Incoming::Epoch(2));
        // A record signed by someone other than the leaf it names.
        let mut forged = w.b.seal(&w.bob, NOTES, b"x").unwrap();
        let n = forged.len();
        forged[n - 1] ^= 1;
        assert_eq!(w.hub.post(w.id, forged), Err(Error::Unauthentic));
        // A KeyPackage published for someone else.
        assert!(w.hub.publish_key_package(&w.alice.public_key(), carol.key_package().unwrap()).is_err());
    }

    fn action(w: &World, device: &Device, kind: &str, body: Vec<u8>, now: u64) -> Vec<u8> {
        Action {
            kind: kind.into(),
            audience: HUB.into(),
            group: Some((w.id, w.hub.epoch(w.id).unwrap())),
            body,
            nonce: [now as u8; 16],
            issued_at: now,
            expires_at: now + 60,
        }
        .sign(&device.issuer().unwrap(), device.signer())
        .unwrap()
    }

    fn at(service: ServiceId, seq: u64) -> Vec<u8> {
        [service.to_be_bytes().as_slice(), &seq.to_be_bytes()].concat()
    }

    #[test]
    fn authors_and_moderators_delete_records() {
        let mut w = world();
        let rec = w.b.seal(&w.bob, NOTES, b"bob's").unwrap();
        let seq = w.hub.post(w.id, rec).unwrap();
        let rec = w.a.seal(&w.alice, NOTES, b"alice's").unwrap();
        let alices = w.hub.post(w.id, rec).unwrap();
        // Bob may delete his own record, not Alice's.
        let no = action(&w, &w.bob, "record.delete", at(NOTES, alices), 10);
        assert!(matches!(w.hub.act(&no, 10), Err(Error::NotAllowed(_))));
        let yes = action(&w, &w.bob, "record.delete", at(NOTES, seq), 11);
        assert_eq!(w.hub.act(&yes, 11).unwrap(), Done::Deleted { service: NOTES, seq });
        assert_eq!(w.hub.act(&yes, 12), Err(Error::Replay));
        // Alice, the owner, may delete anyone's.
        let rec = w.b.seal(&w.bob, NOTES, b"again").unwrap();
        let seq = w.hub.post(w.id, rec).unwrap();
        let mod_ = action(&w, &w.alice, "record.delete", at(NOTES, seq), 13);
        assert!(w.hub.act(&mod_, 13).is_ok());
        assert_eq!(w.hub.records(w.id, NOTES, 0, 10).unwrap().len(), 1);
        assert_eq!(w.hub.tombstones(w.id).len(), 2);
    }

    #[test]
    fn pruning_drops_only_data_records() {
        let mut w = world();
        for i in 0..3 {
            let rec = w.a.seal(&w.alice, NOTES, &[i]).unwrap();
            w.hub.post(w.id, rec).unwrap();
        }
        let link = w.a.link(&w.alice, NOTES).unwrap().unwrap();
        w.hub.post(w.id, link).unwrap();
        let prune = action(&w, &w.alice, "records.prune", at(NOTES, 4), 20);
        assert_eq!(w.hub.act(&prune, 20).unwrap(), Done::Pruned { service: NOTES, removed: 3 });
        let left = w.hub.records(w.id, NOTES, 0, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(record::parse(&left[0].1).unwrap().header.kind, RecordKind::Link);
        // Bob is a member, not trusted by the checkpoint rule alone.
        let no = action(&w, &w.bob, "records.prune", at(NOTES, 4), 21);
        assert!(matches!(w.hub.act(&no, 21), Err(Error::NotAllowed(_))));
    }
}
