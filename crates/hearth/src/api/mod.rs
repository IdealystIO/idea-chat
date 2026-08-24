//! The server API.
//!
//! Every function here compiles twice: on the server the body runs, on the
//! client it becomes an HTTP stub with the same signature minus the injected
//! extractors. That is `#[server]`'s whole trick, and it is why the wire types
//! in [`types`] are the only contract either side needs.
//!
//! Bodies name `crate::srv`, which exists only under `--features server`.

pub mod types;

use server::{server, ServerError};
pub use types::*;

#[cfg(feature = "server")]
use crate::srv;

// ===========================================================================
// Instance
// ===========================================================================

/// What the very first screen should be.
///
/// Unauthenticated by design — a client has to be able to ask before it can
/// possibly have a session.
#[server]
pub async fn instance_status(
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
) -> Result<InstanceStatus, ServerError> {
    let settings = db.0.instance_settings().await.map_err(srv::internal)?;
    Ok(InstanceStatus {
        instance_name: settings.instance_name,
        // Says *that* a claim is pending, never the token. The token only ever
        // exists in the operator's terminal.
        needs_claim: !db.0.has_any_user().await.map_err(srv::internal)?,
        registration_mode: match settings.registration_mode {
            hearth_db::RegistrationMode::InviteOnly => "invite_only",
            hearth_db::RegistrationMode::Open => "open",
            hearth_db::RegistrationMode::Closed => "closed",
        }
        .to_string(),
    })
}

/// Claim a fresh instance: create the first account, as instance admin.
///
/// The token is single-use and short-lived, and this refuses outright once any
/// account exists — so the window in which an unclaimed instance is claimable
/// closes for good the moment somebody claims it, even if the token leaks.
#[server(tags(limit = "5/min"))]
pub async fn claim_instance(
    claim: Claim,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
    #[cfg(feature = "server")] sessions: server::State<server_kit::Sessions>,
    #[cfg(feature = "server")] boot: server::State<srv::Bootstrap>,
) -> Result<LoginOk, ServerError> {
    if db.0.has_any_user().await.map_err(srv::internal)? {
        return Err(ServerError::failed("this instance has already been claimed"));
    }
    if !boot.consume(&claim.token) {
        return Err(ServerError::failed("that claim link is not valid"));
    }

    let user = db
        .0
        .create_user(hearth_db::NewUser {
            handle: claim.handle,
            display_name: claim.display_name,
            email: claim.email,
        })
        .await
        .map_err(srv::user_facing)?;
    db.0.set_password(user.id, &claim.password).await.map_err(srv::user_facing)?;
    // Take the promoted row back: the session principal is built from this
    // value, so using the pre-promotion one would sign the first admin in as
    // an ordinary member.
    let user = db.0.promote_to_instance_admin(user.id).await.map_err(srv::internal)?;

    srv::audit(&db.0, Some(user.id), "instance_claimed").await;
    srv::start_session(&sessions, &db.0, user).await
}

// ===========================================================================
// Registration and sign-in
// ===========================================================================

/// What an invite shows before you accept it.
#[server(tags(limit = "20/min"))]
pub async fn peek_invite(
    code: String,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
) -> Result<Option<InvitePreview>, ServerError> {
    Ok(db.0.peek_invite(&code).await.map_err(srv::internal)?.map(|p| InvitePreview {
        space_name: p.space_name,
        space_slug: p.space_slug,
        invited_by: p.invited_by,
    }))
}

/// Create an account by redeeming an invite.
///
/// The account and the membership are created together: an account that fails
/// to join the space it was invited to would land somewhere with nothing in
/// it and no way back to the invite, which is worse than a failed signup.
#[server(tags(limit = "10/hour"))]
pub async fn register(
    reg: Registration,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
    #[cfg(feature = "server")] sessions: server::State<server_kit::Sessions>,
) -> Result<LoginOk, ServerError> {
    let mode = db.0.instance_settings().await.map_err(srv::internal)?.registration_mode;
    if mode == hearth_db::RegistrationMode::Closed {
        return Err(ServerError::failed("this instance is not accepting new accounts"));
    }

    // Check the invite before creating anything, so a bad code doesn't leave
    // an orphaned account behind. The authoritative check is still the
    // conditional UPDATE inside `redeem_invite`.
    if db.0.peek_invite(&reg.invite_code).await.map_err(srv::internal)?.is_none() {
        return Err(ServerError::failed("that invite link is not valid"));
    }

    let user = db
        .0
        .create_user(hearth_db::NewUser {
            handle: reg.handle,
            display_name: reg.display_name,
            email: reg.email,
        })
        .await
        .map_err(srv::user_facing)?;
    db.0.set_password(user.id, &reg.password).await.map_err(srv::user_facing)?;

    match db.0.redeem_invite(&reg.invite_code, user.id).await.map_err(srv::internal)? {
        hearth_db::Redemption::Joined(_) | hearth_db::Redemption::AlreadyMember(_) => {}
        hearth_db::Redemption::Refused(why) => {
            // The account exists but has nowhere to be. Roll it back rather
            // than stranding somebody in an empty app.
            let _ = db.0.delete_user(user.id).await;
            return Err(ServerError::failed(srv::refusal_copy(why)));
        }
    }

    srv::audit(&db.0, Some(user.id), "registered").await;
    srv::start_session(&sessions, &db.0, user).await
}

/// Sign in.
///
/// Both failure paths — no such account, wrong password — return the same
/// error and take the same time (see `srv::sign_in`). Telling them apart is
/// an account-enumeration oracle.
#[server(tags(limit = "5/min"))]
pub async fn login(
    creds: Credentials,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
    #[cfg(feature = "server")] sessions: server::State<server_kit::Sessions>,
) -> Result<LoginOk, ServerError> {
    srv::sign_in(&db.0, &sessions, &creds.identifier, &creds.password).await
}

/// End this session. Idempotent — signing out twice is not an error.
#[server]
pub async fn logout(
    #[cfg(feature = "server")] sessions: server::State<server_kit::Sessions>,
) -> Result<(), ServerError> {
    sessions.end().await.map_err(|e| ServerError::failed(e.to_string()))
}

// ===========================================================================
// Authenticated reads
// ===========================================================================

#[server]
pub async fn me(
    #[cfg(feature = "server")] who: server_kit::Auth<Principal>,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
) -> Result<Me, ServerError> {
    let id = srv::parse_id(&who.0.user_id)?;
    let user = db
        .0
        .user(id)
        .await
        .map_err(srv::internal)?
        .filter(|u| u.is_active())
        .ok_or_else(|| ServerError::failed("that account no longer exists"))?;
    Ok(srv::to_me(user))
}

/// Every space this user belongs to, with its channels — the whole left half
/// of the app in one call.
///
/// One round trip rather than one per space: the client needs all of it before
/// it can render anything, so N+1 here would be N+1 on every cold start.
#[server]
pub async fn my_spaces(
    #[cfg(feature = "server")] who: server_kit::Auth<Principal>,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
) -> Result<Vec<SpaceSummary>, ServerError> {
    srv::spaces_for(&db.0, srv::parse_id(&who.0.user_id)?).await
}

// ===========================================================================
// Membership
// ===========================================================================

/// Mint an invite link for a space. Moderators and owners only.
#[server(tags(limit = "20/hour"))]
pub async fn create_invite(
    space_id: String,
    max_uses: i32,
    #[cfg(feature = "server")] who: server_kit::Auth<Principal>,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
) -> Result<String, ServerError> {
    let user_id = srv::parse_id(&who.0.user_id)?;
    let space = srv::parse_id(&space_id)?;
    // Read the role now rather than trusting the session — see `Principal`.
    srv::require_moderator(&db.0, space, user_id).await?;
    let invite = db
        .0
        .create_invite(space, user_id, max_uses.clamp(1, 100))
        .await
        .map_err(srv::internal)?;
    Ok(invite.code)
}

/// Create a space. The caller becomes its owner.
#[server(tags(limit = "3/hour"))]
pub async fn create_space(
    slug: String,
    name: String,
    #[cfg(feature = "server")] who: server_kit::Auth<Principal>,
    #[cfg(feature = "server")] db: server::State<srv::AppDb>,
) -> Result<SpaceSummary, ServerError> {
    let user_id = srv::parse_id(&who.0.user_id)?;
    let space = db
        .0
        .create_space(&slug, &name, user_id)
        .await
        .map_err(srv::user_facing)?;
    srv::space_summary(&db.0, space, hearth_db::Role::Owner).await
}
