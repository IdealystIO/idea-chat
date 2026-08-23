//! Postgres-backed [`RoomStore`] — the recommended default.
//!
//! # Why this and not Redis
//!
//! A chat product already runs Postgres. Coordination traffic here is one write
//! per user per room transition — never per audio packet — so it is a few
//! hundred writes per second at serious scale, which Postgres handles without
//! noticing. Redis buys throughput this workload does not need, and costs a
//! second stateful system to run, monitor, back up, and fail over.
//!
//! # How atomicity is achieved
//!
//! `pg_advisory_xact_lock(hashtext(room))` inside a transaction. That serializes
//! concurrent operations on the SAME room — the only place ordering matters —
//! while leaving different rooms fully concurrent. It is the direct analogue of
//! running a Lua script on Redis's single-threaded core, minus the global
//! bottleneck: Redis serializes *everything*, this serializes per room.
//!
//! The lock is released automatically at commit or rollback, so a panicking or
//! disconnecting client cannot wedge a room. That is a genuine advantage over
//! hand-rolled lock keys, which need their own expiry and can be orphaned.
//!
//! Node counters are updated with `UPDATE ... SET x = x + 1`, atomic on their
//! own row and outside the per-room lock, so two different rooms landing on one
//! node do not contend.
//!
//! # Durability
//!
//! Unlike the other backends, assignments survive a restart of the coordination
//! tier. Rooms are still reconciled against node liveness on every join, so a
//! stale row from before an outage is reassigned rather than trusted.

use async_trait::async_trait;
use deadpool_postgres::{Config, Pool, Runtime};
use tokio_postgres::NoTls;

use crate::{
    Assignment, LeaveOutcome, NodeLoad, NodeSpec, NodeView, RoomStore, RoomView, StoreError, Ttls,
};

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Schema. Applied by [`PostgresStore::migrate`]; safe to run repeatedly.
///
/// `voice_room_epochs` is a separate table on purpose: the epoch must OUTLIVE
/// the room it belongs to. If it were a column on `voice_rooms` it would vanish
/// with the room, epochs would restart at 1 on reopen, and a partitioned node's
/// stale claim could pass a fencing check.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS voice_nodes (
    id           TEXT PRIMARY KEY,
    url          TEXT        NOT NULL,
    capacity     INTEGER     NOT NULL DEFAULT 0,
    rooms        INTEGER     NOT NULL DEFAULT 0,
    participants INTEGER     NOT NULL DEFAULT 0,
    draining     BOOLEAN     NOT NULL DEFAULT FALSE,
    last_seen    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS voice_rooms (
    room         TEXT PRIMARY KEY,
    node_id      TEXT        NOT NULL,
    epoch        BIGINT      NOT NULL,
    participants INTEGER     NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS voice_room_epochs (
    room  TEXT PRIMARY KEY,
    epoch BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS voice_rooms_node_idx ON voice_rooms (node_id);
"#;

/// Multi-node [`RoomStore`] over Postgres.
pub struct PostgresStore {
    pool: Pool,
    ttls: Ttls,
}

impl PostgresStore {
    /// Connect using a libpq-style URL (`postgres://user:pass@host:port/db`).
    pub async fn connect(url: &str, ttls: Ttls) -> Result<Self, StoreError> {
        Self::connect_inner(url, None, ttls).await
    }

    /// Connect with every table confined to `schema`.
    ///
    /// Useful for running several isolated fleets against one database, and how
    /// the conformance suite keeps parallel tests from colliding. The schema is
    /// created if absent.
    pub async fn connect_in_schema(
        url: &str,
        schema: &str,
        ttls: Ttls,
    ) -> Result<Self, StoreError> {
        // Create it first: `search_path` pointing at a missing schema is legal
        // but every CREATE TABLE would land in `public` instead.
        let bootstrap = Self::connect_inner(url, None, ttls).await?;
        {
            let client = bootstrap.pool.get().await.map_err(backend)?;
            // Identifier, not a value — parameters cannot be used for DDL
            // names, so reject anything that is not a plain identifier rather
            // than interpolate blind.
            if !schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(StoreError::Backend(format!("unsafe schema name: {schema}")));
            }
            client
                .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
                .await
                .map_err(backend)?;
        }
        Self::connect_inner(url, Some(schema), ttls).await
    }

    async fn connect_inner(
        url: &str,
        schema: Option<&str>,
        ttls: Ttls,
    ) -> Result<Self, StoreError> {
        let url = match schema {
            Some(s) => {
                let sep = if url.contains('?') { '&' } else { '?' };
                format!("{url}{sep}options=-c%20search_path%3D{s}")
            }
            None => url.to_string(),
        };
        let mut cfg = Config::new();
        cfg.url = Some(url);
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(backend)?;
        let store = PostgresStore { pool, ttls };
        // Prove the pool works now rather than on the first user's join.
        let _ = store.pool.get().await.map_err(backend)?;
        Ok(store)
    }

    /// Create the schema if absent.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        client.batch_execute(SCHEMA).await.map_err(backend)
    }

    /// Delete every row. Test helper.
    pub async fn truncate(&self) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .batch_execute(
                "TRUNCATE voice_nodes, voice_rooms, voice_room_epochs RESTART IDENTITY",
            )
            .await
            .map_err(backend)
    }

    /// Node-liveness cutoff as a Postgres interval literal.
    fn node_interval(&self) -> String {
        format!("{} milliseconds", self.ttls.node_ms)
    }
}

#[async_trait]
impl RoomStore for PostgresStore {
    async fn register_node(&self, spec: NodeSpec) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        // `draining` is intentionally NOT reset: a node restarting mid-drain
        // must stay out of the pool rather than silently start taking rooms.
        client
            .execute(
                "INSERT INTO voice_nodes (id, url, capacity, last_seen)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (id) DO UPDATE
                   SET url = EXCLUDED.url,
                       capacity = EXCLUDED.capacity,
                       last_seen = now()",
                &[&spec.id, &spec.url, &(spec.capacity as i32)],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn heartbeat(&self, id: &str, load: NodeLoad) -> Result<bool, StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        // Evict the lapsed before deciding whether THIS node is still known, so
        // the answer matches what `join` would conclude a moment later.
        client
            .execute(
                &format!(
                    "DELETE FROM voice_nodes WHERE last_seen < now() - INTERVAL '{}'",
                    self.node_interval()
                ),
                &[],
            )
            .await
            .map_err(backend)?;

        // The node's own load report is authoritative: it is the only party
        // that knows who is really connected, so this self-corrects any drift
        // the incremental counters accumulate.
        let updated = client
            .execute(
                "UPDATE voice_nodes
                    SET rooms = $2, participants = $3, last_seen = now()
                  WHERE id = $1",
                &[&id, &(load.rooms as i32), &(load.participants as i32)],
            )
            .await
            .map_err(backend)?;
        Ok(updated == 1)
    }

    async fn set_draining(&self, id: &str, draining: bool) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute(
                "UPDATE voice_nodes SET draining = $2 WHERE id = $1",
                &[&id, &draining],
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn remove_node(&self, id: &str) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        client
            .execute("DELETE FROM voice_nodes WHERE id = $1", &[&id])
            .await
            .map_err(backend)?;
        // Rooms are left behind deliberately — `join` already detects an owner
        // that is no longer registered and reassigns. One teardown path, shared
        // by every backend.
        Ok(())
    }

    async fn list_nodes(&self) -> Result<Vec<NodeView>, StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        let rows = client
            .query(
                &format!(
                    "SELECT id, url, capacity, rooms, participants, draining,
                            (EXTRACT(EPOCH FROM (now() - last_seen)) * 1000)::float8
                       FROM voice_nodes
                      WHERE last_seen >= now() - INTERVAL '{}'",
                    self.node_interval()
                ),
                &[],
            )
            .await
            .map_err(backend)?;

        Ok(rows
            .iter()
            .map(|r| NodeView {
                spec: NodeSpec {
                    id: r.get::<_, String>(0),
                    url: r.get::<_, String>(1),
                    capacity: r.get::<_, i32>(2) as u32,
                },
                load: NodeLoad {
                    rooms: r.get::<_, i32>(3) as u32,
                    participants: r.get::<_, i32>(4) as u32,
                },
                draining: r.get::<_, bool>(5),
                last_seen_ms_ago: r.get::<_, f64>(6).max(0.0) as u64,
            })
            .collect())
    }

    async fn join(&self, room: &str) -> Result<Assignment, StoreError> {
        let mut client = self.pool.get().await.map_err(backend)?;
        let tx = client.transaction().await.map_err(backend)?;

        // Serialize concurrent operations on THIS room only. Released on commit
        // or rollback, so a crashed caller cannot wedge the room — the failure
        // mode a hand-rolled lock key has to solve with its own expiry.
        tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&room])
            .await
            .map_err(backend)?;

        tx.execute(
            &format!(
                "DELETE FROM voice_nodes WHERE last_seen < now() - INTERVAL '{}'",
                self.node_interval()
            ),
            &[],
        )
        .await
        .map_err(backend)?;

        // Extend an open room, but only while its owner is still registered.
        let open = tx
            .query_opt(
                "SELECT r.node_id, r.epoch, n.url
                   FROM voice_rooms r
                   JOIN voice_nodes n ON n.id = r.node_id
                  WHERE r.room = $1",
                &[&room],
            )
            .await
            .map_err(backend)?;

        if let Some(row) = open {
            let node_id: String = row.get(0);
            let epoch: i64 = row.get(1);
            let node_url: String = row.get(2);
            let participants: i32 = tx
                .query_one(
                    "UPDATE voice_rooms
                        SET participants = participants + 1, updated_at = now()
                      WHERE room = $1
                  RETURNING participants",
                    &[&room],
                )
                .await
                .map_err(backend)?
                .get(0);
            tx.execute(
                "UPDATE voice_nodes SET participants = participants + 1 WHERE id = $1",
                &[&node_id],
            )
            .await
            .map_err(backend)?;
            tx.commit().await.map_err(backend)?;
            return Ok(Assignment {
                node_id,
                node_url,
                epoch: epoch as u64,
                participants: participants as u32,
                newly_assigned: false,
            });
        }

        // Either no room, or one whose owner has been evicted. Clear any stale
        // row and place it fresh.
        tx.execute("DELETE FROM voice_rooms WHERE room = $1", &[&room])
            .await
            .map_err(backend)?;

        let candidate = tx
            .query_opt(
                "SELECT id, url FROM voice_nodes
                  WHERE draining = FALSE
                    AND (capacity = 0 OR participants < capacity)
                  ORDER BY participants ASC, id ASC
                  LIMIT 1",
                &[],
            )
            .await
            .map_err(backend)?;
        let Some(row) = candidate else {
            // Roll back rather than leave the stale delete committed.
            tx.rollback().await.map_err(backend)?;
            return Err(StoreError::NoCapacity);
        };
        let node_id: String = row.get(0);
        let node_url: String = row.get(1);

        // Monotonic per room, and this table is never cleaned up with the room.
        let epoch: i64 = tx
            .query_one(
                "INSERT INTO voice_room_epochs (room, epoch) VALUES ($1, 1)
                 ON CONFLICT (room) DO UPDATE SET epoch = voice_room_epochs.epoch + 1
                 RETURNING epoch",
                &[&room],
            )
            .await
            .map_err(backend)?
            .get(0);

        tx.execute(
            "INSERT INTO voice_rooms (room, node_id, epoch, participants)
             VALUES ($1, $2, $3, 1)",
            &[&room, &node_id, &epoch],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "UPDATE voice_nodes
                SET rooms = rooms + 1, participants = participants + 1
              WHERE id = $1",
            &[&node_id],
        )
        .await
        .map_err(backend)?;

        tx.commit().await.map_err(backend)?;
        Ok(Assignment {
            node_id,
            node_url,
            epoch: epoch as u64,
            participants: 1,
            newly_assigned: true,
        })
    }

    async fn leave(&self, room: &str) -> Result<LeaveOutcome, StoreError> {
        let mut client = self.pool.get().await.map_err(backend)?;
        let tx = client.transaction().await.map_err(backend)?;
        tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&room])
            .await
            .map_err(backend)?;

        let Some(row) = tx
            .query_opt(
                "SELECT node_id, participants FROM voice_rooms WHERE room = $1",
                &[&room],
            )
            .await
            .map_err(backend)?
        else {
            tx.commit().await.map_err(backend)?;
            return Ok(LeaveOutcome::Unknown);
        };
        let node_id: String = row.get(0);
        let participants: i32 = row.get(1);
        let remaining = (participants - 1).max(0);

        // GREATEST(...,0): a duplicate leave must not drive a node's counter
        // negative, which would make it look infinitely attractive to the
        // balancer and pull the whole fleet's traffic onto it.
        tx.execute(
            "UPDATE voice_nodes
                SET participants = GREATEST(participants - 1, 0)
              WHERE id = $1",
            &[&node_id],
        )
        .await
        .map_err(backend)?;

        if remaining == 0 {
            tx.execute("DELETE FROM voice_rooms WHERE room = $1", &[&room])
                .await
                .map_err(backend)?;
            tx.execute(
                "UPDATE voice_nodes SET rooms = GREATEST(rooms - 1, 0) WHERE id = $1",
                &[&node_id],
            )
            .await
            .map_err(backend)?;
            tx.commit().await.map_err(backend)?;
            return Ok(LeaveOutcome::Closed { node_id });
        }

        tx.execute(
            "UPDATE voice_rooms SET participants = $2, updated_at = now() WHERE room = $1",
            &[&room, &remaining],
        )
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(LeaveOutcome::Active { node_id, participants: remaining as u32 })
    }

    async fn room(&self, room: &str) -> Result<Option<RoomView>, StoreError> {
        let client = self.pool.get().await.map_err(backend)?;
        let row = client
            .query_opt(
                "SELECT node_id, epoch, participants FROM voice_rooms WHERE room = $1",
                &[&room],
            )
            .await
            .map_err(backend)?;
        Ok(row.map(|r| RoomView {
            room: room.to_string(),
            node_id: r.get(0),
            epoch: r.get::<_, i64>(1) as u64,
            participants: r.get::<_, i32>(2) as u32,
        }))
    }
}
