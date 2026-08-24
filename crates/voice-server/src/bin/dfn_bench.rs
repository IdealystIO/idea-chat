//! What DeepFilterNet actually costs, on this machine.
//!
//! Server-side suppression is a capacity question before it is an audio
//! question: a relay that cannot run one instance per talking participant in
//! real time does not have a quality problem, it has an outage. And the number
//! that decides it is not in any README — it depends on the CPU, and on how
//! badly N instances contend for it.
//!
//! Three measurements:
//!
//! - **build** — how long `Denoiser::process` takes to load and plan the
//!   model. Paid once per participant, at join, before any audio flows. This
//!   is join latency, not throughput.
//! - **throughput** — the real-time factor of one instance. `rtf = 0.05` means
//!   one second of audio costs 50 ms of CPU, so one core could in principle
//!   carry 20 streams.
//! - **contention** — the same thing with N instances running at once, which
//!   is the number that matters, because "in principle" assumes the cores are
//!   not fighting over memory bandwidth, and they are.
//!
//! ```console
//! $ cargo run --release -p voice-server --bin dfn-bench
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use denoise::Denoiser;
use media_stream::{AudioFrame, AudioStream};

const RATE: u32 = 48_000;
/// Long enough that the model's internal state is warm and the average is not
/// dominated by the first few frames.
const SECONDS: usize = 20;

/// Speech-shaped-ish noise: a couple of voiced harmonics under broadband hiss.
/// The content barely matters for timing — DeepFilterNet's cost per hop is
/// fixed — but silence would be a suspicious fixture to benchmark on.
fn fixture(seconds: usize) -> Vec<f32> {
    let n = RATE as usize * seconds;
    let mut seed = 0x9E37_79B9u32;
    (0..n)
        .map(|i| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let hiss = (seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0;
            let t = i as f32 / RATE as f32;
            let voice = (t * 140.0 * std::f32::consts::TAU).sin() * 0.25
                + (t * 280.0 * std::f32::consts::TAU).sin() * 0.12;
            voice + hiss * 0.15
        })
        .collect()
}

/// One pipeline pushed flat out. Returns (build time, processing time, hops).
///
/// Timing ends when the output stops arriving rather than when the input is
/// written: the writer only hands chunks to the worker thread, so measuring
/// the writes would measure a memcpy.
fn run_one(audio: &[f32], counter: Arc<AtomicUsize>) -> (f64, f64, usize) {
    let (input, writer) = AudioStream::new();

    let built = Instant::now();
    let clean = pollster::block_on(Denoiser::new().attenuation_limit_db(24.0).post_filter(true).process(&input))
        .expect("model init");
    let build_secs = built.elapsed().as_secs_f64();

    let hops = Arc::new(AtomicUsize::new(0));
    let _sub = {
        let hops = hops.clone();
        let counter = counter.clone();
        clean.subscribe(move |f: &AudioFrame| {
            hops.fetch_add(1, Ordering::Relaxed);
            counter.fetch_add(f.frame_count(), Ordering::Relaxed);
        })
    };

    let started = Instant::now();
    // 10 ms at a time, exactly as the relay will feed it — chunk size changes
    // the framing work, and benchmarking one big write would flatter it.
    for chunk in audio.chunks(480) {
        writer.write_pcm_f32(RATE, 1, chunk);
    }
    let expected = audio.len() / 480;
    while hops.load(Ordering::Relaxed) < expected {
        std::thread::sleep(std::time::Duration::from_millis(1));
        if started.elapsed().as_secs() > 300 {
            eprintln!("gave up waiting: {} of {expected} hops", hops.load(Ordering::Relaxed));
            break;
        }
    }
    (build_secs, started.elapsed().as_secs_f64(), hops.load(Ordering::Relaxed))
}

fn main() {
    let audio = fixture(SECONDS);
    let audio_secs = SECONDS as f64;
    println!("DeepFilterNet 3 — {SECONDS}s of 48 kHz mono per stream, 10 ms hops\n");

    let counter = Arc::new(AtomicUsize::new(0));
    let (build, elapsed, hops) = run_one(&audio, counter.clone());
    let rtf = elapsed / audio_secs;
    println!("build      {:>8.0} ms   (paid once per participant, at join)", build * 1000.0);
    println!(
        "one stream {:>8.3} rtf  ({:.2} ms of CPU per 10 ms hop, {hops} hops)",
        rtf,
        elapsed / hops as f64 * 1000.0,
    );
    println!("           {:>8.0}      streams per core, if they never contended\n", 1.0 / rtf);

    // The honest number. `std::thread` rather than a runtime because each
    // pipeline owns `!Send` handles — the same constraint the relay lives with.
    for streams in [2usize, 4, 8, 16] {
        let counter = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let handles: Vec<_> = (0..streams)
            .map(|_| {
                let audio = audio.clone();
                let counter = counter.clone();
                std::thread::spawn(move || run_one(&audio, counter))
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        let elapsed = started.elapsed().as_secs_f64();
        // Aggregate: how many seconds of audio were cleaned per second of wall
        // clock. Above `streams` means headroom; below means the node is
        // already dropping behind real time at this many talkers.
        println!(
            "{streams:>2} streams  {:>8.1}x real time aggregate  ({:.3} rtf each)",
            streams as f64 * audio_secs / elapsed,
            elapsed / audio_secs,
        );
    }

    println!(
        "\nA participant is only a load while they are TALKING — silence still\n\
         costs a full inference per hop, because the model is what decides it\n\
         is silence. Size for the room, not for the conversation."
    );
}
