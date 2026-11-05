//! The live connection: one WebSocket per open client, carrying the
//! `idea_state` protocol over `chat_model::live`.
//!
//! ```text
//! client                                   server
//!   live_ticket() ─────────────────────▶   (a session-authenticated call)
//!   live(ticket) ── open ──────────────▶   check ticket, register topics
//!                 ◀── Snapshot { seq: 0, state }
//!                 ◀── Event { seq: n, event } …     (seq per connection)
//!   Up::Action(Typing { channel }) ────▶   fan out to who can see it
//!   Up::Ping ──────────────────────────▶
//!                 ◀── Pong
//! ```
//!
//! A ticket rather than the session itself because a native WebSocket open
//! carries no headers here: the ticket rides the URL, is good for a minute,
//! and only opens this one connection.
//!
//! The connection subscribes to its topics *before* taking the snapshot, so
//! an event that lands in between is delivered after it rather than lost —
//! and folds in harmlessly if the snapshot already had it (`apply` is
//! idempotent). A `Resync` event re-takes the snapshot and the topics.
//!
//! The connection is also presence: the first connection of an account
//! brings it online, the last one closing takes it offline, and a call the
//! connection joined ends with it.

use chat_model::live::{Action, Event, State};
use server::ServerError;

use crate::ApiError;

/// Server to client.
pub type Down = idea_state::Down<State, Event>;
/// Client to server.
pub type Up = idea_state::Up<Action>;

/// How long after one typing signal another for the same channel is
/// passed on.
pub const TYPING_EVERY_MS: i64 = 3_000;

#[server::channel]
pub async fn live(mut ch: server::Socket<Up, Down>, ticket: String) -> Result<(), ServerError<ApiError>> {
    use std::collections::HashMap;

    use chat_model::Presence;
    use idea_state::{Down as D, Up as U};

    use crate::srv::{app, channel_topic_of, check_ticket, now_ms, topics_for, topics_of};

    let a = app();
    let Some((user, device)) = check_ticket(&a.live_secret, &ticket, now_ms()) else {
        let _ = ch.send(D::Refused { reason: "signed out".into() }).await;
        return Ok(());
    };
    let conn = chat_db::new_id();
    let was = a.db.presence_one(user).await.map(|s| s.presence).unwrap_or(Presence::Offline);
    if a.db.open_connection(conn, user, device, &a.instance).await.is_err() {
        let _ = ch.send(D::Refused { reason: "unavailable".into() }).await;
        return Ok(());
    }
    if let Some(d) = device {
        let _ = a.db.touch_device(d).await;
    }
    let mut events = a.bus.router().register(conn, topics_of(user).await);
    let mut state = match a.db.snapshot(user).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[chat] snapshot for {user}: {e}");
            a.bus.router().unregister(conn);
            let _ = a.db.disconnect(conn).await;
            let _ = ch.send(D::Refused { reason: "unavailable".into() }).await;
            return Ok(());
        }
    };
    a.bus.router().retopic(conn, topics_for(&state));
    let mut seq = 0u64;
    let open = ch.send(D::Snapshot { seq, state: state.clone() }).await.is_ok();
    if open && was == Presence::Offline {
        crate::srv::announce_presence(user).await;
    }
    let mut typed: HashMap<chat_model::Id, i64> = HashMap::new();
    let mut idle = false;

    while open {
        tokio::select! {
            up = ch.recv() => match up {
                Some(Ok(U::Ping)) => {
                    if ch.send(D::Pong).await.is_err() { break; }
                }
                Some(Ok(U::Action(Action::Typing { channel }))) => {
                    let now = now_ms();
                    if typed.get(&channel).is_some_and(|t| now - t < TYPING_EVERY_MS) { continue; }
                    typed.insert(channel, now);
                    if !state.access(channel).can_post() { continue; }
                    if let Some(topic) = channel_topic_of(&state, channel) {
                        a.bus.publish(vec![topic], Event::Typing { channel, user }).await;
                    }
                }
                Some(Ok(U::Action(Action::Idle { idle: now_idle }))) => {
                    if now_idle != idle {
                        idle = now_idle;
                        let _ = a.db.set_idle(conn, idle).await;
                        crate::srv::announce_presence(user).await;
                    }
                }
                Some(Err(_)) | None => break,
            },
            ev = events.recv() => {
                let Some(ev) = ev else { break };
                match &*ev {
                    Event::Resync => {
                        match a.db.snapshot(user).await {
                            Ok(s) => {
                                state = s;
                                a.bus.router().retopic(conn, topics_for(&state));
                                if ch.send(D::Snapshot { seq, state: state.clone() }).await.is_err() { break; }
                            }
                            Err(e) => eprintln!("[chat] resnapshot for {user}: {e}"),
                        }
                    }
                    // A sealed key is for one device; only its connection hears it.
                    Event::KeyDelivered { key } if device != Some(key.to_device) => {}
                    e => {
                        state.apply(e);
                        seq += 1;
                        if ch.send(D::Event { seq, event: e.clone() }).await.is_err() { break; }
                    }
                }
            }
        }
    }

    a.bus.router().unregister(conn);
    let left = a.db.disconnect(conn).await.ok().flatten();
    crate::calls::connection_left(left).await;
    let now = a.db.presence_one(user).await.map(|s| s.presence).unwrap_or(Presence::Offline);
    if now != was || now == Presence::Offline {
        crate::srv::announce_presence(user).await;
    }
    Ok(())
}
