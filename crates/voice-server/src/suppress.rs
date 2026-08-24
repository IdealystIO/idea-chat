//! Server-side neural suppression: DeepFilterNet on the relay, once per sender.
//!
//! ```text
//!   sender ──▶ [decode] ──▶ AudioWriter ──▶ DFN3 ──▶ Leveler ──▶ [encode] ──▶ room
//!             tokio task    ────────── suppression host thread ──────────
//! ```
//!
//! # Why a thread and not a task
//!
//! `media_stream::AudioStream` is `!Send` by design — on the platform it was
//! written for it holds a main-thread native source. Both ends of the denoise
//! SDK speak in `AudioStream`, so the handles cannot live in a `tokio::spawn`
//! future, which is where every relay connection lives.
//!
//! The `Send` half of the pair, [`AudioWriter`], has no such problem. So the
//! split is: one dedicated OS thread owns every `!Send` handle on the node,
//! and each connection holds only a writer and pushes into it. One thread for
//! the whole node, not one per participant — the actual inference already runs
//! on a worker the SDK spawns per stream, and a second parked thread each
//! would be pure overhead.
//!
//! # What it costs
//!
//! Measured with `cargo run --release -p voice-server --bin dfn-bench` on an
//! 8-core Ryzen 7 7700X: **0.11 ms of CPU per 10 ms hop** — a real-time factor
//! of 0.011, and 443× real time aggregate across 8 concurrent streams. A node
//! carries hundreds of simultaneous talkers before suppression is the thing
//! that runs out. Model build is ~75 ms, paid once per participant at join.
//!
//! That number is why `Mode::Server` is offered at all. It is also why it is
//! still gated (see [`allowed`]): cheap per stream is not the same as free,
//! and a client that could spend a shared machine's CPU by asking is a lever
//! worth keeping a hand on.
//!
//! # The cost that is not CPU
//!
//! Suppression here happens *after* the wire, so the noise has already been
//! paid for in bandwidth by the time it is removed, and roughly 30 ms of
//! DeepFilterNet's algorithmic latency lands in the middle of the call path
//! rather than at its edge. Neither is fatal. Both are reasons `Mode::Client`
//! exists.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use denoise::Denoiser;
use media_stream::{AudioFrame, AudioStream, AudioSubscription, AudioWriter};
use voice_dsp::{Leveler, Tuning};

/// Environment variable that permits [`Mode::Server`](voice_dsp::Mode::Server).
///
/// Off unless set: a node in an unfamiliar deployment should relay, which is
/// the job it advertises, rather than quietly accepting inference work on
/// behalf of whoever connects.
pub const ENV_ALLOW: &str = "VOICE_DENOISE";

/// Whether this node will honour a request for server-side suppression.
pub fn allowed() -> bool {
    matches!(std::env::var(ENV_ALLOW).as_deref(), Ok("on") | Ok("1") | Ok("true"))
}

/// Text frame telling a client what suppression it actually got.
///
/// Sent on every connection, including when the answer is "none" — a client
/// that asked for server-side and silently did not get it would spend the call
/// wondering why it still sounds like a café, and the operator would have no
/// way to tell that from a model that simply is not helping.
pub const INFO_PREFIX: &str = "info:suppress=";

/// Levelled 48 kHz mono audio plus its presentation timestamp, ready to be
/// packetised and fanned out.
///
/// The timestamp is the *denoised* stream's, not the sender's. It has to be:
/// the model reframes to its own 10 ms hops, so an incoming packet's timestamp
/// no longer describes any particular block coming out. This one is a real
/// monotonic 48 kHz clock, which is more than the web capture clock it
/// replaces could claim (see `voice_proto`'s note on `pts_micros`).
pub type Sink = Box<dyn FnMut(&[f32], u64) + Send + 'static>;

enum Cmd {
    Open {
        key: u64,
        tuning: Tuning,
        sink: Sink,
        reply: tokio::sync::oneshot::Sender<Result<AudioWriter, String>>,
    },
    Close {
        key: u64,
    },
}

/// One sender's pipeline, living on the host thread. Field order is the
/// teardown order: the subscription stops first, then the streams, so the
/// engine is never asked for output after its sink is gone.
struct Pipeline {
    _sub: AudioSubscription,
    _clean: AudioStream,
    _input: AudioStream,
}

/// A handle onto the node's suppression host. Cheap to clone; the thread
/// behind it lives as long as the process.
#[derive(Clone)]
pub struct Suppression {
    tx: mpsc::Sender<Cmd>,
}

/// A connection's push end. Dropping it tears down that sender's pipeline.
pub struct Feed {
    key: u64,
    writer: AudioWriter,
    host: mpsc::Sender<Cmd>,
}

impl Feed {
    /// Hand one decoded packet to the suppressor.
    ///
    /// The samples are the packet's own rate and channel count — the engine
    /// downmixes and resamples to 48 kHz mono itself, so a client whose
    /// microphone runs at 44.1 kHz needs no special case here.
    pub fn push(&self, sample_rate: u32, channels: u16, samples: &[f32]) {
        self.writer.write_pcm_f32(sample_rate, channels, samples);
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // Best-effort: if the host thread is gone the process is going down
        // with it, and there is nothing left to leak.
        let _ = self.host.send(Cmd::Close { key: self.key });
    }
}

/// Hands out pipeline keys. Monotonic rather than the participant id, because
/// a reconnecting participant may briefly overlap with their own outgoing
/// connection and must not close its successor's pipeline.
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

impl Suppression {
    /// Start the node's suppression host.
    ///
    /// Called once at startup even when no client ever asks for server-side
    /// suppression — an idle thread blocked on a channel costs nothing, and
    /// starting it lazily would put a thread spawn on the join path.
    pub fn start() -> Suppression {
        let (tx, rx) = mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("suppress-host".into())
            .spawn(move || host(rx))
            .expect("suppression host thread");
        Suppression { tx }
    }

    /// Open a pipeline for one sender.
    ///
    /// `sink` is called on the denoise worker thread with levelled 48 kHz mono
    /// blocks. Awaiting rather than blocking matters: the caller is a tokio
    /// task on a shared runtime, and the model build behind this is ~75 ms.
    pub async fn open(&self, tuning: Tuning, sink: Sink) -> Result<Feed, String> {
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Open { key, tuning, sink, reply })
            .map_err(|_| "suppression host is not running".to_string())?;
        let writer = answer.await.map_err(|_| "suppression host dropped the request".to_string())??;
        Ok(Feed { key, writer, host: self.tx.clone() })
    }
}

/// The host thread's loop.
///
/// Opens are handled one at a time, which serialises model builds. At ~75 ms
/// each that is only visible when a crowd joins the same room in the same
/// second — the tenth arrival waits about three quarters of a second for
/// suppression to come up, and hears an ordinary relay until it does, because
/// the socket is already carrying audio by then.
fn host(rx: mpsc::Receiver<Cmd>) {
    let mut live: HashMap<u64, Pipeline> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Open { key, tuning, sink, reply } => {
                match open_pipeline(tuning, sink) {
                    Ok((pipeline, writer)) => {
                        live.insert(key, pipeline);
                        // A receiver that has gone away means the connection
                        // dropped during the build. Dropping the pipeline we
                        // just made is the right answer, not an error.
                        if reply.send(Ok(writer)).is_err() {
                            live.remove(&key);
                        }
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::Close { key } => {
                live.remove(&key);
            }
        }
    }
}

/// Build one sender's pipeline. Runs on the host thread, where `!Send` is fine.
fn open_pipeline(tuning: Tuning, mut sink: Sink) -> Result<(Pipeline, AudioWriter), String> {
    let (input, writer) = AudioStream::new();

    let mut denoiser = Denoiser::new().post_filter(tuning.post_filter);
    if let Some(limit) = tuning.atten_lim_db {
        denoiser = denoiser.attenuation_limit_db(limit);
    }
    // `block_on` is correct here and nowhere else in this crate: this thread
    // exists to own blocking, `!Send` work, and the future it drives is
    // waiting on the SDK's own worker thread rather than on any I/O this
    // runtime would otherwise be driving.
    let clean = pollster::block_on(denoiser.process(&input))
        .map_err(|e| format!("denoiser: {e}"))?;

    // Levelling runs in the callback so it sees exactly what the model emits:
    // 48 kHz mono, hop-aligned. The scratch buffer is reused because this runs
    // 100 times a second per talker and an allocation per hop is a needless
    // one.
    let mut leveler = tuning.leveling.map(|cfg| Leveler::new(cfg, 48_000));
    let mut scratch: Vec<f32> = Vec::new();
    let sub = clean.subscribe(move |frame: &AudioFrame| {
        match leveler.as_mut() {
            Some(leveler) => {
                scratch.clear();
                scratch.extend_from_slice(frame.samples);
                leveler.process(&mut scratch);
                sink(&scratch, frame.pts_micros);
            }
            None => sink(frame.samples, frame.pts_micros),
        }
    });

    Ok((Pipeline { _sub: sub, _clean: clean, _input: input }, writer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn tuning() -> Tuning {
        Tuning::default()
    }

    // The end-to-end shape the relay depends on: push a sender's packets in at
    // their own rate, get 48 kHz mono blocks out on the sink.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pipeline_cleans_and_emits() {
        let host = Suppression::start();
        let heard = Arc::new(Mutex::new(0usize));
        let feed = {
            let heard = heard.clone();
            host.open(tuning(), Box::new(move |block: &[f32], _pts: u64| {
                *heard.lock().unwrap() += block.len();
            }))
            .await
            .expect("pipeline opens")
        };

        // 1 s of 16 kHz mono hiss — a rate the model must resample.
        let mut seed = 0x1234_5678u32;
        for _ in 0..100 {
            let chunk: Vec<f32> = (0..160)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0
                })
                .collect();
            feed.push(16_000, 1, &chunk);
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while *heard.lock().unwrap() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(*heard.lock().unwrap() > 0, "cleaned audio reached the sink");
    }

    // Dropping the feed must actually tear the pipeline down. If it did not,
    // a node would accumulate one DeepFilterNet instance per call ever made —
    // a leak that only shows up after a day of uptime.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_feed_closes_the_pipeline() {
        let host = Suppression::start();
        let alive = Arc::new(Mutex::new(true));

        {
            let alive = alive.clone();
            let _feed = host
                .open(tuning(), Box::new(move |_: &[f32], _: u64| {
                    let _ = &alive;
                }))
                .await
                .expect("opens");
        }

        // The Close command has to reach the host thread; give it a moment,
        // then prove the host is still serving by opening another.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            host.open(tuning(), Box::new(|_: &[f32], _: u64| {})).await.is_ok(),
            "the host survives a closed pipeline",
        );
        assert!(*alive.lock().unwrap());
    }

    #[test]
    fn the_gate_is_shut_by_default() {
        // Guarding the real variable would make this test order-dependent, so
        // it asserts the parse rule that `allowed` is made of.
        for value in ["on", "1", "true"] {
            assert!(matches!(Some(value), Some("on") | Some("1") | Some("true")));
        }
        for value in ["off", "0", "", "yes"] {
            assert!(!matches!(Some(value), Some("on") | Some("1") | Some("true")), "{value}");
        }
    }
}
