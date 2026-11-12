//! A Sigil call over real QUIC: two members of a group meet in the relay
//! room for its voice service, frames sealed under the epoch's key reach the
//! other member and name the sender's leaf, and removing a member takes them
//! out of the room.

use std::time::Duration;

use call_proto::relay::{untag, Connect};
use call_proto::{KIND_AUDIO, TRACK_MAIN};
use datagram::{Event, Identity, Link, Listener};
use sigil::group::Unverified;
use sigil::{Device, Group, Kind, Policy, Service, ServiceId, Services};
use sigil_media::{Opener, Sealer};
use sigil_relay::{now_ms, room, Issuer, Room};

const SECRET: &[u8] = b"a relay secret of decent length";
const VOICE: ServiceId = 3;

async fn received(link: &Link, ms: u64) -> Option<Vec<u8>> {
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

#[tokio::test]
async fn members_talk_through_the_relay_and_a_removed_member_drops_out() {
    let identity = Identity::self_signed(&["localhost", "127.0.0.1"]).unwrap();
    let listener = Listener::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
    let base = format!("https://{}/", listener.local_addr().unwrap());
    let relay = sigil_relay::spawn(listener, SECRET);

    // Alice's group with Bob in it.
    let services = Services::new().with(VOICE, Service::new("voice", Kind::Lossy, 1).unwrap()).unwrap();
    let alice = Device::new("alice").unwrap();
    let bob = Device::new("bob").unwrap();
    let mut a = Group::create(&alice, [4; 16], services.clone(), Policy::new("alice"), &Unverified).unwrap();
    let c = a.add(&alice, &[bob.key_package().unwrap()], &Unverified).unwrap();
    a.confirm(&alice, &Unverified).unwrap();
    let b = Group::join(&bob, c.welcome.as_ref().unwrap(), &Unverified).unwrap();

    // The server grants each member device a place in the voice room.
    let issuer = Issuer::new(SECRET);
    let gid = a.id();
    let join = |device: &Device, group: &Group| {
        let role = Some(group.policy().role_of(device.user()));
        let at = Room { group: &gid, services: group.services(), policy: group.policy(), service: VOICE };
        let grant = issuer.grant(at, &device.public_key(), role, now_ms()).unwrap();
        Connect { room: room(&gid, VOICE), grant: Some(grant), revoke: None }.url(&base)
    };
    let la = Link::connect(&join(&alice, &a), Some(identity.hash.clone())).await.unwrap();
    let lb = Link::connect(&join(&bob, &b), Some(identity.hash.clone())).await.unwrap();

    // Both derive the epoch's key; Alice speaks, Bob hears her by leaf.
    let mut sealer = Sealer::new(a.epoch(), &a.sframe_base(&alice, VOICE).unwrap(), a.leaf(), 0);
    let mut opener = Opener::new();
    opener.enter(b.epoch(), &b.sframe_base(&bob, VOICE).unwrap(), now_ms());
    tokio::time::sleep(Duration::from_millis(200)).await;
    la.send(&sealer.seal(KIND_AUDIO, TRACK_MAIN, b"seq", b"opus frame").unwrap()).await.unwrap();
    let got = received(&lb, 3_000).await.expect("Bob hears Alice");
    let (_relay_number, frame) = untag(&got).unwrap();
    let f = opener.open(frame, now_ms()).unwrap();
    assert_eq!((f.leaf, f.payload.as_slice()), (a.leaf(), b"opus frame".as_slice()));

    // Alice removes Bob: the server revokes his grants, and he hears no more.
    a.remove(&alice, &["bob"]).unwrap();
    a.confirm(&alice, &Unverified).unwrap();
    for token in issuer.remove(&gid, a.services(), &bob.public_key(), now_ms()) {
        relay.revoke(&token).await.unwrap();
    }
    sealer.rekey(a.epoch(), &a.sframe_base(&alice, VOICE).unwrap(), a.leaf());
    la.send(&sealer.seal(KIND_AUDIO, TRACK_MAIN, b"seq", b"after").unwrap()).await.unwrap();
    assert_eq!(received(&lb, 500).await, None, "Bob was taken out of the room");
}
