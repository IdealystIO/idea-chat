//! Spaces: making them, inviting people in, roles, categories, and each
//! person's own arrangement of them.

use chat_model::{Category, Id, Invite, InvitePreview, Role};
use server::server;
use server_kit::Auth;

use crate::{Answer, Principal};

#[cfg(feature = "server")]
mod body {
    use chat_model::{perms, Id, Role, Space};

    use crate::srv::{app, db, fail, forbidden, me};
    use crate::{ApiError, Failure, Principal};

    /// The space and the caller's role in it; refused unless they are in it.
    pub async fn member(p: &Principal, space: Id) -> Result<(Id, Role, Space), Failure> {
        let account = me(p).await?;
        let s = app().db.space(space).await.map_err(db)?;
        let role = s.role_of(account.id()).ok_or_else(|| fail(ApiError::NotFound))?;
        Ok((account.id(), role, s))
    }

    /// As [`member`], and they may manage it.
    pub async fn manager(p: &Principal, space: Id) -> Result<(Id, Role, Space), Failure> {
        let (me, role, s) = member(p, space).await?;
        if !perms::can_manage_space(role) {
            return Err(forbidden("Only admins can change that."));
        }
        Ok((me, role, s))
    }
}

/// Make a space with a text and a voice channel to start. Returns its id.
#[server(tags(limit = "10/min"))]
pub async fn create_space(name: String, user: Auth<Principal>) -> Answer<Id> {
    use chat_db::NewSpace;

    use crate::srv::{app, db, invalid, me, resync};
    let account = me(&user.0).await?;
    let name = chat_model::text::name(&name).map_err(invalid("name"))?;
    let (space, _) = app().db.create_space(&NewSpace { name, owner: account.id() }).await.map_err(db)?;
    resync(&[account.id()]).await;
    Ok(space.id)
}

/// Rename a space, or change its icon (an upload id from `files`, or
/// `None` to remove it).
#[server]
pub async fn rename_space(space: Id, name: String, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, invalid, space_topic};
    let (_, _, s) = body::manager(&user.0, space).await?;
    let name = chat_model::text::name(&name).map_err(invalid("name"))?;
    app().db.rename_space(space, &name, s.icon.as_deref()).await.map_err(db)?;
    app()
        .bus
        .publish(vec![space_topic(space, Role::Member)], Event::SpaceRenamed { space, name, icon: s.icon })
        .await;
    Ok(())
}

/// Delete a space and everything in it. Owner only.
#[server]
pub async fn delete_space(space: Id, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, forbidden, space_topic};
    let (_, role, _) = body::member(&user.0, space).await?;
    if role != Role::Owner {
        return Err(forbidden("Only the owner can delete a space."));
    }
    let members = app().db.member_ids(space).await.map_err(db)?;
    app().db.delete_space(space).await.map_err(db)?;
    app().bus.publish(vec![space_topic(space, Role::Member)], Event::SpaceRemoved { space }).await;
    crate::srv::resync(&members).await;
    Ok(())
}

/// Leave a space. The owner transfers it first.
#[server]
pub async fn leave_space(space: Id, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, resync, space_topic};
    let (me, _, _) = body::member(&user.0, space).await?;
    app().db.remove_member(space, me).await.map_err(db)?;
    // A call in one of its channels goes with it.
    if let Ok(Some((_, channel, _))) = app().db.my_room(me).await {
        if app().db.channel(channel).await.is_ok_and(|c| c.space == Some(space)) {
            crate::calls::leave_everywhere(me).await;
        }
    }
    app().bus.publish(vec![space_topic(space, Role::Member)], Event::MemberLeft { space, user: me }).await;
    app().bus.to_user(me, Event::SpaceRemoved { space }).await;
    resync(&[me]).await;
    Ok(())
}

/// Remove someone from a space.
#[server]
pub async fn remove_member(space: Id, member: Id, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;
    use chat_model::perms;

    use crate::srv::{app, db, fail, forbidden, resync, space_topic};
    use crate::ApiError;
    let (_, role, s) = body::member(&user.0, space).await?;
    let target = s.role_of(member).ok_or_else(|| fail(ApiError::NotFound))?;
    if !perms::can_moderate(role, target) {
        return Err(forbidden("You can't remove them."));
    }
    app().db.remove_member(space, member).await.map_err(db)?;
    app().bus.publish(vec![space_topic(space, Role::Member)], Event::MemberLeft { space, user: member }).await;
    app().bus.to_user(member, Event::SpaceRemoved { space }).await;
    resync(&[member]).await;
    Ok(())
}

/// Make someone an admin, or a member again.
#[server]
pub async fn set_role(space: Id, member: Id, role: crate::Rank, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;
    use chat_model::perms;

    use crate::srv::{app, db, fail, forbidden, resync, space_topic};
    use crate::ApiError;
    let (_, mine, s) = body::member(&user.0, space).await?;
    let theirs = s.role_of(member).ok_or_else(|| fail(ApiError::NotFound))?;
    if !perms::can_grant(mine, theirs, role) {
        return Err(forbidden("You can't give that role."));
    }
    app().db.set_role(space, member, role).await.map_err(db)?;
    app().bus.publish(vec![space_topic(space, Role::Member)], Event::MemberRole { space, user: member, role }).await;
    // What they hear in the space changes with their role.
    resync(&[member]).await;
    Ok(())
}

/// Hand the space to another member; the owner becomes an admin.
#[server]
pub async fn transfer_space(space: Id, to: Id, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, forbidden, resync, space_topic};
    let (me, role, _) = body::member(&user.0, space).await?;
    if role != Role::Owner {
        return Err(forbidden("Only the owner can transfer a space."));
    }
    app().db.transfer_space(space, me, to).await.map_err(db)?;
    let topic = vec![space_topic(space, Role::Member)];
    app().bus.publish(topic.clone(), Event::MemberRole { space, user: to, role: Role::Owner }).await;
    app().bus.publish(topic, Event::MemberRole { space, user: me, role: Role::Admin }).await;
    resync(&[me, to]).await;
    Ok(())
}

/// Make an invite. `max_uses` and `hours` of `None` mean unlimited.
#[server]
pub async fn create_invite(space: Id, max_uses: Option<u32>, hours: Option<u32>, user: Auth<Principal>) -> Answer<Invite> {
    use crate::srv::{app, db};
    // Anyone in a space may invite to it; managing invites is for admins.
    let (me, _, _) = body::member(&user.0, space).await?;
    let expires = hours.map(|h| time::OffsetDateTime::now_utc() + time::Duration::hours(h.clamp(1, 24 * 30) as i64));
    app().db.create_invite(space, me, max_uses.map(|m| m.clamp(1, 1000)), expires).await.map_err(db)
}

/// A space's invites that still work.
#[server]
pub async fn invites(space: Id, user: Auth<Principal>) -> Answer<Vec<Invite>> {
    use crate::srv::{app, db};
    body::manager(&user.0, space).await?;
    app().db.invites(space).await.map_err(db)
}

#[server]
pub async fn revoke_invite(space: Id, code: String, user: Auth<Principal>) -> Answer<()> {
    use crate::srv::{app, db};
    body::manager(&user.0, space).await?;
    app().db.revoke_invite(space, &code).await.map_err(db)
}

/// What an invite leads to.
#[server(tags(limit = "30/min"))]
pub async fn invite_preview(code: String, user: Auth<Principal>) -> Answer<InvitePreview> {
    use crate::srv::{app, db, me};
    let account = me(&user.0).await?;
    app().db.invite_preview(code.trim(), Some(account.id())).await.map_err(db)
}

/// Join a space by invite. Returns its id.
#[server(tags(limit = "20/min"))]
pub async fn join_space(code: String, user: Auth<Principal>) -> Answer<Id> {
    use chat_db::{InviteOutcome, NewMessage};
    use chat_model::live::Event;
    use chat_model::{ChannelKind, MessageKind};

    use crate::srv::{app, db, me, resync, space_topic};
    let account = me(&user.0).await?;
    let a = app();
    let code = code.trim().rsplit('/').next().unwrap_or("").to_string();
    match a.db.redeem_invite(&code, account.id()).await.map_err(db)? {
        InviteOutcome::AlreadyMember { space } => Ok(space),
        InviteOutcome::Joined { space, member } => {
            a.bus
                .publish(
                    vec![space_topic(space, Role::Member)],
                    Event::MemberJoined { space, user: account.me.user.clone(), member },
                )
                .await;
            // Say so in the space's first text channel everyone can see.
            let channels = a.db.space_channels(space).await.map_err(db)?;
            let mut open: Vec<_> =
                channels.iter().filter(|c| c.kind == ChannelKind::Text && c.view == Role::Member).collect();
            open.sort_by_key(|c| (c.category.is_some(), c.position));
            if let Some(c) = open.first() {
                if let Ok(posted) = a
                    .db
                    .post(&NewMessage {
                        channel: c.id,
                        author: account.id(),
                        kind: MessageKind::Joined,
                        body: String::new(),
                        reply_to: None,
                        mentions: vec![],
                        everyone: false,
                        attachments: vec![],
                    })
                    .await
                {
                    a.bus
                        .publish(vec![space_topic(space, Role::Member)], Event::MessageCreated { message: posted.message })
                        .await;
                }
            }
            resync(&[account.id()]).await;
            crate::srv::announce_presence(account.id()).await;
            Ok(space)
        }
    }
}

#[server]
pub async fn create_category(space: Id, name: String, user: Auth<Principal>) -> Answer<Category> {
    use chat_model::live::Event;

    use crate::srv::{app, db, invalid, space_topic};
    body::manager(&user.0, space).await?;
    let name = chat_model::text::name(&name).map_err(invalid("name"))?;
    let cat = app().db.create_category(space, &name).await.map_err(db)?;
    let categories = app().db.space(space).await.map_err(db)?.categories;
    app().bus.publish(vec![space_topic(space, Role::Member)], Event::CategoriesChanged { space, categories }).await;
    Ok(cat)
}

#[server]
pub async fn rename_category(space: Id, category: Id, name: String, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, invalid, space_topic};
    body::manager(&user.0, space).await?;
    let name = chat_model::text::name(&name).map_err(invalid("name"))?;
    app().db.rename_category(space, category, &name).await.map_err(db)?;
    let categories = app().db.space(space).await.map_err(db)?.categories;
    app().bus.publish(vec![space_topic(space, Role::Member)], Event::CategoriesChanged { space, categories }).await;
    Ok(())
}

/// Delete a category; its channels move to the top.
#[server]
pub async fn delete_category(space: Id, category: Id, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, space_topic};
    body::manager(&user.0, space).await?;
    let a = app();
    a.db.delete_category(space, category).await.map_err(db)?;
    let s = a.db.space(space).await.map_err(db)?;
    let channels = a.db.space_channels(space).await.map_err(db)?;
    let users = a.db.users(&s.members.keys().copied().collect::<Vec<_>>()).await.map_err(db)?;
    a.bus.publish(vec![space_topic(space, Role::Member)], Event::SpaceUpserted { space: s, channels, users }).await;
    Ok(())
}

/// The caller's own order of their spaces, for the switcher.
#[server]
pub async fn set_space_order(spaces: Vec<Id>, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, me};
    let account = me(&user.0).await?;
    app().db.set_space_order(account.id(), &spaces).await.map_err(db)?;
    app().bus.to_user(account.id(), Event::SpaceOrder { spaces }).await;
    Ok(())
}

/// Mute or unmute a space or channel for the caller.
#[server]
pub async fn set_muted(target: Id, muted: bool, user: Auth<Principal>) -> Answer<()> {
    use chat_model::live::Event;

    use crate::srv::{app, db, me};
    let account = me(&user.0).await?;
    app().db.set_muted(account.id(), target, muted).await.map_err(db)?;
    app().bus.to_user(account.id(), Event::MutedChanged { target, muted }).await;
    Ok(())
}
