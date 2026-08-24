//! Invites.
//!
//! The design's copy is specific: "Links last a week and only work once."
//! Making "only work once" *true* rather than usually-true is the whole point
//! of this module — see [`Db::redeem_invite`].

use rand::seq::SliceRandom;
use rand::Rng;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::{model::*, Db, Result};

/// Words for generated codes. Chosen to be readable aloud and typo-resistant:
/// no homophones, nothing that reads differently in another accent, and
/// nothing that could be mistaken for a different word on a phone screen.
const ADJECTIVES: &[&str] = &[
    "quiet", "warm", "amber", "hollow", "northern", "copper", "still", "wild",
    "open", "low", "bright", "soft", "distant", "even", "rough", "clear",
];
const NOUNS: &[&str] = &[
    "porch", "ridge", "kettle", "lantern", "meadow", "harbour", "thicket",
    "cabin", "hearth", "orchard", "bellows", "willow", "cellar", "brook",
];

/// A code like `quiet-porch-42`.
///
/// Two words plus two digits is ~57k combinations per word pair, which is not
/// unguessable on its own — the security comes from the code being useless
/// after one use and dead after a week, not from its entropy. Readability is
/// worth more here: these get read out over a phone.
pub fn generate_code() -> String {
    let mut rng = rand::thread_rng();
    let adjective = ADJECTIVES.choose(&mut rng).unwrap();
    let noun = NOUNS.choose(&mut rng).unwrap();
    format!("{adjective}-{noun}-{:02}", rng.gen_range(0..100))
}

/// The design's promise: a week.
pub const DEFAULT_TTL: Duration = Duration::days(7);

impl Db {
    pub async fn create_invite(
        &self,
        space_id: Uuid,
        created_by: Uuid,
        max_uses: i32,
    ) -> Result<Invite> {
        // Retry on collision rather than pre-checking: the unique index on
        // the primary key is the only reliable arbiter under concurrency, and
        // a SELECT-then-INSERT would race.
        for _ in 0..8 {
            let code = generate_code();
            let attempt = sqlx::query_as::<_, Invite>(
                "INSERT INTO invites (code, space_id, created_by, expires_at, max_uses)
                 VALUES ($1, $2, $3, $4, $5)
                 RETURNING code, space_id, created_by, expires_at, max_uses,
                           use_count, revoked_at",
            )
            .bind(&code)
            .bind(space_id)
            .bind(created_by)
            .bind(OffsetDateTime::now_utc() + DEFAULT_TTL)
            .bind(max_uses)
            .fetch_one(&self.pool)
            .await;

            match attempt {
                Ok(invite) => return Ok(invite),
                Err(e) if is_unique_violation(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(crate::DbError::Invalid("could not allocate an unused invite code"))
    }

    /// What an unauthenticated visitor may see before deciding to accept.
    ///
    /// Deliberately not the member list: anyone holding a link could otherwise
    /// enumerate a private space's roster without joining it.
    pub async fn peek_invite(&self, code: &str) -> Result<Option<InvitePreview>> {
        Ok(sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT s.name, s.slug, u.display_name
               FROM invites i
               JOIN spaces s ON s.id = i.space_id
          LEFT JOIN users u ON u.id = i.created_by
              WHERE i.code = $1",
        )
        .bind(code)
        .fetch_optional(&self.pool)
        .await?
        .map(|(space_name, space_slug, invited_by)| InvitePreview {
            space_name,
            space_slug,
            invited_by,
        }))
    }

    /// Redeem an invite for a user, joining them to its space.
    ///
    /// The conditional `UPDATE … WHERE use_count < max_uses` **inside** the
    /// transaction is what makes "only work once" true. Checking validity and
    /// then updating would let two simultaneous redemptions of a single-use
    /// code both observe `use_count = 0` and both pass; here the second one
    /// updates zero rows and is refused. `tests/invites.rs` fires
    /// sixteen concurrent redemptions at one code to hold this honest.
    ///
    /// Redeeming a code you have already used is not an error and does not
    /// consume a use — it is what happens when someone clicks the link twice.
    pub async fn redeem_invite(&self, code: &str, user_id: Uuid) -> Result<Redemption> {
        let mut tx = self.pool.begin().await?;

        let Some(invite) = sqlx::query_as::<_, Invite>(
            "SELECT code, space_id, created_by, expires_at, max_uses, use_count, revoked_at
               FROM invites WHERE code = $1",
        )
        .bind(code)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(Redemption::Refused(InviteRefusal::Unknown));
        };

        // Report the specific reason: "expired" and "already used" send
        // someone to different next steps.
        if invite.revoked_at.is_some() {
            return Ok(Redemption::Refused(InviteRefusal::Revoked));
        }
        if invite.expires_at <= OffsetDateTime::now_utc() {
            return Ok(Redemption::Refused(InviteRefusal::Expired));
        }

        let already: Option<(Uuid,)> = sqlx::query_as(
            "SELECT user_id FROM space_members WHERE space_id = $1 AND user_id = $2",
        )
        .bind(invite.space_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if already.is_some() {
            tx.commit().await?;
            return Ok(Redemption::AlreadyMember(invite.space_id));
        }

        // The arbiter. Zero rows updated ⇒ someone else took the last use
        // between our read above and this write.
        let claimed = sqlx::query(
            "UPDATE invites
                SET use_count = use_count + 1
              WHERE code = $1
                AND use_count < max_uses
                AND revoked_at IS NULL
                AND expires_at > now()",
        )
        .bind(code)
        .execute(&mut *tx)
        .await?;

        if claimed.rows_affected() == 0 {
            return Ok(Redemption::Refused(InviteRefusal::Exhausted));
        }

        sqlx::query(
            "INSERT INTO space_members (space_id, user_id, role) VALUES ($1, $2, 'member')",
        )
        .bind(invite.space_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Redemption::Joined(invite.space_id))
    }

    pub async fn revoke_invite(&self, code: &str) -> Result<()> {
        sqlx::query("UPDATE invites SET revoked_at = now() WHERE code = $1 AND revoked_at IS NULL")
            .bind(code)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false)
}
