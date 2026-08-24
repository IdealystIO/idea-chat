//! Domain types.
//!
//! These are the shapes the rest of the server passes around. They are
//! deliberately not the wire types and not the presentation types: the client
//! sees a projection (`hearth::model`), and keeping the two apart is what lets
//! the schema change without reshaping the UI.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A member's standing in one space. Three, deliberately — see
/// `docs/data-model.md` §4 for why there is no capability matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Moderator,
    Member,
}

impl Role {
    /// Can this role act on other members — mute in rooms, remove?
    pub fn moderates(self) -> bool {
        matches!(self, Role::Owner | Role::Moderator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Text,
    Voice,
    Video,
    Share,
    Dm,
}

/// How an instance treats people who are not yet members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RegistrationMode {
    /// The default. An account exists only by redeeming an invite.
    InviteOnly,
    /// Anyone may register; email verification is then mandatory.
    Open,
    /// Nobody may register; an instance admin creates accounts.
    Closed,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_instance_admin: bool,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

impl User {
    /// A tombstoned account may not sign in and renders as "Deleted account".
    pub fn is_active(&self) -> bool {
        self.deleted_at.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct NewUser {
    pub handle: String,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Space {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Channel {
    pub id: Uuid,
    pub space_id: Option<Uuid>,
    pub kind: ChannelKind,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub position: i32,
    pub last_message_id: Option<Uuid>,
}

/// A member of a space, as the roster needs them.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Member {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub role: Role,
}

/// One conversation in the direct-message list.
///
/// Carries the other person and the last line, because that is what the list
/// renders — resolving either client-side would be a request per row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DmSummary {
    pub channel_id: Uuid,
    pub other_id: Option<Uuid>,
    pub other_name: Option<String>,
    pub last_body: Option<String>,
    pub last_author: Option<String>,
    pub last_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Message {
    pub id: Uuid,
    pub channel_id: Uuid,
    /// `None` once the author's account is deleted — the message survives, the
    /// attribution becomes "Deleted account".
    pub author_id: Option<Uuid>,
    pub thread_root_id: Option<Uuid>,
    /// `None` once tombstoned.
    pub body: Option<String>,
    pub reply_count: i32,
    pub created_at: OffsetDateTime,
    pub edited_at: Option<OffsetDateTime>,
    pub deleted_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Invite {
    pub code: String,
    pub space_id: Uuid,
    pub created_by: Option<Uuid>,
    pub expires_at: OffsetDateTime,
    pub max_uses: i32,
    pub use_count: i32,
    pub revoked_at: Option<OffsetDateTime>,
}

/// What an unauthenticated visitor may see about an invite before deciding to
/// accept it — enough to render "Join The Cabin?", and nothing more. Notably
/// not the member list, which would leak a private space's roster to anyone
/// holding a link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePreview {
    pub space_name: String,
    pub space_slug: String,
    pub invited_by: Option<String>,
}

/// Why a redemption failed. One variant per reason because the caller shows
/// different copy for each — "that link expired" and "that link was already
/// used" send people to different next steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteRefusal {
    Unknown,
    Expired,
    Revoked,
    Exhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redemption {
    /// Joined, and this is the space.
    Joined(Uuid),
    /// Already a member; the invite was not consumed.
    AlreadyMember(Uuid),
    Refused(InviteRefusal),
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceSettings {
    pub registration_mode: RegistrationMode,
    pub instance_name: String,
}

/// One person, in one voice room, right now.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VoiceOccupant {
    pub channel_id: Uuid,
    pub space_id: Uuid,
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub role: Role,
}
