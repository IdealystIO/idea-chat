# Data model and authentication — design

Status: **proposal, nothing implemented.** This is the design to argue with
before any of it is built. Open questions are collected at the end; one of them
(§12.1, end-to-end encryption) changes enough of the model that I'd want an
answer before writing migrations.

Scope: identity, spaces, channels, messages, membership, read state, and every
authentication flow. Out of scope for this document: media storage internals,
search indexing, push notifications, and the voice media path (which already
exists — see §9 for how the two tiers meet).

---

## 1. What the framework already provides

The design below is deliberately thin, because idealyst carries most of the
mechanism already. Building a parallel stack would be waste.

| Need | What we use | What we still write |
| --- | --- | --- |
| RPC | `#[server]` async fns, isomorphic | the function bodies |
| Live events | `#[channel]` — WebSocket duplex, extractors resolved at upgrade | the protocol enum + fan-out |
| Cross-node fan-out | `pubsub` SDK (redis/postgres backend) | topic naming |
| Sessions | `server_kit::Sessions` — opaque CSPRNG token, httpOnly cookie for web, bearer for native, TTL, guard middleware | the principal type |
| Authorization | `Auth<P>`, `Role<M>`, `require("prefix/")`, `csrf_guard` | role marker types |
| Rate limits | `#[server(tags(limit = "5/min"))]` + `rate_limit().key_by(…)` | the limits themselves |
| Transactional email | `email` SDK | the templates |
| Native secret storage | `credentials` (Keychain/Keystore) | nothing |
| Offline send | `sync` SDK's outbox, or a simpler local queue | see §10 |

**No database opinion ships with the framework.** `server-kit`'s own worked
example is `State<Db>` over an `sqlx::PgPool`, and that's what §3 assumes.

Note a divergence to resolve: `voice-coord` (already in this repo) uses
`tokio-postgres` + `deadpool`, not `sqlx`. I'd use **sqlx** for the chat tier —
compile-time-checked queries and built-in migrations are worth a lot on a
schema this size — and leave `voice-coord` alone rather than churn working
code. Two Postgres clients in one workspace is a real (if small) cost; flagged
in §12.

## 2. Constraints that actually drive the design

Three, and they do most of the work:

**Self-hosting has to be genuinely easy.** This is the stated point of the
project. Concretely it means: no mandatory external identity provider, no
mandatory SMTP, no default password anywhere, and a first-run that can't leave
an instance briefly open to the internet. §7 is shaped almost entirely by this.

**The product is anti-bloat, and the schema should be too.** The design is
emphatic that notifications are one setting rather than a per-channel matrix.
The same conviction applies to permissions: **three roles, no capability
bitfields** (§8). If a permission matrix is wanted later it is an additive
migration; starting with one guarantees we ship the thing the product exists to
avoid.

**A voice tier already exists and is not mine to redesign.** `voice-server` is
a binary WebSocket audio relay with its own coordination store. The chat tier
must authorise voice joins without coupling to it — §9.

## 3. Entity model

Postgres 16+, `citext` extension enabled (one line in the first migration —
case-insensitive handles and emails are worth it, and folding case in the
application is the version of this that eventually gets forgotten somewhere).
All ids are **UUIDv7**, generated application-side (`Uuid::now_v7()`).

Why UUIDv7 rather than a Snowflake: it sorts by time, so it doubles as the
message ordering key and the pagination cursor with no separate sequence
column, and it needs no node-id configuration — which a self-hoster would
otherwise have to understand. Why application-side rather than `DEFAULT`: it
lets the **client** generate a message id before sending, which makes retries
idempotent and lets the optimistic local echo carry its final id (§10). The
server validates that an embedded timestamp is within ±5 minutes of its own
clock and rejects otherwise, so a client can't forge its position in history.

### 3.1 Identity

```sql
CREATE TABLE users (
    id              uuid PRIMARY KEY,
    handle          citext UNIQUE NOT NULL,      -- '@ana', instance-unique, immutable
    display_name    text NOT NULL,
    email           citext UNIQUE,               -- NULL: valid, see §7.6
    email_verified_at timestamptz,
    avatar_key      text,                        -- object-store key, NULL = initials
    is_instance_admin boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- Account deletion tombstones rather than deleting: messages stay
    -- attributed to a user that renders as "Deleted account". Hard delete
    -- would either orphan or rewrite history, and the product's own copy
    -- ("Their messages stay where they are") says history is not rewritten.
    deleted_at      timestamptz
);

-- Credentials are separate from the account so one user can hold several
-- (password AND oidc), and so adding a method later is not a users migration.
CREATE TABLE credentials (
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        text NOT NULL,                   -- 'password' | 'oidc' | 'totp'
    -- password: argon2id PHC string. oidc: NULL.
    secret      text,
    -- oidc: issuer + subject, unique together so a subject can't be re-bound.
    issuer      text,
    subject     text,
    confirmed_at timestamptz,                    -- totp: set on first valid code
    created_at  timestamptz NOT NULL DEFAULT now(),
    -- One identity per (issuer, subject): an OIDC subject cannot be re-bound
    -- to a second account.
    UNIQUE (issuer, subject)
);
-- At most one password and one TOTP per user — but several OIDC providers are
-- legitimate, so this is a partial index rather than UNIQUE (user_id, kind),
-- which would have forbidden the second provider.
CREATE UNIQUE INDEX ON credentials (user_id, kind) WHERE kind IN ('password', 'totp');

-- Security review is a self-hoster's job and they need the trail for it.
CREATE TABLE auth_events (
    id          uuid PRIMARY KEY,
    user_id     uuid REFERENCES users(id) ON DELETE SET NULL,
    kind        text NOT NULL,   -- login_ok | login_fail | logout | reset_* | …
    ip          inet,
    user_agent  text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON auth_events (user_id, created_at DESC);
```

**Sessions are not in Postgres.** `server_kit::Sessions` rides the `cache`
contract (Redis in production, in-memory in dev), which is the right store for
a KV-with-TTL. To support "sign out everywhere" we keep a Redis set per user
(`user:{id}:sessions`) alongside, so revocation is one `SMEMBERS` + `DEL`. A
dual-write into Postgres would buy session *history*, which `auth_events`
already covers.

### 3.2 Spaces and membership

```sql
CREATE TABLE spaces (
    id          uuid PRIMARY KEY,
    slug        citext UNIQUE NOT NULL,          -- used in invite URLs
    name        text NOT NULL,
    description text,
    icon_key    text,
    created_by  uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    archived_at timestamptz
);

CREATE TABLE space_members (
    space_id    uuid NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role        text NOT NULL DEFAULT 'member',  -- owner | moderator | member
    joined_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (space_id, user_id)
);
CREATE INDEX ON space_members (user_id);         -- "which spaces am I in"
```

### 3.3 Channels — one table for rooms and DMs

The design draws text channels, voice rooms, video rooms, share rooms and
direct messages as five things. They are one thing with a `kind`, and DMs are
the same thing with no space:

```sql
CREATE TABLE channels (
    id              uuid PRIMARY KEY,
    space_id        uuid REFERENCES spaces(id) ON DELETE CASCADE,  -- NULL ⇒ DM
    kind            text NOT NULL,               -- text | voice | video | share | dm
    name            text,                        -- NULL for DMs (derived from members)
    topic           text,
    position        int NOT NULL DEFAULT 0,
    -- Denormalised so "does this channel have unread" is a comparison of two
    -- ids rather than an aggregate over messages. Updated in the same
    -- transaction as the insert.
    last_message_id uuid,
    -- Canonical key for a DM: the participant ids sorted and joined. Makes
    -- "open the DM with X" one indexed lookup and makes a duplicate DM
    -- impossible rather than merely unlikely.
    dm_key          text UNIQUE,
    created_at      timestamptz NOT NULL DEFAULT now(),
    archived_at     timestamptz,
    CHECK ((space_id IS NULL) = (kind = 'dm')),
    CHECK ((dm_key  IS NULL) = (kind <> 'dm'))
);
CREATE INDEX ON channels (space_id, position);

-- Only populated for DMs in v1. Space channels derive membership from
-- space_members — the design has no private channels, and adding them later
-- means populating this table, not reshaping it.
CREATE TABLE channel_members (
    channel_id  uuid NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX ON channel_members (user_id);
```

### 3.4 Messages and threads

A thread is not an entity. A reply points at the message it hangs off, which is
exactly what the design shows — a badge on a message, opening a panel of its
replies.

```sql
CREATE TABLE messages (
    id            uuid PRIMARY KEY,              -- UUIDv7 ⇒ this IS the sort key
    channel_id    uuid NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    author_id     uuid REFERENCES users(id) ON DELETE SET NULL,
    -- NULL ⇒ a root message. Set ⇒ a reply in that message's thread.
    thread_root_id uuid REFERENCES messages(id) ON DELETE CASCADE,
    body          text,                          -- NULL once deleted
    -- Denormalised for the "3 replies" badge; incremented with the insert.
    reply_count   int NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    edited_at     timestamptz,
    deleted_at    timestamptz,
    CHECK (deleted_at IS NOT NULL OR body IS NOT NULL)
);
-- The one index the read path actually needs: a channel's messages, newest
-- first, which is also the pagination cursor.
CREATE INDEX ON messages (channel_id, id DESC) WHERE thread_root_id IS NULL;
CREATE INDEX ON messages (thread_root_id, id) WHERE thread_root_id IS NOT NULL;

CREATE TABLE attachments (
    id            uuid PRIMARY KEY,
    message_id    uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename      text NOT NULL,
    content_type  text NOT NULL,
    byte_size     bigint NOT NULL,
    storage_key   text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
```

Pagination is `WHERE channel_id = $1 AND id < $cursor ORDER BY id DESC LIMIT 50`
— no offset, no ordering column, stable under concurrent inserts.

### 3.5 Read state, invites, moderation

```sql
CREATE TABLE read_state (
    user_id              uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id           uuid NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    last_read_message_id uuid,
    mention_count        int NOT NULL DEFAULT 0,
    updated_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, channel_id)
);

CREATE TABLE invites (
    code        text PRIMARY KEY,                -- 'quiet-porch' — human-readable
    space_id    uuid NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    created_by  uuid REFERENCES users(id) ON DELETE SET NULL,
    expires_at  timestamptz NOT NULL,            -- default now() + 7 days
    max_uses    int NOT NULL DEFAULT 1,          -- the design: "only work once"
    use_count   int NOT NULL DEFAULT 0,
    revoked_at  timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- "Mute for me" — a personal filter, invisible to its subject.
CREATE TABLE user_mutes (
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    muted_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, muted_id)
);

-- "Mute in rooms for an hour", "Remove from space" — moderator actions, which
-- are audited because they are done TO someone.
CREATE TABLE moderation_actions (
    id          uuid PRIMARY KEY,
    space_id    uuid NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    target_id   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id    uuid REFERENCES users(id) ON DELETE SET NULL,
    kind        text NOT NULL,                   -- timeout | remove | role_change
    expires_at  timestamptz,                     -- timeouts only
    reason      text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON moderation_actions (space_id, target_id, created_at DESC);
```

### 3.6 What lives in Redis, not Postgres

Ephemeral by nature; writing it to disk would be churn for data that is wrong
the moment the process restarts.

| Key | Value | TTL |
| --- | --- | --- |
| `sess:{token}` | serialized `Principal` | session TTL (sliding) |
| `user:{id}:sessions` | set of tokens | — (pruned on revoke) |
| `presence:{user_id}` | `online` \| `away` | 60s, refreshed by the live socket |
| `typing:{channel_id}:{user_id}` | `1` | 6s |
| voice room state | *(existing `voice-coord`)* | — |

Presence is derived, not stored: a user with at least one live socket is
`online`, one idle past a threshold is `away`, and absence of the key is
`offline`. There is no "set yourself invisible" in the design, so there is no
column for it.

## 4. Roles — three, deliberately

`owner` / `moderator` / `member` per space, plus an instance-level
`is_instance_admin`.

| | member | moderator | owner |
| --- | --- | --- | --- |
| read, post, join rooms | ✓ | ✓ | ✓ |
| mute-in-rooms, remove member | | ✓ | ✓ |
| create/rename/archive channels | | ✓ | ✓ |
| promote/demote moderators | | | ✓ |
| delete the space, transfer ownership | | | ✓ |

These become `Role<Moderator>` marker types on the server fns, so the
authorization is in the signature rather than in the body.

No per-channel overrides and no capability bitfield. That is a product
decision as much as a schema one, and it is the same decision the design makes
about notifications.

## 5. Authentication — the model

Two layers, kept apart:

- **Credential** — proves who you are, once. Password (argon2id), OIDC, or a
  one-time link. Rows in `credentials`.
- **Session** — carries that proof afterwards. `server_kit::Sessions`:
  32 bytes of CSPRNG, opaque, stored in Redis with a TTL. **Not a JWT** — a
  self-hosted admin needs "log this person out now" to actually work, and a
  stateless token cannot offer that without a revocation list, which is a
  session store wearing a disguise.

Transport differs by client and the framework handles the branch: web gets an
httpOnly `Secure` `SameSite=Lax` cookie (the secret never reaches JS, so XSS
cannot read it), native announces `x-idealyst-client: native` and gets a bearer
token for the OS keystore via the `credentials` SDK. One `login` serves both.

The principal we put in the session is deliberately small — it is read on every
request:

```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct Principal {
    pub user_id: Uuid,
    pub handle: String,
    pub is_instance_admin: bool,
}
```

Space roles are **not** in the principal. They change while a session is live
(someone gets promoted, or removed from a space), and a session that cached
them would keep enforcing stale permissions until it expired. They're read per
request from `space_members`, which is a primary-key lookup and belongs behind
a short-TTL cache if it ever shows up in a profile.

## 6. Password handling

argon2id, `m=19456 KiB, t=2, p=1` (the OWASP-recommended floor), PHC-string
encoded so the parameters travel with the hash and can be raised later without
a migration — on next successful login, if the stored params are below current
policy, re-hash transparently.

Login is constant-time in the "does this user exist" sense: if the handle is
unknown we still verify against a dummy hash before returning the same error,
so response timing doesn't enumerate accounts. The error is always "invalid
handle or password", never which half was wrong.

Rate limits, declared on the endpoints:

| endpoint | limit | keyed by |
| --- | --- | --- |
| `login` | 5/min, 20/hour | handle + IP |
| `request_password_reset` | 3/hour | email |
| `redeem_invite` | 10/hour | IP |
| `send_message` | 30/min | user |
| `create_space` | 3/hour | user |

## 7. Authentication flows

### 7.1 First run — instance bootstrap

The flow self-hosted apps most often get wrong, usually by shipping
`admin/admin` or by leaving registration open until someone claims the
instance.

On boot, if `users` is empty, the server mints a single-use claim token (32
CSPRNG bytes, 15-minute TTL, held in Redis) and prints it:

```
Hearth is running, and has no accounts yet.
Claim this instance:  http://localhost:8080/claim/3f9a…c21b
This link works once and expires in 15 minutes. Restart to issue a new one.
```

Visiting it creates the first account as `is_instance_admin`. For automated
deploys, `HEARTH_BOOTSTRAP_TOKEN` sets the token instead of generating one.

No default credentials exist at any point, and the window is 15 minutes rather
than "until someone notices".

### 7.2 Registration

An instance setting, defaulting to the conservative option:

- `invite_only` **(default)** — an account can only be created by redeeming a
  valid invite code. This matches the design, whose join dialog leads with
  "Join with a link".
- `open` — anyone can register. Requires email verification.
- `closed` — no new accounts; an admin creates them.

### 7.3 Sign up by invite

```
client                          server
  │  GET /j/quiet-porch
  ├──────────────────────────────▶  look up invites.code
  │                                 reject: unknown | expired | revoked
  │                                 | use_count >= max_uses
  │  ◀── space name, icon, inviter ─┤   (enough to show "Join The Cabin?")
  │
  │  register(code, handle, display_name, password, email?)
  ├──────────────────────────────▶  ── BEGIN ──
  │                                  insert users, credentials(password)
  │                                  insert space_members(role='member')
  │                                  UPDATE invites SET use_count = use_count + 1
  │                                    WHERE code = $1 AND use_count < max_uses
  │                                    AND revoked_at IS NULL AND expires_at > now()
  │                                  ── COMMIT ──   (0 rows updated ⇒ roll back)
  │  ◀── session ticket ────────────┤
```

The conditional `UPDATE … WHERE use_count < max_uses` inside the transaction is
what makes "only work once" true under concurrency. Checking first and updating
after would let two simultaneous redemptions both pass.

An already-signed-in user redeeming a code just gets `space_members` inserted.

### 7.4 Sign in

`login(handle_or_email, password)` → verify argon2id → `sessions.start(&principal)`
→ cookie or bearer per client kind → `auth_events` row either way.

### 7.5 Session lifetime and revocation

30-day TTL, sliding on use. `GET sessions` lists the user's live sessions with
device label, IP and last-seen. `revoke(session_id)` or `revoke_all_others()`
deletes from Redis, so the next request from that device 401s immediately.

Changing a password revokes every session except the current one.

### 7.6 Password reset — and the no-SMTP case

Self-hosted instances often have no working mail. Email is therefore **optional
on the account** (`users.email` is nullable), and reset has two paths:

- **With email**: standard single-use token, 1-hour TTL, in Redis. The response
  is identical whether or not the address exists — no enumeration. Sent via the
  `email` SDK.
- **Without email**: an instance admin issues a one-time reset link from the
  admin surface and hands it over out of band. Same token mechanism, different
  delivery.

An instance with no SMTP configured hides the "email me a link" affordance
rather than showing a button that silently fails.

### 7.7 OIDC (optional)

Off by default. When configured, standard authorization-code + PKCE; on
callback we look up `credentials(kind='oidc', issuer, subject)` and either sign
in the linked account or create one. An instance can require OIDC exclusively,
in which case password credentials are refused at login even if rows exist.

Linking OIDC to an existing account requires being signed in already —
otherwise "sign in with X" on an unverified email is an account-takeover.

### 7.8 Two-factor (designed now, shipped later)

`credentials(kind='totp')` holds the secret; recovery codes are argon2id hashes
in a `recovery_codes` table. The schema supports it today so enabling it is not
a migration; the flow is not v1 work.

## 8. Live protocol

**One socket per client**, not one per channel — a user in six channels should
not hold six sockets.

```rust
#[channel]
async fn stream(mut s: Socket<ClientEvent, ServerEvent>, me: Auth<Principal>)
    -> Result<(), ServerError>
```

Auth is resolved at upgrade by the same middleware chain as every other
endpoint, so an unauthenticated socket never opens.

```rust
enum ClientEvent {
    Resume { cursors: Vec<(ChannelId, Option<MessageId>)> },
    Typing { channel: ChannelId },
    MarkRead { channel: ChannelId, message: MessageId },
    Heartbeat,
}

enum ServerEvent {
    Ready { user: UserSummary, spaces: Vec<SpaceSummary> },
    MessageCreated(Message),
    MessageEdited { id: MessageId, body: String, edited_at: Timestamp },
    MessageDeleted { id: MessageId, channel: ChannelId },
    TypingStarted { channel: ChannelId, user: UserId },
    PresenceChanged { user: UserId, status: Presence },
    VoiceStateChanged { channel: ChannelId, participants: Vec<UserId> },
    MemberChanged { space: SpaceId, user: UserId, role: Option<Role> },
    ChannelChanged(ChannelSummary),
    ReadStateChanged { channel: ChannelId, last_read: MessageId },
}
```

Fan-out uses the `pubsub` SDK over topics `space:{id}` and `channel:{id}`, so
an event produced on one instance reaches a client connected to another. That
is the SDK's headline use case and it is why this scales past one process
without us writing anything.

**Gap recovery.** A dropped socket must not silently lose messages. On
reconnect the client sends `Resume` with its last seen message id per channel;
the server replies with everything after each cursor (capped, with a
`truncated` flag that tells the client to refetch that channel cold). Because
message ids are UUIDv7 and therefore ordered, "everything after" is one indexed
range scan — no per-connection sequence numbers to maintain.

## 9. Where chat meets the existing voice tier

`voice-server` is a separate binary that must not grow a dependency on the chat
database, and clients must not be able to join a voice room they have no right
to be in. A short-lived capability token bridges the two:

1. Client calls `join_voice(channel_id)` on the API.
2. API checks `space_members` for the channel's space, then mints
   `HMAC-SHA256(instance_secret, user_id | channel_id | exp)` with a 60-second
   expiry, and returns it with the assigned voice node's address (from the
   existing director).
3. Client connects to the voice node presenting the token.
4. `voice-server` verifies the HMAC with the shared secret — **no database
   call, no chat dependency** — and admits the connection.

The chat tier stays the only thing that knows about membership; the voice tier
stays the only thing that knows about audio. The token is the entire contract.

## 10. Client model, and what changes in the app

Today `crates/hearth/src/model.rs` is a presentation model over the design's
fixture — display-name keys, string timestamps, positional message identity.
The seam it does provide is that views read through accessors rather than the
tables, so this is where the real model lands.

The changes that reach the views:

- `Person` keyed by `UserId`, not display name — which also fixes the current
  bug where renaming someone would re-hash their avatar tint.
- `time: String` becomes a real `Timestamp`, formatted at render.
- `Line.key` (`room:index`) becomes the message's own id, which makes the keyed
  `for` loops correct rather than incidentally correct.

**Optimistic send.** The client generates the message UUIDv7, renders it
immediately, and sends. Because the id is the idempotency key, a retry after a
dropped connection cannot double-post. A failed send marks the existing row
rather than inserting a second one.

Whether to adopt the `sync` SDK for offline history is a judgement call I'd
defer: its outbox and merge policy are exactly right for queued sends, but it
assumes single-writer-per-device and is not real-time, so it composes with
`#[channel]` rather than replacing it. For v1 a local queue plus the live socket
is less machinery, and the SDK is a clean upgrade if offline turns out to
matter.

## 11. Suggested build order

Each step ends somewhere demonstrable, and nothing is written twice.

1. **Schema + migrations**, sqlx. Nothing else.
2. **Auth end to end** — bootstrap, register-by-invite, login, logout, session
   revocation. Verifiable with `curl` before any UI moves.
3. **Read paths** — spaces, channels, message pages — wired behind the existing
   accessors in `model.rs`. The UI should light up with real data and no view
   changes.
4. **Write paths** — send, edit, delete, mark-read, with optimistic ids.
5. **The live socket** — `#[channel]`, fan-out, `Resume`. Two browsers talking.
6. **Membership and moderation** — invites, roles, timeouts, removal.
7. **Voice capability token**, joining the existing tier to real identity.

Steps 1–2 are the ones worth reviewing hardest; everything after is mechanical
by comparison.

## 12. Open questions

**12.1 — End-to-end encryption for DMs.** The biggest fork, and the reason this
is a proposal rather than a migration. The design says direct messages "stay
between the two of you", which reads as a *policy* promise, but if it is meant
as a *cryptographic* one then the server cannot index DM bodies, server-side
search over DMs is gone, and multi-device requires key exchange and per-device
identity — a different project, not a later ticket. **This model assumes
transport encryption and server-side storage, not E2EE.** Please confirm, or
say if E2EE is a goal and I'll design for it now rather than retrofit.

**12.2 — Federation.** Assumed out of scope: accounts are instance-scoped and a
space belongs to one instance. Worth stating explicitly because it is
effectively irreversible.

**12.3 — SQLite for small instances.** Postgres is already in
`docker-compose.yml`, so I've assumed it. Supporting SQLite too would help the
smallest self-hosters but costs us compile-time-checked queries against two
backends. Worth it?

**12.4 — Two Postgres clients.** `voice-coord` uses `tokio-postgres`; I'd use
`sqlx`. Live with the duplication, or converge one?

**12.5 — Retention.** Does an instance need message retention limits (age or
count) for disk, or is history kept forever until someone deletes it?

**12.6 — Handles.** Immutable `@handle` plus a mutable display name, as above?
Or display name only, Discord-style, with a discriminator? Immutable handles
are simpler to reason about in mentions and audit trails; they're also a thing
people ask to change.
