//! Server-only support for [`crate::api`].
//!
//! Everything here compiles only under `--features server`. It exists so the
//! `#[server]` bodies read as policy — "check the role, then mint the invite"
//! — rather than as plumbing, and so the parts worth testing (session start,
//! the sign-in timing guarantee) sit in functions rather than inside a macro
//! expansion where nothing can reach them.

use std::sync::{Arc, Mutex};

use server::ServerError;
use uuid::Uuid;

use crate::api::types::*;
use hearth_db::{Db, DbError, InviteRefusal, Role, Space, User};

// ===========================================================================
// Injected state
// ===========================================================================

/// The database, as injected state.
///
/// A newtype rather than installing `Db` directly: `install_state` is keyed by
/// type, so a bare `Db` would collide with anything else that wanted to inject
/// one, and the collision would be a silent overwrite at boot.
#[derive(Clone)]
pub struct AppDb(pub Db);

/// So a handler can write `db.0.users(...)` rather than `db.0.0.users(...)`.
/// The newtype exists to key `install_state`, not to add behaviour.
impl std::ops::Deref for AppDb {
    type Target = Db;
    fn deref(&self) -> &Db {
        &self.0
    }
}

/// The one-time claim token for an unclaimed instance.
///
/// Held in memory rather than the database or the cache, deliberately: it must
/// not survive a restart. An operator who loses the link restarts the process
/// and gets a fresh one, which is a better failure mode than a token that sits
/// in a store waiting to be found.
#[derive(Clone)]
pub struct Bootstrap {
    token: Arc<Mutex<Option<String>>>,
}

impl Bootstrap {
    /// Mint a claim token, or `None` if this instance already has accounts.
    pub async fn arm(db: &Db) -> Result<(Bootstrap, Option<String>), DbError> {
        if db.has_any_user().await? {
            return Ok((Bootstrap { token: Arc::new(Mutex::new(None)) }, None));
        }
        let token = random_token();
        Ok((Bootstrap { token: Arc::new(Mutex::new(Some(token.clone()))) }, Some(token)))
    }

    /// Accept the token exactly once.
    ///
    /// Compared in constant time and taken under the same lock that checks it,
    /// so two simultaneous claims cannot both pass.
    pub fn consume(&self, offered: &str) -> bool {
        let mut held = self.token.lock().expect("bootstrap lock");
        let Some(expected) = held.as_deref() else { return false };
        if !constant_time_eq(expected.as_bytes(), offered.as_bytes()) {
            return false;
        }
        *held = None;
        true
    }
}

/// 32 bytes of CSPRNG, hex.
fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare without leaking where the first difference is.
///
/// The claim token is short-lived and single-use, so a timing oracle here is
/// not much of a prize — but the whole point of a bootstrap flow is that it is
/// the one moment an instance is takeable, and "not much of a prize" is a poor
/// reason to hand out a side channel.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ===========================================================================
// Errors
// ===========================================================================

/// A fault the caller can do nothing about. The detail goes to the log, not
/// the wire — a database error text is an information leak with no upside.
pub fn internal(e: DbError) -> ServerError {
    eprintln!("hearth: {e}");
    ServerError::failed("something went wrong on our end")
}

/// A refusal the caller *can* act on: the handle is taken, the password is too
/// short. These are safe to echo because the user already knows what they
/// typed.
pub fn user_facing(e: DbError) -> ServerError {
    match e {
        DbError::HandleTaken | DbError::EmailTaken | DbError::SlugTaken | DbError::Invalid(_) => {
            ServerError::failed(e.to_string())
        }
        other => internal(other),
    }
}

/// Why an invite bounced, in words. Distinct per reason because they send
/// people to different next steps — an expired link wants a new one, an
/// exhausted one wants a different link entirely.
pub fn refusal_copy(why: InviteRefusal) -> &'static str {
    match why {
        InviteRefusal::Unknown => "that invite link is not valid",
        InviteRefusal::Expired => "that invite link has expired — ask for a fresh one",
        InviteRefusal::Revoked => "that invite link was withdrawn",
        InviteRefusal::Exhausted => "that invite link has already been used",
    }
}

pub fn parse_id(s: &str) -> Result<Uuid, ServerError> {
    Uuid::parse_str(s).map_err(|_| ServerError::failed("malformed id"))
}

// ===========================================================================
// Sessions
// ===========================================================================

/// Start a session and shape the response.
///
/// `Sessions::start` decides cookie-vs-bearer from the request's client
/// header, so nothing here has to know which kind of client is calling — which
/// is exactly why `token` can be `None` without the web path ever having to
/// remember to strip it.
pub async fn start_session(
    sessions: &server_kit::Sessions,
    db: &Db,
    user: User,
) -> Result<LoginOk, ServerError> {
    let principal = Principal {
        user_id: user.id.to_string(),
        handle: user.handle.clone(),
        is_instance_admin: user.is_instance_admin,
    };
    let ticket = sessions
        .start(&principal)
        .await
        .map_err(|e| ServerError::failed(e.to_string()))?;
    audit(db, Some(user.id), "login_ok").await;
    Ok(LoginOk { user: to_me(user), token: ticket.token })
}

/// The sign-in decision, extracted so the timing guarantee is testable.
///
/// Both failure paths must be indistinguishable: same message, same work. An
/// unknown identifier still runs a full argon2 verification against a dummy
/// hash, because returning in microseconds for "no such user" and in ~50ms for
/// "wrong password" enumerates accounts with a stopwatch.
pub async fn sign_in(
    db: &Db,
    sessions: &server_kit::Sessions,
    identifier: &str,
    password: &str,
) -> Result<LoginOk, ServerError> {
    const REFUSED: &str = "invalid handle or password";

    let Some(user) = db.user_by_login(identifier).await.map_err(internal)? else {
        db.verify_absent_user(password);
        return Err(ServerError::failed(REFUSED));
    };
    if !db.verify_password(user.id, password).await.map_err(internal)? {
        audit(db, Some(user.id), "login_fail").await;
        return Err(ServerError::failed(REFUSED));
    }
    start_session(sessions, db, user).await
}

// ===========================================================================
// Authorization
// ===========================================================================

/// Refuse unless this user moderates this space.
///
/// Reads the role now rather than trusting the session: roles change while a
/// session is live, and a demoted moderator must stop being one immediately
/// rather than when their session expires.
///
/// A non-member gets the same error as a member without the power — "not
/// found" rather than "forbidden" — so the endpoint doesn't confirm that a
/// space exists to someone who can't see it.
pub async fn require_moderator(db: &Db, space: Uuid, user: Uuid) -> Result<Role, ServerError> {
    match db.role_in_space(space, user).await.map_err(internal)? {
        Some(role) if role.moderates() => Ok(role),
        _ => Err(ServerError::failed("no such space")),
    }
}

/// Refuse unless this user can see this channel.
///
/// The same "not found" answer for a channel that doesn't exist and one the
/// caller can't see. Distinguishing them would let anyone probe for the
/// existence of private channels by id.
pub async fn require_channel_access(db: &Db, channel: Uuid, user: Uuid) -> Result<(), ServerError> {
    if db.can_see_channel(channel, user).await.map_err(internal)? {
        Ok(())
    } else {
        Err(ServerError::failed("no such channel"))
    }
}

/// Does this user moderate the space this channel belongs to?
///
/// Answers `false` for a DM, which has no space and therefore no moderator —
/// nobody outranks anybody in a conversation between two people.
pub async fn moderates_channel(db: &Db, channel: Uuid, user: Uuid) -> bool {
    let Ok(Some(space)) = db.channel_space(channel).await else { return false };
    matches!(db.role_in_space(space, user).await, Ok(Some(role)) if role.moderates())
}

// ===========================================================================
// Projections
// ===========================================================================

/// Turn stored messages into wire messages, resolving author names.
///
/// Names are looked up once per distinct author for the whole batch rather
/// than once per message: a page of fifty messages from four people is four
/// lookups, not fifty.
pub async fn to_dtos(db: &Db, rows: Vec<hearth_db::Message>) -> Result<Vec<MessageDto>, ServerError> {
    use std::collections::HashMap;

    let mut names: HashMap<Uuid, String> = HashMap::new();
    for row in &rows {
        if let Some(author) = row.author_id {
            if !names.contains_key(&author) {
                let name = db
                    .user(author)
                    .await
                    .map_err(internal)?
                    .map(|u| u.display_name)
                    .unwrap_or_else(|| "Deleted account".into());
                names.insert(author, name);
            }
        }
    }

    Ok(rows
        .into_iter()
        .map(|m| MessageDto {
            id: m.id.to_string(),
            channel_id: m.channel_id.to_string(),
            author_name: m
                .author_id
                .and_then(|a| names.get(&a).cloned())
                .unwrap_or_else(|| "Deleted account".into()),
            author_id: m.author_id.map(|a| a.to_string()),
            thread_root_id: m.thread_root_id.map(|t| t.to_string()),
            body: m.body,
            reply_count: m.reply_count,
            created_at: rfc3339(m.created_at),
            edited_at: m.edited_at.map(rfc3339),
        })
        .collect())
}

/// Timestamps go over the wire as RFC 3339 and are formatted client-side —
/// "9:02" depends on the reader's timezone, which the server does not know.
fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| t.unix_timestamp().to_string())
}

pub fn to_me(user: User) -> Me {
    Me {
        id: user.id.to_string(),
        handle: user.handle,
        display_name: user.display_name,
        email: user.email,
        is_instance_admin: user.is_instance_admin,
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Owner => "owner",
        Role::Moderator => "moderator",
        Role::Member => "member",
    }
}

pub async fn space_summary(db: &Db, space: Space, role: Role) -> Result<SpaceSummary, ServerError> {
    let channels = db.channels_in_space(space.id).await.map_err(internal)?;
    Ok(SpaceSummary {
        id: space.id.to_string(),
        slug: space.slug,
        name: space.name,
        role: role_name(role).to_string(),
        channels: channels
            .into_iter()
            .map(|c| ChannelSummary {
                id: c.id.to_string(),
                kind: format!("{:?}", c.kind).to_lowercase(),
                name: c.name.unwrap_or_default(),
                topic: c.topic,
            })
            .collect(),
    })
}

pub async fn spaces_for(db: &Db, user: Uuid) -> Result<Vec<SpaceSummary>, ServerError> {
    let spaces = db.spaces_for_user(user).await.map_err(internal)?;
    let mut out = Vec::with_capacity(spaces.len());
    for space in spaces {
        let role = db
            .role_in_space(space.id, user)
            .await
            .map_err(internal)?
            .unwrap_or(Role::Member);
        out.push(space_summary(db, space, role).await?);
    }
    Ok(out)
}

// ===========================================================================
// Audit
// ===========================================================================

/// Record a security-relevant event.
///
/// Never fails the request it describes: a login that worked must not be
/// reported as broken because the audit insert did. The failure goes to the
/// log, where an operator will see it.
pub async fn audit(db: &Db, user: Option<Uuid>, kind: &str) {
    if let Err(e) = db.record_auth_event(user, kind).await {
        eprintln!("hearth: could not write audit event {kind}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_token_works_exactly_once() {
        let boot = Bootstrap { token: Arc::new(Mutex::new(Some("abc123".into()))) };
        assert!(boot.consume("abc123"));
        assert!(!boot.consume("abc123"), "a claim token must not be reusable");
    }

    #[test]
    fn a_wrong_claim_token_does_not_consume_the_real_one() {
        let boot = Bootstrap { token: Arc::new(Mutex::new(Some("abc123".into()))) };
        assert!(!boot.consume("wrong"));
        assert!(boot.consume("abc123"), "a failed attempt must not burn the token");
    }

    #[test]
    fn an_unarmed_bootstrap_refuses_everything() {
        let boot = Bootstrap { token: Arc::new(Mutex::new(None)) };
        assert!(!boot.consume(""));
        assert!(!boot.consume("anything"));
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"samf"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }
}
