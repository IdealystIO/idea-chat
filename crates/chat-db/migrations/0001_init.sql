-- Idea Chat's schema.
--
-- Ids are UUIDv7 minted by the server, so every id sorts by creation time:
-- "newer than what I last read" is an id comparison, and history pages by id.
-- Enumerations are text with CHECK constraints rather than Postgres enums, so
-- adding a value is a migration that cannot lock a table.

-- ---------------------------------------------------------------- accounts

CREATE TABLE users (
    id                uuid PRIMARY KEY,
    email             text NOT NULL,
    password_hash     text NOT NULL,
    handle            text NOT NULL,
    display_name      text NOT NULL,
    avatar            text,
    bio               text NOT NULL DEFAULT '',
    status_presence   text NOT NULL DEFAULT 'online'
                      CHECK (status_presence IN ('online', 'idle', 'busy', 'offline')),
    status_text       text,
    email_verified_at timestamptz,
    -- Bumped on password change and "sign out everywhere"; a session minted
    -- under an older epoch is refused.
    auth_epoch        integer NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email ON users (lower(email));
CREATE UNIQUE INDEX users_handle ON users (lower(handle));

-- One live code per account and purpose. Stored hashed: a database leak must
-- not hand out working verification or reset codes.
CREATE TABLE email_codes (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose    text NOT NULL CHECK (purpose IN ('verify', 'reset')),
    code_hash  text NOT NULL,
    attempts   integer NOT NULL DEFAULT 0,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, purpose)
);

CREATE TABLE devices (
    id         uuid PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name       text NOT NULL,
    public_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX devices_user ON devices (user_id);
CREATE UNIQUE INDEX devices_key ON devices (public_key);

-- ------------------------------------------------------------------ people

-- Both directions are stored: (a, b, 'outgoing') beside (b, a, 'incoming'),
-- (a, b, 'friend') beside (b, a, 'friend'). A block is one-sided and
-- replaces whatever the blocker had.
CREATE TABLE relations (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    other_id   uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       text NOT NULL CHECK (kind IN ('friend', 'incoming', 'outgoing', 'blocked')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, other_id),
    CHECK (user_id <> other_id)
);
CREATE INDEX relations_other ON relations (other_id);

-- ------------------------------------------------------------------ spaces

CREATE TABLE spaces (
    id         uuid PRIMARY KEY,
    name       text NOT NULL,
    icon       text,
    owner_id   uuid NOT NULL REFERENCES users (id),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE space_members (
    space_id  uuid NOT NULL REFERENCES spaces (id) ON DELETE CASCADE,
    user_id   uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role      text NOT NULL DEFAULT 'member' CHECK (role IN ('member', 'admin', 'owner')),
    -- The member's own order for their space switcher.
    position  integer NOT NULL DEFAULT 0,
    joined_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (space_id, user_id)
);
CREATE INDEX space_members_user ON space_members (user_id);

CREATE TABLE categories (
    id       uuid PRIMARY KEY,
    space_id uuid NOT NULL REFERENCES spaces (id) ON DELETE CASCADE,
    name     text NOT NULL,
    position integer NOT NULL DEFAULT 0
);
CREATE INDEX categories_space ON categories (space_id);

CREATE TABLE invites (
    code       text PRIMARY KEY,
    space_id   uuid NOT NULL REFERENCES spaces (id) ON DELETE CASCADE,
    created_by uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    uses       integer NOT NULL DEFAULT 0,
    max_uses   integer,
    expires_at timestamptz,
    revoked    boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX invites_space ON invites (space_id);

-- ---------------------------------------------------------------- channels

CREATE TABLE channels (
    id              uuid PRIMARY KEY,
    kind            text NOT NULL CHECK (kind IN ('text', 'voice', 'thread', 'direct', 'group')),
    space_id        uuid REFERENCES spaces (id) ON DELETE CASCADE,
    name            text NOT NULL DEFAULT '',
    topic           text NOT NULL DEFAULT '',
    category_id     uuid REFERENCES categories (id) ON DELETE SET NULL,
    position        integer NOT NULL DEFAULT 0,
    parent_id       uuid REFERENCES channels (id) ON DELETE CASCADE,
    starter_id      uuid,
    view_role       text NOT NULL DEFAULT 'member' CHECK (view_role IN ('member', 'admin', 'owner')),
    post_role       text NOT NULL DEFAULT 'member' CHECK (post_role IN ('member', 'admin', 'owner')),
    archived        boolean NOT NULL DEFAULT false,
    last_message_id uuid,
    message_count   integer NOT NULL DEFAULT 0,
    created_by      uuid NOT NULL REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- A space channel has a space; a conversation does not; a thread has a
    -- parent.
    CHECK ((kind IN ('direct', 'group')) = (space_id IS NULL)),
    CHECK ((kind = 'thread') = (parent_id IS NOT NULL))
);
CREATE INDEX channels_space ON channels (space_id);
CREATE INDEX channels_parent ON channels (parent_id);

-- Who is in a direct or group conversation.
CREATE TABLE channel_members (
    channel_id uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    joined_at  timestamptz NOT NULL DEFAULT now(),
    -- A conversation someone closed stays theirs but leaves their list until
    -- a new message reopens it.
    hidden     boolean NOT NULL DEFAULT false,
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX channel_members_user ON channel_members (user_id);

-- At most one direct conversation per pair, found by (lower id, higher id).
CREATE TABLE direct_pairs (
    user_lo    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    user_hi    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    channel_id uuid NOT NULL UNIQUE REFERENCES channels (id) ON DELETE CASCADE,
    PRIMARY KEY (user_lo, user_hi),
    CHECK (user_lo < user_hi)
);

-- ---------------------------------------------------------------- messages

CREATE TABLE messages (
    id                uuid PRIMARY KEY,
    channel_id        uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    author_id         uuid NOT NULL REFERENCES users (id),
    kind              text NOT NULL DEFAULT 'text' CHECK (kind IN ('text', 'call', 'joined', 'thread_started')),
    -- The call record or the started thread, for the non-text kinds.
    detail            jsonb,
    body              text NOT NULL DEFAULT '',
    reply_to          uuid,
    mentions          uuid[] NOT NULL DEFAULT '{}',
    mentions_everyone boolean NOT NULL DEFAULT false,
    -- The thread grown from this message.
    thread_id         uuid REFERENCES channels (id) ON DELETE SET NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    edited_at         timestamptz,
    search            tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED
);
CREATE INDEX messages_channel ON messages (channel_id, id);
CREATE INDEX messages_search ON messages USING gin (search);
CREATE INDEX messages_mentions ON messages USING gin (mentions);

ALTER TABLE channels
    ADD CONSTRAINT channels_starter FOREIGN KEY (starter_id) REFERENCES messages (id) ON DELETE SET NULL;

-- Uploaded before the message that carries it, then claimed by it. An upload
-- nobody claims is swept.
CREATE TABLE attachments (
    id           uuid PRIMARY KEY,
    uploader_id  uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    message_id   uuid REFERENCES messages (id) ON DELETE CASCADE,
    purpose      text NOT NULL DEFAULT 'message' CHECK (purpose IN ('message', 'avatar', 'icon')),
    object_key   text NOT NULL UNIQUE,
    name         text NOT NULL,
    size         bigint NOT NULL,
    content_type text NOT NULL,
    width        integer,
    height       integer,
    -- Set once the object is confirmed in the bucket.
    uploaded     boolean NOT NULL DEFAULT false,
    position     integer NOT NULL DEFAULT 0,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX attachments_message ON attachments (message_id);

CREATE TABLE reactions (
    message_id uuid NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    emoji      text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (message_id, user_id, emoji)
);

CREATE TABLE read_states (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    channel_id uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    last_read  uuid,
    mentions   integer NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, channel_id)
);

-- Spaces and channels someone muted.
CREATE TABLE mutes (
    user_id   uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    PRIMARY KEY (user_id, target_id)
);

-- ------------------------------------------------------------------- calls

-- One instance of a call: a relay room and its own end-to-end key. A new one
-- starts when someone joins a channel with nobody in it, and ends when the
-- last person leaves.
CREATE TABLE call_rooms (
    id         uuid PRIMARY KEY,
    channel_id uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    started_by uuid NOT NULL REFERENCES users (id),
    started_at timestamptz NOT NULL DEFAULT now(),
    ended_at   timestamptz,
    -- The record in a conversation's stream.
    message_id uuid REFERENCES messages (id) ON DELETE SET NULL,
    -- Everyone who was in it at some point.
    participants uuid[] NOT NULL DEFAULT '{}'
);
CREATE UNIQUE INDEX call_rooms_live ON call_rooms (channel_id) WHERE ended_at IS NULL;

-- Who is ringing in a conversation's call.
CREATE TABLE rings (
    room_id    uuid PRIMARY KEY REFERENCES call_rooms (id) ON DELETE CASCADE,
    channel_id uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    started_by uuid NOT NULL REFERENCES users (id),
    started_at timestamptz NOT NULL DEFAULT now(),
    ringing    uuid[] NOT NULL DEFAULT '{}'
);

-- Live connections: what presence is computed from. A server instance
-- heartbeats its own rows; rows whose heartbeat stops are swept, so an
-- instance that dies takes its people offline within a minute.
CREATE TABLE connections (
    id           uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id    uuid REFERENCES devices (id) ON DELETE SET NULL,
    instance     text NOT NULL,
    idle         boolean NOT NULL DEFAULT false,
    connected_at timestamptz NOT NULL DEFAULT now(),
    heartbeat_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX connections_user ON connections (user_id);

-- Who is in which call, by the connection that joined it: a connection that
-- goes away takes its person out of the call. One call per person.
CREATE TABLE voice_states (
    user_id       uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    channel_id    uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    room_id       uuid NOT NULL REFERENCES call_rooms (id) ON DELETE CASCADE,
    connection_id uuid NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    device_id     uuid NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    muted         boolean NOT NULL DEFAULT false,
    deafened      boolean NOT NULL DEFAULT false,
    camera        boolean NOT NULL DEFAULT false,
    screen        boolean NOT NULL DEFAULT false,
    joined_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX voice_states_channel ON voice_states (channel_id);
