//! Calls and presence: who is connected, who is in which call, call
//! instances, and rings.

use chat_model::{Id, Presence, Ring, Status, VoiceState};
use time::OffsetDateTime;

use crate::{millis, new_id, presence_of, Db, Error, Result};

/// What joining a call did.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinedCall {
    pub state: VoiceState,
    pub room: Id,
    /// Nobody else is in the room: this device makes its key.
    pub make_key: bool,
    /// The call instance is new.
    pub started: bool,
    /// The call the person was in before, which joining this one left.
    pub left: Option<LeftCall>,
}

/// What leaving a call did.
#[derive(Debug, Clone, PartialEq)]
pub struct LeftCall {
    pub user: Id,
    pub channel: Id,
    pub room: Id,
    /// They were the last one; the instance is over.
    pub ended: bool,
}

#[derive(sqlx::FromRow)]
struct VoiceRow {
    user_id: Id,
    channel_id: Id,
    muted: bool,
    deafened: bool,
    camera: bool,
    screen: bool,
    joined_at: OffsetDateTime,
}

impl From<VoiceRow> for VoiceState {
    fn from(r: VoiceRow) -> VoiceState {
        VoiceState {
            user: r.user_id,
            channel: r.channel_id,
            muted: r.muted,
            deafened: r.deafened,
            camera: r.camera,
            screen: r.screen,
            joined_at: millis(r.joined_at),
        }
    }
}

const VOICE_COLS: &str = "user_id, channel_id, muted, deafened, camera, screen, joined_at";

/// A connection whose heartbeat is older than this is gone.
pub const STALE_SECS: i64 = 60;

impl Db {
    // ---- connections and presence ----

    pub async fn open_connection(&self, connection: Id, user: Id, device: Option<Id>, instance: &str) -> Result<()> {
        sqlx::query("INSERT INTO connections (id, user_id, device_id, instance) VALUES ($1, $2, $3, $4)")
            .bind(connection)
            .bind(user)
            .bind(device)
            .bind(instance)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The newest live connection `user` has from `device`.
    pub async fn connection_for(&self, user: Id, device: Id) -> Result<Option<Id>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM connections WHERE user_id = $1 AND device_id = $2 ORDER BY connected_at DESC LIMIT 1",
        )
        .bind(user)
        .bind(device)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Keep an instance's connections alive.
    pub async fn heartbeat(&self, instance: &str) -> Result<()> {
        sqlx::query("UPDATE connections SET heartbeat_at = now() WHERE instance = $1")
            .bind(instance)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Drop a connection, and the call it was in. Returns the call left.
    pub async fn disconnect(&self, connection: Id) -> Result<Option<LeftCall>> {
        let in_call: Option<Id> = sqlx::query_scalar("SELECT user_id FROM voice_states WHERE connection_id = $1")
            .bind(connection)
            .fetch_optional(&self.pool)
            .await?;
        let left = match in_call {
            Some(user) => self.leave_call(user).await?,
            None => None,
        };
        sqlx::query("DELETE FROM connections WHERE id = $1").bind(connection).execute(&self.pool).await?;
        Ok(left)
    }

    /// Connections whose instance stopped heartbeating: removed, with the
    /// people they belonged to and the calls that took them out of.
    pub async fn sweep_connections(&self) -> Result<(Vec<Id>, Vec<LeftCall>)> {
        let stale: Vec<(Id, Id)> = sqlx::query_as(
            "SELECT id, user_id FROM connections WHERE heartbeat_at < now() - make_interval(secs => $1)",
        )
        .bind(STALE_SECS as f64)
        .fetch_all(&self.pool)
        .await?;
        let mut left = Vec::new();
        let mut users = Vec::new();
        for (connection, user) in stale {
            if let Some(l) = self.disconnect(connection).await? {
                left.push(l);
            }
            if !users.contains(&user) {
                users.push(user);
            }
        }
        Ok((users, left))
    }

    /// Remove every connection of `instance` — on a clean start, the
    /// previous run's.
    pub async fn clear_instance(&self, instance: &str) -> Result<Vec<LeftCall>> {
        let ids: Vec<Id> = sqlx::query_scalar("SELECT id FROM connections WHERE instance = $1")
            .bind(instance)
            .fetch_all(&self.pool)
            .await?;
        let mut left = Vec::new();
        for id in ids {
            if let Some(l) = self.disconnect(id).await? {
                left.push(l);
            }
        }
        Ok(left)
    }

    pub async fn set_idle(&self, connection: Id, idle: bool) -> Result<()> {
        sqlx::query("UPDATE connections SET idle = $2 WHERE id = $1")
            .bind(connection)
            .bind(idle)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// What others see of each person: their chosen status while they have a
    /// connection (idle if every connection is), offline otherwise — and
    /// offline if they chose to appear so.
    pub async fn presence(&self, users: &[Id]) -> Result<Vec<(Id, Status)>> {
        let rows: Vec<(Id, String, Option<String>, bool)> = sqlx::query_as(
            "SELECT u.id, u.status_presence, u.status_text, bool_and(c.idle) \
             FROM users u JOIN connections c ON c.user_id = u.id \
             WHERE u.id = ANY($1) AND c.heartbeat_at > now() - make_interval(secs => $2) \
             GROUP BY u.id",
        )
        .bind(users)
        .bind(STALE_SECS as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, chosen, text, all_idle)| {
                let presence = match presence_of(&chosen) {
                    Presence::Offline => return None,
                    Presence::Online if all_idle => Presence::Idle,
                    p => p,
                };
                Some((id, Status { presence, text }))
            })
            .collect())
    }

    /// What one person looks like to others right now.
    pub async fn presence_one(&self, user: Id) -> Result<Status> {
        Ok(self.presence(&[user]).await?.pop().map(|(_, s)| s).unwrap_or_default())
    }

    // ---- calls ----

    /// Put `user` in `channel`'s call, starting an instance if there is none,
    /// and leaving any other call they were in.
    pub async fn join_call(&self, user: Id, channel: Id, connection: Id, device: Id) -> Result<JoinedCall> {
        let current: Option<Id> = sqlx::query_scalar("SELECT channel_id FROM voice_states WHERE user_id = $1")
            .bind(user)
            .fetch_optional(&self.pool)
            .await?;
        let left = match current {
            Some(c) if c != channel => self.leave_call(user).await?,
            _ => None,
        };
        let mut tx = self.pool.begin().await?;
        // Serialise joins per channel, so two first joiners cannot both
        // think they start (and key) the call.
        sqlx::query("SELECT id FROM channels WHERE id = $1 FOR UPDATE").bind(channel).execute(&mut *tx).await?;
        let live: Option<Id> =
            sqlx::query_scalar("SELECT id FROM call_rooms WHERE channel_id = $1 AND ended_at IS NULL")
                .bind(channel)
                .fetch_optional(&mut *tx)
                .await?;
        let (room, started) = match live {
            Some(r) => (r, false),
            None => {
                let r = new_id();
                sqlx::query("INSERT INTO call_rooms (id, channel_id, started_by) VALUES ($1, $2, $3)")
                    .bind(r)
                    .bind(channel)
                    .bind(user)
                    .execute(&mut *tx)
                    .await?;
                (r, true)
            }
        };
        let others: i64 =
            sqlx::query_scalar("SELECT count(*) FROM voice_states WHERE room_id = $1 AND user_id <> $2")
                .bind(room)
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
        let row: VoiceRow = sqlx::query_as(&format!(
            "INSERT INTO voice_states (user_id, channel_id, room_id, connection_id, device_id) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_id) DO UPDATE SET connection_id = EXCLUDED.connection_id, \
               device_id = EXCLUDED.device_id \
             RETURNING {VOICE_COLS}"
        ))
        .bind(user)
        .bind(channel)
        .bind(room)
        .bind(connection)
        .bind(device)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE call_rooms SET participants = array_append(participants, $2) \
             WHERE id = $1 AND NOT ($2 = ANY(participants))",
        )
        .bind(room)
        .bind(user)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE rings SET ringing = array_remove(ringing, $2) WHERE room_id = $1")
            .bind(room)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(JoinedCall { state: row.into(), room, make_key: others == 0, started, left })
    }

    /// Take `user` out of whatever call they are in. The last one out ends
    /// the instance (and any ring).
    pub async fn leave_call(&self, user: Id) -> Result<Option<LeftCall>> {
        let mut tx = self.pool.begin().await?;
        let gone: Option<(Id, Id)> =
            sqlx::query_as("DELETE FROM voice_states WHERE user_id = $1 RETURNING channel_id, room_id")
                .bind(user)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((channel, room)) = gone else { return Ok(None) };
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM voice_states WHERE room_id = $1")
            .bind(room)
            .fetch_one(&mut *tx)
            .await?;
        let ended = remaining == 0;
        if ended {
            sqlx::query("UPDATE call_rooms SET ended_at = now() WHERE id = $1").bind(room).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM rings WHERE room_id = $1").bind(room).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(Some(LeftCall { user, channel, room, ended }))
    }

    /// Change what someone in a call is sending.
    pub async fn update_voice(&self, user: Id, muted: bool, deafened: bool, camera: bool, screen: bool) -> Result<VoiceState> {
        let row: Option<VoiceRow> = sqlx::query_as(&format!(
            "UPDATE voice_states SET muted = $2, deafened = $3, camera = $4, screen = $5 WHERE user_id = $1 \
             RETURNING {VOICE_COLS}"
        ))
        .bind(user)
        .bind(muted || deafened)
        .bind(deafened)
        .bind(camera)
        .bind(screen)
        .fetch_optional(&self.pool)
        .await?;
        row.map(VoiceState::from).ok_or(Error::NotFound)
    }

    /// Everyone in a call in any of the channels.
    pub async fn voice_in(&self, channels: &[Id]) -> Result<Vec<VoiceState>> {
        let rows: Vec<VoiceRow> =
            sqlx::query_as(&format!("SELECT {VOICE_COLS} FROM voice_states WHERE channel_id = ANY($1)"))
                .bind(channels)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(VoiceState::from).collect())
    }

    /// The room `user` is in and the device they joined it from.
    pub async fn my_room(&self, user: Id) -> Result<Option<(Id, Id, Id)>> {
        Ok(sqlx::query_as("SELECT room_id, channel_id, device_id FROM voice_states WHERE user_id = $1")
            .bind(user)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Who is in a room, and the devices (with public keys) they joined from.
    pub async fn room_devices(&self, room: Id) -> Result<Vec<(Id, Id, String)>> {
        Ok(sqlx::query_as(
            "SELECT v.user_id, v.device_id, d.public_key FROM voice_states v JOIN devices d ON d.id = v.device_id \
             WHERE v.room_id = $1",
        )
        .bind(room)
        .fetch_all(&self.pool)
        .await?)
    }

    /// A call instance: its channel, who started it and when, everyone who
    /// was in it, when it ended, and its record message.
    pub async fn room(&self, room: Id) -> Result<(Id, Id, i64, Vec<Id>, Option<i64>, Option<Id>)> {
        let row: Option<(Id, Id, OffsetDateTime, Vec<Id>, Option<OffsetDateTime>, Option<Id>)> = sqlx::query_as(
            "SELECT channel_id, started_by, started_at, participants, ended_at, message_id FROM call_rooms WHERE id = $1",
        )
        .bind(room)
        .fetch_optional(&self.pool)
        .await?;
        let (c, by, at, people, ended, msg) = row.ok_or(Error::NotFound)?;
        Ok((c, by, millis(at), people, ended.map(millis), msg))
    }

    pub async fn set_room_message(&self, room: Id, message: Id) -> Result<()> {
        sqlx::query("UPDATE call_rooms SET message_id = $2 WHERE id = $1")
            .bind(room)
            .bind(message)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- rings ----

    pub async fn start_ring(&self, room: Id, channel: Id, by: Id, ringing: &[Id]) -> Result<Ring> {
        let at: OffsetDateTime = sqlx::query_scalar(
            "INSERT INTO rings (room_id, channel_id, started_by, ringing) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (room_id) DO UPDATE SET ringing = EXCLUDED.ringing RETURNING started_at",
        )
        .bind(room)
        .bind(channel)
        .bind(by)
        .bind(ringing)
        .fetch_one(&self.pool)
        .await?;
        Ok(Ring { call: room, channel, started_by: by, started_at: millis(at), ringing: ringing.to_vec() })
    }

    /// Stop ringing `user` (declined, or answered elsewhere). Returns the
    /// ring as it now stands, or `None` if nobody is left being rung.
    pub async fn stop_ringing(&self, room: Id, user: Option<Id>) -> Result<Option<Ring>> {
        let row: Option<(Id, Id, OffsetDateTime, Vec<Id>)> = match user {
            Some(u) => {
                sqlx::query_as(
                    "UPDATE rings SET ringing = array_remove(ringing, $2) WHERE room_id = $1 \
                     RETURNING channel_id, started_by, started_at, ringing",
                )
                .bind(room)
                .bind(u)
                .fetch_optional(&self.pool)
                .await?
            }
            None => {
                sqlx::query("DELETE FROM rings WHERE room_id = $1").bind(room).execute(&self.pool).await?;
                None
            }
        };
        match row {
            Some((channel, by, at, ringing)) if !ringing.is_empty() => {
                Ok(Some(Ring { call: room, channel, started_by: by, started_at: millis(at), ringing }))
            }
            Some(_) => {
                sqlx::query("DELETE FROM rings WHERE room_id = $1").bind(room).execute(&self.pool).await?;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    pub async fn rings_in(&self, channels: &[Id]) -> Result<Vec<Ring>> {
        let rows: Vec<(Id, Id, Id, OffsetDateTime, Vec<Id>)> = sqlx::query_as(
            "SELECT room_id, channel_id, started_by, started_at, ringing FROM rings WHERE channel_id = ANY($1)",
        )
        .bind(channels)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(call, channel, started_by, at, ringing)| Ring { call, channel, started_by, started_at: millis(at), ringing })
            .collect())
    }
}
