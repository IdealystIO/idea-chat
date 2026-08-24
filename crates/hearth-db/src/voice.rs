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
///
/// Three missed heartbeats. That is the tolerance budget for a network blip,
/// and it has to be a budget rather than zero: presence is a **lease**, and the
/// thing renewing it is a WebSocket that laptops, tunnels and sleeping Wi-Fi
/// interrupt regularly. Ending someone's call the instant that socket hiccups
/// is what made the sidebar shuffle and the join tone fire every few seconds.
///
/// Background-tab throttling would break this — a throttled timer fires once a
/// minute — except that Chrome exempts pages capturing or playing audio, which
/// every page in a call is doing by definition.
pub const PRESENCE_TTL_SECS: i64 = 45;

/// How long someone gets after their live socket goes away.
///
/// A closing socket stops *vouching* for someone rather than evicting them:
/// [`Db::voice_fading`] backdates the lease so it expires in this long unless a
/// heartbeat renews it. A reconnect re-asserts presence immediately (the client
/// does that on every snapshot), so a blip costs nothing — while a genuinely
/// closed tab is gone within this window instead of the full TTL.
pub const PRESENCE_GRACE_SECS: i64 = 20;

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

    /// Stop vouching for `user` without evicting them.
    ///
    /// For the one case that is genuinely ambiguous: their live socket has gone
    /// away, and nothing here can tell a closed tab from a tunnel that blinked.
    /// Deleting the row answers that question wrongly half the time, and the
    /// wrong half is loud — everyone in the room hears them leave and rejoin,
    /// and they move to the bottom of the list.
    ///
    /// So the lease is shortened instead of cancelled. Come back within
    /// [`PRESENCE_GRACE_SECS`] and nothing happened; do not, and the sweeper
    /// reaps it and announces the departure once.
    ///
    /// Scoped to the channel, like [`voice_absent`](Self::voice_absent), and
    /// for the same reason: a socket closing after its owner already moved
    /// rooms must not shorten the lease on the room they are actually in.
    ///
    /// Never *lengthens* a lease — `LEAST` means a row that is already closer
    /// to expiry than the grace period stays where it is.
    pub async fn voice_fading(&self, user: Uuid, channel: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE voice_presence
                SET seen_at = LEAST(seen_at, now() - make_interval(secs => $3))
              WHERE user_id = $1 AND channel_id = $2",
        )
        .bind(user)
        .bind(channel)
        .bind((PRESENCE_TTL_SECS - PRESENCE_GRACE_SECS) as f64)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Reap expired leases, and say who they belonged to.
    ///
    /// This is the *only* thing that removes someone who did not say they were
    /// leaving, which is what makes presence a lease rather than a property of
    /// whichever socket happened to be open. It returns the rows it took so the
    /// caller can announce each departure exactly once — a sweep that silently
    /// deleted them would leave every other client drawing a ghost until it
    /// next reconnected.
    ///
    /// Reads already filter on freshness, so a row that has expired but not yet
    /// been swept is invisible either way. The sweep exists to turn that into
    /// an *event*.
    pub async fn voice_sweep(&self) -> Result<Vec<(Uuid, Uuid, Uuid)>> {
        Ok(sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
            "DELETE FROM voice_presence p
              USING channels c
              WHERE c.id = p.channel_id
                AND p.seen_at < now() - make_interval(secs => $1)
          RETURNING p.user_id, p.channel_id, c.space_id",
        )
        .bind(PRESENCE_TTL_SECS as f64)
        .fetch_all(&self.pool)
        .await?)
    }
}
