//! A Sigil group over openmls (spec §4).
//!
//! [`Device`] is one installation: its MLS signature key and credential.
//! [`Group`] is one MLS group seen from one device, plus what Sigil keeps
//! alongside: the services table and policy (both GroupContext extensions),
//! the state key of every epoch this device has been in, and who held each
//! leaf in each epoch, so a stored record's signature can be checked after
//! the epoch is gone.
//!
//! # A commit's life
//!
//! ```text
//! committer: add / remove / change_policy ──▶ Commit { commit, welcome }
//!            server accepts it (first commit for the epoch wins)
//!            confirm()  ──▶ link records to store       (or discard() if refused)
//! members:   process(commit) ──▶ policy checked ──▶ merged, or refused
//! newcomer:  join(welcome)
//! ```
//!
//! A commit the policy doesn't allow is refused by every member, so the
//! epoch it would start never forms. The server can't add anyone: it has no
//! signature key in the group, and members verify every credential.

use std::collections::{BTreeMap, HashMap};

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::keys::{self, Exporter, StateKey};
use crate::policy::{Change, History, Policy, Proposal as Change1};
use crate::record::{self, Header, RecordKind};
use crate::service::{Kind, ServiceId, Services};
use crate::{Error, GroupId, Result, UserId};

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;
/// GroupContext extension carrying the [`Services`] table.
pub const EXT_SERVICES: u16 = 0xF5A1;
/// GroupContext extension carrying the [`Policy`].
pub const EXT_POLICY: u16 = 0xF5A2;

/// Handshake messages go in the clear, so the server sees the roster and
/// policy it enforces; application messages are always encrypted.
const WIRE: WireFormatPolicy = MIXED_PLAINTEXT_WIRE_FORMAT_POLICY;

fn mls<E: std::fmt::Debug>(e: E) -> Error {
    Error::Mls(format!("{e:?}"))
}

/// Turns a credential into the person it names, or refuses it. In
/// production this checks an identity-service certificate (spec §4.2).
pub trait Credentials {
    fn verify(&self, credential: &Credential, signature_key: &[u8]) -> Result<UserId>;
}

/// Takes a basic credential's identity at its word. For tests and local
/// development only: anyone can claim any name with it.
pub struct Unverified;

impl Credentials for Unverified {
    fn verify(&self, credential: &Credential, _: &[u8]) -> Result<UserId> {
        let basic = BasicCredential::try_from(credential.clone()).map_err(|e| Error::Credential(format!("{e:?}")))?;
        String::from_utf8(basic.identity().to_vec()).map_err(|_| Error::Credential("identity is not text".into()))
    }
}

fn capabilities() -> Capabilities {
    Capabilities::builder()
        .extensions(vec![ExtensionType::Unknown(EXT_SERVICES), ExtensionType::Unknown(EXT_POLICY)])
        .build()
}

/// One installation: its MLS state, signature key and credential.
pub struct Device {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    credential: CredentialWithKey,
    user: UserId,
}

impl Device {
    /// A device for `user`, with a fresh signature key.
    ///
    /// Its credential is a basic one naming `user`; replacing it with an
    /// identity-service certificate is the identity spec's work.
    pub fn new(user: &str) -> Result<Device> {
        let provider = OpenMlsRustCrypto::default();
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm()).map_err(mls)?;
        signer.store(provider.storage()).map_err(mls)?;
        let credential = CredentialWithKey {
            credential: BasicCredential::new(user.as_bytes().to_vec()).into(),
            signature_key: signer.to_public_vec().into(),
        };
        Ok(Device { provider, signer, credential, user: user.to_string() })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// The device's signature public key: how servers name it.
    pub fn public_key(&self) -> Vec<u8> {
        self.signer.to_public_vec()
    }

    /// Sign `payload` with the device key, labelled so the signature can't
    /// be mistaken for one over a record, action or MLS structure.
    pub fn sign(&self, label: &str, payload: &[u8]) -> Result<Vec<u8>> {
        use openmls_traits::signatures::Signer as _;
        self.signer.sign(&[label.as_bytes(), b"\0", payload].concat()).map_err(mls)
    }

    /// The device's credential with its key, serialized: the issuer of an
    /// [`action`](crate::action).
    pub fn issuer(&self) -> Result<crate::action::Issuer> {
        Ok(crate::action::Issuer {
            credential: self.credential.credential.tls_serialize_detached().map_err(mls)?,
            signature_key: self.public_key(),
        })
    }

    /// The device's signer, for [`action::Action::sign`](crate::action::Action::sign).
    pub fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }

    /// A fresh KeyPackage, for the server to hand to whoever adds this
    /// device. Each is good for one add.
    pub fn key_package(&self) -> Result<Vec<u8>> {
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(capabilities())
            .build(CIPHERSUITE, &self.provider, &self.signer, self.credential.clone())
            .map_err(mls)?;
        bundle.key_package().tls_serialize_detached().map_err(mls)
    }
}

/// An outgoing commit: send `commit` to the server, and `welcome` to the
/// newcomers once the server has accepted it.
#[derive(Debug)]
pub struct Commit {
    pub commit: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
}

/// What an incoming message turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    /// The group moved to a new epoch.
    Epoch(u64),
    /// This device was removed. The group is no longer usable.
    Removed,
    /// A reliable realtime message.
    Message { service: ServiceId, sender: UserId, bytes: Vec<u8> },
}

/// An opened record.
#[derive(Debug, PartialEq, Eq)]
pub struct Opened {
    pub header: Header,
    pub sender: UserId,
    pub body: Vec<u8>,
}

/// Who held a leaf in an epoch.
#[derive(Debug, Clone)]
struct Holder {
    user: UserId,
    signature_key: Vec<u8>,
    /// Their MLS credential, serialized, so a link can carry it on.
    credential: Vec<u8>,
}

/// Who held which leaf in an epoch.
type Roster = HashMap<u32, Holder>;

pub struct Group {
    mls: MlsGroup,
    id: GroupId,
    services: Services,
    policy: Policy,
    /// The state key of every (stateful service, epoch) this device can read.
    keys: HashMap<(ServiceId, u64), StateKey>,
    /// From each epoch on, until the next entry.
    rosters: BTreeMap<u64, Roster>,
    /// The next seq this device writes, per service.
    seqs: HashMap<ServiceId, u64>,
    /// Whether the pending commit (ours, not yet confirmed) adds anyone.
    pending_adds: bool,
}

struct Epoch<'a>(&'a MlsGroup, &'a OpenMlsRustCrypto);

impl Exporter for Epoch<'_> {
    fn export(&self, label: &str, context: &[u8], len: usize) -> Result<Vec<u8>> {
        self.0.export_secret(self.1.crypto(), label, context, len).map_err(mls)
    }
}

fn extensions(services: &Services, policy: &Policy) -> Result<Extensions<GroupContext>> {
    let ours = [ExtensionType::Unknown(EXT_SERVICES), ExtensionType::Unknown(EXT_POLICY)];
    Extensions::from_vec(vec![
        Extension::Unknown(EXT_SERVICES, UnknownExtension(services.encode())),
        Extension::Unknown(EXT_POLICY, UnknownExtension(policy.encode())),
        Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(&ours, &[], &[])),
    ])
    .map_err(mls)
}

/// What a staged commit does, as the policy needs to see it. `user_at`
/// names the member at a leaf in the commit's epoch. Shared with servers,
/// which check commits against the same policy members do.
pub fn describe_commit(
    committer: &str,
    staged: &StagedCommit,
    user_at: impl Fn(u32) -> Result<UserId>,
    creds: &impl Credentials,
) -> Result<Change> {
    let mut proposals = Vec::new();
    for q in staged.queued_proposals() {
        let author = match q.sender() {
            Sender::Member(leaf) => user_at(leaf.u32())?,
            _ => return Err(Error::NotAllowed("proposals from outside the group".into())),
        };
        let p = match q.proposal() {
            Proposal::Add(add) => {
                let leaf = add.key_package().leaf_node();
                Change1::Add(creds.verify(leaf.credential(), leaf.signature_key().as_slice())?)
            }
            Proposal::Remove(rm) => Change1::Remove(user_at(rm.removed().u32())?),
            Proposal::GroupContextExtensions(gce) => {
                // Refuse a change that would leave the group without
                // readable Sigil extensions.
                read_extensions(gce.extensions())?;
                Change1::Extensions
            }
            Proposal::Update(_) => continue,
            other => return Err(Error::NotAllowed(format!("proposal {:?} is not used by Sigil", other.proposal_type()))),
        };
        proposals.push((author, p));
    }
    Ok(Change { committer: committer.to_string(), proposals })
}

/// A group's services and policy, from its GroupContext extensions.
pub fn read_extensions(ext: &Extensions<GroupContext>) -> Result<(Services, Policy)> {
    let find = |t: u16| match ext.unknown(t) {
        Some(UnknownExtension(bytes)) => Ok(bytes.as_slice()),
        None => Err(Error::Malformed("group without Sigil extensions")),
    };
    Ok((Services::decode(find(EXT_SERVICES)?)?, Policy::decode(find(EXT_POLICY)?)?))
}

fn parse_message(bytes: &[u8]) -> Result<MlsMessageBodyIn> {
    Ok(MlsMessageIn::tls_deserialize_exact(bytes).map_err(mls)?.extract())
}

impl Group {
    /// Start a group with `device` as its only member. The policy should
    /// name `device`'s user as owner, or nobody can change it.
    pub fn create(device: &Device, id: GroupId, services: Services, policy: Policy, creds: &impl Credentials) -> Result<Group> {
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .wire_format_policy(WIRE)
            .use_ratchet_tree_extension(true)
            .capabilities(capabilities())
            .with_group_context_extensions(extensions(&services, &policy)?)
            .build();
        let mls = MlsGroup::new_with_group_id(&device.provider, &device.signer, &config, openmls::group::GroupId::from_slice(&id), device.credential.clone())
            .map_err(mls)?;
        Group::start(device, mls, creds)
    }

    /// Join from a Welcome. Every member's credential is verified first.
    pub fn join(device: &Device, welcome: &[u8], creds: &impl Credentials) -> Result<Group> {
        let MlsMessageBodyIn::Welcome(welcome) = parse_message(welcome)? else {
            return Err(Error::Malformed("welcome"));
        };
        let config = MlsGroupJoinConfig::builder().wire_format_policy(WIRE).use_ratchet_tree_extension(true).build();
        let mls = StagedWelcome::new_from_welcome(&device.provider, &config, welcome, None)
            .map_err(mls)?
            .into_group(&device.provider)
            .map_err(mls)?;
        Group::start(device, mls, creds)
    }

    fn start(device: &Device, mls: MlsGroup, creds: &impl Credentials) -> Result<Group> {
        let id: GroupId = mls.group_id().as_slice().try_into().map_err(|_| Error::Malformed("group id"))?;
        let (services, policy) = read_extensions(mls.extensions())?;
        let mut g = Group { mls, id, services, policy, keys: HashMap::new(), rosters: BTreeMap::new(), seqs: HashMap::new(), pending_adds: false };
        g.entered_epoch(device, creds)?;
        Ok(g)
    }

    pub fn id(&self) -> GroupId {
        self.id
    }

    pub fn epoch(&self) -> u64 {
        self.mls.epoch().as_u64()
    }

    pub fn services(&self) -> &Services {
        &self.services
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The people in the group now, with each one's leaves.
    pub fn people(&self) -> BTreeMap<UserId, Vec<u32>> {
        let mut out: BTreeMap<UserId, Vec<u32>> = BTreeMap::new();
        for (leaf, h) in self.roster_at(self.epoch()).into_iter().flatten() {
            out.entry(h.user.clone()).or_default().push(*leaf);
        }
        out
    }

    fn roster_at(&self, epoch: u64) -> Option<&Roster> {
        self.rosters.range(..=epoch).next_back().map(|(_, r)| r)
    }

    fn user_at(&self, epoch: u64, leaf: u32) -> Result<&Holder> {
        self.roster_at(epoch)
            .and_then(|r| r.get(&leaf))
            .ok_or_else(|| Error::Credential(format!("no member at leaf {leaf} in epoch {epoch}")))
    }

    fn own_leaf(&self) -> u32 {
        self.mls.own_leaf_index().u32()
    }

    /// Everything that follows a new epoch: policy and services from the
    /// GroupContext, the roster, and a state key per stateful service.
    fn entered_epoch(&mut self, device: &Device, creds: &impl Credentials) -> Result<()> {
        let (services, policy) = read_extensions(self.mls.extensions())?;
        let mut roster = Roster::new();
        for m in self.mls.members() {
            let user = creds.verify(&m.credential, &m.signature_key)?;
            let credential = m.credential.tls_serialize_detached().map_err(mls)?;
            roster.insert(m.index.u32(), Holder { user, signature_key: m.signature_key, credential });
        }
        let epoch = self.epoch();
        for (id, svc) in services.iter().filter(|(_, s)| s.kind == Kind::Stateful) {
            let key = StateKey::derive(&Epoch(&self.mls, &device.provider), svc)?;
            self.keys.insert((id, epoch), key);
        }
        self.services = services;
        self.policy = policy;
        self.rosters.insert(epoch, roster);
        Ok(())
    }

    // === Committing ===

    /// Add the devices whose KeyPackages these are. Their credentials are
    /// verified, and the policy checked, before anything is sent.
    pub fn add(&mut self, device: &Device, key_packages: &[Vec<u8>], creds: &impl Credentials) -> Result<Commit> {
        let mut kps = Vec::new();
        let mut proposals = Vec::new();
        for bytes in key_packages {
            let kp = KeyPackageIn::tls_deserialize_exact(bytes.as_slice())
                .map_err(mls)?
                .validate(device.provider.crypto(), ProtocolVersion::Mls10)
                .map_err(mls)?;
            let leaf = kp.leaf_node();
            let user = creds.verify(leaf.credential(), leaf.signature_key().as_slice())?;
            proposals.push((device.user.clone(), Change1::Add(user)));
            kps.push(kp);
        }
        self.policy.check(&Change { committer: device.user.clone(), proposals })?;
        let (commit, welcome, _) = self.mls.add_members(&device.provider, &device.signer, &kps).map_err(mls)?;
        self.pending_adds = true;
        Ok(Commit { commit: commit.tls_serialize_detached().map_err(mls)?, welcome: Some(welcome.tls_serialize_detached().map_err(mls)?) })
    }

    /// Remove every device of each of `users`.
    pub fn remove(&mut self, device: &Device, users: &[&str]) -> Result<Commit> {
        let roster = self.roster_at(self.epoch()).cloned().unwrap_or_default();
        let leaves: Vec<LeafNodeIndex> =
            roster.iter().filter(|(_, h)| users.contains(&h.user.as_str())).map(|(leaf, _)| LeafNodeIndex::new(*leaf)).collect();
        if leaves.is_empty() {
            return Err(Error::NotAllowed("none of them are in the group".into()));
        }
        let proposals = leaves.iter().map(|l| (device.user.clone(), Change1::Remove(roster[&l.u32()].user.clone()))).collect();
        self.policy.check(&Change { committer: device.user.clone(), proposals })?;
        let (commit, welcome, _) = self.mls.remove_members(&device.provider, &device.signer, &leaves).map_err(mls)?;
        self.pending_adds = false;
        Ok(Commit { commit: commit.tls_serialize_detached().map_err(mls)?, welcome: welcome.map(|w| w.tls_serialize_detached()).transpose().map_err(mls)? })
    }

    /// Replace the policy and services table.
    pub fn change(&mut self, device: &Device, services: Services, policy: Policy) -> Result<Commit> {
        self.policy.check(&Change { committer: device.user.clone(), proposals: vec![(device.user.clone(), Change1::Extensions)] })?;
        let (commit, welcome, _) =
            self.mls.update_group_context_extensions(&device.provider, extensions(&services, &policy)?, &device.signer).map_err(mls)?;
        self.pending_adds = false;
        Ok(Commit { commit: commit.tls_serialize_detached().map_err(mls)?, welcome: welcome.map(|w| w.tls_serialize_detached()).transpose().map_err(mls)? })
    }

    /// A commit that only refreshes this device's keys: what `rotate_every`
    /// asks for.
    pub fn rotate(&mut self, device: &Device) -> Result<Commit> {
        let bundle = self.mls.self_update(&device.provider, &device.signer, LeafNodeParameters::default()).map_err(mls)?;
        self.pending_adds = false;
        Ok(Commit { commit: bundle.commit().tls_serialize_detached().map_err(mls)?, welcome: None })
    }

    /// The server accepted our commit: move to the new epoch. Returns the
    /// `link` records to store (spec §5.3).
    pub fn confirm(&mut self, device: &Device, creds: &impl Credentials) -> Result<Vec<Vec<u8>>> {
        self.mls.merge_pending_commit(&device.provider).map_err(mls)?;
        self.entered_epoch(device, creds)?;
        let adds = std::mem::take(&mut self.pending_adds);
        let ids: Vec<ServiceId> = self.services.iter().filter(|(_, s)| s.kind == Kind::Stateful).map(|(id, _)| id).collect();
        let mut links = Vec::new();
        for id in ids {
            if adds && self.policy.service(id).history == History::None {
                continue;
            }
            if let Some(link) = self.link(device, id)? {
                links.push(link);
            }
        }
        Ok(links)
    }

    /// The server refused our commit (another got there first).
    pub fn discard(&mut self, device: &Device) -> Result<()> {
        self.pending_adds = false;
        self.mls.clear_pending_commit(device.provider.storage()).map_err(mls)
    }

    /// A `link` record from the current epoch's key to the previous one's,
    /// if this device holds both. Any member may post a missing link.
    pub fn link(&mut self, device: &Device, service: ServiceId) -> Result<Option<Vec<u8>>> {
        let epoch = self.epoch();
        let Some(prev) = epoch.checked_sub(1).and_then(|p| self.keys.get(&(service, p))).cloned() else {
            return Ok(None);
        };
        let roster: Vec<record::RosterEntry> = self
            .roster_at(epoch - 1)
            .into_iter()
            .flatten()
            .map(|(leaf, h)| (*leaf, h.credential.clone(), h.signature_key.clone()))
            .collect();
        let body = record::link_body(epoch - 1, &prev, &roster);
        self.write(device, service, RecordKind::Link, &body).map(Some)
    }

    // === Receiving ===

    /// Process a commit or a reliable message from the server.
    pub fn process(&mut self, device: &Device, bytes: &[u8], creds: &impl Credentials) -> Result<Incoming> {
        let message: ProtocolMessage = match parse_message(bytes)? {
            MlsMessageBodyIn::PublicMessage(m) => m.into(),
            MlsMessageBodyIn::PrivateMessage(m) => m.into(),
            _ => return Err(Error::Malformed("protocol message")),
        };
        let processed = self.mls.process_message(&device.provider, message).map_err(mls)?;
        let sender = match processed.sender() {
            Sender::Member(leaf) => leaf.u32(),
            _ => return Err(Error::NotAllowed("only members may send".into())),
        };
        let epoch = self.epoch();
        let author = self.user_at(epoch, sender)?.user.clone();
        let aad = processed.aad().to_vec();
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                let service = u16::from_be_bytes(aad.get(..2).and_then(|a| a.try_into().ok()).ok_or(Error::Malformed("message service"))?);
                if self.services.get(service)?.kind != Kind::Reliable {
                    return Err(Error::NotAllowed(format!("service {service} is not reliable realtime")));
                }
                if !self.policy.service(service).may_send.contains(self.policy.role_of(&author)) {
                    return Err(Error::NotAllowed(format!("{author} may not send on {service}")));
                }
                Ok(Incoming::Message { service, sender: author, bytes: app.into_bytes() })
            }
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                let change = describe_commit(&author, &staged, |leaf| Ok(self.user_at(epoch, leaf)?.user.clone()), creds)?;
                self.policy.check(&change)?;
                let removed = staged.self_removed();
                self.mls.merge_staged_commit(&device.provider, *staged).map_err(mls)?;
                if removed {
                    return Ok(Incoming::Removed);
                }
                self.entered_epoch(device, creds)?;
                Ok(Incoming::Epoch(self.epoch()))
            }
            _ => Err(Error::NotAllowed("only commits and application messages are accepted".into())),
        }
    }

    // === Sending ===

    fn write(&mut self, device: &Device, service: ServiceId, kind: RecordKind, body: &[u8]) -> Result<Vec<u8>> {
        let epoch = self.epoch();
        let sender = self.own_leaf();
        let key = self.keys.get(&(service, epoch)).ok_or(Error::NoKey { service, epoch })?;
        let seq = self.seqs.entry(service).or_insert(0);
        let header = Header { kind, group: self.id, epoch, service, sender, seq: *seq };
        let out = record::seal(&header, key, body, &device.signer)?;
        *seq += 1;
        Ok(out)
    }

    fn may_send(&self, device: &Device, service: ServiceId, kind: Kind) -> Result<()> {
        let svc = self.services.get(service)?;
        if svc.kind != kind {
            return Err(Error::NotAllowed(format!("{} is not a {kind:?} service", svc.name)));
        }
        if !self.policy.service(service).may_send.contains(self.policy.role_of(&device.user)) {
            return Err(Error::NotAllowed(format!("{} may not send on {}", device.user, svc.name)));
        }
        Ok(())
    }

    /// Seal a data record for a stateful service, for the server to store.
    pub fn seal(&mut self, device: &Device, service: ServiceId, body: &[u8]) -> Result<Vec<u8>> {
        self.may_send(device, service, Kind::Stateful)?;
        self.write(device, service, RecordKind::Data, body)
    }

    /// Seal a protocol record (checkpoint or cosign) for a stateful service.
    pub fn seal_kind(&mut self, device: &Device, service: ServiceId, kind: RecordKind, body: &[u8]) -> Result<Vec<u8>> {
        self.may_send(device, service, Kind::Stateful)?;
        self.write(device, service, kind, body)
    }

    /// Open a stored record. A `link` record also teaches this device the
    /// earlier epoch's key and roster, so following links walks back through
    /// history, verifying as it goes.
    pub fn open(&mut self, bytes: &[u8], creds: &impl Credentials) -> Result<Opened> {
        let parsed = record::parse(bytes)?;
        let h = parsed.header;
        if h.group != self.id {
            return Err(Error::Malformed("record from another group"));
        }
        let key = self.keys.get(&(h.service, h.epoch)).ok_or(Error::NoKey { service: h.service, epoch: h.epoch })?.clone();
        let holder = self.user_at(h.epoch, h.sender)?.clone();
        let (header, body) = record::open(bytes, &key, &holder.signature_key)?;
        if header.kind == RecordKind::Link {
            self.learn_link(header, &body, creds)?;
        }
        Ok(Opened { header, sender: holder.user, body })
    }

    fn learn_link(&mut self, header: Header, body: &[u8], creds: &impl Credentials) -> Result<()> {
        let (epoch, prev, entries) = record::parse_link(body)?;
        if epoch >= header.epoch {
            return Err(Error::Malformed("link forward in time"));
        }
        if let std::collections::btree_map::Entry::Vacant(slot) = self.rosters.entry(epoch) {
            let mut roster = Roster::new();
            for (leaf, credential, signature_key) in entries {
                let parsed = Credential::tls_deserialize_exact(credential.as_slice()).map_err(mls)?;
                let user = creds.verify(&parsed, &signature_key)?;
                roster.insert(leaf, Holder { user, signature_key, credential });
            }
            slot.insert(roster);
        }
        self.keys.entry((header.service, epoch)).or_insert(prev);
        Ok(())
    }

    /// Whether this device can read `service` records from `epoch`.
    pub fn holds_key(&self, service: ServiceId, epoch: u64) -> bool {
        self.keys.contains_key(&(service, epoch))
    }

    /// Encrypt a reliable realtime message (an MLS application message).
    pub fn send(&mut self, device: &Device, service: ServiceId, bytes: &[u8]) -> Result<Vec<u8>> {
        self.may_send(device, service, Kind::Reliable)?;
        self.mls.set_aad(service.to_be_bytes().to_vec());
        let out = self.mls.create_message(&device.provider, &device.signer, bytes).map_err(mls)?;
        self.mls.set_aad(Vec::new());
        out.tls_serialize_detached().map_err(mls)
    }

    /// The current epoch's SFrame base key for a lossy service.
    pub fn sframe_base(&self, device: &Device, service: ServiceId) -> Result<[u8; 16]> {
        keys::sframe_base(&Epoch(&self.mls, &device.provider), self.services.get(service)?)
    }

    /// A signed GroupInfo with the ratchet tree, for the server to start
    /// following the group from (spec §8).
    pub fn group_info(&self, device: &Device) -> Result<Vec<u8>> {
        self.mls.export_group_info(device.provider.crypto(), &device.signer, true).map_err(mls)?.tls_serialize_detached().map_err(mls)
    }

    /// This device's leaf index in the group.
    pub fn leaf(&self) -> u32 {
        self.own_leaf()
    }

    /// MLS's epoch authenticator: members who compare it out of band and
    /// match are in the same group (spec §4.3).
    pub fn epoch_authenticator(&self) -> Vec<u8> {
        self.mls.epoch_authenticator().as_slice().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Role, ServicePolicy};
    use crate::service::Service;

    const NOTES: ServiceId = 1;
    const PRESENCE: ServiceId = 2;
    const VOICE: ServiceId = 3;
    const SECRET: ServiceId = 4;

    fn services() -> Services {
        Services::new()
            .with(NOTES, Service::new("notes", Kind::Stateful, 1).unwrap())
            .unwrap()
            .with(PRESENCE, Service::new("presence", Kind::Reliable, 1).unwrap())
            .unwrap()
            .with(VOICE, Service::new("voice", Kind::Lossy, 1).unwrap())
            .unwrap()
            .with(SECRET, Service::new("secret", Kind::Stateful, 1).unwrap())
            .unwrap()
    }

    fn policy() -> Policy {
        let mut p = Policy::new("alice");
        p.services.insert(SECRET, ServicePolicy { history: History::None, ..Default::default() });
        p
    }

    /// Alice's group with Bob in it, both at the same epoch.
    fn pair() -> (Device, Group, Device, Group) {
        let alice = Device::new("alice").unwrap();
        let bob = Device::new("bob").unwrap();
        let mut a = Group::create(&alice, [1; 16], services(), policy(), &Unverified).unwrap();
        let c = a.add(&alice, &[bob.key_package().unwrap()], &Unverified).unwrap();
        a.confirm(&alice, &Unverified).unwrap();
        let b = Group::join(&bob, c.welcome.as_ref().unwrap(), &Unverified).unwrap();
        (alice, a, bob, b)
    }

    #[test]
    fn members_share_epoch_keys_and_authenticator() {
        let (alice, a, bob, b) = pair();
        assert_eq!(a.epoch(), 1);
        assert_eq!(b.epoch(), 1);
        assert_eq!(a.epoch_authenticator(), b.epoch_authenticator());
        assert_eq!(a.sframe_base(&alice, VOICE).unwrap(), b.sframe_base(&bob, VOICE).unwrap());
        assert_eq!(a.keys[&(NOTES, 1)], b.keys[&(NOTES, 1)]);
        assert_ne!(a.keys[&(NOTES, 1)], a.keys[&(SECRET, 1)]);
        assert_eq!(b.people().keys().cloned().collect::<Vec<_>>(), ["alice", "bob"]);
    }

    #[test]
    fn records_open_for_members_and_name_their_writer() {
        let (alice, mut a, bob, mut b) = pair();
        let rec = b.seal(&bob, NOTES, b"from bob").unwrap();
        let opened = a.open(&rec, &Unverified).unwrap();
        assert_eq!((opened.sender.as_str(), opened.body.as_slice()), ("bob", b"from bob".as_slice()));
        assert_eq!(opened.header.seq, 0);
        assert_eq!(b.seal(&bob, NOTES, b"again").map(|r| record::parse(&r).unwrap().header.seq), Ok(1));
        // Kinds are enforced: a record can't go out on a realtime service.
        assert!(a.seal(&alice, PRESENCE, b"x").is_err());
    }

    #[test]
    fn reliable_messages_carry_their_service() {
        let (alice, mut a, bob, mut b) = pair();
        let m = a.send(&alice, PRESENCE, b"typing").unwrap();
        assert_eq!(
            b.process(&bob, &m, &Unverified).unwrap(),
            Incoming::Message { service: PRESENCE, sender: "alice".into(), bytes: b"typing".to_vec() }
        );
        assert!(a.send(&alice, NOTES, b"x").is_err());
    }

    #[test]
    fn a_removed_member_cannot_read_what_follows() {
        let (alice, mut a, bob, mut b) = pair();
        let c = a.remove(&alice, &["bob"]).unwrap();
        a.confirm(&alice, &Unverified).unwrap();
        assert_eq!(b.process(&bob, &c.commit, &Unverified).unwrap(), Incoming::Removed);
        let after = a.seal(&alice, NOTES, b"after bob").unwrap();
        assert_eq!(b.open(&after, &Unverified), Err(Error::NoKey { service: NOTES, epoch: 2 }));
    }

    #[test]
    fn members_refuse_a_commit_the_policy_forbids() {
        let (alice, mut a, bob, mut b) = pair();
        let carol = Device::new("carol").unwrap();
        // Bob's own check stops him first…
        assert!(matches!(b.add(&bob, &[carol.key_package().unwrap()], &Unverified), Err(Error::NotAllowed(_))));
        // …and if he goes around it, Alice refuses the commit.
        let kp = KeyPackageIn::tls_deserialize_exact(carol.key_package().unwrap().as_slice())
            .unwrap()
            .validate(bob.provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
        let (commit, _, _) = b.mls.add_members(&bob.provider, &bob.signer, &[kp]).unwrap();
        let bytes = commit.tls_serialize_detached().unwrap();
        assert!(matches!(a.process(&alice, &bytes, &Unverified), Err(Error::NotAllowed(_))));
        assert_eq!(a.epoch(), 1);
        // Once Alice makes Bob an admin, the same kind of commit goes through.
        let mut p = a.policy().clone();
        p.roles.insert("bob".into(), Role::Admin);
        let c = a.change(&alice, services(), p).unwrap();
        a.confirm(&alice, &Unverified).unwrap();
        b.discard(&bob).unwrap();
        assert_eq!(b.process(&bob, &c.commit, &Unverified).unwrap(), Incoming::Epoch(2));
        assert!(b.add(&bob, &[carol.key_package().unwrap()], &Unverified).is_ok());
    }

    #[test]
    fn history_follows_links_back_unless_the_service_says_none() {
        let alice = Device::new("alice").unwrap();
        let bob = Device::new("bob").unwrap();
        let mut a = Group::create(&alice, [2; 16], services(), policy(), &Unverified).unwrap();
        let old_note = a.seal(&alice, NOTES, b"before bob").unwrap();
        let old_secret = a.seal(&alice, SECRET, b"before bob").unwrap();
        let c = a.add(&alice, &[bob.key_package().unwrap()], &Unverified).unwrap();
        let links = a.confirm(&alice, &Unverified).unwrap();
        // One link: notes keeps history, secret doesn't.
        assert_eq!(links.len(), 1);
        assert_eq!(record::parse(&links[0]).unwrap().header.service, NOTES);
        let mut b = Group::join(&bob, c.welcome.as_ref().unwrap(), &Unverified).unwrap();
        assert_eq!(b.open(&old_note, &Unverified), Err(Error::NoKey { service: NOTES, epoch: 0 }));
        assert_eq!(b.open(&links[0], &Unverified).unwrap().header.kind, RecordKind::Link);
        let opened = b.open(&old_note, &Unverified).unwrap();
        assert_eq!((opened.sender.as_str(), opened.body.as_slice()), ("alice", b"before bob".as_slice()));
        assert_eq!(b.open(&old_secret, &Unverified), Err(Error::NoKey { service: SECRET, epoch: 0 }));
    }

    #[test]
    fn a_refused_commit_can_be_discarded() {
        let (alice, mut a, _bob, _b) = pair();
        a.rotate(&alice).unwrap();
        a.discard(&alice).unwrap();
        assert_eq!(a.epoch(), 1);
        a.rotate(&alice).unwrap();
        a.confirm(&alice, &Unverified).unwrap();
        assert_eq!(a.epoch(), 2);
    }
}
