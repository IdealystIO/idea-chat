//! Wire types — the vocabulary both halves of the app speak.
//!
//! These are neither the database types (`hearth_db::model`) nor the
//! presentation types (`crate::model`). Keeping three layers sounds like two
//! too many until the first time a column is renamed: this is the only one
//! that is a compatibility surface, because it is the only one that has to
//! match across a version skew between a deployed server and a cached client.
//!
//! Nothing here carries a secret. `LoginOk::token` is the one exception and it
//! is `None` for web callers by construction — see the note on the field.

use serde::{Deserialize, Serialize};

/// Who you are, as far as the server is concerned.
///
/// Deliberately small: it is deserialized out of the session store on every
/// single request. Space roles are **not** here — they change while a session
/// is live, and a cached role keeps enforcing a stale answer until the session
/// expires. They are read per request instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Principal {
    pub user_id: String,
    pub handle: String,
    pub is_instance_admin: bool,
}

/// What a successful sign-in returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginOk {
    pub user: Me,
    /// Bearer token, for native clients only.
    ///
    /// `None` on web, where the session rides an httpOnly cookie the browser
    /// attaches itself — the secret must never reach JS, or an XSS becomes a
    /// session theft. `server_kit::Sessions` decides which branch applies from
    /// the `x-idealyst-client` header; nothing here has to get it right.
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Me {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_instance_admin: bool,
}

/// What an unauthenticated visitor may learn about an instance.
///
/// Answers exactly one question — "what should the first screen be?" — and
/// deliberately no more: an unclaimed instance is a security-relevant state,
/// so it says whether a claim is pending but never the claim token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub instance_name: String,
    /// No accounts exist yet; the operator holds a one-time claim link.
    pub needs_claim: bool,
    /// `invite_only` | `open` | `closed`.
    pub registration_mode: String,
}

/// What an invite link shows before you accept it.
///
/// Not the member list: anyone holding a link could otherwise enumerate a
/// private space's roster without joining it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePreview {
    pub space_name: String,
    pub space_slug: String,
    pub invited_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// A handle or an email — one field, because making people remember which
    /// one they signed up with is a self-inflicted support burden.
    pub identifier: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub invite_code: String,
    pub handle: String,
    pub display_name: String,
    pub password: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub token: String,
    pub handle: String,
    pub display_name: String,
    pub password: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpaceSummary {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub role: String,
    pub channels: Vec<ChannelSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelSummary {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub topic: Option<String>,
}

/// One message, as the client sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageDto {
    pub id: String,
    pub channel_id: String,
    /// Display name at read time. Denormalised into the response rather than
    /// looked up client-side: the client would otherwise need every author's
    /// profile before it could render a single line.
    pub author_name: String,
    pub author_id: Option<String>,
    pub thread_root_id: Option<String>,
    /// `None` once deleted; the client renders a tombstone rather than
    /// dropping the row, so replies keep their parent.
    pub body: Option<String>,
    pub reply_count: i32,
    /// RFC 3339. Formatted client-side, because "9:02" is a presentation
    /// decision that depends on the reader's timezone and locale.
    pub created_at: String,
    pub edited_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub messages: Vec<MessageDto>,
    /// Pass back as `before` to fetch older. `None` means this is the top.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catchup {
    pub messages: Vec<MessageDto>,
    /// Too much happened while you were away — refetch this channel cold
    /// rather than trusting this partial list to be contiguous.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemberDto {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    pub role: String,
}

/// One row of the direct-message list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DmDto {
    pub channel_id: String,
    pub other_id: Option<String>,
    pub other_name: String,
    /// Already prefixed with "You: " when you sent it — the list renders it
    /// verbatim, and deciding that here keeps every client agreeing.
    pub preview: String,
}

/// Everything the shell needs to render, in one call.
///
/// The client makes this request once on boot and once after any change that
/// could reshape the sidebar. Splitting it into three would mean three round
/// trips before the first frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    pub me: Me,
    pub spaces: Vec<SpaceSummary>,
    pub dms: Vec<DmDto>,
    pub unread: Vec<String>,
}

/// Permission to be in one voice room, for a couple of minutes.
///
/// The relay fleet knows nothing about spaces or membership, so the chat
/// server answers the only question it can answer — "may this person be in
/// this room?" — in a form the relay can check by itself. See `voice_auth`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceTicket {
    /// Where to reach the fleet. Same-origin under `/_voice` so the session
    /// cookie's rules and the page's TLS both carry over unchanged.
    pub relay: String,
    /// The room name: the channel's id, so a room is exactly a channel.
    pub room: String,
    /// This connection's number on the wire. Not the user id — the transport
    /// wants something small, and the user is named inside the ticket.
    pub participant: u32,
    /// The signed ticket, presented on connect.
    pub token: String,
}

/// Who is in one voice room, right now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomPresence {
    pub channel_id: String,
    /// The people connected, as the roster already describes them — so the
    /// sidebar draws the same name and the same avatar colour it does
    /// everywhere else, rather than a second rendering of the same person.
    pub people: Vec<MemberDto>,
}
