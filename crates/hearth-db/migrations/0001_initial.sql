-- Hearth's initial schema. See docs/data-model.md for the reasoning behind
-- each decision; this file carries only the reasons that would be lost if the
-- SQL were read on its own.

-- Case-insensitive handles and emails. Folding case in the application is the
-- version of this that eventually gets forgotten in one query.
CREATE EXTENSION IF NOT EXISTS citext;

-- ===========================================================================
-- Identity
-- ===========================================================================

CREATE TABLE users (
    id                uuid PRIMARY KEY,
    handle            citext UNIQUE NOT NULL,
    display_name      text NOT NULL,
    email             citext UNIQUE,
    email_verified_at timestamptz,
    avatar_key        text,
    is_instance_admin boolean NOT NULL DEFAULT false,
    created_at        timestamptz NOT NULL DEFAULT now(),
    -- Tombstone rather than delete: messages stay attributed to an account
    -- that renders as "Deleted account". The product's own copy — "their
    -- messages stay where they are" — says history is not rewritten.
    deleted_at        timestamptz,
    -- Applies to live accounts only. The rule describes what a person may
    -- CHOOSE; a tombstone's handle is generated (and longer than this allows),
    -- so holding it to the same shape would make deletion impossible.
    CONSTRAINT users_handle_shape
        CHECK (deleted_at IS NOT NULL OR handle ~ '^[a-z0-9_]{2,32}$')
);

-- Separate from the account so one user can hold several methods, and so
-- adding a method later is not a `users` migration.
CREATE TABLE credentials (
    id           uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind         text NOT NULL,
    secret       text,          -- password: argon2id PHC string
    issuer       text,          -- oidc
    subject      text,          -- oidc
    confirmed_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT credentials_kind CHECK (kind IN ('password', 'oidc', 'totp')),
    -- One identity per (issuer, subject): an OIDC subject cannot be re-bound
    -- to a second account.
    CONSTRAINT credentials_oidc_identity UNIQUE (issuer, subject)
);
-- One password and one TOTP per user — but several OIDC providers are
-- legitimate, so this is partial rather than UNIQUE (user_id, kind), which
-- would have forbidden the second provider.
CREATE UNIQUE INDEX credentials_one_per_kind
    ON credentials (user_id, kind) WHERE kind IN ('password', 'totp');

CREATE TABLE auth_events (
    id         uuid PRIMARY KEY,
    user_id    uuid REFERENCES users(id) ON DELETE SET NULL,
    kind       text NOT NULL,
    ip         inet,
    user_agent text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX auth_events_by_user ON auth_events (user_id, created_at DESC);

-- ===========================================================================
-- Spaces and membership
-- ===========================================================================

CREATE TABLE spaces (
    id          uuid PRIMARY KEY,
    slug        citext UNIQUE NOT NULL,
    name        text NOT NULL,
    description text,
    icon_key    text,
    created_by  uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    archived_at timestamptz,
    CONSTRAINT spaces_slug_shape CHECK (slug ~ '^[a-z0-9-]{2,48}$')
);

CREATE TABLE space_members (
    space_id  uuid NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    user_id   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role      text NOT NULL DEFAULT 'member',
    joined_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (space_id, user_id),
    CONSTRAINT space_members_role CHECK (role IN ('owner', 'moderator', 'member'))
);
CREATE INDEX space_members_by_user ON space_members (user_id);

-- ===========================================================================
-- Channels — one table for space channels and direct messages
-- ===========================================================================

CREATE TABLE channels (
    id              uuid PRIMARY KEY,
    space_id        uuid REFERENCES spaces(id) ON DELETE CASCADE,
    kind            text NOT NULL,
    name            text,
    topic           text,
    position        int NOT NULL DEFAULT 0,
    -- Denormalised so "does this channel have unread" compares two ids rather
    -- than aggregating over messages. Written in the same transaction as the
    -- message insert.
    last_message_id uuid,
    -- Participant ids sorted and joined. Makes "open the DM with X" one
    -- indexed lookup, and a duplicate DM impossible rather than unlikely.
    dm_key          text UNIQUE,
    created_at      timestamptz NOT NULL DEFAULT now(),
    archived_at     timestamptz,
    CONSTRAINT channels_kind CHECK (kind IN ('text', 'voice', 'video', 'share', 'dm')),
    -- A DM has no space and carries a dm_key; everything else is the inverse.
    CONSTRAINT channels_dm_shape CHECK ((space_id IS NULL) = (kind = 'dm')),
    CONSTRAINT channels_dm_key_shape CHECK ((dm_key IS NULL) = (kind <> 'dm')),
    CONSTRAINT channels_named CHECK (kind = 'dm' OR name IS NOT NULL)
);
CREATE INDEX channels_by_space ON channels (space_id, position);

-- Populated for DMs. Space channels derive membership from space_members —
-- there are no private channels yet, and adding them means populating this
-- table rather than reshaping it.
CREATE TABLE channel_members (
    channel_id uuid NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX channel_members_by_user ON channel_members (user_id);

-- ===========================================================================
-- Messages
-- ===========================================================================

CREATE TABLE messages (
    -- UUIDv7: this column IS the sort key and the pagination cursor.
    id             uuid PRIMARY KEY,
    channel_id     uuid NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    author_id      uuid REFERENCES users(id) ON DELETE SET NULL,
    -- NULL ⇒ a root message. Set ⇒ a reply in that message's thread.
    thread_root_id uuid REFERENCES messages(id) ON DELETE CASCADE,
    body           text,
    reply_count    int NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    edited_at      timestamptz,
    deleted_at     timestamptz,
    CONSTRAINT messages_body_or_tombstone CHECK (deleted_at IS NOT NULL OR body IS NOT NULL)
);
-- The channel view shows roots only; replies live in the thread panel.
CREATE INDEX messages_channel_page ON messages (channel_id, id DESC)
    WHERE thread_root_id IS NULL;
CREATE INDEX messages_thread ON messages (thread_root_id, id)
    WHERE thread_root_id IS NOT NULL;

ALTER TABLE channels
    ADD CONSTRAINT channels_last_message
    FOREIGN KEY (last_message_id) REFERENCES messages(id) ON DELETE SET NULL;

CREATE TABLE attachments (
    id           uuid PRIMARY KEY,
    message_id   uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename     text NOT NULL,
    content_type text NOT NULL,
    byte_size    bigint NOT NULL,
    storage_key  text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX attachments_by_message ON attachments (message_id);

-- ===========================================================================
-- Read state, invites, moderation
-- ===========================================================================

CREATE TABLE read_state (
    user_id              uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id           uuid NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    last_read_message_id uuid,
    mention_count        int NOT NULL DEFAULT 0,
    updated_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, channel_id)
);

CREATE TABLE invites (
    code       text PRIMARY KEY,
    space_id   uuid NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    created_by uuid REFERENCES users(id) ON DELETE SET NULL,
    expires_at timestamptz NOT NULL,
    max_uses   int NOT NULL DEFAULT 1,
    use_count  int NOT NULL DEFAULT 0,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT invites_uses_sane CHECK (use_count >= 0 AND max_uses >= 1)
);
CREATE INDEX invites_by_space ON invites (space_id);

-- "Mute for me" — a personal filter, invisible to its subject.
CREATE TABLE user_mutes (
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    muted_id   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, muted_id),
    CONSTRAINT user_mutes_not_self CHECK (user_id <> muted_id)
);

-- "Mute in rooms for an hour", "Remove from space" — audited, because they
-- are done TO someone.
CREATE TABLE moderation_actions (
    id         uuid PRIMARY KEY,
    space_id   uuid NOT NULL REFERENCES spaces(id) ON DELETE CASCADE,
    target_id  uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id   uuid REFERENCES users(id) ON DELETE SET NULL,
    kind       text NOT NULL,
    expires_at timestamptz,
    reason     text,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT moderation_kind CHECK (kind IN ('timeout', 'remove', 'role_change'))
);
CREATE INDEX moderation_by_target ON moderation_actions (space_id, target_id, created_at DESC);

-- ===========================================================================
-- Instance settings — exactly one row, enforced.
-- ===========================================================================

CREATE TABLE instance_settings (
    id                integer PRIMARY KEY DEFAULT 1,
    registration_mode text NOT NULL DEFAULT 'invite_only',
    instance_name     text NOT NULL DEFAULT 'Hearth',
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT instance_settings_singleton CHECK (id = 1),
    CONSTRAINT registration_mode CHECK (registration_mode IN ('invite_only', 'open', 'closed'))
);
INSERT INTO instance_settings (id) VALUES (1);
