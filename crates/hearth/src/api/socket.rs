//! The live connection.
//!
//! One socket per signed-in client. On connect it is handed a snapshot of
//! everything it can see; after that both ends move through the same events.
//! Nothing here polls.
//!
//! The server half decides; the client half folds. An [`Action`] is a request
//! and can be refused — it is authorized against the same rules as the HTTP
//! API, because it *is* the same rules — while an [`Event`] has already
//! happened by the time it is on the wire.

use idea_state::{Down, Up};
use server::{Socket, ServerError};

use crate::live::{Action, Event, State};

#[cfg(feature = "server")]
use crate::api::types::Principal;
#[cfg(feature = "server")]
use crate::srv;

/// What the client sends and receives.
pub type Frames = Down<State, Event>;

/// The live channel.
///
/// Extractors run at upgrade, so an unauthenticated socket never opens —
/// authorization is not something this loop has to remember to do.
#[server::channel]
pub async fn live(
    mut ch: Socket<Up<Action>, Frames>,
    #[cfg(feature = "server")] who: server_kit::Auth<Principal>,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
    #[cfg(feature = "server")] hub: server::State<srv::LiveHub>,
) -> Result<(), ServerError> {
    let user = srv::parse_id(&who.0.user_id)?;

    // What this connection may see, resolved once. Everything downstream —
    // the snapshot and every event — is scoped by this list, so there is one
    // answer to "what can you see" rather than one per code path.
    let topics: Vec<idea_state::Topic> = db
        .0
        .spaces_for_user(user)
        .await
        .map_err(srv::internal)?
        .into_iter()
        .map(|s| idea_state::Topic::space(&s.id.to_string()))
        .collect();

    let mut sub = hub.0.connect(&user.to_string(), topics);

    // Whatever room this connection put the user in, so hanging up the socket
    // takes them out of it. A closed tab is the common case and it says
    // nothing on its way out; without this the only thing removing them would
    // be the presence TTL, which is a backstop rather than a mechanism.
    let mut in_room: Option<String> = None;

    let outcome = loop {
        // `select!` drops the unfinished futures when it completes, which is
        // what releases the borrow on `ch` — so the socket can be written to
        // in the branch bodies below.
        enum Next {
            FromClient(Option<Result<Up<Action>, server::SocketError>>),
            ToClient(Option<Frames>),
        }
        let next = tokio::select! {
            m = ch.recv() => Next::FromClient(m),
            f = sub.rx.recv() => Next::ToClient(f),
        };

        match next {
            // The hub has something for this client.
            Next::ToClient(Some(frame)) => {
                if ch.send(frame).await.is_err() {
                    break Ok(());
                }
            }
            // The hub dropped us — too far behind to catch up. Closing is the
            // honest move: the client reconnects into a fresh snapshot rather
            // than applying a backlog that no longer describes anything.
            Next::ToClient(None) => break Ok(()),

            Next::FromClient(Some(Ok(Up::Ping))) => {
                if ch.send(Down::Pong).await.is_err() {
                    break Ok(());
                }
            }
            Next::FromClient(Some(Ok(Up::Action(action)))) => {
                match act(&db.0, &hub.0, user, &action).await {
                    Ok(now_in) => in_room = now_in,
                    Err(e) => {
                        // A refusal is addressed to the one client that asked,
                        // not published: nobody else needs to know what you
                        // were not allowed to do.
                        let reason = match e {
                            ServerError::Failed(m) => m,
                            other => other.to_string(),
                        };
                        if ch.send(Down::Refused { reason }).await.is_err() {
                            break Ok(());
                        }
                    }
                }
            }
            // A frame we could not decode. One bad message is not a reason to
            // drop a working connection.
            Next::FromClient(Some(Err(_))) => continue,
            // The client hung up.
            Next::FromClient(None) => break Ok(()),
        }
    };

    if let Some(channel_id) = in_room {
        // Best effort on the way out. If this fails the TTL still catches it.
        let _ = leave(&db.0, &hub.0, user, &channel_id).await;
    }
    outcome
}

/// Authorize an action, make it true, and tell everyone who can see it.
///
/// Returns which voice room this connection is now in, so the socket can undo
/// it if the client disappears.
#[cfg(feature = "server")]
async fn act(
    db: &hearth_db::Db,
    hub: &srv::LiveHub,
    user: uuid::Uuid,
    action: &Action,
) -> Result<Option<String>, ServerError> {
    match action {
        Action::JoinVoice { channel_id } | Action::StillHere { channel_id } => {
            let channel = srv::parse_id(channel_id)?;
            // The same gate as everywhere else. A socket is not a way around
            // the rules, it is another way to reach them.
            srv::require_channel_access(db, channel, user).await?;
            let space = db
                .channel_space(channel)
                .await
                .map_err(srv::internal)?
                .ok_or_else(|| ServerError::failed("that room is not in a space"))?;

            let already = db.voice_present(user, channel).await.map_err(srv::internal)?;
            // A heartbeat for a room they are already in changes nothing, and
            // republishing it would wake every client in the space to be told
            // what they already know.
            if !already {
                let person = srv::member_dto(db, space, user).await?;
                hub.publish(
                    &idea_state::Topic::space(&space.to_string()),
                    Event::VoiceJoined {
                        space_id: space.to_string(),
                        channel_id: channel_id.clone(),
                        person,
                    },
                );
            }
            Ok(Some(channel_id.clone()))
        }
        Action::LeaveVoice { channel_id } => {
            leave(db, hub, user, channel_id).await?;
            Ok(None)
        }
    }
}

/// Take someone out of a room and say so.
#[cfg(feature = "server")]
async fn leave(
    db: &hearth_db::Db,
    hub: &srv::LiveHub,
    user: uuid::Uuid,
    channel_id: &str,
) -> Result<(), ServerError> {
    let channel = srv::parse_id(channel_id)?;
    let Some(space) = db.channel_space(channel).await.map_err(srv::internal)? else {
        return Ok(());
    };
    // Only announce a departure that actually happened — a duplicate leave is
    // harmless to the reducer but would still be a frame sent to everyone.
    if db.voice_absent(user, channel).await.map_err(srv::internal)? {
        hub.publish(
            &idea_state::Topic::space(&space.to_string()),
            Event::VoiceLeft {
                space_id: space.to_string(),
                channel_id: channel_id.to_string(),
                user_id: user.to_string(),
            },
        );
    }
    Ok(())
}
