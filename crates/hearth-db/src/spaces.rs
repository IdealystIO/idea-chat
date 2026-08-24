//! Spaces, their channels, and membership.

use uuid::Uuid;

use crate::{ids, model::*, Db, DbError, Result};

/// What a new space starts with.
///
/// The design's join dialog promises "four channels and two rooms to start.
/// Rename or delete any of them later" — so this list is a product promise,
/// not a default, and it lives next to the code that keeps it.
const STARTER_CHANNELS: &[(&str, ChannelKind)] = &[
    ("welcome", ChannelKind::Text),
    ("general", ChannelKind::Text),
    ("random", ChannelKind::Text),
    ("photos", ChannelKind::Text),
    ("The Porch", ChannelKind::Voice),
    ("Movie Night", ChannelKind::Video),
];

impl Db {
    /// Create a space, seed its starter channels, and make the creator its
    /// owner — all or nothing.
    ///
    /// One transaction because a space with no owner is unreachable: nobody
    /// could invite anyone to it, and no moderator action would be possible.
    /// Partial success here is worse than failure.
    pub async fn create_space(&self, slug: &str, name: &str, owner: Uuid) -> Result<Space> {
        let mut tx = self.pool.begin().await?;

        let space = sqlx::query_as::<_, Space>(
            "INSERT INTO spaces (id, slug, name, created_by)
             VALUES ($1, $2, $3, $4)
             RETURNING id, slug, name, description, created_at",
        )
        .bind(ids::new_id())
        .bind(slug)
        .bind(name)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from_unique)?;

        sqlx::query(
            "INSERT INTO space_members (space_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(space.id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;

        for (position, (name, kind)) in STARTER_CHANNELS.iter().enumerate() {
            sqlx::query(
                "INSERT INTO channels (id, space_id, kind, name, position)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(ids::new_id())
            .bind(space.id)
            .bind(kind)
            .bind(name)
            .bind(position as i32)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(space)
    }

    /// This user's standing in this space, or `None` if they are not a member.
    ///
    /// Read per request rather than cached in the session: roles change while
    /// a session is live, and a cached role keeps enforcing a stale answer
    /// until the session expires.
    pub async fn role_in_space(&self, space_id: Uuid, user_id: Uuid) -> Result<Option<Role>> {
        let row: Option<(Role,)> = sqlx::query_as(
            "SELECT role FROM space_members WHERE space_id = $1 AND user_id = $2",
        )
        .bind(space_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(r,)| r))
    }

    pub async fn spaces_for_user(&self, user_id: Uuid) -> Result<Vec<Space>> {
        Ok(sqlx::query_as::<_, Space>(
            "SELECT s.id, s.slug, s.name, s.description, s.created_at
               FROM spaces s
               JOIN space_members m ON m.space_id = s.id
              WHERE m.user_id = $1 AND s.archived_at IS NULL
              ORDER BY s.created_at",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn channels_in_space(&self, space_id: Uuid) -> Result<Vec<Channel>> {
        Ok(sqlx::query_as::<_, Channel>(
            "SELECT id, space_id, kind, name, topic, position, last_message_id
               FROM channels
              WHERE space_id = $1 AND archived_at IS NULL
              ORDER BY position, created_at",
        )
        .bind(space_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Change someone's role. Refuses to remove the last owner, because a
    /// space with no owner can never regain one.
    pub async fn set_role(&self, space_id: Uuid, user_id: Uuid, role: Role) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        if role != Role::Owner {
            let (owners,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM space_members
                  WHERE space_id = $1 AND role = 'owner' AND user_id <> $2",
            )
            .bind(space_id)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
            if owners == 0 {
                return Err(DbError::Invalid("a space must keep at least one owner"));
            }
        }
        sqlx::query("UPDATE space_members SET role = $3 WHERE space_id = $1 AND user_id = $2")
            .bind(space_id)
            .bind(user_id)
            .bind(role)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Remove someone from a space. Their messages stay — the design is
    /// explicit that removal is not deletion.
    pub async fn remove_member(&self, space_id: Uuid, user_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM space_members WHERE space_id = $1 AND user_id = $2")
            .bind(space_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl Db {
    /// Everyone in a space, owners first.
    ///
    /// Ordered in the database rather than the client so every surface that
    /// lists members agrees on the order without each one re-deciding it.
    pub async fn members_of(&self, space_id: Uuid) -> Result<Vec<Member>> {
        Ok(sqlx::query_as::<_, Member>(
            "SELECT u.id AS user_id, u.handle, u.display_name, m.role
               FROM space_members m
               JOIN users u ON u.id = m.user_id
              WHERE m.space_id = $1
              ORDER BY CASE m.role
                         WHEN 'owner' THEN 0
                         WHEN 'moderator' THEN 1
                         ELSE 2
                       END,
                       u.display_name",
        )
        .bind(space_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// This user's conversations, most recent first, each with the other
    /// person and the last thing said.
    ///
    /// One query rather than one per conversation: the list renders in a
    /// single frame, so a round trip per row would be a round trip per row on
    /// every cold start.
    pub async fn dm_list(&self, user_id: Uuid) -> Result<Vec<DmSummary>> {
        Ok(sqlx::query_as::<_, DmSummary>(
            "SELECT c.id                AS channel_id,
                    other.id            AS other_id,
                    other.display_name  AS other_name,
                    last.body           AS last_body,
                    author.display_name AS last_author,
                    last.created_at     AS last_at
               FROM channels c
               JOIN channel_members mine
                 ON mine.channel_id = c.id AND mine.user_id = $1
               LEFT JOIN channel_members theirs
                 ON theirs.channel_id = c.id AND theirs.user_id <> $1
               LEFT JOIN users other ON other.id = theirs.user_id
               LEFT JOIN messages last ON last.id = c.last_message_id
               LEFT JOIN users author ON author.id = last.author_id
              WHERE c.kind = 'dm'
              ORDER BY c.last_message_id DESC NULLS LAST",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }
}
