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
    let mut topics: Vec<idea_state::Topic> = db
        .0
        .spaces_for_user(user)
        .await
        .map_err(srv::internal)?
        .into_iter()
        .map(|s| idea_state::Topic::space(&s.id.to_string()))
        .collect();
    // Every channel this account can see, by the same rule that governs
    // reading one. A message is published to its channel's topic, so a DM
    // reaches its two participants and nobody else — a space topic would have
    // been wrong for exactly the conversations that matter most.
    topics.extend(
        db.0
            .visible_channels(user)
            .await
            .map_err(srv::internal)?
            .into_iter()
            .map(|c| idea_state::Topic::channel(&c.to_string())),
    );
    // Its own account, for things that concern this person and no one else.
    topics.push(idea_state::Topic::user(&user.to_string()));

    let mut sub = hub.0.connect(&user.to_string(), topics);

    // Whatever room this connection put the user in, so hanging up the socket
    // takes them out of it. A closed tab is the common case and it says
    // nothing on its way out; without this the only thing removing them would
    // be the presence TTL, which is a backstop rather than a mechanism.
    let mut in_room: Option<String> = None;

    // The channel this connection is reading. Held here rather than looked up,
    // because it is a fact about this socket and nothing else knows it.
    let mut watching: Option<uuid::Uuid> = None;

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
            Next::FromClient(Some(Ok(Up::Action(Action::Watch { channel_id })))) => {
                match watch(&db.0, &hub.0, &sub, user, &channel_id).await {
                    Ok(ch) => watching = Some(ch),
                    Err(e) => {
                        if send_refusal(&mut ch, e).await.is_err() {
                            break Ok(());
                        }
                    }
                }
            }
            Next::FromClient(Some(Ok(Up::Action(action)))) => {
                match act(&db.0, &hub.0, user, watching, &action).await {
                    Ok(now_in) => {
                        if let Some(room) = now_in {
                            in_room = Some(room);
                        }
                    }
                    Err(e) => {
                        if send_refusal(&mut ch, e).await.is_err() {
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

/// Tell one client what a channel currently holds.
///
/// Answered as a restatement rather than an event, because "here is a
/// different channel's page" is not a change to what the client had — it is a
/// different subject. Numbering stays dense, so the next event still applies.
#[cfg(feature = "server")]
async fn watch(
    db: &hearth_db::Db,
    hub: &srv::LiveHub,
    sub: &idea_state::hub::Subscription<crate::live::Live>,
    user: uuid::Uuid,
    channel_id: &str,
) -> Result<uuid::Uuid, ServerError> {
    let channel = srv::parse_id(channel_id)?;
    srv::require_channel_access(db, channel, user).await?;

    let rows = db.channel_page(channel, None).await.map_err(srv::internal)?;
    let transcript = srv::to_dtos(db, rows).await?;

    // Everything else in the state is unchanged, so it is read back out rather
    // than recomputed — the snapshot has to describe the whole world, not just
    // the part that moved.
    let mut state = hub.snapshot(&user.to_string(), sub.topics());
    state.watching = Some(channel_id.to_string());
    state.transcript = transcript;
    state.unread = db
        .unread_channels(user)
        .await
        .map_err(srv::internal)?
        .into_iter()
        .map(|c| c.to_string())
        .filter(|c| c != channel_id)
        .collect();

    hub.resnapshot(sub.id(), state);
    Ok(channel)
}

/// Send a refusal to the one client that asked.
///
/// Never published: nobody else needs to know what you were not allowed to do.
#[cfg(feature = "server")]
async fn send_refusal(
    ch: &mut Socket<Up<Action>, Frames>,
    e: ServerError,
) -> Result<(), server::SocketError> {
    let reason = match e {
        ServerError::Failed(m) => m,
        other => other.to_string(),
    };
    ch.send(Down::Refused { reason }).await
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
    watching: Option<uuid::Uuid>,
    action: &Action,
) -> Result<Option<String>, ServerError> {
    match action {
        // Handled by the caller, which owns the subscription it answers on.
        Action::Watch { .. } => Ok(None),

        Action::Post { id, channel_id, body, thread_root_id } => {
            let channel = srv::parse_id(channel_id)?;
            srv::require_channel_access(db, channel, user).await?;
            let message = db
                .post_message(
                    srv::parse_id(id)?,
                    channel,
                    user,
                    body,
                    thread_root_id.as_deref().map(srv::parse_id).transpose()?,
                )
                .await
                .map_err(srv::user_facing)?;
            let dto = srv::to_dtos(db, vec![message]).await?.pop().ok_or_else(|| {
                ServerError::failed("that message could not be read back")
            })?;

            // Reading a channel is what marks it read, and posting into one is
            // reading it — so the author's own pip never lights up for their
            // own message.
            if watching == Some(channel) {
                let _ = db.mark_read(user, channel, srv::parse_id(id)?).await;
            }

            hub.publish(
                &idea_state::Topic::channel(channel_id),
                Event::MessagePosted { channel_id: channel_id.clone(), message: dto },
            );
            Ok(None)
        }

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
