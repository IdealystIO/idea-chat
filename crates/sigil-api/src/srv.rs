//! What the server bodies share, and the work they do. Server build only.
//!
//! The `#[server]` functions check the session and call [`ops`]. Ops are
//! plain functions over the installed [`App`], so they run — and are tested
//! — without HTTP. They hold the hub's lock only while working, never across
//! an await.
//!
//! Fan-out is in process: [`Router`] maps each device to its open live
//! connections. The hub keeps groups in memory too, so a Sigil server is one
//! instance until a stored hub exists.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use hmac::{Hmac, Mac};
use server::ServerError;
use server_kit::Sessions;
use sha2::Sha256;
use sigil::group::Unverified;
use sigil::{GroupId, ServiceId};
use sigil_relay::Issuer;
use sigil_server::{Challenges, DeviceId, Hub, MemoryStore};
use tokio::sync::mpsc;

use crate::live::Down;
use crate::{b64, hex, ApiError, Failure, Principal, RelayTicket, Stored};

/// The hub this server runs. Credentials are taken at their word until the
/// identity service exists (spec §4.2) — fine for development, not for a
/// server people rely on.
pub type SigilHub = Hub<MemoryStore, Unverified>;

/// The relay this server issues grants for.
pub struct Relay {
    pub url: String,
    pub cert_hash: String,
    pub issuer: Issuer,
    /// Hands a revocation to the relay.
    pub revoke: Arc<dyn Fn(String) + Send + Sync>,
}

pub struct App {
    pub hub: Mutex<SigilHub>,
    pub challenges: Mutex<Challenges>,
    pub sessions: Sessions,
    /// Signs live-connection tickets.
    pub live_secret: Vec<u8>,
    pub relay: Option<Relay>,
    pub router: Router,
}

static APP: OnceLock<App> = OnceLock::new();

pub fn install(app: App) {
    if APP.set(app).is_err() {
        panic!("sigil_api::srv::install called twice");
    }
}

pub fn app() -> &'static App {
    APP.get().expect("sigil_api::srv::install was not called before serving")
}

// ---------------------------------------------------------------- errors

pub fn fail(e: ApiError) -> Failure {
    ServerError::Failed(e)
}

fn invalid(message: impl Into<String>) -> Failure {
    fail(ApiError::Invalid { message: message.into() })
}

fn forbidden(message: impl Into<String>) -> Failure {
    fail(ApiError::Forbidden { message: message.into() })
}

/// A Sigil refusal, as the caller should hear it.
pub fn refused(e: sigil::Error) -> Failure {
    use sigil::Error as E;
    match e {
        E::NotAllowed(m) if m.starts_with("stale") => fail(ApiError::Stale),
        E::NotAllowed(m) | E::Credential(m) => forbidden(m),
        E::Malformed(what) => invalid(format!("malformed {what}")),
        E::Unauthentic => forbidden("a signature didn't verify"),
        E::UnknownService(id) => invalid(format!("no service {id} in that group")),
        E::NoKey { .. } => invalid("no key for that"),
        E::Replay => forbidden("already used"),
        E::Expired => forbidden("expired"),
        E::WrongAudience => forbidden("meant for another server"),
        E::Mls(m) => invalid(m),
    }
}

fn bytes(text: &str, what: &str) -> Result<Vec<u8>, Failure> {
    b64::decode(text).ok_or_else(|| invalid(format!("{what} is not base64url")))
}

fn group_id(text: &str) -> Result<GroupId, Failure> {
    hex::decode(text).and_then(|b| b.try_into().ok()).ok_or_else(|| invalid("a group id is 32 hex digits"))
}

/// The device a session stands for.
pub fn device_of(p: &Principal) -> Result<DeviceId, Failure> {
    hex::decode(&p.device).filter(|d| d.len() == 32).ok_or_else(|| fail(ApiError::SignedOut))
}

fn hub() -> std::sync::MutexGuard<'static, SigilHub> {
    app().hub.lock().unwrap_or_else(|e| e.into_inner())
}

fn member(hub: &SigilHub, group: GroupId, device: &DeviceId) -> Result<(), Failure> {
    if hub.is_member(group, device) {
        Ok(())
    } else {
        Err(forbidden("this device isn't in that group"))
    }
}

pub fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    sigil_relay::now_ms()
}

// ---------------------------------------------------------------- live

/// One device's live connections, by connection id.
type Conns = Vec<(u64, mpsc::UnboundedSender<Down>)>;

/// Each device's open live connections.
#[derive(Default)]
pub struct Router {
    conns: Mutex<HashMap<DeviceId, Conns>>,
    next: AtomicU64,
}

impl Router {
    pub fn register(&self, device: DeviceId) -> (u64, mpsc::UnboundedReceiver<Down>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.conns.lock().unwrap_or_else(|e| e.into_inner()).entry(device).or_default().push((id, tx));
        (id, rx)
    }

    pub fn unregister(&self, device: &DeviceId, id: u64) {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = conns.get_mut(device) {
            list.retain(|(c, _)| *c != id);
            if list.is_empty() {
                conns.remove(device);
            }
        }
    }

    /// Send `down` to every connection of each of `devices`.
    pub fn send(&self, devices: &[DeviceId], down: Down) {
        let conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        for d in devices {
            for (_, tx) in conns.get(d).into_iter().flatten() {
                let _ = tx.send(down.clone());
            }
        }
    }
}

const TICKET_TTL_MS: u64 = 60_000;

fn sign(secret: &[u8], body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("any key length");
    mac.update(b"sigil-live v1\0");
    mac.update(body.as_bytes());
    hex::encode(&mac.finalize().into_bytes())
}

/// A ticket opening one live connection for `device`, good for a minute.
pub fn mint_ticket(secret: &[u8], device: &DeviceId, now_ms: u64) -> String {
    let body = format!("{}.{}", hex::encode(device), now_ms + TICKET_TTL_MS);
    format!("{body}.{}", sign(secret, &body))
}

pub fn check_ticket(secret: &[u8], ticket: &str, now_ms: u64) -> Option<DeviceId> {
    let (body, mac) = ticket.rsplit_once('.')?;
    let expected = sign(secret, body);
    let same = expected.len() == mac.len() && expected.bytes().zip(mac.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    let (device, expires) = body.split_once('.')?;
    (same && now_ms < expires.parse::<u64>().ok()?).then(|| hex::decode(device)).flatten()
}

// ---------------------------------------------------------------- ops

pub mod ops {
    use super::*;

    pub fn challenge() -> String {
        b64::encode(&app().challenges.lock().unwrap_or_else(|e| e.into_inner()).issue(rand::random(), now_s()))
    }

    /// Check a signed challenge; returns the device it proves.
    pub fn answer(device: &str, nonce: &str, signature: &str) -> Result<DeviceId, Failure> {
        let device = hex::decode(device).ok_or_else(|| invalid("a device is its key in hex"))?;
        let nonce: [u8; 32] = bytes(nonce, "nonce")?.try_into().map_err(|_| invalid("a nonce is 32 bytes"))?;
        let sig = bytes(signature, "signature")?;
        app().challenges.lock().unwrap_or_else(|e| e.into_inner()).answer(&device, &nonce, &sig, now_s()).map_err(refused)
    }

    pub fn publish_key_packages(me: &DeviceId, packages: &[String]) -> Result<(), Failure> {
        let mut hub = hub();
        for p in packages {
            hub.publish_key_package(me, bytes(p, "key package")?).map_err(refused)?;
        }
        Ok(())
    }

    pub fn claim_key_package(device: &str) -> Result<Option<String>, Failure> {
        let device = hex::decode(device).ok_or_else(|| invalid("a device is its key in hex"))?;
        Ok(hub().claim_key_package(&device).map(|kp| b64::encode(&kp)))
    }

    pub fn create_group(me: &DeviceId, group_info: &str) -> Result<String, Failure> {
        let id = hub().create(me, &bytes(group_info, "group info")?).map_err(refused)?;
        Ok(hex::encode(&id))
    }

    /// Take a commit, deliver it, hand Welcomes to the devices it adds, and
    /// take the devices it removes out of the relay.
    pub fn commit(me: &DeviceId, group: &str, commit: &str, welcome: Option<&str>) -> Result<u64, Failure> {
        let gid = group_id(group)?;
        let commit = bytes(commit, "commit")?;
        let welcome = welcome.map(|w| bytes(w, "welcome")).transpose()?;
        let (accepted, removed, services) = {
            let mut hub = hub();
            member(&hub, gid, me)?;
            let accepted = hub.commit(gid, &commit, welcome.clone()).map_err(refused)?;
            let now = hub.members(gid).map_err(refused)?;
            let removed: Vec<DeviceId> = accepted.notify.iter().filter(|d| !now.contains(d)).cloned().collect();
            (accepted, removed, hub.services(gid).map_err(refused)?.clone())
        };
        let others: Vec<DeviceId> = accepted.notify.iter().filter(|d| *d != me).cloned().collect();
        app().router.send(&others, Down::Commit { group: group.to_string(), commit: b64::encode(&commit) });
        if let Some(w) = &welcome {
            app().router.send(&accepted.welcomed, Down::Welcome { welcome: b64::encode(w) });
        }
        if let Some(relay) = &app().relay {
            for d in &removed {
                for token in relay.issuer.remove(&gid, &services, d, now_ms()) {
                    (relay.revoke)(token);
                }
            }
        }
        Ok(accepted.epoch)
    }

    pub fn welcomes(me: &DeviceId) -> Vec<String> {
        hub().welcomes(me).iter().map(|w| b64::encode(w)).collect()
    }

    /// Deliver a reliable realtime message to the group's other devices.
    pub fn send(me: &DeviceId, group: &str, message: &str) -> Result<(), Failure> {
        let gid = group_id(group)?;
        let bytes = bytes(message, "message")?;
        let to = {
            let hub = hub();
            member(&hub, gid, me)?;
            hub.relay(gid, &bytes).map_err(refused)?
        };
        let others: Vec<DeviceId> = to.into_iter().filter(|d| d != me).collect();
        app().router.send(&others, Down::Message { group: group.to_string(), message: message.to_string() });
        Ok(())
    }

    pub fn post_record(me: &DeviceId, group: &str, record: &str) -> Result<u64, Failure> {
        let gid = group_id(group)?;
        let bytes = bytes(record, "record")?;
        let service = sigil::record::parse(&bytes).map_err(refused)?.header.service;
        let (seq, others) = {
            let mut hub = hub();
            member(&hub, gid, me)?;
            let seq = hub.post(gid, bytes).map_err(refused)?;
            (seq, hub.members(gid).map_err(refused)?.into_iter().filter(|d| d != me).collect::<Vec<_>>())
        };
        app().router.send(&others, Down::Record { group: group.to_string(), service, seq });
        Ok(seq)
    }

    pub fn records(me: &DeviceId, group: &str, service: ServiceId, after: u64, limit: u32) -> Result<Vec<Stored>, Failure> {
        let gid = group_id(group)?;
        let hub = hub();
        member(&hub, gid, me)?;
        let out = hub.records(gid, service, after, limit as usize).map_err(refused)?;
        Ok(out.into_iter().map(|(seq, r)| Stored { seq, record: b64::encode(&r) }).collect())
    }

    pub fn act(action: &str) -> Result<(), Failure> {
        let bytes = bytes(action, "action")?;
        hub().act(&bytes, now_s()).map(|_| ()).map_err(refused)
    }

    pub fn relay_ticket(me: &DeviceId, group: &str, service: ServiceId) -> Result<RelayTicket, Failure> {
        let gid = group_id(group)?;
        let relay = app().relay.as_ref().ok_or_else(|| fail(ApiError::Unavailable { message: "this server runs no relay".into() }))?;
        let hub = hub();
        member(&hub, gid, me)?;
        let grant = relay
            .issuer
            .grant(
                sigil_relay::Room { group: &gid, services: hub.services(gid).map_err(refused)?, policy: hub.policy(gid).map_err(refused)?, service },
                me,
                hub.role(gid, me),
                now_ms(),
            )
            .map_err(|e| forbidden(format!("{e:?}")))?;
        Ok(RelayTicket { url: relay.url.clone(), cert_hash: relay.cert_hash.clone(), room: sigil_relay::room(&gid, service), grant })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil::policy::ServicePolicy;
    use sigil::{Device, Group, Incoming, Kind, Policy, Service, Services};

    const NAME: &str = "sigil.test";

    fn install_once() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            install(App {
                hub: Mutex::new(Hub::new(NAME, MemoryStore::default(), Unverified)),
                challenges: Mutex::new(Challenges::new(NAME)),
                sessions: Sessions::new(cache::MemoryCache::new()),
                live_secret: b"live secret for tests".to_vec(),
                relay: Some(Relay {
                    url: "https://relay.test/".into(),
                    cert_hash: String::new(),
                    issuer: Issuer::new(b"relay secret for tests"),
                    revoke: Arc::new(|_| {}),
                }),
                router: Router::default(),
            })
        });
    }

    fn services() -> Services {
        Services::new()
            .with(1, Service::new("notes", Kind::Stateful, 1).unwrap())
            .unwrap()
            .with(2, Service::new("presence", Kind::Reliable, 1).unwrap())
            .unwrap()
            .with(3, Service::new("voice", Kind::Lossy, 1).unwrap())
            .unwrap()
    }

    #[test]
    fn devices_sign_in_by_challenge() {
        install_once();
        let d = Device::new("alice").unwrap();
        let nonce = ops::challenge();
        let n: [u8; 32] = b64::decode(&nonce).unwrap().try_into().unwrap();
        let sig = d.sign(sigil_server::auth::LABEL, &sigil_server::auth::payload(NAME, &n)).unwrap();
        let id = ops::answer(&hex::encode(&d.public_key()), &nonce, &b64::encode(&sig)).unwrap();
        assert_eq!(id, d.public_key());
        assert!(ops::answer(&hex::encode(&d.public_key()), &nonce, &b64::encode(&sig)).is_err(), "once only");
    }

    #[test]
    fn tickets_open_one_device_for_a_minute() {
        let t = mint_ticket(b"k", &vec![7; 32], 1_000);
        assert_eq!(check_ticket(b"k", &t, 1_001), Some(vec![7; 32]));
        assert_eq!(check_ticket(b"k", &t, 1_000 + TICKET_TTL_MS), None);
        assert_eq!(check_ticket(b"other", &t, 1_001), None);
    }

    #[tokio::test]
    async fn a_group_works_end_to_end_through_the_ops() {
        install_once();
        let alice = Device::new("alice2").unwrap();
        let bob = Device::new("bob2").unwrap();
        let (me_a, me_b) = (alice.public_key(), bob.public_key());
        let (_, mut bob_live) = app().router.register(me_b.clone());

        ops::publish_key_packages(&me_b, &[b64::encode(&bob.key_package().unwrap())]).unwrap();
        let mut policy = Policy::new("alice2");
        policy.services.insert(1, ServicePolicy::default());
        let mut a = Group::create(&alice, [0x5a; 16], services(), policy, &Unverified).unwrap();
        let gid = ops::create_group(&me_a, &b64::encode(&a.group_info(&alice).unwrap())).unwrap();
        assert_eq!(gid, "5a".repeat(16));

        let kp = ops::claim_key_package(&hex::encode(&me_b)).unwrap().unwrap();
        let c = a.add(&alice, &[b64::decode(&kp).unwrap()], &Unverified).unwrap();
        let w = c.welcome.as_ref().map(|w| b64::encode(w));
        assert_eq!(ops::commit(&me_a, &gid, &b64::encode(&c.commit), w.as_deref()).unwrap(), 1);
        a.confirm(&alice, &Unverified).unwrap();
        let Some(Down::Welcome { welcome }) = bob_live.recv().await else { panic!("Bob is welcomed live") };
        let mut b = Group::join(&bob, &b64::decode(&welcome).unwrap(), &Unverified).unwrap();
        assert!(ops::welcomes(&me_b).len() == 1, "and can fetch it");

        // A record, stored and announced.
        let rec = a.seal(&alice, 1, b"hello").unwrap();
        let seq = ops::post_record(&me_a, &gid, &b64::encode(&rec)).unwrap();
        assert_eq!(bob_live.recv().await, Some(Down::Record { group: gid.clone(), service: 1, seq }));
        let stored = ops::records(&me_b, &gid, 1, 0, 50).unwrap();
        assert_eq!(b.open(&b64::decode(&stored[0].record).unwrap(), &Unverified).unwrap().body, b"hello");

        // A reliable message, delivered live.
        let m = a.send(&alice, 2, b"typing").unwrap();
        ops::send(&me_a, &gid, &b64::encode(&m)).unwrap();
        let Some(Down::Message { message, .. }) = bob_live.recv().await else { panic!("Bob hears it") };
        assert!(matches!(b.process(&bob, &b64::decode(&message).unwrap(), &Unverified).unwrap(), Incoming::Message { .. }));

        // A relay ticket for members only.
        assert!(ops::relay_ticket(&me_b, &gid, 3).is_ok());
        let carol = Device::new("carol2").unwrap();
        assert!(ops::relay_ticket(&carol.public_key(), &gid, 3).is_err());
        assert!(ops::records(&carol.public_key(), &gid, 1, 0, 50).is_err(), "outsiders can't fetch even ciphertext");

        // Removal: Bob hears the commit and finds himself out.
        let c = a.remove(&alice, &["bob2"]).unwrap();
        ops::commit(&me_a, &gid, &b64::encode(&c.commit), None).unwrap();
        a.confirm(&alice, &Unverified).unwrap();
        let Some(Down::Commit { commit, .. }) = bob_live.recv().await else { panic!("Bob hears his removal") };
        assert_eq!(b.process(&bob, &b64::decode(&commit).unwrap(), &Unverified).unwrap(), Incoming::Removed);
        assert!(ops::records(&me_b, &gid, 1, 0, 50).is_err());
    }
}
