//! Redis-backed [`RoomStore`] for a multi-node fleet.
//!
//! # Why every compound operation is a Lua script
//!
//! Redis executes a script on its single-threaded core with nothing interleaved,
//! so a script is the only way to make "read the assignment, bump the count, and
//! maybe delete it" one indivisible step. Doing the same work as separate
//! `GET`/`SET`/`DEL` round trips reintroduces exactly the race this crate exists
//! to remove — and it is a race that loses audio silently rather than erroring.
//!
//! `WATCH`/`MULTI` could also work, but optimistic retries under contention on
//! the hottest key in the system (a popular room) is the wrong trade. A script
//! is one round trip with no retry loop.
//!
//! # Clock
//!
//! Liveness uses Redis's own `TIME`, never the caller's clock. With several
//! directors on separate machines, client clock skew would make one director
//! consider a node dead while another considers it healthy — nodes would flap in
//! and out of the pool for no reason. One clock, no skew.
//!
//! # Key layout
//!
//! ```text
//! {p}:nodes           ZSET  member = node id, score = last heartbeat (ms)
//! {p}:node:{id}       HASH  url, capacity, rooms, participants, draining
//! {p}:room:{room}     HASH  node, epoch, count          (TTL: room_ms)
//! {p}:epoch:{room}    STR   monotonic assignment counter (TTL: 24h)
//! ```
//!
//! The epoch key deliberately outlives its room. Reusing an epoch after a room
//! closes and reopens would let a partitioned node's stale claim pass a fencing
//! check, which is precisely what it is there to stop.
//!
//! # Cluster note
//!
//! Scripts touch keys derived inside the script (the chosen node's hash), so
//! this expects a single logical Redis, not Redis Cluster. Coordination state is
//! small and low-traffic — one instance (replicated for HA) is the right shape.
//! Sharding it would need the node registry co-located by hash tag, which buys
//! nothing at this size.

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};

use crate::{
    Assignment, LeaveOutcome, NodeLoad, NodeSpec, NodeView, RoomStore, RoomView, StoreError, Ttls,
};

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Prune lapsed nodes, then either extend an open room or assign a fresh node.
///
/// Returns `{0}` when nothing has capacity, otherwise
/// `{1, node_id, node_url, epoch, participants, newly_assigned}`.
const JOIN_LUA: &str = r#"
local prefix   = ARGV[1]
local node_ttl = tonumber(ARGV[2])
local room_ttl = tonumber(ARGV[3])
local room_key  = KEYS[1]
local epoch_key = KEYS[2]
local nodes_key = prefix .. ':nodes'

-- Redis' own clock: several directors must agree on liveness, and their
-- machine clocks will not.
local t = redis.call('TIME')
local now = (tonumber(t[1]) * 1000) + math.floor(tonumber(t[2]) / 1000)
local cutoff = now - node_ttl

-- 1. Evict nodes that have missed the heartbeat window.
local dead = redis.call('ZRANGEBYSCORE', nodes_key, '-inf', '(' .. cutoff)
for _, id in ipairs(dead) do
  redis.call('ZREM', nodes_key, id)
  redis.call('DEL', prefix .. ':node:' .. id)
end

-- 2. Extend an open room, but only while its owner is still alive.
local owner = redis.call('HGET', room_key, 'node')
if owner then
  if redis.call('ZSCORE', nodes_key, owner) then
    local count = redis.call('HINCRBY', room_key, 'count', 1)
    local epoch = tonumber(redis.call('HGET', room_key, 'epoch'))
    local url   = redis.call('HGET', prefix .. ':node:' .. owner, 'url')
    redis.call('PEXPIRE', room_key, room_ttl)
    redis.call('HINCRBY', prefix .. ':node:' .. owner, 'participants', 1)
    return {1, owner, url or '', epoch, count, 0}
  end
  -- Owner is gone: the record is stale. Drop it and reassign below.
  redis.call('DEL', room_key)
end

-- 3. Choose the least-loaded alive, non-draining, under-capacity node.
local alive = redis.call('ZRANGEBYSCORE', nodes_key, cutoff, '+inf')
local best, best_load, best_url
for _, id in ipairs(alive) do
  local h = redis.call('HMGET', prefix .. ':node:' .. id,
                       'url', 'participants', 'draining', 'capacity')
  local url      = h[1]
  local parts    = tonumber(h[2]) or 0
  local draining = h[3]
  local cap      = tonumber(h[4]) or 0
  if url and draining ~= '1' and (cap == 0 or parts < cap) then
    if best == nil or parts < best_load then
      best, best_load, best_url = id, parts, url
    end
  end
end
if best == nil then return {0} end

-- 4. Claim it under a fresh epoch.
local epoch = redis.call('INCR', epoch_key)
redis.call('PEXPIRE', epoch_key, 86400000)
redis.call('HSET', room_key, 'node', best, 'epoch', epoch, 'count', 1)
redis.call('PEXPIRE', room_key, room_ttl)
redis.call('HINCRBY', prefix .. ':node:' .. best, 'rooms', 1)
redis.call('HINCRBY', prefix .. ':node:' .. best, 'participants', 1)
return {1, best, best_url, epoch, 1, 1}
"#;

/// Decrement, and close the room in the same step when it empties.
///
/// `{0}` unknown room, `{1, node}` closed, `{2, node, remaining}` still active.
const LEAVE_LUA: &str = r#"
local prefix   = ARGV[1]
local room_ttl = tonumber(ARGV[2])
local room_key = KEYS[1]

local owner = redis.call('HGET', room_key, 'node')
if not owner then return {0} end

local count = redis.call('HINCRBY', room_key, 'count', -1)
local node_key = prefix .. ':node:' .. owner

-- Clamp rather than let a duplicate leave drive counters negative; a negative
-- participant count would make a node look infinitely attractive to the
-- balancer and it would absorb the whole fleet's traffic.
local parts = tonumber(redis.call('HGET', node_key, 'participants')) or 0
if parts > 0 then redis.call('HINCRBY', node_key, 'participants', -1) end

if count <= 0 then
  redis.call('DEL', room_key)
  local rooms = tonumber(redis.call('HGET', node_key, 'rooms')) or 0
  if rooms > 0 then redis.call('HINCRBY', node_key, 'rooms', -1) end
  return {1, owner}
end

redis.call('PEXPIRE', room_key, room_ttl)
return {2, owner, count}
"#;

/// Refresh liveness + load, reporting whether the node is still registered.
const HEARTBEAT_LUA: &str = r#"
local prefix = ARGV[1]
local id     = ARGV[2]
local rooms  = ARGV[3]
local parts  = ARGV[4]
local ttl    = tonumber(ARGV[5])
local nodes_key = prefix .. ':nodes'
local node_key  = prefix .. ':node:' .. id

local t = redis.call('TIME')
local now = (tonumber(t[1]) * 1000) + math.floor(tonumber(t[2]) / 1000)

-- Liveness is the ZSET score, the same signal `join` prunes on. Checking the
-- hash's TTL instead would leave a window where heartbeat reports "still
-- registered" while join has already evicted the node.
local score = redis.call('ZSCORE', nodes_key, id)
if (not score) or (redis.call('EXISTS', node_key) == 0)
   or (now - tonumber(score) > ttl) then
  redis.call('ZREM', nodes_key, id)
  redis.call('DEL', node_key)
  return 0
end

redis.call('ZADD', nodes_key, now, id)
-- The node's own view of its load is authoritative: it is the only party that
-- knows who is actually connected, and it self-corrects any drift the
-- coordination counters accumulate.
redis.call('HSET', node_key, 'rooms', rooms, 'participants', parts)
redis.call('PEXPIRE', node_key, ttl * 3)
return 1
"#;

/// Multi-node [`RoomStore`] over Redis.
pub struct RedisStore {
    conn: ConnectionManager,
    prefix: String,
    ttls: Ttls,
    join: Script,
    leave: Script,
    heartbeat: Script,
}

impl RedisStore {
    /// Connect to `url` (`redis://host:port`), namespacing keys under `prefix`.
    pub async fn connect(url: &str, prefix: &str, ttls: Ttls) -> Result<Self, StoreError> {
        let client = redis::Client::open(url).map_err(backend)?;
        let conn = ConnectionManager::new(client).await.map_err(backend)?;
        Ok(RedisStore {
            conn,
            prefix: prefix.to_string(),
            ttls,
            join: Script::new(JOIN_LUA),
            leave: Script::new(LEAVE_LUA),
            heartbeat: Script::new(HEARTBEAT_LUA),
        })
    }

    fn node_key(&self, id: &str) -> String {
        format!("{}:node:{}", self.prefix, id)
    }
    fn room_key(&self, room: &str) -> String {
        format!("{}:room:{}", self.prefix, room)
    }
    fn epoch_key(&self, room: &str) -> String {
        format!("{}:epoch:{}", self.prefix, room)
    }
    fn nodes_key(&self) -> String {
        format!("{}:nodes", self.prefix)
    }
}

#[async_trait]
impl RoomStore for RedisStore {
    async fn register_node(&self, spec: NodeSpec) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let node_key = self.node_key(&spec.id);
        // `draining` is set only when absent, so a node restarting mid-drain
        // does not silently rejoin the pool and take new rooms.
        redis::pipe()
            .atomic()
            .hset(&node_key, "url", &spec.url)
            .ignore()
            .hset(&node_key, "capacity", spec.capacity)
            .ignore()
            .cmd("HSETNX")
            .arg(&node_key)
            .arg("draining")
            .arg("0")
            .ignore()
            .cmd("HSETNX")
            .arg(&node_key)
            .arg("rooms")
            .arg(0)
            .ignore()
            .cmd("HSETNX")
            .arg(&node_key)
            .arg("participants")
            .arg(0)
            .ignore()
            .pexpire(&node_key, (self.ttls.node_ms * 3) as i64)
            .ignore()
            .cmd("ZADD")
            .arg(self.nodes_key())
            .arg("GT")
            .arg(now_ms_via(&mut conn).await?)
            .arg(&spec.id)
            .ignore()
            .query_async::<_, ()>(&mut conn)
            .await
            .map_err(backend)
    }

    async fn heartbeat(&self, id: &str, load: NodeLoad) -> Result<bool, StoreError> {
        let mut conn = self.conn.clone();
        let alive: i64 = self
            .heartbeat
            .key(self.node_key(id))
            .arg(&self.prefix)
            .arg(id)
            .arg(load.rooms)
            .arg(load.participants)
            .arg(self.ttls.node_ms)
            .invoke_async(&mut conn)
            .await
            .map_err(backend)?;
        Ok(alive == 1)
    }

    async fn set_draining(&self, id: &str, draining: bool) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        let _: () = conn
            .hset(self.node_key(id), "draining", if draining { "1" } else { "0" })
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn remove_node(&self, id: &str) -> Result<(), StoreError> {
        let mut conn = self.conn.clone();
        redis::pipe()
            .atomic()
            .del(self.node_key(id))
            .ignore()
            .zrem(self.nodes_key(), id)
            .ignore()
            .query_async::<_, ()>(&mut conn)
            .await
            .map_err(backend)?;
        // Rooms owned by the removed node are left to be detected as stale on
        // the next join (their owner is no longer in the ZSET), which keeps the
        // teardown path to a single well-tested code path.
        Ok(())
    }

    async fn list_nodes(&self) -> Result<Vec<NodeView>, StoreError> {
        let mut conn = self.conn.clone();
        let now = now_ms_via(&mut conn).await?;
        let cutoff = now.saturating_sub(self.ttls.node_ms);
        let ids: Vec<String> = conn
            .zrangebyscore(self.nodes_key(), cutoff as f64, f64::INFINITY)
            .await
            .map_err(backend)?;

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let key = self.node_key(&id);
            let vals: Vec<Option<String>> = conn
                .hget(&key, &["url", "capacity", "rooms", "participants", "draining"])
                .await
                .map_err(backend)?;
            let Some(url) = vals.first().cloned().flatten() else { continue };
            let num = |i: usize| -> u32 {
                vals.get(i).cloned().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
            };
            let score: Option<f64> =
                conn.zscore(self.nodes_key(), &id).await.map_err(backend)?;
            out.push(NodeView {
                spec: NodeSpec { id: id.clone(), url, capacity: num(1) },
                load: NodeLoad { rooms: num(2), participants: num(3) },
                draining: vals.get(4).cloned().flatten().as_deref() == Some("1"),
                last_seen_ms_ago: score
                    .map(|s| now.saturating_sub(s as u64))
                    .unwrap_or(u64::MAX),
            });
        }
        Ok(out)
    }

    async fn join(&self, room: &str) -> Result<Assignment, StoreError> {
        let mut conn = self.conn.clone();
        let reply: Vec<redis::Value> = self
            .join
            .key(self.room_key(room))
            .key(self.epoch_key(room))
            .arg(&self.prefix)
            .arg(self.ttls.node_ms)
            .arg(self.ttls.room_ms)
            .invoke_async(&mut conn)
            .await
            .map_err(backend)?;

        match reply.first() {
            Some(redis::Value::Int(1)) => Ok(Assignment {
                node_id: as_string(reply.get(1))?,
                node_url: as_string(reply.get(2))?,
                epoch: as_u64(reply.get(3))?,
                participants: as_u64(reply.get(4))? as u32,
                newly_assigned: as_u64(reply.get(5))? == 1,
            }),
            Some(redis::Value::Int(0)) => Err(StoreError::NoCapacity),
            other => Err(StoreError::Backend(format!("unexpected join reply: {other:?}"))),
        }
    }

    async fn leave(&self, room: &str) -> Result<LeaveOutcome, StoreError> {
        let mut conn = self.conn.clone();
        let reply: Vec<redis::Value> = self
            .leave
            .key(self.room_key(room))
            .arg(&self.prefix)
            .arg(self.ttls.room_ms)
            .invoke_async(&mut conn)
            .await
            .map_err(backend)?;

        match reply.first() {
            Some(redis::Value::Int(0)) => Ok(LeaveOutcome::Unknown),
            Some(redis::Value::Int(1)) => {
                Ok(LeaveOutcome::Closed { node_id: as_string(reply.get(1))? })
            }
            Some(redis::Value::Int(2)) => Ok(LeaveOutcome::Active {
                node_id: as_string(reply.get(1))?,
                participants: as_u64(reply.get(2))? as u32,
            }),
            other => Err(StoreError::Backend(format!("unexpected leave reply: {other:?}"))),
        }
    }

    async fn room(&self, room: &str) -> Result<Option<RoomView>, StoreError> {
        let mut conn = self.conn.clone();
        let vals: Vec<Option<String>> = conn
            .hget(self.room_key(room), &["node", "epoch", "count"])
            .await
            .map_err(backend)?;
        let Some(node_id) = vals.first().cloned().flatten() else { return Ok(None) };
        Ok(Some(RoomView {
            room: room.to_string(),
            node_id,
            epoch: vals.get(1).cloned().flatten().and_then(|v| v.parse().ok()).unwrap_or(0),
            participants: vals
                .get(2)
                .cloned()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }))
    }
}

/// Redis server time in milliseconds — the one clock all directors share.
async fn now_ms_via(conn: &mut ConnectionManager) -> Result<u64, StoreError> {
    let t: Vec<u64> = redis::cmd("TIME").query_async(conn).await.map_err(backend)?;
    Ok(t.first().copied().unwrap_or(0) * 1000 + t.get(1).copied().unwrap_or(0) / 1000)
}

fn as_string(v: Option<&redis::Value>) -> Result<String, StoreError> {
    match v {
        Some(redis::Value::Data(bytes)) => Ok(String::from_utf8_lossy(bytes).into_owned()),
        Some(redis::Value::Status(s)) => Ok(s.clone()),
        Some(redis::Value::Int(i)) => Ok(i.to_string()),
        other => Err(StoreError::Backend(format!("expected string, got {other:?}"))),
    }
}

fn as_u64(v: Option<&redis::Value>) -> Result<u64, StoreError> {
    match v {
        Some(redis::Value::Int(i)) => Ok(*i as u64),
        Some(redis::Value::Data(bytes)) => String::from_utf8_lossy(bytes)
            .parse()
            .map_err(|_| StoreError::Backend("non-numeric".into())),
        other => Err(StoreError::Backend(format!("expected int, got {other:?}"))),
    }
}
