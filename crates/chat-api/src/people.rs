//! People: friend requests, friendships and blocks.

use chat_model::{Id, Relation, User};
use server::server;
use server_kit::Auth;

use crate::{Answer, Principal};

#[cfg(feature = "server")]
mod body {
    use chat_db::RelationChange;
    use chat_model::live::Event;
    use chat_model::Id;

    use crate::srv::{app, db, resync};
    use crate::Failure;

    /// Tell both sides how they now relate, and have them restate: what
    /// presence each may see of the other changed with it.
    pub async fn announce(me: Id, other: Id, change: RelationChange) -> Result<(), Failure> {
        let a = app();
        let users = a.db.users(&[me, other]).await.map_err(db)?;
        let find = |id: Id| users.iter().find(|u| u.id == id).cloned();
        if let Some(them) = find(other) {
            a.bus.to_user(me, Event::RelationChanged { user: them, relation: change.mine }).await;
        }
        // The blocked side only sees the friendship or request end, never
        // the block itself: their relation is `None`, not `Blocked`.
        if let Some(mine) = find(me) {
            a.bus.to_user(other, Event::RelationChanged { user: mine, relation: change.theirs }).await;
        }
        resync(&[me, other]).await;
        Ok(())
    }
}

/// Someone by username.
#[server(tags(limit = "60/min"))]
pub async fn find_user(handle: String, user: Auth<Principal>) -> Answer<Option<User>> {
    use crate::srv::{app, db, me};
    me(&user.0).await?;
    let handle = handle.trim().trim_start_matches('@');
    app().db.user_by_handle(handle).await.map_err(db)
}

/// Send a friend request by username — or accept one they sent.
#[server(tags(limit = "30/min"))]
pub async fn add_friend(handle: String, user: Auth<Principal>) -> Answer<Relation> {
    use crate::srv::{app, db, fail, me};
    use crate::ApiError;
    let account = me(&user.0).await?;
    let a = app();
    let handle = handle.trim().trim_start_matches('@');
    let Some(other) = a.db.user_by_handle(handle).await.map_err(db)? else {
        return Err(fail(ApiError::invalid("handle", "Nobody has that username. Check the spelling.")));
    };
    if other.id == account.id() {
        return Err(fail(ApiError::invalid("handle", "That's you.")));
    }
    let change = a.db.befriend(account.id(), other.id).await.map_err(db)?;
    body::announce(account.id(), other.id, change).await?;
    Ok(change.mine.unwrap_or(Relation::Outgoing))
}

/// Accept a request, by person.
#[server(tags(limit = "60/min"))]
pub async fn accept_friend(other: Id, user: Auth<Principal>) -> Answer<()> {
    use crate::srv::{app, db, fail, me};
    use crate::ApiError;
    let account = me(&user.0).await?;
    let a = app();
    if a.db.relation(account.id(), other).await.map_err(db)? != Some(Relation::Incoming) {
        return Err(fail(ApiError::NotFound));
    }
    let change = a.db.befriend(account.id(), other).await.map_err(db)?;
    body::announce(account.id(), other, change).await
}

/// Remove a friend, decline a request, cancel one sent, or unblock.
#[server(tags(limit = "60/min"))]
pub async fn remove_relation(other: Id, user: Auth<Principal>) -> Answer<()> {
    use crate::srv::{app, db, me};
    let account = me(&user.0).await?;
    let change = app().db.unfriend(account.id(), other).await.map_err(db)?;
    body::announce(account.id(), other, change).await
}

/// Block someone: no messages from them, no friendship, no requests.
#[server(tags(limit = "30/min"))]
pub async fn block(other: Id, user: Auth<Principal>) -> Answer<()> {
    use crate::srv::{app, db, me};
    let account = me(&user.0).await?;
    let change = app().db.block(account.id(), other).await.map_err(db)?;
    body::announce(account.id(), other, change).await
}
