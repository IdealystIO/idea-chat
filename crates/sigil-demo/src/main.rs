//! A narrated run of Sigil against a live `sigil-host`.
//!
//! Three devices — Alice, Bob, Carol — talk to the server over plain HTTP
//! and to its relay over QUIC, exactly as a client would. At each step the
//! demo prints what the server holds next to what members read.
//!
//! ```text
//! cargo run -p sigil-host &          # or tools/sigil-demo.sh, which does both
//! cargo run -p sigil-demo [http://127.0.0.1:8091]
//! ```

use std::time::Duration;

use call_proto::relay::{untag, Connect};
use call_proto::{KIND_AUDIO, TRACK_MAIN};
use datagram::{Event, Link};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use sigil::group::Unverified;
use sigil::record::RecordKind;
use sigil::{Device, Group, Kind, Policy, Service, Services};
use sigil_api::{b64, hex, RelayTicket, Session, Stored};
use sigil_media::{Opener, Sealer};

const MESSAGES: u16 = 1;
const VOICE: u16 = 3;

// ---------------------------------------------------------------- output

fn step(n: u32, title: &str) {
    println!("\n\x1b[1m{n}. {title}\x1b[0m");
}
fn say(who: &str, what: impl AsRef<str>) {
    println!("   \x1b[36m{who:>6}\x1b[0m  {}", what.as_ref());
}
fn server(what: impl AsRef<str>) {
    println!("   \x1b[33mserver\x1b[0m  {}", what.as_ref());
}
fn ok(what: impl AsRef<str>) {
    println!("   \x1b[32m    ✓\x1b[0m   {}", what.as_ref());
}
fn no(what: impl AsRef<str>) {
    println!("   \x1b[31m    ✗\x1b[0m   {}", what.as_ref());
}

/// The first bytes of a ciphertext, as the server stores it.
fn peek(bytes: &[u8]) -> String {
    let n = bytes.len().min(24);
    format!("{}… ({} bytes)", hex::encode(&bytes[..n]), bytes.len())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------- client

/// One device's connection to the API: plain HTTP, as any client sends it.
struct Api {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Api {
    async fn call<T: DeserializeOwned>(&self, name: &str, args: Value) -> Result<T, String> {
        let mut req = self
            .http
            .post(format!("{}/_srv/sigil/{name}", self.base))
            .header("x-idealyst-client", "native")
            .json(&args);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.map_err(|e| format!("network: {e}"))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{status}: {text}"));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))?;
        match v {
            Value::Object(mut m) if m.contains_key("Ok") => serde_json::from_value(m.remove("Ok").unwrap()).map_err(|e| e.to_string()),
            Value::Object(m) if m.contains_key("Err") => Err(m["Err"].to_string()),
            other => Err(format!("unexpected answer {other}")),
        }
    }
}

struct Person {
    name: &'static str,
    device: Device,
    api: Api,
    group: Option<Group>,
}

impl Person {
    async fn sign_in(name: &'static str, base: &str, audience: &str) -> Result<Person, String> {
        let device = Device::new(name).map_err(|e| e.to_string())?;
        let mut api = Api { http: reqwest::Client::new(), base: base.to_string(), token: None };
        let nonce: String = api.call("challenge", json!([])).await?;
        let n: [u8; 32] = b64::decode(&nonce).and_then(|b| b.try_into().ok()).ok_or("bad nonce")?;
        let payload = [audience.as_bytes(), b"\0", &n].concat();
        let sig = device.sign("sigil/auth/v1", &payload).map_err(|e| e.to_string())?;
        let session: Session = api.call("sign_in", json!([hex::encode(&device.public_key()), nonce, b64::encode(&sig)])).await?;
        api.token = session.token;
        Ok(Person { name, device, api, group: None })
    }

    fn id(&self) -> String {
        hex::encode(&self.device.public_key())
    }

    async fn publish_key_packages(&self, n: usize) -> Result<(), String> {
        let kps: Vec<String> = (0..n).map(|_| b64::encode(&self.device.key_package().unwrap())).collect();
        self.api.call::<()>("publish_key_packages", json!([kps])).await
    }

    /// Send a commit, merge it, and store the links it produced.
    async fn commit(&mut self, gid: &str, c: sigil::Commit) -> Result<u64, String> {
        let epoch: u64 =
            self.api.call("commit", json!([gid, b64::encode(&c.commit), c.welcome.as_ref().map(|w| b64::encode(w))])).await?;
        let links = self.group.as_mut().unwrap().confirm(&self.device, &Unverified).map_err(|e| e.to_string())?;
        for link in links {
            self.api.call::<u64>("post_record", json!([gid, b64::encode(&link)])).await?;
        }
        Ok(epoch)
    }

    async fn join(&mut self) -> Result<(), String> {
        let welcomes: Vec<String> = self.api.call("welcomes", json!([])).await?;
        let w = welcomes.last().ok_or("no welcome waiting")?;
        self.group = Some(Group::join(&self.device, &b64::decode(w).unwrap(), &Unverified).map_err(|e| e.to_string())?);
        Ok(())
    }

    async fn post(&mut self, gid: &str, text: &str) -> Result<(u64, Vec<u8>), String> {
        let rec = self.group.as_mut().unwrap().seal(&self.device, MESSAGES, text.as_bytes()).map_err(|e| e.to_string())?;
        let seq = self.api.call("post_record", json!([gid, b64::encode(&rec)])).await?;
        Ok((seq, rec))
    }

    async fn fetch(&self, gid: &str) -> Result<Vec<(u64, Vec<u8>)>, String> {
        let stored: Vec<Stored> = self.api.call("records", json!([gid, MESSAGES, 0, 100])).await?;
        Ok(stored.into_iter().map(|s| (s.seq, b64::decode(&s.record).unwrap())).collect())
    }

    /// Open every record this device can, following links back through
    /// history until nothing more opens. Returns the messages read.
    fn read_all(&mut self, records: &[(u64, Vec<u8>)]) -> Vec<(String, String)> {
        let mut read = std::collections::BTreeMap::new();
        loop {
            let before = read.len();
            for (seq, rec) in records.iter().rev() {
                if read.contains_key(seq) {
                    continue;
                }
                if let Ok(o) = self.group.as_mut().unwrap().open(rec, &Unverified) {
                    let text = match o.header.kind {
                        RecordKind::Data => String::from_utf8_lossy(&o.body).into_owned(),
                        _ => format!("(link to epoch {})", o.header.epoch - 1),
                    };
                    read.insert(*seq, (o.sender, text));
                }
            }
            if read.len() == before {
                break;
            }
        }
        read.into_values().filter(|(_, t)| !t.starts_with("(link")).collect()
    }

    async fn voice(&self, gid: &str) -> Result<Link, String> {
        let t: RelayTicket = self.api.call("relay_ticket", json!([gid, VOICE])).await?;
        let url = Connect { room: t.room, grant: Some(t.grant), revoke: None }.url(&t.url);
        let hash = (!t.cert_hash.is_empty()).then(|| datagram::parse_hash(&t.cert_hash)).flatten();
        Link::connect(&url, hash).await.map_err(|e| e.to_string())
    }
}

async fn hear(link: &Link, ms: u64) -> Option<Vec<u8>> {
    tokio::time::timeout(Duration::from_millis(ms), async {
        loop {
            match link.recv().await {
                Event::Data(d) => return Some(d),
                Event::Closed => return None,
                Event::Control(_) => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[tokio::main]
async fn main() {
    let base = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:8091".into());
    let audience = std::env::var("SIGIL_NAME").unwrap_or_else(|_| "localhost".into());
    if let Err(e) = run(&base, &audience).await {
        eprintln!("\n\x1b[31mdemo stopped:\x1b[0m {e}");
        std::process::exit(1);
    }
}

async fn run(base: &str, audience: &str) -> Result<(), String> {
    println!("\x1b[1mSigil, live against {base}\x1b[0m");

    step(1, "Three devices sign in by signing a challenge with their own key");
    let mut alice = Person::sign_in("alice", base, audience).await?;
    let mut bob = Person::sign_in("bob", base, audience).await?;
    let mut carol = Person::sign_in("carol", base, audience).await?;
    for p in [&alice, &bob, &carol] {
        say(p.name, format!("signed in as device {}…", &p.id()[..16]));
    }
    bob.publish_key_packages(2).await?;
    carol.publish_key_packages(2).await?;
    say("bob", "published 2 KeyPackages");
    say("carol", "published 2 KeyPackages");

    step(2, "Alice creates a group with three services");
    let services = Services::new()
        .with(MESSAGES, Service::new("messages", Kind::Stateful, 1).unwrap())
        .unwrap()
        .with(2, Service::new("typing", Kind::Reliable, 1).unwrap())
        .unwrap()
        .with(VOICE, Service::new("voice", Kind::Lossy, 1).unwrap())
        .unwrap();
    let id: [u8; 16] = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(std::process::id() as u8));
    let group = Group::create(&alice.device, id, services, Policy::new("alice"), &Unverified).map_err(|e| e.to_string())?;
    let gid: String = alice.api.call("create_group", json!([b64::encode(&group.group_info(&alice.device).unwrap())])).await?;
    alice.group = Some(group);
    say("alice", format!("group {gid}: messages (stored), typing (realtime), voice (relay)"));
    server("follows the group's public state: who's in it and its policy. Holds no key.");

    step(3, "Alice adds Bob");
    let kp: Option<String> = alice.api.call("claim_key_package", json!([bob.id()])).await?;
    let c = alice.group.as_mut().unwrap().add(&alice.device, &[b64::decode(&kp.ok_or("Bob has no KeyPackage")?).unwrap()], &Unverified).map_err(|e| e.to_string())?;
    let epoch = alice.commit(&gid, c).await?;
    bob.join().await?;
    say("alice", format!("commit accepted: the group is at epoch {epoch}"));
    say("bob", format!("joined from his Welcome, at epoch {}", bob.group.as_mut().unwrap().epoch()));
    let same = alice.group.as_mut().unwrap().epoch_authenticator() == bob.group.as_mut().unwrap().epoch_authenticator();
    ok(format!("their epoch authenticators match: {same}"));

    step(4, "Alice posts a message; the server stores it");
    let secret = "Meet at 6, the usual place.";
    let (seq, stored) = alice.post(&gid, secret).await?;
    server(format!("stored record #{seq}: {}", peek(&stored)));
    if contains(&stored, secret.as_bytes()) {
        no("the plaintext is in what the server stored");
    } else {
        ok("the text appears nowhere in what the server stored");
    }
    let records = bob.fetch(&gid).await?;
    for (from, text) in bob.read_all(&records) {
        say("bob", format!("reads {from}: \"{text}\""));
    }

    step(5, "A voice frame through the relay");
    let la = alice.voice(&gid).await?;
    let lb = bob.voice(&gid).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut mic = Sealer::new(alice.group.as_mut().unwrap().epoch(), &alice.group.as_mut().unwrap().sframe_base(&alice.device, VOICE).unwrap(), alice.group.as_mut().unwrap().leaf(), 0);
    let mut ears = Opener::new();
    ears.enter(bob.group.as_mut().unwrap().epoch(), &bob.group.as_mut().unwrap().sframe_base(&bob.device, VOICE).unwrap(), sigil_relay_now());
    let frame = mic.seal(KIND_AUDIO, TRACK_MAIN, b"seq/ts", b"<opus: hello bob>").unwrap();
    la.send(&frame).await.map_err(|e| e.to_string())?;
    let got = hear(&lb, 3_000).await.ok_or("Bob heard nothing")?;
    let (_, relayed) = untag(&got).ok_or("bad relay tag")?;
    server(format!("relay forwarded {}", peek(relayed)));
    let f = ears.open(relayed, sigil_relay_now()).map_err(|e| format!("{e:?}"))?;
    say("bob", format!("hears \"{}\" from leaf {} (Alice's)", String::from_utf8_lossy(&f.payload), f.leaf));
    ok("keys came from the group's epoch; the relay never had one");

    step(6, "Carol, not in the group, tries to get in");
    match carol.fetch(&gid).await {
        Ok(_) => no("Carol fetched the group's records"),
        Err(e) => ok(format!("records refused: {}", short(&e))),
    }
    match carol.voice(&gid).await {
        Ok(_) => no("Carol got into the voice room"),
        Err(e) => ok(format!("voice refused: {}", short(&e))),
    }
    match bob.group.as_mut().unwrap().add(&bob.device, &[carol.device.key_package().unwrap()], &Unverified) {
        Ok(_) => no("Bob, a member, could add Carol"),
        Err(e) => ok(format!("Bob tries to add her himself: {e}")),
    }

    step(7, "Alice removes Bob");
    let c = alice.group.as_mut().unwrap().remove(&alice.device, &["bob"]).map_err(|e| e.to_string())?;
    let epoch = alice.commit(&gid, c).await?;
    say("alice", format!("commit accepted: epoch {epoch}, with new keys Bob can't compute"));
    let (_, after) = alice.post(&gid, "Bob's out. New plan: 7.").await?;
    match bob.fetch(&gid).await {
        Ok(_) => no("Bob still fetches records"),
        Err(e) => ok(format!("Bob's fetch refused: {}", short(&e))),
    }
    match bob.group.as_mut().unwrap().open(&after, &Unverified) {
        Ok(_) => no("Bob read the new message"),
        Err(e) => ok(format!("handed the ciphertext anyway, Bob can't open it: {e}")),
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    mic.rekey(alice.group.as_mut().unwrap().epoch(), &alice.group.as_mut().unwrap().sframe_base(&alice.device, VOICE).unwrap(), alice.group.as_mut().unwrap().leaf());
    la.send(&mic.seal(KIND_AUDIO, TRACK_MAIN, b"seq/ts", b"<opus: bob can't hear this>").unwrap()).await.map_err(|e| e.to_string())?;
    match hear(&lb, 800).await {
        Some(_) => no("Bob still hears the call"),
        None => ok("the relay took Bob out of the voice room"),
    }

    step(8, "Alice adds Carol, who reads the history through links");
    let kp: Option<String> = alice.api.call("claim_key_package", json!([carol.id()])).await?;
    let c = alice.group.as_mut().unwrap().add(&alice.device, &[b64::decode(&kp.ok_or("Carol has no KeyPackage")?).unwrap()], &Unverified).map_err(|e| e.to_string())?;
    alice.commit(&gid, c).await?;
    carol.join().await?;
    say("carol", format!("joined at epoch {}; her keys start here", carol.group.as_mut().unwrap().epoch()));
    let records = carol.fetch(&gid).await?;
    let read = carol.read_all(&records);
    for (from, text) in &read {
        say("carol", format!("reads {from}: \"{text}\""));
    }
    if read.len() == 2 {
        ok("each epoch's link handed her the key and roster of the one before");
    } else {
        no(format!("Carol read {} of 2 messages", read.len()));
    }

    println!("\n\x1b[1mDone.\x1b[0m The server stored and routed every byte above and read none of them.");
    Ok(())
}

fn sigil_relay_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A refusal's message, without the JSON around it.
fn short(e: &str) -> String {
    serde_json::from_str::<Value>(e)
        .ok()
        .and_then(|v| v.pointer("/Failed/message").or_else(|| v.pointer("/message")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| e.chars().take(90).collect())
}
