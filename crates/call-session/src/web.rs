//! The web session; see the crate docs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use std::cell::Cell;

use call_proto::relay::{untag, Announce, Connect, FromRelay, LossReport, ToRelay, TAG};
use call_proto::Datagram;
use datagram::{Event, Link, LinkError};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};

use call_audio::{ModelSettings, Processing, Stats, Suppression, VoiceStages};

/// Voice packets waiting to be sent. 200 ms: a send path that far behind will
/// not catch up by being allowed to fall further.
const AUDIO_QUEUE: usize = 10;
const DIAL_TIMEOUT_MS: i32 = 5_000;

/// A fresh grant, fetched however the issuer hands them out.
pub type Fetch = Rc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, String>>>>>;

/// Where fresh grants come from as the current one runs out.
#[derive(Clone)]
pub enum Renew {
    /// GET this URL; the body is the grant.
    Url(String),
    /// Ask the app — a server function behind a session, say.
    With(Fetch),
}

/// Where in the relay we go, and what lets us in.
#[derive(Clone, Default)]
pub struct Access {
    /// Empty for a relay without rooms.
    pub room: String,
    /// A `call-auth` grant, for a relay that checks them.
    pub grant: Option<String>,
    /// Where fresh grants come from. Without one the call ends when the
    /// grant does.
    pub renew: Option<Renew>,
    /// The room's end-to-end key (see `call_e2ee`). With one, everything we
    /// send is sealed and only sealed media from others is played; without,
    /// media goes as the relay sees it. Everyone in the room needs the same.
    pub key: Option<call_e2ee::RoomKey>,
}

/// How the call's audio starts.
#[derive(Debug, Clone)]
pub struct Settings {
    pub processing: Processing,
    pub voice: VoiceStages,
    pub model: ModelSettings,
    /// Where the DeepFilterNet model is served from, for the built-in
    /// neural stage (see `call_audio`).
    pub model_path: &'static str,
    /// Our gains to start with: by user (on a relay that names people), and
    /// for everyone else — so the first word anyone says is already at the
    /// level this device keeps for them.
    pub gains: HashMap<String, f32>,
    pub default_gain: f32,
}

/// Who is in the room, as the relay and the clients say: names by relay
/// number (from grants — empty on a relay that takes none), and who has
/// their mic off.
///
/// One user can hold several numbers at once — two tabs, or a connection
/// that has not timed out yet behind the one that replaced it — so lookups by
/// user answer with all of them.
#[derive(Debug, Clone, Default)]
pub struct Roster {
    pub names: HashMap<u32, String>,
    pub mic_off: HashSet<u32>,
    /// Our own relay number, once told.
    pub me: Option<u32>,
    /// Our gain on each user, by name, and on anyone not named here — applied
    /// to every number a user has, the moment the relay names it, so nobody
    /// is heard at the wrong level for their first word.
    pub gains: HashMap<String, f32>,
    pub default_gain: f32,
    /// Who is talking, as each says of themselves ([`Announce::Speaking`]).
    pub speaking: HashSet<u32>,
    /// Relay numbers whose media we cannot play: with a key, theirs does not
    /// open with it (another key, or none); without one, theirs is sealed.
    /// Cleared for anyone as soon as something of theirs plays.
    pub unreadable: HashSet<u32>,
}

impl Roster {
    /// Every relay number whose grant names `user`.
    pub fn ids_of(&self, user: &str) -> Vec<u32> {
        self.names.iter().filter(|(_, u)| u.as_str() == user).map(|(&id, _)| id).collect()
    }

    /// One of them, if any.
    pub fn id_of(&self, user: &str) -> Option<u32> {
        self.ids_of(user).into_iter().max()
    }

    /// Our gain on relay number `id`.
    pub fn gain_of(&self, id: u32) -> f32 {
        self.names.get(&id).and_then(|u| self.gains.get(u)).copied().unwrap_or(self.default_gain)
    }
}

/// How often we ask the relay for a round trip.
const PING_MS: i32 = 2_000;

/// How often our own voice detection is read for the room.
const ACTIVITY_MS: i32 = 50;
/// How long we must be quiet before the room is told: a word's gaps would
/// otherwise flicker everyone's indicator. Starting is told at once.
const QUIET_MS: f64 = 250.0;

/// The link's own numbers, for the connection indicator. Bytes are
/// cumulative, each way, by what they carry; the page turns them into rates.
#[derive(Debug, Default, Clone, Copy)]
pub struct Traffic {
    pub audio_up: u64,
    pub audio_down: u64,
    pub video_up: u64,
    pub video_down: u64,
    /// Round trip to the relay, smoothed, in ms. `None` until the first pong.
    pub rtt_ms: Option<f64>,
    /// When the last pong came, on `call_audio::now_ms`. A stale one means
    /// the relay has stopped answering.
    pub last_pong_ms: f64,
    /// Whether the link is still open.
    pub open: bool,
    /// Datagrams dropped for not opening — see [`Roster::unreadable`].
    pub unreadable: u64,
    /// What a throttling relay is holding back from us, our download being
    /// unable to take it all: 0 for nothing; see `call_rate::THROTTLE_MAX`.
    pub held_back: u8,
}

/// A live call. Dropping it hangs up, releases the mic and stops playback.
pub struct Call {
    link: Link,
    /// Whether our mic is on, as announced to the room — again to each
    /// newcomer, by the receive loop.
    mic_on: Rc<Cell<bool>>,
    /// Whether we are talking, as told to the room — held over a word's gaps.
    speaking: Rc<Cell<bool>>,
    audio: call_audio::Sender,
    volumes: call_audio::Volumes,
    pub stats: Rc<RefCell<Stats>>,
    /// The camera, and everyone else's.
    pub video: call_video::Video,
    /// Everyone we can hear, with how loud they were just now.
    pub people: call_audio::Levels,
    pub traffic: Rc<RefCell<Traffic>>,
    pub roster: Rc<RefCell<Roster>>,
    encrypted: bool,
    rate: Rc<RefCell<call_rate::Controller>>,
}

/// What bitrate control is doing, for the page.
#[derive(Debug, Clone, Copy)]
pub struct RateStatus {
    pub mode: call_rate::Mode,
    /// The last decision; `None` in the first second.
    pub plan: Option<call_rate::Plan>,
    /// What the voice is coded at, and the video sent at, now.
    pub audio: call_audio::Rate,
    pub video_bps: u32,
    /// The loss receivers report for us — the least of them; `None` with
    /// nobody reporting.
    pub loss: Option<f64>,
}

/// How often loss is reported and the bitrate decided.
const RATE_MS: i32 = 1_000;

impl Drop for Call {
    fn drop(&mut self) {
        // Closing the link ends the receive loop, which drops every output.
        // Dropping the sender drops the send queue, which ends the send task.
        self.link.close(0, "bye");
    }
}

/// Dial the relay and start the call. `on_control` hears every control
/// message the session does not consume itself, as text
/// (`call_proto::relay::FromRelay::parse` reads it).
pub async fn join(
    url: &str,
    cert_hash: &str,
    access: Access,
    settings: Settings,
    on_control: impl Fn(String) + 'static,
) -> Result<Call, String> {
    let Settings { processing, voice, model, model_path, gains, default_gain } = settings;
    // No hash: the relay has a CA-issued certificate and the browser checks it
    // the ordinary way.
    let hash = match cert_hash.trim() {
        "" => None,
        hex => Some(
            datagram::parse_hash(hex).ok_or_else(|| "the certificate hash is not 64 hex characters".to_string())?,
        ),
    };
    if processing.suppression == Suppression::Neural {
        call_audio::prepare_neural(model_path).await?;
    }

    let target = Connect { room: access.room.clone(), grant: access.grant.clone(), revoke: None }.url(url.trim());
    let dial = Link::connect(&target, hash).fuse();
    let deadline = sleep(DIAL_TIMEOUT_MS).fuse();
    futures::pin_mut!(dial, deadline);
    let link = futures::select! {
        dialled = dial => dialled.map_err(|e| match access.grant {
            // A refused grant and an unreachable relay look the same from
            // here: the browser does not say which status came back.
            Some(_) => format!("{url} did not let us in: {e} — is the grant for this room, and still valid?"),
            None => format!("{url} did not open: {e}"),
        })?,
        _ = deadline => return Err(format!(
            "{url} did not answer within {DIAL_TIMEOUT_MS} ms — is the relay running, \
             and is its UDP port reachable from here?"
        )),
    };

    // One sealer for audio and video both, so they count on one counter.
    let sealer = match &access.key {
        Some(key) => Some(Rc::new(RefCell::new(call_e2ee::Sealer::new(key, 0, random_u32()?)))),
        None => None,
    };
    let opener = access.key.clone().map(|key| call_e2ee::Opener::new(key, 0));
    let encrypted = sealer.is_some();

    let stats = Rc::new(RefCell::new(Stats::default()));
    let max_datagram = link.max_datagram().await;
    stats.borrow_mut().max_datagram = max_datagram;

    let traffic = Rc::new(RefCell::new(Traffic { open: true, ..Default::default() }));
    let audio_packets = {
        let (stats, traffic) = (stats.clone(), traffic.clone());
        spawn_send(link.clone(), AUDIO_QUEUE, "audio", sealer.clone(), move |sent, bytes| {
            let mut s = stats.borrow_mut();
            if sent {
                s.sent += 1;
                traffic.borrow_mut().audio_up += bytes as u64;
            } else {
                s.unsent += 1
            }
        })
    };
    let audio = call_audio::Sender::open(
        call_audio::SendSettings { processing, voice, model, model_path },
        audio_packets,
        stats.clone(),
    )
    .await?;

    let video_packets = {
        let traffic = traffic.clone();
        spawn_send(link.clone(), call_video::SEND_QUEUE, "video", sealer.clone(), move |sent, bytes| {
            if sent {
                traffic.borrow_mut().video_up += bytes as u64;
            }
        })
    };
    let control: Rc<dyn Fn(String)> = {
        let link = link.clone();
        Rc::new(move |text: String| {
            let link = link.clone();
            runtime_core::driver::spawn_async(async move {
                let _ = link.control(&text).await;
            });
        })
    };
    // Room for the relay's sender tag on every datagram, and for sealing.
    let room = TAG + if encrypted { call_e2ee::OVERHEAD } else { 0 };
    let video = call_video::Video::new(video_packets, control, max_datagram.saturating_sub(room));

    let receiver = call_audio::Receiver::new(stats.clone());
    let (people, volumes) = (receiver.levels(), receiver.volumes());
    let mic_on = Rc::new(Cell::new(true));
    let speaking = Rc::new(Cell::new(false));
    let roster = Rc::new(RefCell::new(Roster { gains, default_gain, ..Default::default() }));
    let shared =
        Shared { traffic: traffic.clone(), mic_on: mic_on.clone(), speaking: speaking.clone(), roster: roster.clone() };
    let named = access.grant.is_some();
    let rate = Rc::new(RefCell::new(call_rate::Controller::default()));
    let meter = Rc::new(RefCell::new(call_rate::LossMeter::default()));
    let rating = Rating { controller: rate.clone(), meter: meter.clone() };
    spawn_receive(link.clone(), receiver, video.clone(), access.renew, named, opener, shared, rating, on_control);
    spawn_ping(link.clone());
    spawn_activity(link.clone(), stats.clone(), mic_on.clone(), speaking.clone(), traffic.clone());
    spawn_rate(link.clone(), rate.clone(), meter, audio.rate_cell(), video.clone(), stats.clone(), traffic.clone());
    Ok(Call { link, mic_on, speaking, audio, volumes, stats, video, people, traffic, roster, encrypted, rate })
}

/// Set our gain on one person's audio track, live: linear, 1.0 as sent.
pub fn set_volume(active: &Rc<RefCell<Option<Call>>>, id: u32, track: u8, gain: f32) {
    if let Some(call) = active.borrow().as_ref() {
        call.volumes.set(id, track, gain);
    }
}

impl Call {
    /// Our gain on a person by their grant's user — on every connection they
    /// have, and on any they open later. False when nobody by that name is in
    /// the room yet (the gain still applies when they arrive).
    pub fn set_user_volume(&self, user: &str, gain: f32) -> bool {
        let ids = {
            let mut r = self.roster.borrow_mut();
            r.gains.insert(user.to_string(), gain);
            r.ids_of(user)
        };
        for &id in &ids {
            self.set_volume_both(id, gain);
        }
        !ids.is_empty()
    }

    /// Our gain on everyone without a gain of their own.
    pub fn set_default_volume(&self, gain: f32) {
        let ids: Vec<u32> = {
            let mut r = self.roster.borrow_mut();
            r.default_gain = gain;
            r.names.iter().filter(|(_, u)| !r.gains.contains_key(*u)).map(|(&id, _)| id).collect()
        };
        for id in ids {
            self.set_volume_both(id, gain);
        }
    }

    fn set_volume_both(&self, id: u32, gain: f32) {
        self.volumes.set(id, call_proto::TRACK_MAIN, gain);
        self.volumes.set(id, call_proto::TRACK_SCREEN, gain);
    }

    /// Whether our media is end-to-end encrypted — sealed with the room key
    /// before it leaves, and others' played only if it opens with it.
    pub fn encrypted(&self) -> bool {
        self.encrypted
    }

    /// Choose how the bitrate is set: automatically (the default) or by hand.
    /// Takes effect at the next decision, within a second.
    pub fn set_rate_mode(&self, mode: call_rate::Mode) {
        self.rate.borrow_mut().set_mode(mode);
    }

    /// What bitrate control is doing.
    pub fn rate_status(&self) -> RateStatus {
        let c = self.rate.borrow();
        RateStatus {
            mode: c.mode(),
            plan: c.plan(),
            audio: self.audio.rate(),
            video_bps: self.video.bitrate(),
            loss: c.loss(call_audio::now_ms()),
        }
    }

    /// Whether the link is still open.
    pub fn is_open(&self) -> bool {
        self.traffic.borrow().open
    }

    /// Whether our mic is muted.
    /// Whether we are talking, as the room is told: our own voice
    /// detection, held over a word's gaps, and never while muted.
    pub fn speaking(&self) -> bool {
        self.speaking.get()
    }

    pub fn muted(&self) -> bool {
        self.audio.muted()
    }

    /// The audio processing running now.
    pub fn processing(&self) -> Processing {
        self.audio.processing()
    }

    /// Our gain on one relay number's audio track.
    pub fn set_volume(&self, id: u32, track: u8, gain: f32) {
        self.volumes.set(id, track, gain);
    }
}

impl Call {
    /// Whether [`sync_screen_audio`] has something to do.
    pub fn screen_audio_out_of_sync(&self) -> bool {
        self.audio.screen_out_of_sync(self.video.screen_on() && self.video.screen_audio().is_some())
    }
}

/// Mute or unmute the microphone, live, and tell the room.
pub fn set_muted(active: &Rc<RefCell<Option<Call>>>, on: bool) {
    if let Some(call) = active.borrow().as_ref() {
        call.audio.set_muted(on);
        call.mic_on.set(!on);
        announce_mic(&call.link, !on);
    }
}

fn announce_mic(link: &Link, on: bool) {
    let link = link.clone();
    runtime_core::driver::spawn_async(async move {
        let _ = link.control(&Announce::Mic(on).to_text()).await;
    });
}

/// Start or stop sending the screen share's audio to match the share. Called
/// from the page's frame loop, so it follows the share whichever way it
/// changes — including being stopped from the browser's own control.
pub async fn sync_screen_audio(active: &Rc<RefCell<Option<Call>>>) {
    let job = {
        let slot = active.borrow();
        let Some(call) = slot.as_ref() else { return };
        let stream = if call.video.screen_on() { call.video.screen_audio() } else { None };
        let video = call.video.clone();
        call.audio.sync_screen(stream, move || video.screen_on())
    };
    job.await
}

/// Change DeepFilterNet's tuning on a live call.
pub async fn retune(active: &Rc<RefCell<Option<Call>>>, model: ModelSettings) -> Result<(), String> {
    let job = active.borrow().as_ref().map(|c| c.audio.retune(model));
    match job {
        Some(job) => job.await,
        None => Ok(()),
    }
}

/// Change the call's processing in place, keeping the link.
pub async fn reprocess(active: &Rc<RefCell<Option<Call>>>, processing: Processing) -> Result<(), String> {
    let job = active.borrow().as_ref().map(|c| c.audio.reprocess(processing));
    match job {
        Some(job) => job.await,
        None => Ok(()),
    }
}

/// Once a second, until the link closes: say what we lost of everyone, and
/// decide what we send at from what they say they lost of us — see
/// `call_rate`.
fn spawn_rate(
    link: Link,
    controller: Rc<RefCell<call_rate::Controller>>,
    meter: Rc<RefCell<call_rate::LossMeter>>,
    voice: Rc<Cell<call_audio::Rate>>,
    video: call_video::Video,
    stats: Rc<RefCell<Stats>>,
    traffic: Rc<RefCell<Traffic>>,
) {
    runtime_core::driver::spawn_async(async move {
        loop {
            sleep(RATE_MS).await;
            if !traffic.borrow().open {
                break;
            }
            let lost = meter.borrow_mut().window();
            if !lost.is_empty() {
                let _ = link.control(&LossReport(lost).to_text()).await;
            }
            let (video_min, video_max) = video.bitrate_range();
            let signals = call_rate::Signals {
                now_ms: call_audio::now_ms(),
                rtt_ms: traffic.borrow().rtt_ms,
                dropped: stats.borrow().unsent + video.stats().frames_dropped,
                video_min,
                video_max,
            };
            let plan = controller.borrow_mut().tick(signals);
            voice.set(call_audio::Rate { bps: plan.audio.bps, fullband: plan.audio.fullband });
            video.set_bitrate(plan.video_bps);
        }
    });
}

/// Choose which voice-processing stages run, live.
pub fn set_voice_processing(active: &Rc<RefCell<Option<Call>>>, stages: VoiceStages) {
    if let Some(call) = active.borrow().as_ref() {
        call.audio.set_voice(stages);
    }
}

/// Tell the room when we start and stop talking, until the link closes.
/// Rising at once; falling after [`QUIET_MS`] of quiet. Muted is quiet, said
/// at once, so nobody is lit for a word that was never sent.
fn spawn_activity(
    link: Link,
    stats: Rc<RefCell<Stats>>,
    mic_on: Rc<Cell<bool>>,
    told: Rc<Cell<bool>>,
    traffic: Rc<RefCell<Traffic>>,
) {
    runtime_core::driver::spawn_async(async move {
        let mut last_voice = f64::NEG_INFINITY;
        while traffic.borrow().open {
            let now = call_audio::now_ms();
            let on = mic_on.get();
            if on && stats.borrow().speaking {
                last_voice = now;
            }
            let speaking = on && now - last_voice < QUIET_MS;
            if speaking != told.get() {
                told.set(speaking);
                let _ = link.control(&Announce::Speaking(speaking).to_text()).await;
            }
            sleep(ACTIVITY_MS).await;
        }
    });
}

/// Ask the relay for a round trip every [`PING_MS`], until the link closes.
/// The answer is read by the receive loop.
fn spawn_ping(link: Link) {
    runtime_core::driver::spawn_async(async move {
        loop {
            let sent = call_audio::now_ms() as u64;
            if link.control(&ToRelay::Ping(sent).to_text()).await.is_err() {
                break;
            }
            sleep(PING_MS).await;
        }
    });
}

/// A queue of datagrams, drained onto the link — sealed on the way, in an
/// encrypted call. `on_sent` hears whether each one went, and its size.
fn spawn_send(
    link: Link,
    depth: usize,
    what: &'static str,
    sealer: Option<Rc<RefCell<call_e2ee::Sealer>>>,
    on_sent: impl Fn(bool, usize) + 'static,
) -> mpsc::Sender<Vec<u8>> {
    let (packets, mut queued) = mpsc::channel::<Vec<u8>>(depth);
    runtime_core::driver::spawn_async(async move {
        while let Some(packet) = queued.next().await {
            // Sealed at the last moment, so datagrams go out in counter order.
            let packet = match &sealer {
                Some(sealer) => match sealer.borrow_mut().seal(&packet) {
                    Some(sealed) => sealed,
                    None => continue,
                },
                None => packet,
            };
            match link.send(&packet).await {
                Ok(()) => on_sent(true, packet.len()),
                Err(LinkError::TooLarge { size, limit }) => {
                    on_sent(false, packet.len());
                    log(&format!("[call-session] {size}-byte {what} datagram over the {limit}-byte limit"));
                }
                Err(e) => {
                    log(&format!("[call-session] {what} send failed, link closed: {e}"));
                    break;
                }
            }
        }
    });
    packets
}

/// Bitrate control's half of the receive loop: what we lose of others, and
/// what they say they lose of us.
struct Rating {
    controller: Rc<RefCell<call_rate::Controller>>,
    meter: Rc<RefCell<call_rate::LossMeter>>,
}

/// What the receive loop keeps up to date for the page.
struct Shared {
    traffic: Rc<RefCell<Traffic>>,
    mic_on: Rc<Cell<bool>>,
    speaking: Rc<Cell<bool>>,
    roster: Rc<RefCell<Roster>>,
}

fn spawn_receive(
    link: Link,
    mut receiver: call_audio::Receiver,
    video: call_video::Video,
    renew: Option<Renew>,
    named: bool,
    mut opener: Option<call_e2ee::Opener>,
    shared: Shared,
    rating: Rating,
    on_control: impl Fn(String) + 'static,
) {
    let Shared { traffic, mic_on, speaking, roster } = shared;
    let Rating { controller, meter } = rating;
    let volumes = receiver.volumes();
    let on_control = Rc::new(on_control);
    // Bumped by each expiry notice, so only the newest schedules a renewal.
    let renewal = Rc::new(Cell::new(0u64));
    // `?lipsync=off` on the page shows video the moment it decodes, as before
    // lip sync — for hearing the difference, and for measuring it.
    let lipsync = !web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|q| q.contains("lipsync=off"));
    runtime_core::driver::spawn_async(async move {
        loop {
            match link.recv().await {
                Event::Data(bytes) => {
                    let Some((from, datagram)) = untag(&bytes) else { continue };
                    {
                        let mut t = traffic.borrow_mut();
                        match call_proto::media(datagram) {
                            Some(call_proto::Media::Audio) => t.audio_down += bytes.len() as u64,
                            Some(_) => t.video_down += bytes.len() as u64,
                            None => {}
                        }
                    }
                    // In an encrypted call, only what opens with the room
                    // key plays: anything else is someone with another key,
                    // someone not encrypting, or the relay meddling.
                    let opened;
                    let datagram = match opener.as_mut() {
                        // Sealed, and we hold no key: theirs is an encrypted
                        // call we are not part of.
                        None if call_proto::sealed(datagram) => {
                            if roster.borrow_mut().unreadable.insert(from) {
                                log(&format!("[call-session] media from #{from} is end-to-end encrypted, and we have no key"));
                            }
                            traffic.borrow_mut().unreadable += 1;
                            continue;
                        }
                        None => datagram,
                        Some(opener) => match opener.open(from, datagram) {
                            Ok(plain) => {
                                if roster.borrow().unreadable.contains(&from) {
                                    roster.borrow_mut().unreadable.remove(&from);
                                }
                                opened = plain;
                                &opened[..]
                            }
                            Err(why) => {
                                traffic.borrow_mut().unreadable += 1;
                                if roster.borrow_mut().unreadable.insert(from) {
                                    log(&format!("[call-session] media from #{from} does not open with the room key: {why:?}"));
                                }
                                continue;
                            }
                        },
                    };
                    let parsed = call_proto::parse(datagram);
                    // Every voice packet counts toward what we lose of them,
                    // held or not.
                    match &parsed {
                        Some(Datagram::Audio(p)) if p.track == call_proto::TRACK_MAIN => {
                            meter.borrow_mut().observe(from, p.seq)
                        }
                        Some(Datagram::Video(f)) => meter.borrow_mut().observe_video(from, f.track, f.frame, f.count),
                        _ => {}
                    }
                    match parsed {
                        Some(Datagram::Video(fragment)) => video.on_fragment(from, fragment),
                        // On a relay that names people, someone's level is
                        // known once they are named — and their name comes on
                        // the control stream, which QUIC does not order against
                        // datagrams. Until it lands their audio waits, rather
                        // than playing its first words at the wrong level.
                        Some(Datagram::Audio(_)) if named && !roster.borrow().names.contains_key(&from) => {}
                        Some(Datagram::Audio(packet)) => {
                            let track = packet.track;
                            // Audio is the clock video is held to.
                            if let Some(clock) = receiver.play(from, packet).await {
                                if lipsync {
                                    video.set_audio_clock(from, track, clock);
                                }
                            }
                        }
                        None => {}
                    }
                }
                Event::Control(text) => {
                    // Keyframe requests: video's business, not the page's.
                    if video.on_control(&text) {
                        continue;
                    }
                    match FromRelay::parse(&text) {
                        // Ours: a round trip, smoothed — not news for the page.
                        FromRelay::Pong(sent) => {
                            let now = call_audio::now_ms();
                            let rtt = (now - sent as f64).max(0.0);
                            let mut t = traffic.borrow_mut();
                            t.rtt_ms = Some(t.rtt_ms.map_or(rtt, |old| old * 0.7 + rtt * 0.3));
                            t.last_pong_ms = now;
                            continue;
                        }
                        FromRelay::Left(id) => {
                            receiver.left(id);
                            video.left(id);
                            controller.borrow_mut().left(id);
                            meter.borrow_mut().left(id);
                            if let Some(opener) = opener.as_mut() {
                                opener.left(id);
                            }
                            let mut r = roster.borrow_mut();
                            r.names.remove(&id);
                            r.mic_off.remove(&id);
                            r.speaking.remove(&id);
                            r.unreadable.remove(&id);
                        }
                        // A newcomer learns our mic state from us — and that
                        // we are mid-word, if we are.
                        FromRelay::Joined(_) => {
                            announce_mic(&link, mic_on.get());
                            if speaking.get() {
                                let link = link.clone();
                                runtime_core::driver::spawn_async(async move {
                                    let _ = link.control(&Announce::Speaking(true).to_text()).await;
                                });
                            }
                        }
                        FromRelay::You(id) => roster.borrow_mut().me = Some(id),
                        FromRelay::Throttled(level) => traffic.borrow_mut().held_back = level,
                        // Named before any of their audio is forwarded: their
                        // level applies from the first packet.
                        FromRelay::Named { id, ref user } => {
                            let gain = {
                                let mut r = roster.borrow_mut();
                                r.names.insert(id, user.clone());
                                r.gain_of(id)
                            };
                            volumes.set(id, call_proto::TRACK_MAIN, gain);
                            volumes.set(id, call_proto::TRACK_SCREEN, gain);
                        }
                        // What someone loses of everyone: ours is bitrate
                        // control's, and none of it is news for the page.
                        FromRelay::Said { from, ref text } if text.starts_with("rx") && LossReport::parse(text).is_some() => {
                            let report = LossReport::parse(text).expect("checked");
                            if let Some(loss) = roster.borrow().me.and_then(|me| report.loss_of(me)) {
                                controller.borrow_mut().report(from, loss, call_audio::now_ms());
                            }
                            continue;
                        }
                        FromRelay::Said { from, ref text } => {
                            match Announce::parse(text) {
                                Some(Announce::Mic(on)) => {
                                    let mut r = roster.borrow_mut();
                                    if on { r.mic_off.remove(&from) } else { r.mic_off.insert(from) };
                                    // Muted is quiet, whatever came last.
                                    if !on {
                                        r.speaking.remove(&from);
                                    }
                                }
                                Some(Announce::Speaking(on)) => {
                                    let mut r = roster.borrow_mut();
                                    if on { r.speaking.insert(from) } else { r.speaking.remove(&from) };
                                }
                                None => {}
                            }
                        }
                        FromRelay::Expires { secs } if secs > 0 => {
                            if let Some(renew) = &renew {
                                schedule_renewal(&link, renew.clone(), secs, &renewal, on_control.clone());
                            }
                        }
                        _ => {}
                    }
                    on_control(text);
                }
                Event::Closed => break,
            }
        }
        traffic.borrow_mut().open = false;
        on_control("link closed".to_string());
    });
}

/// Halfway to `secs` from now, fetch a fresh grant and hand it to the relay —
/// unless a newer expiry notice has scheduled its own by then.
fn schedule_renewal(link: &Link, renew: Renew, secs: u64, generation: &Rc<Cell<u64>>, on_control: Rc<dyn Fn(String)>) {
    let mine = generation.get() + 1;
    generation.set(mine);
    let (link, generation) = (link.clone(), generation.clone());
    runtime_core::driver::spawn_async(async move {
        sleep((secs * 1000 / 2).clamp(1000, i32::MAX as u64) as i32).await;
        if generation.get() != mine {
            return;
        }
        let fetched = match renew {
            Renew::With(fetch) => fetch().await,
            Renew::Url(url) => fetch_url(&url).await,
        };
        match fetched.map(|g| g.trim().to_string()).and_then(|g| if g.is_empty() { Err("an empty answer".into()) } else { Ok(g) }) {
            Ok(grant) => {
                let _ = link.control(&ToRelay::Renew(grant).to_text()).await;
            }
            Err(e) => on_control(FromRelay::Refused(format!("could not fetch a new grant: {e}")).to_text()),
        }
    });
}

/// A random id for our KID, from the browser's CSPRNG.
fn random_u32() -> Result<u32, String> {
    let mut bytes = [0u8; 4];
    web_sys::window()
        .and_then(|w| w.crypto().ok())
        .ok_or("no crypto.getRandomValues here, so no end-to-end encryption")?
        .get_random_values_with_u8_array(&mut bytes)
        .map_err(|e| format!("crypto.getRandomValues failed: {e:?}"))?;
    Ok(u32::from_be_bytes(bytes))
}

async fn fetch_url(url: &str) -> Result<String, String> {
    let response = net::Client::new().get(url).send().await.map_err(|e| e.to_string())?;
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

pub fn log(message: &str) {
    web_sys::console::log_1(&message.into());
}
