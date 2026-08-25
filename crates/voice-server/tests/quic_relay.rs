//! A real call, relayed over WebTransport.
//!
//! Not a unit test of the carrier: this stands up the actual node — the same
//! `serve` the WebSocket path runs, the same room bookkeeping — puts two
//! participants on it over QUIC, and checks that what one says the other hears.
//!
//! What it is really guarding is the pair of claims that are easy to get wrong
//! and impossible to see from a passing WebSocket suite:
//!
//! 1. Audio crosses the room over datagrams, decoded back to something that
//!    correlates with what was sent.
//! 2. A talker does not hear themselves. The no-self-echo rule is the relay's,
//!    and a new transport that lost it would produce a feedback loop the moment
//!    anyone opened a speaker.

use std::time::Duration;

use datagram::Link;
use voice_proto::codec::{Decoded, Encoded};
use voice_proto::Packet;

const RATE: u32 = 48_000;
const FRAME: usize = 960;

/// Speech-shaped, so the codec is being asked to do the job it was tuned for.
fn speech(n: usize, from: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = (from + i) as f32 / RATE as f32;
            let env = 0.5 + 0.5 * (t * 3.0 * std::f32::consts::TAU).sin();
            let mut v = 0.0;
            for h in 1..=6 {
                v += (t * 150.0 * h as f32 * std::f32::consts::TAU).sin() / h as f32;
            }
            v * 0.3 * env
        })
        .collect()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}

fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len()) as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        num += (x - ma) * (y - mb);
        da += (x - ma) * (x - ma);
        db += (y - mb) * (y - mb);
    }
    num / (da.sqrt() * db.sqrt()).max(f32::EPSILON)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_crosses_a_room_over_webtransport() {
    // Single-node mode: no coordination store, so this node owns every room.
    let (_router, state) = voice_server::node(None);
    let slot = state.webtransport_slot();

    // Port 0, and the LITERAL address on both sides. `localhost` resolves to
    // `::1` first on plenty of machines, and a client dialling that against a
    // v4-bound listener simply times out — indistinguishable from a broken
    // handshake or a bad certificate. It cost an hour once.
    let endpoint = voice_server::webtransport::listen(
        "127.0.0.1:0".parse().unwrap(),
        &["127.0.0.1"],
        "127.0.0.1",
        state,
        slot,
    )
    .expect("listener binds");
    let hash = unhex(&endpoint.cert_hash);

    let ana = Link::connect(&format!("{}?room=porch&id=11", endpoint.url), Some(hash.clone()))
        .await
        .expect("ana connects");
    let theo = Link::connect(&format!("{}?room=porch&id=22", endpoint.url), Some(hash))
        .await
        .expect("theo connects");

    // A datagram really is smaller than a raw packet would be. If this ever
    // stops holding, the assertion below about hearing anything would be the
    // one that fails, and it would look like a codec bug.
    let limit = ana.max_datagram().await;
    assert!(limit < 1952, "a datagram ({limit}) is smaller than 20 ms of raw PCM");

    // Theo listens while Ana talks. Both run in the background so the relay is
    // being driven from both ends at once, as in a real call.
    let listen = tokio::spawn(async move {
        let mut decoded = Decoded::default();
        let mut heard: Vec<f32> = Vec::new();
        let mut senders = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while heard.len() < FRAME * 20 && tokio::time::Instant::now() < deadline {
            let left = deadline - tokio::time::Instant::now();
            let Ok(event) = tokio::time::timeout(left, theo.recv()).await else { break };
            match event {
                datagram::Event::Data(bytes) => {
                    let packet = Packet::decode(&bytes).expect("a well-formed packet");
                    senders.push(packet.sender);
                    heard.extend(decoded.pcm(&packet).expect("decodable"));
                }
                // Notices — the node says what suppression the room is getting.
                datagram::Event::Control(_) => {}
                datagram::Event::Closed => break,
            }
        }
        (heard, senders)
    });

    let mut encoder = Encoded::open(RATE, 1, FRAME);
    assert_eq!(
        encoder.encoding(),
        voice_proto::Encoding::Opus,
        "raw PCM would not fit a datagram, so this test would be measuring the wrong thing",
    );
    let mut said: Vec<f32> = Vec::new();
    let mut ana_heard_itself = 0;
    for seq in 0..40u32 {
        let block = speech(FRAME, seq as usize * FRAME);
        let packet = encoder.packet(11, seq, RATE, 1, seq as u64 * 20_000, &block);
        ana.send(&packet.encode()).await.expect("a frame fits a datagram");
        said.extend(block);
        // Real time, because the relay is a real server and a burst of forty
        // frames in no time is not what it is being asked to survive.
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Anything coming back to Ana within this window would be her own
        // voice: nobody else is talking.
        if let Ok(datagram::Event::Data(_)) =
            tokio::time::timeout(Duration::from_millis(1), ana.recv()).await
        {
            ana_heard_itself += 1;
        }
    }

    let (heard, senders) = listen.await.expect("the listener finishes");

    assert!(!heard.is_empty(), "theo heard nothing at all");
    assert!(
        senders.iter().all(|s| *s == 11),
        "everything theo heard should be from ana, got {senders:?}",
    );
    assert_eq!(ana_heard_itself, 0, "a talker must not hear their own voice back");

    // Aligned at the start: Opus adds no algorithmic delay of its own at these
    // settings, and the buffer is fed in order.
    let n = heard.len().min(said.len());
    // The first few frames are the encoder warming up; correlating them
    // measures that rather than the transport.
    let from = FRAME * 4;
    assert!(n > from + FRAME * 4, "not enough audio arrived to judge: {n} samples");
    let corr = correlation(&said[from..n], &heard[from..n]);
    assert!(
        corr > 0.6,
        "what theo heard should resemble what ana said, correlation {corr}",
    );

    println!(
        "over QUIC: {} packets, {} samples, correlation {corr:.3}, datagram limit {limit}",
        senders.len(),
        heard.len(),
    );
}
