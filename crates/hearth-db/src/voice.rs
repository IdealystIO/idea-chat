//! Who is in which voice room.
//!
//! See `migrations/0002_voice_presence.sql` for why this is the chat server's
//! state rather than something read off the relay fleet. In short: a relay
//! forwards bytes and should not be part of the app's data model, and presence
//! that lives there vanishes whenever it is unreachable.

use uuid::Uuid;

use crate::{model::*, Db, Result};

/// How long a heartbeat vouches for someone.
///
/// A row older than this is not presence, it is a ghost — a tab that was closed
/// badly, a laptop that slept, a network that went away without saying so. The
/// client renews well inside this window (see `HEARTBEAT`), so the only rows
/// this excludes are the ones nobody is behind.
pub const PRESENCE_TTL_SECS: i64 = 45;

impl Db {
    /// Record that `user` is in `channel`, or that they still are.
    ///
    /// The same call for arriving and for staying, because they are the same
    /// fact and a separate "join" would be a second way to get it wrong. One
    /// row per person, so moving to another room moves them — there is no
    /// state in which somebody is in two calls.
    /// Returns `true` when this is an ARRIVAL — a room they were not already
    /// in. A heartbeat for the room they are in returns `false`.
    ///
    /// The caller needs the difference to decide whether anything is worth
    /// telling anyone. Publishing on every heartbeat would wake every client
    /// in the space every fifteen seconds to be told what it already knows.
    pub async fn voice_present(&self, user: Uuid, channel: Uuid) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        // Read the room they are in before overwriting it. In the same
        // transaction as the write, so two connections racing to move the same
        // account cannot both report an arrival.
        let was: Option<(Uuid,)> =
            sqlx::query_as("SELECT channel_id FROM voice_presence WHERE user_id = $1 FOR UPDATE")
                .bind(user)
                .fetch_optional(&mut *tx)
                .await?;

        sqlx::query(
            "INSERT INTO voice_presence (user_id, channel_id)
             VALUES ($1, $2)
             ON CONFLICT (user_id) DO UPDATE
                SET channel_id = excluded.channel_id,
                    seen_at    = now(),
                    -- Only reset the arrival time when the room actually
                    -- changed; a heartbeat must not keep bumping someone to
                    -- the bottom of a list ordered by when they got there.
                    joined_at  = CASE
                        WHEN voice_presence.channel_id = excluded.channel_id
                        THEN voice_presence.joined_at
                        ELSE now()
                    END",
        )
        .bind(user)
        .bind(channel)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(was.map(|(c,)| c) != Some(channel))
    }

    /// Remove `user` from `channel`.
    ///
    /// Scoped to the channel on purpose. A "leave" for a room someone has
    /// already moved on from arrives late often enough to matter — a slow
    /// request, a reconnect — and an unscoped delete would take them out of the
    /// call they are actually in.
    /// Returns `true` when a row was actually removed — so a caller can avoid
    /// announcing a departure that did not happen.
    pub async fn voice_absent(&self, user: Uuid, channel: Uuid) -> Result<bool> {
        let done = sqlx::query("DELETE FROM voice_presence WHERE user_id = $1 AND channel_id = $2")
            .bind(user)
            .bind(channel)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Everyone currently in any voice room of `space`, oldest arrival first.
    ///
    /// One query rather than one per room: a space has a handful of rooms and
    /// this is drawn on every sidebar poll.
    pub async fn voice_in_space(&self, space: Uuid) -> Result<Vec<VoiceOccupant>> {
        Ok(sqlx::query_as::<_, VoiceOccupant>(
            "SELECT p.channel_id, c.space_id, p.user_id, u.handle, u.display_name, m.role
               FROM voice_presence p
               JOIN channels c     ON c.id = p.channel_id
               JOIN users u        ON u.id = p.user_id
               JOIN space_members m ON m.space_id = c.space_id AND m.user_id = p.user_id
              WHERE c.space_id = $1
                AND p.seen_at > now() - make_interval(secs => $2)
                AND u.deleted_at IS NULL
              ORDER BY p.channel_id, p.joined_at",
        )
        .bind(space)
        .bind(PRESENCE_TTL_SECS as f64)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Everyone in any voice room of any space `user` belongs to.
    ///
    /// The whole picture in one query, so the first payload of a session can
    /// carry it. Asking per space would mean the sidebar filling in a space at
    /// a time as answers arrived, which is exactly the staggered load this is
    /// meant to remove.
    pub async fn voice_for_user(&self, user: Uuid) -> Result<Vec<VoiceOccupant>> {
        Ok(sqlx::query_as::<_, VoiceOccupant>(
            "SELECT p.channel_id, c.space_id, p.user_id, u.handle, u.display_name, m.role
               FROM voice_presence p
               JOIN channels c      ON c.id = p.channel_id
               JOIN users u         ON u.id = p.user_id
               JOIN space_members m ON m.space_id = c.space_id AND m.user_id = p.user_id
              WHERE c.space_id IN (SELECT space_id FROM space_members WHERE user_id = $1)
                AND p.seen_at > now() - make_interval(secs => $2)
                AND u.deleted_at IS NULL
              ORDER BY p.channel_id, p.joined_at",
        )
        .bind(user)
        .bind(PRESENCE_TTL_SECS as f64)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Everyone in any voice room anywhere.
    ///
    /// For building the server's in-memory projection at boot. Not reachable
    /// from any request — a client is only ever shown the spaces it belongs
    /// to, and the filtering happens above this.
    pub async fn voice_everywhere(&self) -> Result<Vec<VoiceOccupant>> {
        Ok(sqlx::query_as::<_, VoiceOccupant>(
            "SELECT p.channel_id, c.space_id, p.user_id, u.handle, u.display_name, m.role
               FROM voice_presence p
               JOIN channels c      ON c.id = p.channel_id
               JOIN users u         ON u.id = p.user_id
               JOIN space_members m ON m.space_id = c.space_id AND m.user_id = p.user_id
              WHERE p.seen_at > now() - make_interval(secs => $1)
                AND u.deleted_at IS NULL
              ORDER BY p.channel_id, p.joined_at",
        )
        .bind(PRESENCE_TTL_SECS as f64)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Drop rows nobody is behind any more.
    ///
    /// Reads already filter on freshness, so this is only housekeeping — it
    /// keeps a table that is written on every heartbeat from accumulating the
    /// remains of every call ever made.
    pub async fn voice_sweep(&self) -> Result<u64> {
        let done = sqlx::query(
            "DELETE FROM voice_presence WHERE seen_at < now() - make_interval(secs => $1)",
        )
        .bind((PRESENCE_TTL_SECS * 4) as f64)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}
